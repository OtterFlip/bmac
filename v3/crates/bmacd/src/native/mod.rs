// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Native operations: Rust code run as a job. The supervisor starts each as
//! `bmacd native OPERATION --json ARGS` and treats it as it treats a script:
//! it speaks `bmac-ui` v2 on stdout, gets answers on stdin, and is cancelled
//! with a `cancelled` answer or SIGINT. [`Workflow`] is that protocol from
//! the operation's side, plus checkpoints in the job's work directory, so a
//! resumed job skips what it already finished.

pub mod cluster;
pub mod disks;
pub mod join;
pub mod network;
pub mod orchestration;
pub mod packages;
pub mod setup;
pub mod storage;

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use bmac_core::job::{Field, Instruction, PhaseStatus, RequestForm, Severity};
use bmac_exec::CancellationToken;
use bmac_proto::bmac_ui::{CompletedStatus, PROTOCOL_NAME, PROTOCOL_VERSION, WorkflowEvent};
use serde_json::{Map, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt};
use tokio::sync::mpsc;

/// The checkpoint file in the job's work directory.
const CHECKPOINTS: &str = "checkpoints.json";
/// Written in the job's work directory before the job restarts its host:
/// the boot it restarts from. A starting `bmacd` resumes the job only if
/// the host has booted since.
pub const RESTART_MARKER: &str = "restart-expected";
const BOOT_ID: &str = "/proc/sys/kernel/random/boot_id";

/// This boot's ID, which changes on every boot.
pub fn boot_id() -> Result<String, String> {
    std::fs::read_to_string(BOOT_ID)
        .map(|id| id.trim().to_string())
        .map_err(|e| format!("cannot read {BOOT_ID}: {e}"))
}

/// Whether the job whose work directory is `work` asked for a restart and
/// the host has booted since, given this boot's ID.
pub fn restarted_since_asked(work: &Path, boot_id: &str) -> bool {
    std::fs::read_to_string(work.join(RESTART_MARKER))
        .is_ok_and(|before| !before.trim().is_empty() && before.trim() != boot_id)
}

/// Why an operation stopped before finishing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    Cancelled,
    Failed {
        message: String,
        details: Option<String>,
    },
}

impl Stop {
    pub fn failed(message: impl Into<String>) -> Stop {
        Stop::Failed {
            message: message.into(),
            details: None,
        }
    }
}

impl From<String> for Stop {
    fn from(message: String) -> Stop {
        Stop::failed(message)
    }
}

/// An answer, as the supervisor relays it.
#[derive(Debug, Clone, PartialEq)]
enum Reply {
    Values(Map<String, Value>),
    Confirmed(bool),
    Acknowledged,
    Cancelled,
}

fn parse_reply(line: &str) -> Option<(String, Reply)> {
    let value: Value = serde_json::from_str(line).ok()?;
    match value["type"].as_str()? {
        "cancel" => Some((String::new(), Reply::Cancelled)),
        "response" => {
            let id = value["request_id"].as_str()?.to_string();
            let reply = if value["cancelled"] == true {
                Reply::Cancelled
            } else if let Some(values) = value["values"].as_object() {
                Reply::Values(values.clone())
            } else if let Some(confirmed) = value["confirmed"].as_bool() {
                Reply::Confirmed(confirmed)
            } else if value["acknowledged"] == true {
                Reply::Acknowledged
            } else {
                return None;
            };
            Some((id, reply))
        }
        _ => None,
    }
}

fn request_event(request_id: String, form: RequestForm) -> WorkflowEvent {
    match form {
        RequestForm::Input {
            title,
            description,
            context,
            field,
        } => WorkflowEvent::Input {
            request_id,
            title,
            description,
            context,
            field,
        },
        RequestForm::InputGroup {
            title,
            description,
            context,
            layout,
            fields,
        } => WorkflowEvent::InputGroup {
            request_id,
            title,
            description,
            context,
            layout,
            fields,
        },
        RequestForm::Confirm {
            title,
            message,
            severity,
            confirm_label,
            cancel_label,
            confirmation_text,
            context,
            details,
        } => WorkflowEvent::Confirm {
            request_id,
            title,
            message,
            severity,
            confirm_label,
            cancel_label,
            confirmation_text,
            context,
            details,
        },
        RequestForm::ManualAction {
            title,
            instructions,
            acknowledge_label,
            context,
        } => WorkflowEvent::ManualAction {
            request_id,
            title,
            instructions,
            acknowledge_label,
            context,
        },
    }
}

