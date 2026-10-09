//! Runs allowlisted workflows and supervises them.
//!
//! Each script starts as `bash SCRIPT --json ARGS...` (argv, never a shell
//! string) in its own process group, with its working directory at the
//! repository root. Its stdout must be bmac-ui v2 NDJSON beginning with a
//! `protocol` event; every line is parsed and validated before it is
//! forwarded. Responses are validated against the outstanding request before
//! they are written to the script's stdin.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::error::{EngineError, Result};
use crate::history::{
    CancelRecord, History, NextStepRecord, PhaseState, ResponseRecord, RunDetails, RunEvent, RunFiles, RunRecord,
    RunStatus,
};
use crate::platform::PlatformInfo;
use crate::protocol::{
    build_response, parse_line, CompletedStatus, LogLevel, PhaseStatus, ResponsePayload, WorkflowEvent, PROTOCOL_NAME,
    PROTOCOL_VERSION, REDACTED,
};
use crate::registry::{availability, build_invocation, Registry, WorkflowMode};

const MAX_LIVE_LOGS: usize = 100_000;
const MAX_RESULT_BYTES: usize = 8 * 1024 * 1024;
const FLUSH_INTERVAL: Duration = Duration::from_millis(80);
const FORCE_KILL_AFTER: Duration = Duration::from_secs(10);

/// Why the frontend may want to draw the operator's attention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Attention {
    NeedsInput,
    Finished,
}

/// Where the supervisor delivers events. The Tauri layer forwards them to the
/// webview; tests collect them.
pub trait EventSink: Send + Sync + 'static {
    fn events(&self, run_id: &str, events: Vec<RunEvent>);
    fn updated(&self, record: &RunRecord);
    fn attention(&self, _record: &RunRecord, _why: Attention) {}
}

struct ActiveRun {
    record: RunRecord,
    stdin: Option<mpsc::UnboundedSender<String>>,
    pgid: Option<i32>,
    seq: u64,
    pending: Vec<RunEvent>,
    events: Vec<RunEvent>,
    logs: VecDeque<RunEvent>,
    logs_dropped: bool,
    awaiting: Option<WorkflowEvent>,
    secrets: Vec<String>,
    protocol_ok: bool,
    protocol_failure: Option<String>,
    supports_cancel: bool,
    completed: Option<(CompletedStatus, Option<String>)>,
    unparsed_warned: bool,
    files: Option<RunFiles>,
}

pub struct Supervisor {
    registry: Registry,
    history: History,
    sink: Arc<dyn EventSink>,
    runs: Mutex<HashMap<String, ActiveRun>>,
    bash: Mutex<Option<std::path::PathBuf>>,
    /// Child processes and their reader tasks need a Tokio reactor, and
    /// callers (such as synchronous Tauri commands on the GUI thread) may not
    /// be inside one.
    runtime: tokio::runtime::Handle,
}

pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Quote one argument the way an operator would type it in a POSIX shell.
pub fn shell_quote(arg: &str) -> String {
    if !arg.is_empty() && arg.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_./=:,@%+".contains(&b)) {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

fn signal_group(pgid: Option<i32>, signal: i32) {
    if let Some(pgid) = pgid.filter(|p| *p > 1) {
        // SAFETY: kill(2) with a negative pid signals one process group.
        unsafe {
            libc::kill(-pgid, signal);
        }
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
    let Ok(mut value) = serde_json::to_value(&event) else { return event };
    walk(&mut value, secrets);
    serde_json::from_value(value).unwrap_or(event)
}

impl Supervisor {
    pub fn new(
        registry: Registry,
        history: History,
        sink: Arc<dyn EventSink>,
        runtime: tokio::runtime::Handle,
    ) -> Arc<Self> {
        history.recover();
        Arc::new(Self {
            registry,
            history,
            sink,
            runs: Mutex::new(HashMap::new()),
            bash: Mutex::new(None),
            runtime,
        })
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    pub fn history(&self) -> &History {
        &self.history
    }

    fn bash(&self) -> Result<std::path::PathBuf> {
        let mut cached = self.bash.lock().unwrap();
        if cached.is_none() {
            *cached = crate::preflight::find_bash();
        }
        cached.clone().ok_or_else(|| EngineError::Unavailable("Bash 4.4 or newer was not found.".into()))
    }

    pub fn start(
        self: &Arc<Self>,
        root: &Path,
        platform: &PlatformInfo,
        workflow_id: &str,
        values: &BTreeMap<String, Value>,
    ) -> Result<RunRecord> {
        let _runtime = self.runtime.enter();
        let workflow = self.registry.get(workflow_id)?.clone();
        if let Some(reason) = availability(&workflow, platform) {
            return Err(EngineError::Unavailable(reason));
        }
        let root = crate::repo::validate(root)?;
        let script = root.join(&workflow.script);
        if !script.is_file() {
            return Err(EngineError::Unavailable(format!("{} is missing", workflow.script)));
        }
        let invocation = build_invocation(&workflow, values)?;
        let bash = self.bash()?;

        let mut runs = self.runs.lock().unwrap();
        if workflow.mode == WorkflowMode::Mutating {
            if let Some(busy) = runs.values().find(|r| r.record.mode == WorkflowMode::Mutating) {
                return Err(EngineError::Busy(busy.record.workflow_title.clone()));
            }
        }

        let run_id = uuid::Uuid::new_v4().to_string();
        let mut argv = vec!["--json".to_string()];
        argv.extend(invocation.args.iter().cloned());
        let quoted = |args: &[String]| args.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ");
        let command_line = format!("{} {}", workflow.script, quoted(&argv));
        let terminal_command = format!("{} {}", workflow.script, quoted(&invocation.args)).trim_end().to_string();

        let mut command = Command::new(&bash);
        command
            .arg(&script)
            .args(&argv)
            .current_dir(&root)
            .env("PATH", crate::preflight::script_path())
            .env("BMAC_UI_RUN_ID", &run_id)
            .env_remove("BMAC_UI_JSON")
            .env_remove("BMAC_UI_TOKEN")
            .env_remove("BMAC_UI_EVENT_FD")
            .env_remove("BMAC_UI_INPUT_FD")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(false);
        let mut child = command.spawn().map_err(|e| EngineError::Io(format!("could not start bash: {e}")))?;
        let pid = child.id();

        let record = RunRecord {
            run_id: run_id.clone(),
            workflow_id: workflow.id.clone(),
            workflow_title: workflow.title.clone(),
            mode: workflow.mode,
            destructive: workflow.destructive,
            script: workflow.script.clone(),
            argv,
            command_line,
            terminal_command,
            launch_values: invocation.summary.clone(),
            target: invocation.target.clone(),
            dry_run: invocation.dry_run,
            started_at: now(),
            ended_at: None,
            status: RunStatus::Starting,
            message: None,
            exit_code: None,
            signal: None,
            pid,
            cancel: None,
            cancel_allowed: true,
            phases: vec![],
            current_activity: Some("Starting".into()),
            pending_request: None,
            result: None,
            next_steps: vec![],
            errors: vec![],
            warnings: 0,
            responses: vec![],
            log_lines: 0,
            last_log: None,
            event_count: 0,
        };
        let _ = self.history.save(&record);

        let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<String>();
        let mut stdin = child.stdin.take();
        tokio::spawn(async move {
            while let Some(line) = stdin_rx.recv().await {
                let Some(pipe) = stdin.as_mut() else { break };
                if pipe.write_all(format!("{line}\n").as_bytes()).await.is_err() || pipe.flush().await.is_err() {
                    break;
                }
            }
            drop(stdin);
        });

        runs.insert(
            run_id.clone(),
            ActiveRun {
                record: record.clone(),
                stdin: Some(stdin_tx),
                pgid: pid.map(|p| p as i32),
                seq: 0,
                pending: vec![],
                events: vec![],
                logs: VecDeque::new(),
                logs_dropped: false,
                awaiting: None,
                secrets: vec![],
                protocol_ok: false,
                protocol_failure: None,
                supports_cancel: true,
                completed: None,
                unparsed_warned: false,
                files: self.history.create_files(&run_id).ok(),
            },
        );
        drop(runs);
        self.sink.updated(&record);

        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");
        let out_task = tokio::spawn(Arc::clone(self).read_stream(run_id.clone(), stdout, true));
        let err_task = tokio::spawn(Arc::clone(self).read_stream(run_id.clone(), stderr, false));
        let ticker = Arc::clone(self);
        let ticker_id = run_id.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(FLUSH_INTERVAL).await;
                if !ticker.flush(&ticker_id) {
                    break;
                }
            }
        });
        let waiter = Arc::clone(self);
        let waiter_id = run_id.clone();
        tokio::spawn(async move {
            let status = child.wait().await;
            let _ = out_task.await;
            let _ = err_task.await;
            waiter.finalize(&waiter_id, status.ok());
        });
        Ok(record)
    }

    async fn read_stream<R: AsyncRead + Unpin>(self: Arc<Self>, run_id: String, stream: R, stdout: bool) {
        let mut reader = BufReader::new(stream);
        let mut buf = Vec::with_capacity(8192);
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let text = String::from_utf8_lossy(&buf);
            let line = text.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                continue;
            }
            if stdout {
                match parse_line(line) {
                    Ok(event) => self.ingest(&run_id, event),
                    Err(error) => self.unparsed(&run_id, line, &error.to_string()),
                }
            } else {
                let level = if line.starts_with("ERROR") || line.contains(": line ") {
                    LogLevel::Error
                } else {
                    LogLevel::Info
                };
                self.ingest(&run_id, WorkflowEvent::log("stderr", level, line, Some("process")));
            }
        }
    }

    fn unparsed(&self, run_id: &str, line: &str, error: &str) {
        let first = {
            let mut runs = self.runs.lock().unwrap();
            let Some(run) = runs.get_mut(run_id) else { return };
            let first = !run.unparsed_warned;
            run.unparsed_warned = true;
            first
        };
        if first {
            self.ingest(
                run_id,
                WorkflowEvent::Warning { message: format!("The script wrote output outside the bmac-ui protocol ({error}).") },
            );
        }
        self.ingest(run_id, WorkflowEvent::log("stdout", LogLevel::Warning, line, Some("unparsed")));
    }

    fn ingest(&self, run_id: &str, event: WorkflowEvent) {
        let mut runs = self.runs.lock().unwrap();
        let Some(run) = runs.get_mut(run_id) else { return };
        let event = scrub(event, &run.secrets);
        let is_log = matches!(event, WorkflowEvent::Log { .. });
        let changed = self.apply(run, &event);
        let auto_cancel = event.is_request() && run.record.cancel.is_some();
        run.seq += 1;
        run.record.event_count = run.seq;
        let item = RunEvent { seq: run.seq, at: now(), event };
        if let Some(files) = run.files.as_mut() {
            files.append(&item);
        }
        if is_log {
            run.logs.push_back(item.clone());
            if run.logs.len() > MAX_LIVE_LOGS {
                run.logs.pop_front();
                run.logs_dropped = true;
            }
        } else {
            run.events.push(item.clone());
        }
        run.pending.push(item);
        if !is_log || run.pending.len() >= 256 {
            let batch = std::mem::take(&mut run.pending);
            self.sink.events(run_id, batch);
        }
        if changed {
            self.sink.updated(&run.record);
            if run.record.pending_request.is_some() && !auto_cancel {
                self.sink.attention(&run.record, Attention::NeedsInput);
            }
        }
        if auto_cancel {
            if let Some(request) = run.record.pending_request.take() {
                if let Ok((line, _)) = build_response(&request, &ResponsePayload::Cancel) {
                    if let Some(tx) = &run.stdin {
                        let _ = tx.send(line);
                    }
                }
                run.record.status = RunStatus::Cancelling;
                self.sink.updated(&run.record);
            }
        }
        if let Some(reason) = run.protocol_failure.clone() {
            if run.record.status != RunStatus::Cancelling {
                run.record.status = RunStatus::Cancelling;
                run.record.message = Some(reason);
                signal_group(run.pgid, libc::SIGTERM);
                self.sink.updated(&run.record);
            }
        }
    }

    /// Update the run record for one event. Returns whether the record changed
    /// in a way the frontend should see right away.
    fn apply(&self, run: &mut ActiveRun, event: &WorkflowEvent) -> bool {
        let record = &mut run.record;
        if !run.protocol_ok && run.protocol_failure.is_none() && !matches!(event, WorkflowEvent::Log { stream: Some(s), .. } if s == "stderr") {
            match event {
                WorkflowEvent::Protocol { protocol, version, .. } if protocol == PROTOCOL_NAME && *version == PROTOCOL_VERSION => {
                    run.protocol_ok = true;
                    record.status = RunStatus::Running;
                    return true;
                }
                WorkflowEvent::Protocol { protocol, version, .. } => {
                    run.protocol_failure = Some(format!(
                        "The script speaks {protocol} version {version}; this dashboard supports {PROTOCOL_NAME} version {PROTOCOL_VERSION}."
                    ));
                    return true;
                }
                _ => {
                    run.protocol_failure =
                        Some("The script did not begin with a bmac-ui protocol event, so its output cannot be trusted.".into());
                    return true;
                }
            }
        }
        match event {
            WorkflowEvent::Protocol { .. } => false,
            WorkflowEvent::WorkflowStarted { .. } => {
                record.current_activity = Some("Running".into());
                true
            }
            WorkflowEvent::WorkflowMetadata { supports_cancel, .. } => {
                if *supports_cancel == Some(false) {
                    run.supports_cancel = false;
                    record.cancel_allowed = false;
                }
                true
            }
            WorkflowEvent::Input { .. } | WorkflowEvent::InputGroup { .. } => {
                record.pending_request = Some(event.clone());
                run.awaiting = None;
                if record.status != RunStatus::Cancelling {
                    record.status = RunStatus::WaitingInput;
                }
                true
            }
            WorkflowEvent::Confirm { .. } => {
                record.pending_request = Some(event.clone());
                run.awaiting = None;
                if record.status != RunStatus::Cancelling {
                    record.status = RunStatus::WaitingConfirmation;
                }
                true
            }
            WorkflowEvent::ManualAction { .. } => {
                record.pending_request = Some(event.clone());
                run.awaiting = None;
                if record.status != RunStatus::Cancelling {
                    record.status = RunStatus::WaitingManualAction;
                }
                true
            }
            WorkflowEvent::ValidationError { request_id, .. } => {
                if run.awaiting.as_ref().and_then(|r| r.request_id()) == Some(request_id.as_str()) {
                    record.pending_request = run.awaiting.take();
                    if record.status != RunStatus::Cancelling {
                        record.status = RunStatus::WaitingInput;
                    }
                }
                true
            }
            WorkflowEvent::Phase { id, label, status, cancel_allowed } => {
                let at = now();
                let allowed = cancel_allowed.unwrap_or(true);
                match record.phases.iter_mut().find(|p| &p.id == id) {
                    Some(phase) => {
                        phase.label = label.clone();
                        phase.status = *status;
                        phase.cancel_allowed = allowed;
                        if *status == PhaseStatus::Running && phase.started_at.is_none() {
                            phase.started_at = Some(at.clone());
                        }
                        if matches!(status, PhaseStatus::Complete | PhaseStatus::Failed | PhaseStatus::Skipped) {
                            phase.ended_at = Some(at);
                        }
                    }
                    None => record.phases.push(PhaseState {
                        id: id.clone(),
                        label: label.clone(),
                        status: *status,
                        cancel_allowed: allowed,
                        started_at: (*status == PhaseStatus::Running).then(|| at.clone()),
                        ended_at: matches!(status, PhaseStatus::Complete | PhaseStatus::Failed | PhaseStatus::Skipped)
                            .then_some(at),
                    }),
                }
                if *status == PhaseStatus::Running {
                    record.current_activity = Some(label.clone());
                }
                record.cancel_allowed = run.supports_cancel
                    && !record.phases.iter().any(|p| p.status == PhaseStatus::Running && !p.cancel_allowed);
                true
            }
            WorkflowEvent::Progress { message, .. } => {
                record.current_activity = Some(message.clone());
                true
            }
            WorkflowEvent::Result { data } => {
                if serde_json::to_vec(data).map(|v| v.len()).unwrap_or(usize::MAX) <= MAX_RESULT_BYTES {
                    record.result = Some(data.clone());
                }
                true
            }
            WorkflowEvent::NextStep { text, command, workflow, args } => {
                let known = workflow.as_ref().is_some_and(|w| self.registry.get(w).is_ok());
                let step = NextStepRecord {
                    text: text.clone(),
                    command: command.clone(),
                    workflow: if known { workflow.clone() } else { None },
                    args: if known { args.clone() } else { None },
                };
                if !record.next_steps.contains(&step) && record.next_steps.len() < 50 {
                    record.next_steps.push(step);
                }
                true
            }
            WorkflowEvent::Error { message, .. } => {
                if record.errors.len() < 50 {
                    record.errors.push(message.clone());
                }
                true
            }
            WorkflowEvent::Warning { .. } => {
                record.warnings += 1;
                true
            }
            WorkflowEvent::Info { .. } | WorkflowEvent::Plan { .. } => true,
            WorkflowEvent::Log { text, .. } => {
                record.log_lines += 1;
                if !text.trim().is_empty() {
                    record.last_log = Some(text.chars().take(400).collect());
                }
                false
            }
            WorkflowEvent::Completed { status, message, .. } => {
                run.completed = Some((*status, message.clone()));
                true
            }
        }
    }

    /// Send buffered events. Returns false once the run is no longer active.
    fn flush(&self, run_id: &str) -> bool {
        let mut runs = self.runs.lock().unwrap();
        let Some(run) = runs.get_mut(run_id) else { return false };
        if !run.pending.is_empty() {
            let batch = std::mem::take(&mut run.pending);
            self.sink.events(run_id, batch);
        }
        if let Some(files) = run.files.as_mut() {
            files.flush();
        }
        true
    }

    fn finalize(&self, run_id: &str, status: Option<ExitStatus>) {
        use std::os::unix::process::ExitStatusExt;
        let mut runs = self.runs.lock().unwrap();
        let Some(mut run) = runs.remove(run_id) else { return };
        let exit_code = status.and_then(|s| s.code());
        let signal = status.and_then(|s| s.signal());
        let record = &mut run.record;
        record.exit_code = exit_code;
        record.signal = signal;
        record.ended_at = Some(now());
        record.pending_request = None;
        record.cancel_allowed = false;
        let how = match (exit_code, signal) {
            (Some(code), _) => format!("status {code}"),
            (None, Some(sig)) => format!("signal {sig}"),
            _ => "an unknown status".into(),
        };
        let (final_status, message) = if let Some(reason) = run.protocol_failure.clone() {
            (RunStatus::Failed, Some(reason))
        } else {
            match run.completed.clone() {
                Some((CompletedStatus::Success, _)) if exit_code == Some(0) => (RunStatus::Succeeded, None),
                Some((CompletedStatus::Success, _)) => {
                    (RunStatus::Failed, Some(format!("The script reported success but exited with {how}.")))
                }
                Some((CompletedStatus::Failed, message)) => {
                    (RunStatus::Failed, message.or_else(|| Some(format!("The script failed with {how}."))))
                }
                Some((CompletedStatus::Cancelled, message)) => (RunStatus::Cancelled, message),
                None if record.cancel.is_some() => (
                    RunStatus::Cancelled,
                    Some(format!(
                        "{} The script stopped with {how}.",
                        if record.cancel.as_ref().is_some_and(|c| c.forced) { "Forcibly stopped." } else { "Cancelled." }
                    )),
                ),
                None => (
                    RunStatus::Failed,
                    Some(format!("The script exited with {how} without reporting how it finished. Treat this as a failure.")),
                ),
            }
        };
        record.status = final_status;
        record.message = message;
        let phase_end = match final_status {
            RunStatus::Succeeded => PhaseStatus::Complete,
            RunStatus::Cancelled => PhaseStatus::Skipped,
            _ => PhaseStatus::Failed,
        };
        let at = now();
        for phase in record.phases.iter_mut().filter(|p| p.status == PhaseStatus::Running) {
            phase.status = phase_end;
            phase.ended_at = Some(at.clone());
        }
        record.current_activity = None;
        if !run.pending.is_empty() {
            self.sink.events(run_id, std::mem::take(&mut run.pending));
        }
        if let Some(files) = run.files.as_mut() {
            files.flush();
        }
        let _ = self.history.save(&run.record);
        drop(runs);
        self.sink.updated(&run.record);
        self.sink.attention(&run.record, Attention::Finished);
    }

    pub fn respond(&self, run_id: &str, request_id: &str, payload: ResponsePayload) -> Result<RunRecord> {
        let mut runs = self.runs.lock().unwrap();
        let run = runs.get_mut(run_id).ok_or_else(|| EngineError::UnknownRun(run_id.to_string()))?;
        let request = run
            .record
            .pending_request
            .clone()
            .filter(|r| r.request_id() == Some(request_id))
            .ok_or_else(|| EngineError::InvalidResponse("That request is no longer waiting for an answer.".into()))?;
        let (line, redacted) = build_response(&request, &payload).map_err(EngineError::InvalidResponse)?;
        let tx = run.stdin.as_ref().ok_or_else(|| EngineError::InvalidResponse("The script's input is closed.".into()))?;
        tx.send(line).map_err(|_| EngineError::InvalidResponse("The script's input is closed.".into()))?;

        let fields: Vec<_> = match &request {
            WorkflowEvent::Input { field, .. } => vec![field.clone()],
            WorkflowEvent::InputGroup { fields, .. } => fields.clone(),
            _ => vec![],
        };
        if let ResponsePayload::Values { values } = &payload {
            for field in fields.iter().filter(|f| f.is_sensitive()) {
                if let Some(Value::String(secret)) = values.get(&field.id) {
                    if secret.len() >= 3 && !run.secrets.contains(secret) {
                        run.secrets.push(secret.clone());
                    }
                }
            }
        }
        let title = match &request {
            WorkflowEvent::Input { title, field, .. } => title.clone().unwrap_or_else(|| field.label.clone()),
            WorkflowEvent::InputGroup { title, .. }
            | WorkflowEvent::Confirm { title, .. }
            | WorkflowEvent::ManualAction { title, .. } => title.clone(),
            _ => String::new(),
        };
        let (kind, confirmed) = match &payload {
            ResponsePayload::Values { .. } => ("values", None),
            ResponsePayload::Confirm { confirmed } => ("confirm", Some(*confirmed)),
            ResponsePayload::Acknowledge => ("acknowledge", None),
            ResponsePayload::Cancel => ("cancel", None),
        };
        run.record.responses.push(ResponseRecord {
            request_id: request_id.to_string(),
            at: now(),
            title,
            kind: kind.into(),
            values: redacted,
            confirmed,
        });
        run.record.pending_request = None;
        run.awaiting = Some(request);
        if payload == ResponsePayload::Cancel {
            run.record.cancel.get_or_insert(CancelRecord { requested_at: now(), forced: false });
            run.record.status = RunStatus::Cancelling;
        } else if run.record.status != RunStatus::Cancelling {
            run.record.status = RunStatus::Running;
        }
        let _ = self.history.save(&run.record);
        self.sink.updated(&run.record);
        Ok(run.record.clone())
    }

    pub fn cancel(self: &Arc<Self>, run_id: &str, force: bool) -> Result<RunRecord> {
        let _runtime = self.runtime.enter();
        let mut runs = self.runs.lock().unwrap();
        let run = runs.get_mut(run_id).ok_or_else(|| EngineError::UnknownRun(run_id.to_string()))?;
        if !force && !run.record.cancel_allowed && run.record.pending_request.is_none() {
            let phase = run.record.current_activity.clone().unwrap_or_else(|| "this phase".into());
            return Err(EngineError::NotCancellable(format!(
                "The script marked \"{phase}\" as unsafe to interrupt. Wait for it to finish."
            )));
        }
        let first_forced = force && !run.record.cancel.as_ref().is_some_and(|c| c.forced);
        run.record.cancel = Some(CancelRecord {
            requested_at: run.record.cancel.as_ref().map(|c| c.requested_at.clone()).unwrap_or_else(now),
            forced: force || run.record.cancel.as_ref().is_some_and(|c| c.forced),
        });
        run.record.status = RunStatus::Cancelling;
        if force {
            signal_group(run.pgid, libc::SIGTERM);
            if first_forced {
                let me = Arc::clone(self);
                let id = run_id.to_string();
                let pgid = run.pgid;
                tokio::spawn(async move {
                    tokio::time::sleep(FORCE_KILL_AFTER).await;
                    if me.runs.lock().unwrap().contains_key(&id) {
                        signal_group(pgid, libc::SIGKILL);
                    }
                });
            }
        } else if let Some(request) = run.record.pending_request.take() {
            // A script waiting for an answer stops cleanly on a cancelled
            // response, running its own cleanup.
            if let (Ok((line, _)), Some(tx)) = (build_response(&request, &ResponsePayload::Cancel), &run.stdin) {
                let _ = tx.send(line);
            } else {
                signal_group(run.pgid, libc::SIGINT);
            }
        } else {
            signal_group(run.pgid, libc::SIGINT);
        }
        let _ = self.history.save(&run.record);
        self.sink.updated(&run.record);
        Ok(run.record.clone())
    }

    /// Interrupt every active run, as the dashboard is closing.
    pub fn interrupt_all(&self) {
        let mut runs = self.runs.lock().unwrap();
        for run in runs.values_mut() {
            run.record.cancel.get_or_insert(CancelRecord { requested_at: now(), forced: false });
            run.record.status = RunStatus::Cancelling;
            signal_group(run.pgid, libc::SIGINT);
        }
    }

    pub fn active_runs(&self) -> Vec<RunRecord> {
        let runs = self.runs.lock().unwrap();
        let mut records: Vec<_> = runs.values().map(|r| r.record.clone()).collect();
        records.sort_by(|a, b| a.started_at.cmp(&b.started_at));
        records
    }

    pub fn details(&self, run_id: &str) -> Result<RunDetails> {
        if let Some(run) = self.runs.lock().unwrap().get(run_id) {
            return Ok(RunDetails {
                record: run.record.clone(),
                events: run.events.clone(),
                logs: run.logs.iter().cloned().collect(),
                logs_truncated: run.logs_dropped,
            });
        }
        self.history.details(run_id)
    }

    pub fn list_history(&self, limit: usize) -> Vec<RunRecord> {
        let active: HashMap<String, RunRecord> =
            self.runs.lock().unwrap().iter().map(|(k, v)| (k.clone(), v.record.clone())).collect();
        self.history
            .list(limit)
            .into_iter()
            .map(|record| active.get(&record.run_id).cloned().unwrap_or(record))
            .collect()
    }

    pub fn delete_history(&self, run_id: &str) -> Result<()> {
        if self.runs.lock().unwrap().contains_key(run_id) {
            return Err(EngineError::InvalidArgument("An active run cannot be deleted.".into()));
        }
        self.history.delete(run_id)
    }
}
