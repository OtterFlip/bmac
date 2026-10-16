// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Runs the jobs targeted at this host: claims each admitted one, starts its
//! script under the supervisor, keeps the lease renewed with what the script
//! is doing, carries out cancel orders from the job record, and records how
//! the job ended.
//!
//! If renewing fails because the lease expired, the script keeps running:
//! the job is already marked interrupted for the operator, and the holder
//! still records how it ended.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bmac_core::job::{Answer, EventBody, JobEvent, JobState};
use bmac_core::{HostSlot, JobId};
use bmac_proto::api::{ApiError, ErrorCode, Role};
use bmac_proto::operations::OperationKind;
use bmac_proto::stream::StreamBody;
use tokio::sync::{Notify, mpsc, oneshot, watch};

use crate::engine::events::EventHub;
use crate::jobs::{JobRecord, Jobs, Progress};
use crate::store::{Entry, Store};
use crate::supervisor::{JobSink, Outcome, RunView, Supervisor};

/// How often admitted jobs are looked for, besides when one is launched here.
pub const POLL_EVERY: Duration = Duration::from_secs(2);
/// How often a running job's record is read for a cancel order placed on
/// another host.
pub const CANCEL_CHECK_EVERY: Duration = Duration::from_secs(2);
/// Changes to what a script is doing are gathered for this long before the
/// lease record is rewritten.
const PROGRESS_SETTLE: Duration = Duration::from_millis(200);
/// How often, and how far apart, a job's end is retried when the records
/// refuse it.
const FINISH_TRIES: u32 = 60;
const FINISH_RETRY: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub struct RunnerConfig {
    pub poll_every: Duration,
    pub renew_every: Duration,
    pub cancel_check_every: Duration,
}

impl Default for RunnerConfig {
    fn default() -> RunnerConfig {
        RunnerConfig {
            poll_every: POLL_EVERY,
            renew_every: Duration::from_millis(crate::jobs::LEASE_RENEW_MS as u64),
            cancel_check_every: CANCEL_CHECK_EVERY,
        }
    }
}

enum Write {
    Events(Vec<JobEvent>),
    Flush(oneshot::Sender<()>),
}

/// Stores a running job's events, then publishes them on this host's
/// stream, in order, and tells the runner when what the script is doing
/// changed.
pub struct EngineSink {
    hub: Arc<EventHub>,
    store: Store,
    changes: mpsc::UnboundedSender<JobId>,
    writes: mpsc::UnboundedSender<Write>,
}

impl EngineSink {
    /// Starts the writer task.
    pub fn new(
        hub: Arc<EventHub>,
        store: Store,
    ) -> (Arc<EngineSink>, mpsc::UnboundedReceiver<JobId>) {
        let (changes, changes_rx) = mpsc::unbounded_channel();
        let (writes, mut writes_rx) = mpsc::unbounded_channel::<Write>();
        let (writer_hub, writer_store) = (hub.clone(), store.clone());
        tokio::spawn(async move {
            while let Some(first) = writes_rx.recv().await {
                let mut queue = vec![first];
                while let Ok(more) = writes_rx.try_recv() {
                    queue.push(more);
                }
                let mut events = Vec::new();
                for write in queue {
                    match write {
                        Write::Events(batch) => events.extend(batch),
                        Write::Flush(done) => {
                            store_and_publish(
                                &writer_store,
                                &writer_hub,
                                std::mem::take(&mut events),
                            )
                            .await;
                            let _ = done.send(());
                        }
                    }
                }
                store_and_publish(&writer_store, &writer_hub, events).await;
            }
        });
        (
            Arc::new(EngineSink {
                hub,
                store,
                changes,
                writes,
            }),
            changes_rx,
        )
    }