/// The request in `event`, if it is one, with its ID.
pub fn request_form(event: &WorkflowEvent) -> Option<(String, RequestForm)> {
    let event = event.clone();
    Some(match event {
        WorkflowEvent::Input {
            request_id,
            title,
            description,
            context,
            field,
        } => (
            request_id,
            RequestForm::Input {
                title,
                description,
                context,
                field,
            },
        ),
        WorkflowEvent::InputGroup {
            request_id,
            title,
            description,
            context,
            layout,
            fields,
        } => (
            request_id,
            RequestForm::InputGroup {
                title,
                description,
                context,
                layout,
                fields,
            },
        ),
        WorkflowEvent::Confirm {
            request_id,
            title,
            message,
            severity,
            confirm_label,
            cancel_label,
            confirmation_text,
            context,
            details,
        } => (
            request_id,
            RequestForm::Confirm {
                title,
                message,
                severity,
                confirm_label,
                cancel_label,
                confirmation_text,
                context,
                details,
            },
        ),
        WorkflowEvent::ManualAction {
            request_id,
            title,
            instructions,
            acknowledge_label,
            context,
        } => (
            request_id,
            RequestForm::ManualAction {
                title,
                instructions,
                acknowledge_label,
                context,
            },
        ),
        _ => return None,
    })
}

/// How many requests the operation has asked, kept with its checkpoints:
/// one resumed after a restart is the same job, so its request IDs carry on
/// rather than repeat ones already answered.
const REQUESTS_FILE: &str = "requests";

/// A native operation's side of the protocol.
pub struct Workflow {
    out: Mutex<Box<dyn Write + Send>>,
    replies: tokio::sync::Mutex<mpsc::UnboundedReceiver<(String, Reply)>>,
    cancel: CancellationToken,
    requests: AtomicU64,
    work: PathBuf,
}

