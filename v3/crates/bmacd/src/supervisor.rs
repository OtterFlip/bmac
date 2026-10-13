// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Runs script operations on the target host, copied from v2's
//! `dashboard/engine/src/supervisor.rs`.
//!
//! Each script starts as `bash SCRIPT --json ARGS...` (an argument vector,
//! never a shell string) in its own process group, from the legacy runtime
//! directory, with a fixed environment. Its stdout must be `bmac-ui` v2
//! NDJSON beginning with a `protocol` event, or the run is stopped; every
//! line is parsed and checked before it becomes a job event. Answers are
//! checked against the pending request before they reach the script, and
//! secrets an operator typed are scrubbed from every later event. How the job
//! ended is decided here, not taken from the script's word: a run without
//! `completed` failed, and `completed: success` with a non-zero exit failed.

use std::collections::HashMap;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bmac_core::JobId;
use bmac_core::job::{Answer, EventBody, JobEvent, JobState, LogLevel, PhaseStatus, REDACTED};
use bmac_proto::api::{ApiError, ErrorCode};
use bmac_proto::bmac_ui::{
    CompletedStatus, PROTOCOL_NAME, PROTOCOL_VERSION, WorkflowEvent, build_response, parse_line,
};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, watch};

use crate::auth::Clock;
use crate::log;

/// Where adapted scripts and their protocol library are installed.
pub const LEGACY_ROOT: &str = "/usr/lib/bmac/legacy";
/// Where the package installs `bmacd`, which also runs native operations.
pub const NATIVE_PROGRAM: &str = "/usr/lib/bmac/bmacd";
/// Each run's artifacts go in `<WORK_ROOT>/<job>`, kept after the run.
pub const WORK_ROOT: &str = "/var/lib/bmac/work";
pub const MAX_RESULT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_NEXT_STEPS: usize = 50;
/// The longest stdout line read: a full `result` plus room for its framing.
pub const MAX_LINE: usize = MAX_RESULT_BYTES + 1024 * 1024;
const FLUSH_INTERVAL: Duration = Duration::from_millis(80);
const FLUSH_EVENTS: usize = 256;
const MIN_SECRET: usize = 3;

/// What the job is doing, for its lease record and the cancel rules.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunView {
    pub phase: Option<String>,
    /// Whether a soft cancel may interrupt it now.
    pub cancel_allowed: bool,
    pub pending_request: Option<String>,
    pub cancel_requested: bool,
    /// The sequence number of the newest event.
    pub last_seq: u64,
}

impl RunView {
    /// Whether everything but the event count is unchanged.
    pub fn same_status(&self, other: &RunView) -> bool {
        RunView {
            last_seq: 0,
            ..self.clone()
        } == RunView {
            last_seq: 0,
            ..other.clone()
        }
    }
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub state: JobState,
    pub message: Option<String>,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub last_seq: u64,
}

/// Where the supervisor delivers a job's events, in order, and changes to
/// what it is doing.
pub trait JobSink: Send + Sync + 'static {
    fn events(&self, events: Vec<JobEvent>);
    fn changed(&self, _job: JobId, _view: &RunView) {}
}

#[derive(Clone)]
pub struct SupervisorConfig {
    /// The legacy runtime: `scripts/lib`, `scripts/utilities`, and the
    /// scripts, which run with this as their working directory.
    pub root: PathBuf,
    pub bash: PathBuf,
    /// The program native operations run as: `bmacd` itself.
    pub native: PathBuf,
    /// The whole environment a script gets, besides `BMAC_UI_RUN_ID` and
    /// `BMAC_WORK_DIR`.
    pub env: Vec<(String, String)>,
    /// Where each run gets its own work directory (mode 0700), named
    /// `BMAC_WORK_DIR` in its environment.
    pub work_root: PathBuf,
    pub force_kill_after: Duration,
    /// How long the output may stay open after the script exits: a process
    /// it left running, such as a VM's QEMU, can hold it open for good.
    pub output_drain: Duration,
    pub clock: Clock,
    /// Whether a next step's operation ID names an operation.
    pub known_operation: Arc<dyn Fn(&str) -> bool + Send + Sync>,
    /// Runs before every script starts; a script doesn't start if it fails.
    pub prepare: Option<Arc<dyn Fn() -> Result<(), String> + Send + Sync>>,
}