    pub fn hub(&self) -> &Arc<EventHub> {
        &self.hub
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Waits until every event handed over so far is stored and published.
    pub async fn flush(&self) {
        let (done, wait) = oneshot::channel();
        if self.writes.send(Write::Flush(done)).is_ok() {
            let _ = wait.await;
        }
    }
}

async fn store_and_publish(store: &Store, hub: &EventHub, events: Vec<JobEvent>) {
    if events.is_empty() {
        return;
    }
    let entries = events.iter().map(Entry::job_event).collect();
    if let Err(error) = store.append(entries).await {
        crate::log::error(&format!("cannot store job events: {error}"));
    }
    for event in events {
        hub.publish(StreamBody::JobEvent { event });
    }
}

impl JobSink for EngineSink {
    fn events(&self, events: Vec<JobEvent>) {
        let _ = self.writes.send(Write::Events(events));
    }

    fn changed(&self, job: JobId, _view: &RunView) {
        let _ = self.changes.send(job);
    }
}

/// An operator's answer, and who gave it, on its way to the job's host.
#[derive(Debug, Clone)]
pub struct RelayedAnswer {
    pub user: String,
    pub role: Role,
    pub job: JobId,
    pub request_id: String,
    pub answer: Answer,
}

/// Carries answers to the host running the job.
pub trait AnswerRelay: Send + Sync + 'static {
    fn forward(
        &self,
        to: HostSlot,
        answer: RelayedAnswer,
    ) -> Pin<Box<dyn Future<Output = Result<(), ApiError>> + Send + '_>>;
}

pub struct Runner {
    jobs: Arc<Jobs>,
    relay: Option<Arc<dyn AnswerRelay>>,
    supervisor: Arc<Supervisor>,
    sink: Arc<EngineSink>,
    config: RunnerConfig,
    active: Mutex<HashMap<JobId, Arc<Notify>>>,
    kick: Notify,
}

impl std::fmt::Debug for Runner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runner")
            .field("host", &self.jobs.host())
            .finish_non_exhaustive()
    }
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(work)
        .await
        .unwrap_or_else(|e| Err(ApiError::new(ErrorCode::Internal, e.to_string())))
}

fn progress(view: &RunView) -> Progress {
    Progress {
        phase: view.phase.clone(),
        cancel_allowed: view.cancel_allowed,
        pending_request: view.pending_request.clone(),
        last_seq: view.last_seq,
    }
}

impl Runner {
    pub fn new(
        jobs: Arc<Jobs>,
        supervisor: Arc<Supervisor>,
        sink: Arc<EngineSink>,
        config: RunnerConfig,
    ) -> Arc<Runner> {
        Arc::new(Runner {
            jobs,
            supervisor,
            sink,
            config,
            relay: None,
            active: Mutex::new(HashMap::new()),
            kick: Notify::new(),
        })
    }

    /// Lets answers given here reach jobs running on other hosts. Set before
    /// the runner is shared.
    pub fn with_relay(mut self: Arc<Self>, relay: Arc<dyn AnswerRelay>) -> Arc<Runner> {
        Arc::get_mut(&mut self)
            .expect("the relay is set before the runner is shared")
            .relay = Some(relay);
        self
    }

    pub fn jobs(&self) -> &Arc<Jobs> {
        &self.jobs
    }

    pub fn is_active(&self, job: JobId) -> bool {
        self.active.lock().unwrap().contains_key(&job)
    }

    /// Whether no job runs here; a job leaves only after its end is recorded.
    pub fn is_idle(&self) -> bool {
        self.active.lock().unwrap().is_empty()
    }

    /// Looks for admitted jobs now, as when one was just launched or resumed.
    pub fn kick(&self) {
        self.kick.notify_one();
    }

    /// Has a running job reread its record now, as after a cancel order.
    pub fn nudge(&self, job: JobId) {
        if let Some(wake) = self.active.lock().unwrap().get(&job) {
            wake.notify_one();
        }
    }