impl Workflow {
    /// Reads answers from `input` until it closes; a `cancel` line, or the
    /// input closing, cancels.
    pub fn new(
        out: Box<dyn Write + Send>,
        input: impl AsyncBufRead + Unpin + Send + 'static,
        work: PathBuf,
    ) -> Workflow {
        let cancel = CancellationToken::new();
        let (tx, rx) = mpsc::unbounded_channel();
        let token = cancel.clone();
        tokio::spawn(async move {
            let mut lines = input.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                match parse_reply(&line) {
                    Some((id, Reply::Cancelled)) if id.is_empty() => token.cancel(),
                    Some(reply) => {
                        let _ = tx.send(reply);
                    }
                    None => {}
                }
            }
            token.cancel();
        });
        let asked = std::fs::read_to_string(work.join(REQUESTS_FILE))
            .ok()
            .and_then(|n| n.trim().parse().ok())
            .unwrap_or(0);
        Workflow {
            out: Mutex::new(out),
            replies: tokio::sync::Mutex::new(rx),
            cancel,
            requests: AtomicU64::new(asked),
            work,
        }
    }

    /// On the supervisor's pipes, with SIGINT as a cancel.
    pub fn stdio() -> Workflow {
        let work = std::env::var_os("BMAC_WORK_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        Workflow::stdio_in(work)
    }

    /// On stdin and stdout with its checkpoints in `work`.
    pub fn stdio_in(work: PathBuf) -> Workflow {
        let workflow = Workflow::new(
            Box::new(std::io::stdout()),
            tokio::io::BufReader::new(tokio::io::stdin()),
            work,
        );
        let token = workflow.cancel.clone();
        tokio::spawn(async move {
            if let Ok(mut interrupt) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            {
                interrupt.recv().await;
                token.cancel();
            }
        });
        workflow
    }

    pub fn token(&self) -> &CancellationToken {
        &self.cancel
    }

    pub fn work_dir(&self) -> &Path {
        &self.work
    }

    /// Stops with [`Stop::Cancelled`] once a cancel has been asked for.
    pub fn check(&self) -> Result<(), Stop> {
        if self.cancel.is_cancelled() {
            Err(Stop::Cancelled)
        } else {
            Ok(())
        }
    }

    /// Passes on an event another operation sent, such as a phase or a
    /// message from a host this one drives.
    pub fn forward(&self, event: &WorkflowEvent) {
        self.emit(event);
    }

    fn emit(&self, event: &WorkflowEvent) {
        if let Ok(line) = serde_json::to_string(event) {
            let mut out = self.out.lock().unwrap();
            let _ = writeln!(out, "{line}");
            let _ = out.flush();
        }
    }

    /// The protocol header and the workflow's description.
    pub fn begin(&self, workflow: &str, title: &str, description: &str) {
        self.emit(&WorkflowEvent::Protocol {
            protocol: PROTOCOL_NAME.into(),
            version: PROTOCOL_VERSION,
            bmac_version: Some(crate::VERSION.into()),
        });
        self.emit(&WorkflowEvent::WorkflowStarted {
            workflow: workflow.into(),
            run_id: std::env::var("BMAC_UI_RUN_ID").unwrap_or_default(),
            script: None,
            argv: Vec::new(),
            pid: Some(std::process::id()),
            started_at: None,
        });
        self.emit(&WorkflowEvent::WorkflowMetadata {
            workflow: Some(workflow.into()),
            title: Some(title.into()),
            description: Some(description.into()),
            category: None,
            destructive: None,
            supports_dry_run: Some(false),
            supports_cancel: Some(true),
        });
    }

    pub fn phase(&self, id: &str, label: &str, status: PhaseStatus) {
        self.phase_with(id, label, status, true);
    }

    /// A phase a soft cancel may not interrupt while it runs.
    pub fn phase_with(&self, id: &str, label: &str, status: PhaseStatus, cancel_allowed: bool) {
        self.emit(&WorkflowEvent::Phase {
            id: id.into(),
            label: label.into(),
            status,
            cancel_allowed: Some(cancel_allowed),
        });
    }

    pub fn info(&self, message: impl Into<String>) {
        self.emit(&WorkflowEvent::Info {
            message: message.into(),
        });
    }

    pub fn warning(&self, message: impl Into<String>) {
        self.emit(&WorkflowEvent::Warning {
            message: message.into(),
        });
    }

    pub fn result(&self, data: Value) {
        self.emit(&WorkflowEvent::Result { data });
    }

    pub fn next_step(&self, text: impl Into<String>) {
        self.emit(&WorkflowEvent::NextStep {
            text: text.into(),
            command: None,
            workflow: None,
            args: None,
        });
    }

    async fn ask(&self, form: RequestForm) -> Result<(String, Reply), Stop> {
        self.check()?;
        let n = self.requests.fetch_add(1, Ordering::Relaxed) + 1;
        std::fs::write(self.work.join(REQUESTS_FILE), format!("{n}\n"))
            .map_err(|e| Stop::failed(format!("cannot record the request count: {e}")))?;
        let id = format!("r{n}");
        self.emit(&request_event(id.clone(), form));
        self.reply(&id).await.map(|reply| (id, reply))
    }

    async fn reply(&self, id: &str) -> Result<Reply, Stop> {
        let mut replies = self.replies.lock().await;
        loop {
            let next = tokio::select! {
                next = replies.recv() => next,
                _ = self.cancel.cancelled() => return Err(Stop::Cancelled),
            };
            match next {
                None => return Err(Stop::Cancelled),
                Some((_, Reply::Cancelled)) => return Err(Stop::Cancelled),
                Some((answered, reply)) if answered == id => return Ok(reply),
                Some(_) => {}
            }
        }
    }

    /// Asks for values; `check` refuses them with per-field messages, and
    /// the same request stays open until it accepts them.
    pub async fn values(
        &self,
        form: RequestForm,
        check: impl Fn(&Map<String, Value>) -> Result<(), BTreeMap<String, String>>,
    ) -> Result<Map<String, Value>, Stop> {
        let (id, mut reply) = self.ask(form).await?;
        loop {
            let Reply::Values(values) = reply else {
                return Err(Stop::failed("the answer was not a set of values"));
            };
            match check(&values) {
                Ok(()) => return Ok(values),
                Err(field_errors) => {
                    self.emit(&WorkflowEvent::ValidationError {
                        request_id: id.clone(),
                        field_errors,
                        message: None,
                    });
                    reply = self.reply(&id).await?;
                }
            }
        }
    }

    pub async fn input_group(
        &self,
        title: &str,
        description: Option<&str>,
        fields: Vec<Field>,
        check: impl Fn(&Map<String, Value>) -> Result<(), BTreeMap<String, String>>,
    ) -> Result<Map<String, Value>, Stop> {
        self.values(
            RequestForm::InputGroup {
                title: title.into(),
                description: description.map(str::to_string),
                context: None,
                layout: None,
                fields,
            },
            check,
        )
        .await
    }

    /// Whether the operator confirmed.
    pub async fn confirm(
        &self,
        title: &str,
        message: &str,
        severity: Severity,
        confirmation_text: Option<&str>,
        details: Vec<String>,
    ) -> Result<bool, Stop> {
        match self
            .ask(RequestForm::Confirm {
                title: title.into(),
                message: message.into(),
                severity,
                confirm_label: None,
                cancel_label: None,
                confirmation_text: confirmation_text.map(str::to_string),
                context: None,
                details,
            })
            .await?
            .1
        {
            Reply::Confirmed(confirmed) => Ok(confirmed),
            _ => Err(Stop::failed("the answer was not a confirmation")),
        }
    }

    /// Waits for the operator to say a manual step is done.
    pub async fn manual(
        &self,
        title: &str,
        instructions: Vec<Instruction>,
        acknowledge_label: Option<&str>,
    ) -> Result<(), Stop> {
        match self
            .ask(RequestForm::ManualAction {
                title: title.into(),
                instructions,
                acknowledge_label: acknowledge_label.map(str::to_string),
                context: None,
            })
            .await?
            .1
        {
            Reply::Acknowledged => Ok(()),
            _ => Err(Stop::failed("the answer was not an acknowledgement")),
        }
    }

    /// Asks the operator `form` on behalf of another operation and returns
    /// the answer as that operation reads it: the `response` line's fields
    /// other than its type and request ID. Nothing is checked here; the
    /// other operation refuses an answer it can't take.
    pub async fn relay(&self, form: RequestForm) -> Result<Map<String, Value>, Stop> {
        let mut answer = Map::new();
        match self.ask(form).await?.1 {
            Reply::Values(values) => {
                answer.insert("values".into(), Value::Object(values));
            }
            Reply::Confirmed(confirmed) => {
                answer.insert("confirmed".into(), Value::Bool(confirmed));
            }
            Reply::Acknowledged => {
                answer.insert("acknowledged".into(), Value::Bool(true));
            }
            Reply::Cancelled => return Err(Stop::Cancelled),
        }
        Ok(answer)
    }

    /// The last event: how the operation ended.
    pub fn finish(&self, outcome: &Result<(), Stop>) -> ExitCode {
        let (status, message, code) = match outcome {
            Ok(()) => (CompletedStatus::Success, None, 0),
            Err(Stop::Cancelled) => (CompletedStatus::Cancelled, Some("Cancelled.".into()), 130),
            Err(Stop::Failed { message, details }) => {
                self.emit(&WorkflowEvent::Error {
                    code: None,
                    message: message.clone(),
                    details: details.clone(),
                    recoverable: false,
                });
                (CompletedStatus::Failed, Some(message.clone()), 1)
            }
        };
        self.emit(&WorkflowEvent::Completed {
            status,
            message,
            exit_code: Some(code),
            finished_at: None,
        });
        ExitCode::from(code as u8)
    }

    pub fn checkpoints(&self) -> Checkpoints {
        Checkpoints::load(self.work.join(CHECKPOINTS))
    }

    /// Call just before restarting the host: once it has booted again,
    /// `bmacd` resumes this job by itself.
    pub fn expect_restart(&self, boot_id: &str) -> Result<(), Stop> {
        write_atomic(&self.work.join(RESTART_MARKER), boot_id.as_bytes())
            .map_err(|e| Stop::failed(format!("cannot record the expected restart: {e}")))
    }

    /// After resuming: `Some(true)` if the restart this job asked for
    /// happened, `Some(false)` if the host hasn't booted since, `None` if it
    /// asked for none. A restart that happened is forgotten, so the next
    /// one is asked for afresh.
    pub fn take_restart(&self, boot_id: &str) -> Result<Option<bool>, Stop> {
        let marker = self.work.join(RESTART_MARKER);
        if !marker.exists() {
            return Ok(None);
        }
        if !restarted_since_asked(&self.work, boot_id) {
            return Ok(Some(false));
        }
        std::fs::remove_file(&marker)
            .map_err(|e| Stop::failed(format!("cannot clear {}: {e}", marker.display())))?;
        Ok(Some(true))
    }
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let temporary = path.with_extension("new");
    let mut file = std::fs::File::create(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&temporary, path)
}