impl SupervisorConfig {
    pub fn system(
        clock: Clock,
        known_operation: Arc<dyn Fn(&str) -> bool + Send + Sync>,
    ) -> SupervisorConfig {
        SupervisorConfig {
            root: PathBuf::from(LEGACY_ROOT),
            bash: PathBuf::from("/bin/bash"),
            native: std::env::current_exe().unwrap_or_else(|_| PathBuf::from(NATIVE_PROGRAM)),
            env: vec![
                ("PATH".into(), "/usr/sbin:/usr/bin:/sbin:/bin".into()),
                ("LANG".into(), "C.UTF-8".into()),
                ("HOME".into(), "/root".into()),
            ],
            work_root: PathBuf::from(WORK_ROOT),
            force_kill_after: Duration::from_secs(10),
            output_drain: Duration::from_secs(10),
            clock,
            known_operation,
            prepare: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct PhaseState {
    id: String,
    status: PhaseStatus,
    cancel_allowed: bool,
    label: String,
}

struct ActiveRun {
    stdin: Option<mpsc::UnboundedSender<String>>,
    pgid: Option<i32>,
    seq: u64,
    pending: Vec<JobEvent>,
    awaiting: Option<WorkflowEvent>,
    pending_request: Option<WorkflowEvent>,
    secrets: Vec<String>,
    protocol_ok: bool,
    protocol_failure: Option<String>,
    supports_cancel: bool,
    completed: Option<(CompletedStatus, Option<String>)>,
    unparsed_warned: bool,
    phases: Vec<PhaseState>,
    activity: Option<String>,
    cancel: Option<bool>,
    next_steps: Vec<(String, Option<String>)>,
    outcome: watch::Sender<Option<Outcome>>,
}

impl ActiveRun {
    fn view(&self) -> RunView {
        RunView {
            phase: self.activity.clone(),
            cancel_allowed: self.supports_cancel
                && !self
                    .phases
                    .iter()
                    .any(|p| p.status == PhaseStatus::Running && !p.cancel_allowed),
            pending_request: self
                .pending_request
                .as_ref()
                .and_then(|r| r.request_id().map(str::to_string)),
            cancel_requested: self.cancel.is_some(),
            last_seq: self.seq,
        }
    }
}

fn signal_group(pgid: Option<i32>, signal: rustix::process::Signal) {
    if let Some(pid) = pgid
        .filter(|p| *p > 1)
        .and_then(rustix::process::Pid::from_raw)
    {
        let _ = rustix::process::kill_process_group(pid, signal);
    }
}

fn scrub(event: WorkflowEvent, secrets: &[String]) -> WorkflowEvent {
    if secrets.is_empty() {
        return event;
    }
    fn walk(value: &mut Value, secrets: &[String]) {
        match value {
            Value::String(text) => {
                for secret in secrets {
                    if text.contains(secret.as_str()) {
                        *text = text.replace(secret.as_str(), REDACTED);
                    }
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|v| walk(v, secrets)),
            Value::Object(map) => map.values_mut().for_each(|v| walk(v, secrets)),
            _ => {}
        }
    }
    let Ok(mut value) = serde_json::to_value(&event) else {
        return event;
    };
    walk(&mut value, secrets);
    serde_json::from_value(value).unwrap_or(event)
}

/// Makes `<root>/<job>`, mode 0700, and its parents as needed. An existing
/// one is reused: a job that resumes after a restart keeps its artifacts.
fn work_dir(root: &std::path::Path, job: JobId) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(root)?;
    let dir = root.join(job.to_string());
    match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(e),
        _ => {}
    }
    if !std::fs::symlink_metadata(&dir)?.is_dir() {
        return Err(std::io::Error::other("it is not a directory"));
    }
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

fn error(code: ErrorCode, message: impl Into<String>) -> ApiError {
    ApiError::new(code, message)
}

pub struct Supervisor {
    config: SupervisorConfig,
    sink: Arc<dyn JobSink>,
    runs: Mutex<HashMap<JobId, ActiveRun>>,
    stopping: AtomicBool,
}

impl Supervisor {
    pub fn new(config: SupervisorConfig, sink: Arc<dyn JobSink>) -> Arc<Supervisor> {
        Arc::new(Supervisor {
            config,
            sink,
            runs: Mutex::new(HashMap::new()),
            stopping: AtomicBool::new(false),
        })
    }

    /// bmacd is stopping: a script that ends from now on without saying how
    /// it finished is left to the next start's recovery (which interrupts
    /// it, or resumes it after a restart it asked for) instead of failing.
    /// Every script gets SIGTERM, so its traps run before systemd kills
    /// what's left.
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        for run in self.runs.lock().unwrap().values() {
            signal_group(run.pgid, rustix::process::Signal::TERM);
        }
    }

    pub fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    fn now(&self) -> i64 {
        (self.config.clock)()
    }

    pub fn is_running(&self, job: JobId) -> bool {
        self.runs.lock().unwrap().contains_key(&job)
    }

    pub fn view(&self, job: JobId) -> Option<RunView> {
        self.runs.lock().unwrap().get(&job).map(ActiveRun::view)
    }

    /// The request waiting for an answer, as the script sent it.
    pub fn pending_request(&self, job: JobId) -> Option<WorkflowEvent> {
        self.runs
            .lock()
            .unwrap()
            .get(&job)
            .and_then(|r| r.pending_request.clone())
    }

    /// Starts a script for a claimed job. `first_seq` is the sequence number
    /// of its first event, after any the caller already recorded.
    pub fn start(
        self: &Arc<Self>,
        job: JobId,
        script: &str,
        args: &[String],
        first_seq: u64,
    ) -> Result<watch::Receiver<Option<Outcome>>, ApiError> {
        if script.starts_with('/') || script.split('/').any(|p| p.is_empty() || p == "..") {
            return Err(error(
                ErrorCode::Internal,
                format!("unsafe script path {script:?}"),
            ));
        }
        let path = self.config.root.join(script);
        if !path.symlink_metadata().is_ok_and(|m| m.is_file()) {
            return Err(error(
                ErrorCode::Unavailable,
                format!("{script} is not installed on this host"),
            ));
        }
        let mut command = Command::new(&self.config.bash);
        command.arg(&path).arg("--json").args(args);
        self.spawn(job, command, first_seq)
    }

    /// Where `job`'s work directory is, whether or not it exists.
    pub fn work_path(&self, job: JobId) -> PathBuf {
        self.config.work_root.join(job.to_string())
    }

    /// Starts a native operation for a claimed job: `bmacd native OPERATION
    /// --json ARGS`, speaking the same protocol as a script.
    pub fn start_native(
        self: &Arc<Self>,
        job: JobId,
        operation: &str,
        args: &[String],
        first_seq: u64,
    ) -> Result<watch::Receiver<Option<Outcome>>, ApiError> {
        if !bmac_core::job::is_event_id(operation) {
            return Err(error(
                ErrorCode::Internal,
                format!("unsafe operation id {operation:?}"),
            ));
        }
        let mut command = Command::new(&self.config.native);
        command
            .arg("native")
            .arg(operation)
            .arg("--json")
            .args(args);
        self.spawn(job, command, first_seq)
    }

    fn spawn(
        self: &Arc<Self>,
        job: JobId,
        mut command: Command,
        first_seq: u64,
    ) -> Result<watch::Receiver<Option<Outcome>>, ApiError> {
        if self.is_running(job) {
            return Err(error(
                ErrorCode::Conflict,
                format!("job {job} is already running"),
            ));
        }
        if let Some(prepare) = &self.config.prepare {
            prepare().map_err(|e| error(ErrorCode::Unavailable, e))?;
        }
        let work = work_dir(&self.config.work_root, job).map_err(|e| {
            error(
                ErrorCode::Unavailable,
                format!("cannot make the work directory for job {job}: {e}"),
            )
        })?;
        let mut runs = self.runs.lock().unwrap();
        if runs.contains_key(&job) {
            return Err(error(
                ErrorCode::Conflict,
                format!("job {job} is already running"),
            ));
        }
        command
            .current_dir(&self.config.root)
            .env_clear()
            .envs(
                self.config
                    .env
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.as_str())),
            )
            .env("BMAC_UI_RUN_ID", job.to_string())
            .env("BMAC_WORK_DIR", &work)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(false);
        let mut child = command.spawn().map_err(|e| {
            let program = command
                .as_std()
                .get_program()
                .to_string_lossy()
                .into_owned();
            error(ErrorCode::Internal, format!("cannot start {program}: {e}"))
        })?;
        let pid = child.id();

        let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<String>();
        let mut stdin = child.stdin.take();
        tokio::spawn(async move {
            while let Some(line) = stdin_rx.recv().await {
                let Some(pipe) = stdin.as_mut() else { break };
                if pipe
                    .write_all(format!("{line}\n").as_bytes())
                    .await
                    .is_err()
                    || pipe.flush().await.is_err()
                {
                    break;
                }
            }
        });
        let (outcome_tx, outcome_rx) = watch::channel(None);
        runs.insert(
            job,
            ActiveRun {
                stdin: Some(stdin_tx),
                pgid: pid.and_then(|p| i32::try_from(p).ok()),
                seq: first_seq.saturating_sub(1),
                pending: Vec::new(),
                awaiting: None,
                pending_request: None,
                secrets: Vec::new(),
                protocol_ok: false,
                protocol_failure: None,
                supports_cancel: true,
                completed: None,
                unparsed_warned: false,
                phases: Vec::new(),
                activity: Some("Starting".into()),
                cancel: None,
                next_steps: Vec::new(),
                outcome: outcome_tx,
            },
        );
        drop(runs);

        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");
        let out_task = tokio::spawn(Arc::clone(self).read_stream(job, stdout, true));
        let err_task = tokio::spawn(Arc::clone(self).read_stream(job, stderr, false));
        let ticker = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(FLUSH_INTERVAL).await;
                if !ticker.flush(job) {
                    break;
                }
            }
        });
        let waiter = Arc::clone(self);
        let drain = self.config.output_drain;
        tokio::spawn(async move {
            let (mut out_task, mut err_task) = (out_task, err_task);
            let status = child.wait().await;
            let drained = tokio::time::timeout(drain, async {
                let _ = (&mut out_task).await;
                let _ = (&mut err_task).await;
            })
            .await;
            if drained.is_err() {
                out_task.abort();
                err_task.abort();
                log::warning(&format!(
                    "job {job}: a process the script left running still holds its output; finishing without it"
                ));
            }
            waiter.finalize(job, status.ok());
        });
        Ok(outcome_rx)
    }

    async fn read_stream<R: AsyncRead + Unpin>(
        self: Arc<Self>,
        job: JobId,
        stream: R,
        stdout: bool,
    ) {
        let mut reader = BufReader::new(stream);
        let mut buf = Vec::with_capacity(8192);
        let mut discard = false;
        loop {
            buf.clear();
            match (&mut reader)
                .take(MAX_LINE as u64)
                .read_until(b'\n', &mut buf)
                .await
            {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            if discard {
                continue;
            }
            if stdout && buf.len() >= MAX_LINE && !buf.ends_with(b"\n") {
                self.violation(
                    job,
                    format!("the script wrote a line longer than {MAX_LINE} bytes"),
                );
                discard = true;
                continue;
            }
            let text = String::from_utf8_lossy(&buf);
            let line = text.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                continue;
            }
            if stdout {
                match parse_line(line) {
                    Ok(event) => self.ingest(job, event),
                    Err(e) => self.unparsed(job, line, &e.to_string()),
                }
            } else {
                let level = if line.starts_with("ERROR") || line.contains(": line ") {
                    LogLevel::Error
                } else {
                    LogLevel::Info
                };
                self.ingest(
                    job,
                    WorkflowEvent::log("stderr", level, line, Some("process")),
                );
            }
        }
    }

    /// Stops the run for breaking the protocol; its output is drained and
    /// dropped from here on, so the script can't block on a full pipe.
    fn violation(&self, job: JobId, reason: String) {
        let mut runs = self.runs.lock().unwrap();
        let Some(run) = runs.get_mut(&job) else {
            return;
        };
        let before = run.view();
        if run.protocol_failure.is_none() {
            run.protocol_failure = Some(reason.clone());
        }
        if run.cancel.is_none() {
            run.cancel = Some(true);
            run.activity = Some(reason);
            signal_group(run.pgid, rustix::process::Signal::TERM);
        }
        let after = run.view();
        if !after.same_status(&before) {
            self.sink.changed(job, &after);
        }
    }

    fn unparsed(&self, job: JobId, line: &str, why: &str) {
        let first = {
            let mut runs = self.runs.lock().unwrap();
            let Some(run) = runs.get_mut(&job) else {
                return;
            };
            !std::mem::replace(&mut run.unparsed_warned, true)
        };
        if first {
            self.ingest(
                job,
                WorkflowEvent::Warning {
                    message: format!(
                        "The script wrote output outside the bmac-ui protocol ({why})."
                    ),
                },
            );
        }
        self.ingest(
            job,
            WorkflowEvent::log("stdout", LogLevel::Warning, line, Some("unparsed")),
        );
    }

    fn push(&self, run: &mut ActiveRun, job: JobId, body: EventBody, urgent: bool) {
        run.seq += 1;
        run.pending.push(JobEvent {
            job,
            seq: run.seq,
            at_ms: self.now(),
            body,
        });
        if urgent || run.pending.len() >= FLUSH_EVENTS {
            self.sink.events(std::mem::take(&mut run.pending));
        }
    }

    fn ingest(&self, job: JobId, event: WorkflowEvent) {
        let mut runs = self.runs.lock().unwrap();
        let Some(run) = runs.get_mut(&job) else {
            return;
        };
        let mut event = scrub(event, &run.secrets);
        let before = run.view();
        let keep = self.apply(run, &mut event);
        let auto_cancel = event.is_request() && run.cancel.is_some();
        if keep && let Some(body) = event.into_job_event() {
            let urgent = !matches!(body, EventBody::Output { .. } | EventBody::Progress { .. });
            self.push(run, job, body, urgent);
        }
        if auto_cancel
            && let Some(request) = run.pending_request.take()
            && let (Ok((line, _)), Some(tx)) =
                (build_response(&request, &Answer::Cancel), &run.stdin)
        {
            let _ = tx.send(line);
        }
        if let Some(reason) = run.protocol_failure.clone()
            && run.cancel.is_none()
        {
            run.cancel = Some(true);
            run.activity = Some(reason);
            signal_group(run.pgid, rustix::process::Signal::TERM);
        }
        let after = run.view();
        if !after.same_status(&before) {
            self.sink.changed(job, &after);
        }
    }

    /// Updates the run for one event. Returns whether it becomes a job event.
    fn apply(&self, run: &mut ActiveRun, event: &mut WorkflowEvent) -> bool {
        let stderr = matches!(event, WorkflowEvent::Log { stream: Some(s), .. } if s == "stderr");
        if !run.protocol_ok && run.protocol_failure.is_none() && !stderr {
            run.protocol_failure = match event {
                WorkflowEvent::Protocol { protocol, version, .. }
                    if protocol == PROTOCOL_NAME && *version == PROTOCOL_VERSION =>
                {
                    run.protocol_ok = true;
                    run.activity = Some("Running".into());
                    return false;
                }
                WorkflowEvent::Protocol { protocol, version, .. } => Some(format!(
                    "The script speaks {protocol} version {version}; BMAC supports {PROTOCOL_NAME} version {PROTOCOL_VERSION}."
                )),
                _ => Some(
                    "The script did not begin with a bmac-ui protocol event, so its output cannot be trusted."
                        .into(),
                ),
            };
            return false;
        }
        match event {
            WorkflowEvent::Protocol { .. } | WorkflowEvent::WorkflowStarted { .. } => false,
            WorkflowEvent::WorkflowMetadata {
                supports_cancel, ..
            } => {
                if *supports_cancel == Some(false) {
                    run.supports_cancel = false;
                }
                false
            }
            WorkflowEvent::Input { .. }
            | WorkflowEvent::InputGroup { .. }
            | WorkflowEvent::Confirm { .. }
            | WorkflowEvent::ManualAction { .. } => {
                run.pending_request = Some(event.clone());
                run.awaiting = None;
                true
            }
            WorkflowEvent::ValidationError { request_id, .. } => {
                if run.awaiting.as_ref().and_then(|r| r.request_id()) == Some(request_id.as_str()) {
                    run.pending_request = run.awaiting.take();
                }
                true
            }
            WorkflowEvent::Phase {
                id,
                label,
                status,
                cancel_allowed,
            } => {
                let allowed = cancel_allowed.unwrap_or(true);
                match run.phases.iter_mut().find(|p| &p.id == id) {
                    Some(phase) => {
                        phase.status = *status;
                        phase.cancel_allowed = allowed;
                        phase.label = label.clone();
                    }
                    None => run.phases.push(PhaseState {
                        id: id.clone(),
                        status: *status,
                        cancel_allowed: allowed,
                        label: label.clone(),
                    }),
                }
                if *status == PhaseStatus::Running {
                    run.activity = Some(label.clone());
                }
                true
            }
            WorkflowEvent::Progress { message, .. } => {
                run.activity = Some(message.clone());
                true
            }
            WorkflowEvent::Result { data } => {
                serde_json::to_vec(data)
                    .map(|v| v.len())
                    .unwrap_or(usize::MAX)
                    <= MAX_RESULT_BYTES
            }
            WorkflowEvent::NextStep {
                text,
                workflow,
                args,
                ..
            } => {
                if !workflow
                    .as_deref()
                    .is_some_and(|w| (self.config.known_operation)(w))
                {
                    *workflow = None;
                    *args = None;
                }
                let key = (text.clone(), workflow.clone());
                if run.next_steps.contains(&key) || run.next_steps.len() >= MAX_NEXT_STEPS {
                    return false;
                }
                run.next_steps.push(key);
                true
            }
            WorkflowEvent::Completed {
                status, message, ..
            } => {
                run.completed = Some((*status, message.clone()));
                false
            }
            WorkflowEvent::Info { .. }
            | WorkflowEvent::Plan { .. }
            | WorkflowEvent::Warning { .. }
            | WorkflowEvent::Error { .. }
            | WorkflowEvent::Log { .. } => true,
        }
    }

    /// Sends buffered events; `false` once the run has ended.
    fn flush(&self, job: JobId) -> bool {
        let mut runs = self.runs.lock().unwrap();
        let Some(run) = runs.get_mut(&job) else {
            return false;
        };
        if !run.pending.is_empty() {
            self.sink.events(std::mem::take(&mut run.pending));
        }
        true
    }

    fn finalize(&self, job: JobId, status: Option<ExitStatus>) {
        let mut runs = self.runs.lock().unwrap();
        let Some(mut run) = runs.remove(&job) else {
            return;
        };
        if self.is_stopping()
            && run.completed.is_none()
            && run.cancel.is_none()
            && run.protocol_failure.is_none()
        {
            if !run.pending.is_empty() {
                self.sink.events(std::mem::take(&mut run.pending));
            }
            // Dropping the outcome sender tells the runner it was left.
            return;
        }
        let exit_code = status.and_then(|s| s.code());
        let signal = status.and_then(|s| s.signal());
        let how = match (exit_code, signal) {
            (Some(code), _) => format!("status {code}"),
            (None, Some(signal)) => format!("signal {signal}"),
            _ => "an unknown status".into(),
        };
        let (state, message) = if let Some(reason) = run.protocol_failure.clone() {
            (JobState::Failed, Some(reason))
        } else {
            match run.completed.clone() {
                Some((CompletedStatus::Success, _)) if exit_code == Some(0) => {
                    (JobState::Completed, None)
                }
                Some((CompletedStatus::Success, _)) => (
                    JobState::Failed,
                    Some(format!(
                        "The script reported success but exited with {how}."
                    )),
                ),
                Some((CompletedStatus::Failed, message)) => (
                    JobState::Failed,
                    message.or_else(|| Some(format!("The script failed with {how}."))),
                ),
                Some((CompletedStatus::Cancelled, message)) => (JobState::Cancelled, message),
                None if run.cancel.is_some() => (
                    JobState::Cancelled,
                    Some(format!(
                        "{} The script stopped with {how}.",
                        if run.cancel == Some(true) {
                            "Forcibly stopped."
                        } else {
                            "Cancelled."
                        }
                    )),
                ),
                None => (
                    JobState::Failed,
                    Some(format!(
                        "The script exited with {how} without reporting how it finished. Treat this as a failure."
                    )),
                ),
            }
        };
        let phase_end = match state {
            JobState::Completed => PhaseStatus::Complete,
            JobState::Cancelled => PhaseStatus::Skipped,
            _ => PhaseStatus::Failed,
        };
        for phase in run
            .phases
            .clone()
            .into_iter()
            .filter(|p| p.status == PhaseStatus::Running)
        {
            self.push(
                &mut run,
                job,
                EventBody::Phase {
                    id: phase.id,
                    label: phase.label,
                    status: phase_end,
                    cancel_allowed: None,
                },
                false,
            );
        }
        let terminal = match state {
            JobState::Completed => EventBody::Completed {
                message: message.clone(),
            },
            JobState::Cancelled => EventBody::Cancelled {
                by: None,
                message: message.clone(),
            },
            _ => EventBody::Failed {
                code: None,
                message: message.clone().unwrap_or_else(|| "failed".into()),
                details: None,
            },
        };
        self.push(&mut run, job, terminal, true);
        drop(runs);
        let _ = run.outcome.send(Some(Outcome {
            state,
            message,
            exit_code,
            signal,
            last_seq: run.seq,
        }));
    }

    /// Passes an answer to the script after checking it against the pending
    /// request. A second answer to the same request finds nothing pending.
    pub fn answer(
        &self,
        job: JobId,
        request_id: &str,
        answer: &Answer,
        by: &str,
    ) -> Result<(), ApiError> {
        let mut runs = self.runs.lock().unwrap();
        let run = runs.get_mut(&job).ok_or_else(|| {
            error(
                ErrorCode::NotFound,
                format!("job {job} is not running here"),
            )
        })?;
        let request = run
            .pending_request
            .clone()
            .filter(|r| r.request_id() == Some(request_id))
            .ok_or_else(|| {
                error(
                    ErrorCode::Conflict,
                    "that request is no longer waiting for an answer",
                )
            })?;
        let (line, redacted) =
            build_response(&request, answer).map_err(|why| error(ErrorCode::Invalid, why))?;
        let tx = run
            .stdin
            .as_ref()
            .ok_or_else(|| error(ErrorCode::Conflict, "the script's input is closed"))?;
        tx.send(line)
            .map_err(|_| error(ErrorCode::Conflict, "the script's input is closed"))?;
        let fields = match &request {
            WorkflowEvent::Input { field, .. } => vec![field.clone()],
            WorkflowEvent::InputGroup { fields, .. } => fields.clone(),
            _ => Vec::new(),
        };
        if let Answer::Values { values } = answer {
            for field in fields.iter().filter(|f| f.is_sensitive()) {
                if let Some(Value::String(secret)) = values.get(&field.id)
                    && secret.len() >= MIN_SECRET
                    && !run.secrets.contains(secret)
                {
                    run.secrets.push(secret.clone());
                }
            }
        }
        let before = run.view();
        run.pending_request = None;
        run.awaiting = Some(request);
        if *answer == Answer::Cancel {
            run.cancel.get_or_insert(false);
        }
        self.push(
            run,
            job,
            EventBody::Answered {
                request_id: request_id.into(),
                by: by.into(),
                values: redacted,
            },
            true,
        );
        let after = run.view();
        if !after.same_status(&before) {
            self.sink.changed(job, &after);
        }
        Ok(())
    }

    /// A soft cancel answers a pending request with a cancel, or interrupts
    /// the script; it is refused while the current phase forbids it and
    /// nothing is pending. A forced cancel terminates the process group, then
    /// kills it if it hasn't stopped.
    pub fn cancel(self: &Arc<Self>, job: JobId, force: bool) -> Result<(), ApiError> {
        let mut runs = self.runs.lock().unwrap();
        let run = runs.get_mut(&job).ok_or_else(|| {
            error(
                ErrorCode::NotFound,
                format!("job {job} is not running here"),
            )
        })?;
        let view = run.view();
        if !force && !view.cancel_allowed && run.pending_request.is_none() {
            let phase = view.phase.unwrap_or_else(|| "this phase".into());
            return Err(error(
                ErrorCode::Conflict,
                format!(
                    "the script marked \"{phase}\" as unsafe to interrupt; wait for it to finish"
                ),
            ));
        }
        let first_forced = force && run.cancel != Some(true);
        run.cancel = Some(force || run.cancel == Some(true));
        if force {
            signal_group(run.pgid, rustix::process::Signal::TERM);
            if first_forced {
                let me = Arc::clone(self);
                let pgid = run.pgid;
                let after = self.config.force_kill_after;
                tokio::spawn(async move {
                    tokio::time::sleep(after).await;
                    if me.is_running(job) {
                        signal_group(pgid, rustix::process::Signal::KILL);
                    }
                });
            }
        } else if let Some(request) = run.pending_request.take() {
            match (build_response(&request, &Answer::Cancel), &run.stdin) {
                (Ok((line, _)), Some(tx)) => {
                    let _ = tx.send(line);
                }
                _ => signal_group(run.pgid, rustix::process::Signal::INT),
            }
        } else {
            signal_group(run.pgid, rustix::process::Signal::INT);
        }
        let after = run.view();
        if after != view {
            self.sink.changed(job, &after);
        }
        Ok(())
    }
}