    /// Polls for admitted jobs and forwards the supervisor's change notices
    /// until aborted.
    pub fn spawn(
        self: &Arc<Self>,
        mut changes: mpsc::UnboundedReceiver<JobId>,
    ) -> tokio::task::JoinHandle<()> {
        let runner = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(runner.config.poll_every);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = tick.tick() => runner.poll().await,
                    _ = runner.kick.notified() => runner.poll().await,
                    Some(job) = changes.recv() => runner.nudge(job),
                }
            }
        })
    }

    /// What `bmacd` does once when it starts: interrupts the jobs this
    /// host was running, and puts back in line those that asked for the
    /// restart that has since happened (`boot_id` is this boot's). Retries
    /// every `every` while pmxcfs refuses changes, as it does until the
    /// cluster is quorate again, for at most `within`.
    pub fn spawn_recover(
        self: &Arc<Self>,
        boot_id: String,
        every: Duration,
        within: Duration,
    ) -> tokio::task::JoinHandle<()> {
        let runner = Arc::clone(self);
        tokio::spawn(async move {
            let until = tokio::time::Instant::now() + within;
            loop {
                let (done, _) = runner.recover(&boot_id).await;
                if done || tokio::time::Instant::now() >= until {
                    if !done {
                        crate::log::warning(
                            "gave up resuming the jobs that asked for this restart; resume them in the Dashboard",
                        );
                    }
                    return;
                }
                tokio::time::sleep(every).await;
            }
        })
    }

    /// One pass of [`Runner::spawn_recover`]: whether it is done, and the
    /// jobs it resumed.
    pub async fn recover(self: &Arc<Self>, boot_id: &str) -> (bool, Vec<JobId>) {
        let jobs = self.jobs.clone();
        match blocking(move || jobs.interrupt_own()).await {
            Ok(changed) => changed.iter().for_each(|r| self.publish(r)),
            Err(error) => {
                crate::log::warning(&format!(
                    "interrupting the jobs this host was running: {}",
                    error.message
                ));
                return (false, Vec::new());
            }
        }
        let jobs = self.jobs.clone();
        let host = jobs.host();
        let records = match blocking(move || jobs.list()).await {
            Ok(records) => records,
            Err(error) => {
                crate::log::warning(&format!("listing jobs to resume: {}", error.message));
                return (false, Vec::new());
            }
        };
        let (mut done, mut resumed) = (true, Vec::new());
        for record in records {
            let held = record.lease.as_ref().is_some_and(|l| l.holder == host);
            let work = self.supervisor.work_path(record.id);
            if record.state != JobState::Interrupted
                || !held
                || !(crate::native::restarted_since_asked(&work, boot_id)
                    || crate::native::service_restarted_since_asked(&work, std::process::id()))
            {
                continue;
            }
            let jobs = self.jobs.clone();
            let id = record.id;
            match blocking(move || jobs.resume_after_restart(id)).await {
                Ok(record) => {
                    crate::log::info(&format!(
                        "resuming job {id} ({}) after the restart it asked for",
                        record.operation
                    ));
                    self.publish(&record);
                    resumed.push(id);
                }
                Err(error) => {
                    crate::log::warning(&format!("resuming job {id}: {}", error.message));
                    done &= error.code == ErrorCode::Conflict;
                }
            }
        }
        if !resumed.is_empty() {
            self.kick();
        }
        (done, resumed)
    }

    /// Claims and starts every admitted job targeted at this host.
    pub async fn poll(self: &Arc<Self>) {
        let jobs = self.jobs.clone();
        let host = jobs.host();
        let waiting = match blocking(move || jobs.list()).await {
            Ok(records) => records
                .into_iter()
                .filter(|r| r.state == JobState::Admitted && r.target == host)
                .collect::<Vec<_>>(),
            Err(error) => {
                crate::log::warning(&format!("job poll: {}", error.message));
                return;
            }
        };
        for record in waiting {
            if self.is_active(record.id) {
                continue;
            }
            let jobs = self.jobs.clone();
            let id = record.id;
            match blocking(move || jobs.claim(id)).await {
                Ok(claim) => self.start(claim.job, claim.nonce),
                Err(error) if error.code == ErrorCode::Conflict => {}
                Err(error) => crate::log::warning(&format!("claiming job {id}: {}", error.message)),
            }
        }
    }

    fn publish(&self, record: &JobRecord) {
        self.sink.hub().publish(StreamBody::Job {
            job: record.summary(),
        });
    }

    fn start(self: &Arc<Self>, record: JobRecord, nonce: String) {
        let wake = Arc::new(Notify::new());
        self.active.lock().unwrap().insert(record.id, wake.clone());
        self.publish(&record);
        let first = record.last_seq + 1;
        self.sink.events(vec![JobEvent {
            job: record.id,
            seq: first,
            at_ms: record.updated_at_ms,
            body: EventBody::Started {
                operation: record.operation.clone(),
                host: self.jobs.host(),
                user: record.user.clone(),
                dry_run: record.dry_run,
            },
        }]);
        let operation = self.jobs.registry().get(record.operation.as_str());
        let outcome = match operation.map(|op| (op.kind, op.script.clone())) {
            Some((OperationKind::Script, Some(script))) => {
                self.supervisor
                    .start(record.id, &script, &record.args, first + 1)
            }
            Some((OperationKind::Native, None)) => self.supervisor.start_native(
                record.id,
                record.operation.as_str(),
                &record.args,
                first + 1,
            ),
            _ => Err(ApiError::new(
                ErrorCode::Unavailable,
                format!("{} has no runner on this host", record.operation),
            )),
        };
        let runner = Arc::clone(self);
        tokio::spawn(async move {
            let id = record.id;
            let ended = match outcome {
                Ok(outcome) => runner.drive(record, &nonce, outcome, wake).await,
                Err(error) => Some((JobState::Failed, error.message, first)),
            };
            let Some((state, message, last_seq)) = ended else {
                runner.active.lock().unwrap().remove(&id);
                runner.sink.flush().await;
                return;
            };
            // pmxcfs refuses writes for a moment while membership changes,
            // which is when cluster jobs end.
            let mut tries = 0;
            loop {
                let (jobs, nonce, message) = (runner.jobs.clone(), nonce.clone(), message.clone());
                match blocking(move || jobs.finish(id, &nonce, state, message, last_seq)).await {
                    Ok(record) => runner.publish(&record),
                    Err(error)
                        if matches!(error.code, ErrorCode::Internal | ErrorCode::Unavailable)
                            && tries < FINISH_TRIES =>
                    {
                        tries += 1;
                        crate::log::warning(&format!(
                            "finishing job {id}: {}; trying again",
                            error.message
                        ));
                        tokio::time::sleep(FINISH_RETRY).await;
                        continue;
                    }
                    Err(error) => {
                        crate::log::warning(&format!("finishing job {id}: {}", error.message))
                    }
                }
                break;
            }
            runner.active.lock().unwrap().remove(&id);
            runner.sink.flush().await;
            if let Err(error) = runner.sink.store().compact_job(id).await {
                crate::log::warning(&format!("compacting job {id}'s events: {error}"));
            }
        });
    }

    /// Watches one running script until it ends; `None` if bmacd is stopping
    /// and left the job running for the next start's recovery.
    async fn drive(
        self: &Arc<Self>,
        record: JobRecord,
        nonce: &str,
        mut outcome: watch::Receiver<Option<Outcome>>,
        wake: Arc<Notify>,
    ) -> Option<(JobState, String, u64)> {
        let id = record.id;
        let mut renew = tokio::time::interval(self.config.renew_every);
        renew.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        renew.tick().await;
        let mut check = tokio::time::interval(self.config.cancel_check_every);
        check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut reported = Progress {
            phase: record.phase.clone(),
            cancel_allowed: record.cancel_allowed,
            pending_request: record.pending_request.clone(),
            last_seq: record.last_seq,
        };
        let mut applied: Option<bool> = None;
        let mut leased = true;
        loop {
            let renew_now = tokio::select! {
                done = async { outcome.wait_for(Option::is_some).await.ok().and_then(|o| o.clone()) } => {
                    return match done {
                        Some(done) => Some((done.state, done.message.unwrap_or_else(|| "finished".into()), done.last_seq)),
                        None if self.supervisor.is_stopping() => None,
                        None => Some((JobState::Failed, "the supervisor stopped watching the script".into(), record.last_seq)),
                    };
                }
                _ = renew.tick() => true,
                _ = wake.notified() => {
                    tokio::time::sleep(PROGRESS_SETTLE).await;
                    true
                }
                _ = check.tick() => false,
            };
            let view = self.supervisor.view(id).unwrap_or_default();
            let now = progress(&view);
            let jobs = self.jobs.clone();
            let current = if renew_now && leased {
                let nonce = nonce.to_string();
                let sent = now.clone();
                match blocking(move || jobs.renew(id, &nonce, &sent)).await {
                    Ok(record) => {
                        if now != reported {
                            self.publish(&record);
                            reported = now;
                        }
                        Some(record)
                    }
                    Err(error) => {
                        if error.code == ErrorCode::Conflict {
                            leased = false;
                        }
                        crate::log::warning(&format!("renewing job {id}: {}", error.message));
                        None
                    }
                }
            } else {
                blocking(move || jobs.get(id)).await.ok()
            };
            if let Some(order) = current.and_then(|r| r.cancel) {
                let wanted = order.forced;
                if applied.is_none_or(|forced| wanted && !forced) {
                    match self.supervisor.cancel(id, wanted) {
                        Ok(()) => applied = Some(wanted),
                        Err(error) => {
                            crate::log::warning(&format!("cancelling job {id}: {}", error.message))
                        }
                    }
                }
            }
        }
    }

    /// Checks that `role` may answer `record`'s job and that it is running.
    fn check_answer(&self, record: &JobRecord, role: Role) -> Result<(), ApiError> {
        let operation = self
            .jobs
            .registry()
            .get(record.operation.as_str())
            .ok_or_else(|| {
                ApiError::new(
                    ErrorCode::Internal,
                    format!("unknown operation {}", record.operation),
                )
            })?;
        if role < operation.min_role {
            return Err(ApiError::new(
                ErrorCode::Forbidden,
                format!(
                    "answering {} needs the {:?} role",
                    operation.title, operation.min_role
                ),
            ));
        }
        if record.state != JobState::Running {
            return Err(ApiError::new(
                ErrorCode::Conflict,
                format!("job {} is {:?}, not running", record.id, record.state),
            ));
        }
        Ok(())
    }

    fn deliver(&self, record: &JobRecord, relayed: &RelayedAnswer) -> Result<(), ApiError> {
        if !self.supervisor.is_running(record.id) {
            return Err(ApiError::new(
                ErrorCode::Unavailable,
                format!(
                    "job {} is not running on {}; it may have just ended",
                    record.id,
                    self.jobs.host()
                ),
            ));
        }
        self.supervisor.answer(
            record.id,
            &relayed.request_id,
            &relayed.answer,
            &relayed.user,
        )
    }

    /// Delivers an operator's answer to the job, here or, through the relay,
    /// on the host running it.
    pub async fn answer(&self, relayed: RelayedAnswer) -> Result<JobRecord, ApiError> {
        let jobs = self.jobs.clone();
        let id = relayed.job;
        let record = blocking(move || jobs.get(id)).await?;
        self.check_answer(&record, relayed.role)?;
        if record.target == self.jobs.host() {
            self.deliver(&record, &relayed)?;
        } else {
            let relay = self.relay.as_ref().ok_or_else(|| {
                ApiError::new(
                    ErrorCode::Unavailable,
                    format!(
                        "job {id} runs on {}; answer it from that host's dashboard",
                        record.target
                    ),
                )
            })?;
            relay.forward(record.target, relayed).await?;
        }
        Ok(record)
    }

    /// An answer another host relayed, checked again against this host's
    /// view of the job.
    pub async fn receive(&self, relayed: RelayedAnswer) -> Result<(), ApiError> {
        let jobs = self.jobs.clone();
        let id = relayed.job;
        let record = blocking(move || jobs.get(id)).await?;
        self.check_answer(&record, relayed.role)?;
        if record.target != self.jobs.host() {
            return Err(ApiError::new(
                ErrorCode::Conflict,
                format!("job {id} runs on {}, not here", record.target),
            ));
        }
        self.deliver(&record, &relayed)
    }
}