/// What a job has finished, kept in its work directory across restarts.
#[derive(Debug)]
pub struct Checkpoints {
    path: PathBuf,
    done: BTreeMap<String, Value>,
}

impl Checkpoints {
    pub fn load(path: PathBuf) -> Checkpoints {
        let done = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Checkpoints { path, done }
    }

    pub fn get(&self, step: &str) -> Option<&Value> {
        self.done.get(step)
    }

    pub fn is_done(&self, step: &str) -> bool {
        self.done.contains_key(step)
    }

    /// Records `step` as done, written atomically before this returns.
    pub fn record(&mut self, step: &str, data: Value) -> Result<(), Stop> {
        self.done.insert(step.into(), data);
        let text = serde_json::to_vec_pretty(&self.done).map_err(|e| e.to_string())?;
        write_atomic(&self.path, &text)
            .map_err(|e| Stop::failed(format!("cannot record checkpoint {step}: {e}")))
    }
}

/// `bmacd native OPERATION --json ARGS`.
pub fn main(args: &[String]) -> ExitCode {
    let (Some(operation), Some("--json")) = (args.first(), args.get(1).map(String::as_str)) else {
        eprintln!("usage: bmacd native OPERATION --json [ARGS...]");
        return ExitCode::from(2);
    };
    let rest = &args[2..];
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("cannot start the runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let code = runtime.block_on(async {
        match operation.as_str() {
            setup::OPERATION if rest.iter().any(|a| a == "--driven-by") => {
                let work = PathBuf::from(setup::DRIVEN_WORK);
                if let Err(e) = std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(&work)
                {
                    eprintln!("cannot create {}: {e}", work.display());
                    return ExitCode::FAILURE;
                }
                setup::run(&Workflow::stdio_in(work), rest).await
            }
            setup::OPERATION => setup::run(&Workflow::stdio(), rest).await,
            join::OPERATION => join::run(&Workflow::stdio(), rest).await,
            id if disks::OPERATIONS.contains(&id) => disks::run(id, &Workflow::stdio(), rest).await,
            crate::backup::capture::OPERATION => {
                crate::backup::capture::run(&Workflow::stdio(), rest).await
            }
            crate::backup::restore::OPERATION => {
                crate::backup::restore::run(&Workflow::stdio(), rest).await
            }
            other => {
                eprintln!("{other} is not a native operation");
                ExitCode::from(2)
            }
        }
    });
    // The engine keeps stdin open until this process exits, and a plain
    // drop would wait for the blocked read of it.
    runtime.shutdown_background();
    code
}

#[cfg(test)]
pub(crate) mod tests;
