// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Long work in the Deployer, shown like a `bmacd` job: phases, requests,
//! output, a result, and an end state. The work is ordinary async Rust
//! that reports through a [`TaskContext`]; the operator's answers come back
//! through [`Tasks::answer`]. Tasks live only in memory: the Deployer keeps
//! no history between runs.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bmac_core::job::JobState;
use bmac_core::job::{Answer, EventBody, Field, LogLevel, PhaseStatus, REDACTED, RequestForm};
use bmac_proto::deployer::{AnswerTask, TaskEvent, TaskSummary, TaskView};
use serde_json::Value;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

pub type Values = BTreeMap<String, Value>;
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// Where task changes go: the window, through Tauri events.
pub trait TaskSink: Send + Sync {
    fn event(&self, event: &TaskEvent);
    fn updated(&self, summary: &TaskSummary);
}

/// A sink that drops everything, for tests and headless use.
pub struct NoSink;

impl TaskSink for NoSink {
    fn event(&self, _: &TaskEvent) {}
    fn updated(&self, _: &TaskSummary) {}
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TaskError {
    #[error("there is no task {0}")]
    NotFound(String),
    #[error("{0}")]
    Refused(String),
}

/// How a task's work stops early.
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

impl From<&str> for Stop {
    fn from(message: &str) -> Stop {
        Stop::failed(message)
    }
}

pub type TaskResult = Result<(), Stop>;

struct Pending {
    request_id: String,
    form: RequestForm,
    reply: oneshot::Sender<Answer>,
}

struct Entry {
    summary: TaskSummary,
    parameters: BTreeMap<String, String>,
    events: Vec<TaskEvent>,
    pending: Option<Pending>,
    cancel: CancellationToken,
}

struct Shared {
    tasks: Mutex<BTreeMap<String, Entry>>,
    sink: Arc<dyn TaskSink>,
    clock: Clock,
    requests: AtomicU64,
}

impl Shared {
    /// Records `body` on task `id` and tells the sink.
    fn emit(&self, id: &str, body: EventBody) {
        let (event, summary) = {
            let mut tasks = self.tasks.lock().unwrap_or_else(|p| p.into_inner());
            let Some(entry) = tasks.get_mut(id) else {
                return;
            };
            let now = (self.clock)();
            let event = TaskEvent {
                task: id.to_string(),
                seq: entry.summary.last_seq + 1,
                at_ms: now,
                body,
            };
            entry.summary.last_seq = event.seq;
            entry.summary.updated_at_ms = now;
            if let Some(state) = event.body.state() {
                entry.summary.state = state;
            }
            match &event.body {
                EventBody::Completed { message } | EventBody::Cancelled { message, .. } => {
                    entry.summary.message = message.clone();
                }
                EventBody::Failed { message, .. } => entry.summary.message = Some(message.clone()),
                _ => {}
            }
            entry.events.push(event.clone());
            (event, entry.summary.clone())
        };
        self.sink.event(&event);
        self.sink.updated(&summary);
    }

    fn update(&self, id: &str, change: impl FnOnce(&mut Entry)) {
        let summary = {
            let mut tasks = self.tasks.lock().unwrap_or_else(|p| p.into_inner());
            let Some(entry) = tasks.get_mut(id) else {
                return;
            };
            change(entry);
            entry.summary.updated_at_ms = (self.clock)();
            entry.summary.clone()
        };
        self.sink.updated(&summary);
    }
}

/// Every task of this run of the Deployer.
#[derive(Clone)]
pub struct Tasks {
    shared: Arc<Shared>,
}

impl Tasks {
    pub fn new(sink: Arc<dyn TaskSink>, clock: Clock) -> Tasks {
        Tasks {
            shared: Arc::new(Shared {
                tasks: Mutex::new(BTreeMap::new()),
                sink,
                clock,
                requests: AtomicU64::new(0),
            }),
        }
    }

    /// Starts `work` on the Tokio runtime. A panic in it fails the task.
    pub fn start<F, Fut>(
        &self,
        kind: &str,
        title: &str,
        parameters: BTreeMap<String, String>,
        work: F,
    ) -> TaskSummary
    where
        F: FnOnce(TaskContext) -> Fut + Send + 'static,
        Fut: Future<Output = TaskResult> + Send + 'static,
    {
        let now = (self.shared.clock)();
        let id = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
        let cancel = CancellationToken::new();
        let summary = TaskSummary {
            id: id.clone(),
            kind: kind.to_string(),
            title: title.to_string(),
            state: JobState::Running,
            created_at_ms: now,
            updated_at_ms: now,
            pending_request: None,
            message: None,
            cancel_requested: false,
            last_seq: 0,
        };
        self.shared
            .tasks
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(
                id.clone(),
                Entry {
                    summary: summary.clone(),
                    parameters,
                    events: Vec::new(),
                    pending: None,
                    cancel: cancel.clone(),
                },
            );
        self.shared.sink.updated(&summary);
        let context = TaskContext {
            shared: self.shared.clone(),
            id: id.clone(),
            cancel,
        };
        let shared = self.shared.clone();
        tokio::spawn(async move {
            let ended = tokio::spawn(work(context)).await;
            let body = match ended {
                Ok(Ok(())) => EventBody::Completed { message: None },
                Ok(Err(Stop::Cancelled)) => EventBody::Cancelled {
                    by: None,
                    message: Some("Cancelled.".into()),
                },
                Ok(Err(Stop::Failed { message, details })) => EventBody::Failed {
                    code: None,
                    message,
                    details,
                },
                Err(error) => EventBody::Failed {
                    code: Some("internal".into()),
                    message: format!("the task stopped unexpectedly: {error}"),
                    details: None,
                },
            };
            shared.update(&id, |entry| {
                entry.pending = None;
                entry.summary.pending_request = None;
            });
            shared.emit(&id, body);
        });
        summary
    }

    /// Newest first.
    pub fn list(&self) -> Vec<TaskSummary> {
        let tasks = self.shared.tasks.lock().unwrap_or_else(|p| p.into_inner());
        let mut list: Vec<TaskSummary> = tasks.values().map(|e| e.summary.clone()).collect();
        list.sort_by(|a, b| b.created_at_ms.cmp(&a.created_at_ms).then(b.id.cmp(&a.id)));
        list
    }

    pub fn view(&self, id: &str) -> Result<TaskView, TaskError> {
        let tasks = self.shared.tasks.lock().unwrap_or_else(|p| p.into_inner());
        let entry = tasks
            .get(id)
            .ok_or_else(|| TaskError::NotFound(id.into()))?;
        Ok(TaskView {
            task: entry.summary.clone(),
            parameters: entry.parameters.clone(),
            events: entry.events.clone(),
        })
    }

    /// Hands the operator's answer to the waiting work, after checking it
    /// fits the pending request. The work records it once it accepts it.
    pub fn answer(&self, answer: AnswerTask) -> Result<(), TaskError> {
        let pending = {
            let mut tasks = self.shared.tasks.lock().unwrap_or_else(|p| p.into_inner());
            let entry = tasks
                .get_mut(&answer.task)
                .ok_or_else(|| TaskError::NotFound(answer.task.clone()))?;
            if entry.summary.state.is_terminal() {
                return Err(TaskError::Refused("the task has already ended".into()));
            }
            let pending = match &entry.pending {
                Some(p) if p.request_id == answer.request_id => p,
                _ => {
                    return Err(TaskError::Refused(
                        "that request is no longer waiting for an answer".into(),
                    ));
                }
            };
            fits(&pending.form, &answer.answer).map_err(TaskError::Refused)?;
            entry.summary.pending_request = None;
            entry.pending.take()
        };
        if let Some(pending) = pending {
            let _ = pending.reply.send(answer.answer);
        }
        let task = answer.task.clone();
        self.shared.update(&task, |_| {});
        Ok(())
    }

    /// Asks the task to stop: a pending request is withdrawn, and the work
    /// sees its token cancelled.
    pub fn cancel(&self, id: &str) -> Result<(), TaskError> {
        let cancel = {
            let mut tasks = self.shared.tasks.lock().unwrap_or_else(|p| p.into_inner());
            let entry = tasks
                .get_mut(id)
                .ok_or_else(|| TaskError::NotFound(id.into()))?;
            if entry.summary.state.is_terminal() {
                return Ok(());
            }
            entry.summary.cancel_requested = true;
            entry.cancel.clone()
        };
        cancel.cancel();
        self.shared.update(id, |_| {});
        Ok(())
    }

    /// How many tasks haven't ended.
    pub fn running(&self) -> usize {
        let tasks = self.shared.tasks.lock().unwrap_or_else(|p| p.into_inner());
        tasks
            .values()
            .filter(|e| !e.summary.state.is_terminal())
            .count()
    }

    pub fn cancel_all(&self) {
        let ids: Vec<String> = self.list().into_iter().map(|s| s.id).collect();
        for id in ids {
            let _ = self.cancel(&id);
        }
    }
}

/// Whether `answer` is the right kind for `form`, with every required field
/// and no unknown one.
fn fits(form: &RequestForm, answer: &Answer) -> Result<(), String> {
    match (form, answer) {
        (_, Answer::Cancel) => Ok(()),
        (RequestForm::Input { .. } | RequestForm::InputGroup { .. }, Answer::Values { values }) => {
            let fields = form.fields();
            if let Some(unknown) = values.keys().find(|k| !fields.iter().any(|f| &f.id == *k)) {
                return Err(format!("{unknown} is not a field of this request"));
            }
            for field in fields.iter().filter(|f| f.required && !f.disabled) {
                match values.get(&field.id) {
                    None | Some(Value::Null) => return Err(format!("{} is required", field.label)),
                    Some(Value::String(s)) if s.trim().is_empty() => {
                        return Err(format!("{} is required", field.label));
                    }
                    _ => {}
                }
            }
            Ok(())
        }
        (RequestForm::Confirm { .. }, Answer::Confirm { .. }) => Ok(()),
        (RequestForm::ManualAction { .. }, Answer::Acknowledge) => Ok(()),
        _ => Err("that answer doesn't fit the request".into()),
    }
}

fn redacted(fields: &[Field], values: &Values) -> Values {
    values
        .iter()
        .map(|(k, v)| {
            let secret = fields.iter().any(|f| &f.id == k && f.is_sensitive());
            (
                k.clone(),
                if secret {
                    Value::String(REDACTED.into())
                } else {
                    v.clone()
                },
            )
        })
        .collect()
}

/// What a task's work uses to report and to ask.
#[derive(Clone)]
pub struct TaskContext {
    shared: Arc<Shared>,
    id: String,
    cancel: CancellationToken,
}

impl TaskContext {
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Cancelled when the operator cancels the task.
    pub fn token(&self) -> &CancellationToken {
        &self.cancel
    }

    /// `Err(Stop::Cancelled)` once the operator has cancelled.
    pub fn check(&self) -> TaskResult {
        if self.cancel.is_cancelled() {
            Err(Stop::Cancelled)
        } else {
            Ok(())
        }
    }

    pub fn phase(&self, id: &str, label: &str, status: PhaseStatus) {
        self.shared.emit(
            &self.id,
            EventBody::Phase {
                id: id.into(),
                label: label.into(),
                status,
                cancel_allowed: None,
            },
        );
    }

    pub fn log(&self, level: LogLevel, text: impl Into<String>) {
        self.shared.emit(
            &self.id,
            EventBody::Output {
                stream: None,
                level,
                text: text.into(),
                source: None,
            },
        );
    }

    pub fn output(&self, text: impl Into<String>) {
        self.log(LogLevel::Info, text);
    }

    pub fn warning(&self, message: impl Into<String>) {
        self.shared.emit(
            &self.id,
            EventBody::Warning {
                message: message.into(),
            },
        );
    }

    pub fn progress(
        &self,
        message: impl Into<String>,
        current: Option<f64>,
        total: Option<f64>,
        unit: Option<&str>,
    ) {
        self.shared.emit(
            &self.id,
            EventBody::Progress {
                message: message.into(),
                current,
                total,
                unit: unit.map(str::to_string),
                phase: None,
            },
        );
    }

    pub fn result(&self, data: Value) {
        self.shared.emit(&self.id, EventBody::Result { data });
    }

    pub fn next_step(&self, text: impl Into<String>) {
        self.shared.emit(
            &self.id,
            EventBody::NextStep {
                text: text.into(),
                operation: None,
                args: BTreeMap::new(),
            },
        );
    }

    /// Raises `form` and waits for an answer that fits it. A cancel, by
    /// answer or by cancelling the task, stops the work.
    async fn raise(&self, form: RequestForm) -> Result<(String, RequestForm, Answer), Stop> {
        let mut form = form;
        form.normalize().map_err(Stop::failed)?;
        let request_id = format!(
            "r{}",
            self.shared.requests.fetch_add(1, Ordering::Relaxed) + 1
        );
        self.shared.emit(
            &self.id,
            EventBody::Request {
                request_id: request_id.clone(),
                form: Box::new(form.clone()),
            },
        );
        let answer = self.wait(&request_id, &form).await?;
        Ok((request_id, form, answer))
    }

    /// Registers `request_id` as pending and waits for its answer.
    async fn wait(&self, request_id: &str, form: &RequestForm) -> Result<Answer, Stop> {
        let (reply, receive) = oneshot::channel();
        self.shared.update(&self.id, |entry| {
            entry.summary.pending_request = Some(request_id.to_string());
            entry.pending = Some(Pending {
                request_id: request_id.to_string(),
                form: form.clone(),
                reply,
            });
        });
        let answer = tokio::select! {
            answer = receive => answer.map_err(|_| Stop::Cancelled)?,
            _ = self.cancel.cancelled() => {
                self.shared.update(&self.id, |entry| {
                    entry.pending = None;
                    entry.summary.pending_request = None;
                });
                return Err(Stop::Cancelled);
            }
        };
        if answer == Answer::Cancel {
            self.answered(request_id, None);
            return Err(Stop::Cancelled);
        }
        Ok(answer)
    }

    fn answered(&self, request_id: &str, values: Option<Values>) {
        self.shared.emit(
            &self.id,
            EventBody::Answered {
                request_id: request_id.into(),
                by: "operator".into(),
                values,
            },
        );
    }

    /// Asks for values; `validate` may refuse them with an error per field
    /// and a message, and the same request stays open until it accepts.
    pub async fn values(
        &self,
        form: RequestForm,
        validate: impl Fn(&Values) -> Result<(), (BTreeMap<String, String>, Option<String>)>,
    ) -> Result<Values, Stop> {
        let (request_id, form, mut answer) = self.raise(form).await?;
        loop {
            let Answer::Values { values } = answer else {
                return Err(Stop::failed("an answer of the wrong kind arrived"));
            };
            match validate(&values) {
                Ok(()) => {
                    self.answered(&request_id, Some(redacted(form.fields(), &values)));
                    return Ok(values);
                }
                Err((field_errors, message)) => {
                    self.shared.emit(
                        &self.id,
                        EventBody::ValidationError {
                            request_id: request_id.clone(),
                            field_errors,
                            message,
                        },
                    );
                    answer = self.wait(&request_id, &form).await?;
                }
            }
        }
    }

    /// Asks a yes-or-no question; `false` when declined.
    pub async fn confirm(&self, form: RequestForm) -> Result<bool, Stop> {
        let (request_id, _, answer) = self.raise(form).await?;
        let Answer::Confirm { confirmed } = answer else {
            return Err(Stop::failed("an answer of the wrong kind arrived"));
        };
        self.answered(&request_id, None);
        Ok(confirmed)
    }

    /// Shows a manual action and waits until the operator says it is done.
    pub async fn acknowledge(&self, form: RequestForm) -> TaskResult {
        let (request_id, _, _) = self.raise(form).await?;
        self.answered(&request_id, None);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmac_core::job::{FieldType, Instruction, Severity};
    use serde_json::json;
    use std::time::Duration;

    #[derive(Default)]
    struct Recorder {
        events: Mutex<Vec<TaskEvent>>,
    }

    impl TaskSink for Recorder {
        fn event(&self, event: &TaskEvent) {
            self.events.lock().unwrap().push(event.clone());
        }
        fn updated(&self, _: &TaskSummary) {}
    }

    fn tasks() -> (Tasks, Arc<Recorder>) {
        let recorder = Arc::new(Recorder::default());
        let clock: Clock = Arc::new(|| 1_000);
        (Tasks::new(recorder.clone(), clock), recorder)
    }

    fn field(id: &str, kind: FieldType, required: bool) -> Field {
        serde_json::from_value(json!({"id": id, "type": kind, "label": id, "required": required}))
            .unwrap()
    }

    async fn until(tasks: &Tasks, id: &str, test: impl Fn(&TaskView) -> bool) -> TaskView {
        for _ in 0..500 {
            let view = tasks.view(id).unwrap();
            if test(&view) {
                return view;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!(
            "task {id} never reached the expected state: {:?}",
            tasks.view(id)
        );
    }

    #[tokio::test]
    async fn a_task_reports_phases_and_completes() {
        let (tasks, recorder) = tasks();
        let summary = tasks.start("check", "Check", BTreeMap::new(), |ctx| async move {
            ctx.phase("one", "First", PhaseStatus::Running);
            ctx.output("working");
            ctx.phase("one", "First", PhaseStatus::Complete);
            ctx.result(json!({"ok": true}));
            Ok(())
        });
        let view = until(&tasks, &summary.id, |v| v.task.state.is_terminal()).await;
        assert_eq!(view.task.state, JobState::Completed);
        let seqs: Vec<u64> = view.events.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![1, 2, 3, 4, 5]);
        assert_eq!(recorder.events.lock().unwrap().len(), 5);
        assert_eq!(tasks.running(), 0);
    }

    #[tokio::test]
    async fn refused_values_keep_the_request_open_and_secrets_are_redacted() {
        let (tasks, _) = tasks();
        let summary = tasks.start("ask", "Ask", BTreeMap::new(), |ctx| async move {
            let form = RequestForm::InputGroup {
                title: "Settings".into(),
                description: None,
                context: None,
                layout: None,
                fields: vec![
                    field("name", FieldType::String, true),
                    field("key", FieldType::Password, true),
                ],
            };
            let values = ctx
                .values(form, |values| {
                    if values["name"] == json!("bad") {
                        Err((
                            BTreeMap::from([("name".to_string(), "not that one".to_string())]),
                            None,
                        ))
                    } else {
                        Ok(())
                    }
                })
                .await?;
            ctx.result(json!({"name": values["name"]}));
            Ok(())
        });
        let id = summary.id.clone();
        let view = until(&tasks, &id, |v| v.task.pending_request.is_some()).await;
        let request_id = view.task.pending_request.clone().unwrap();

        let missing = AnswerTask {
            task: id.clone(),
            request_id: request_id.clone(),
            answer: Answer::Values {
                values: BTreeMap::from([("name".into(), json!("x"))]),
            },
        };
        assert_eq!(
            tasks.answer(missing),
            Err(TaskError::Refused("key is required".into()))
        );

        let answer = |name: &str| AnswerTask {
            task: id.clone(),
            request_id: request_id.clone(),
            answer: Answer::Values {
                values: BTreeMap::from([
                    ("name".into(), json!(name)),
                    ("key".into(), json!("s3cret")),
                ]),
            },
        };
        tasks.answer(answer("bad")).unwrap();
        until(&tasks, &id, |v| {
            v.task.pending_request.is_some()
                && v.events
                    .iter()
                    .any(|e| matches!(e.body, EventBody::ValidationError { .. }))
        })
        .await;
        tasks.answer(answer("good")).unwrap();
        let view = until(&tasks, &id, |v| v.task.state.is_terminal()).await;
        assert_eq!(view.task.state, JobState::Completed);
        let text = serde_json::to_string(&view.events).unwrap();
        assert!(
            !text.contains("s3cret"),
            "the password was recorded: {text}"
        );
        assert!(text.contains(REDACTED));
    }

    #[tokio::test]
    async fn cancelling_withdraws_the_pending_request() {
        let (tasks, _) = tasks();
        let summary = tasks.start("ask", "Ask", BTreeMap::new(), |ctx| async move {
            ctx.acknowledge(RequestForm::ManualAction {
                title: "Plug it in".into(),
                instructions: vec![Instruction::Text("Plug in the USB drive.".into())],
                acknowledge_label: None,
                context: None,
            })
            .await
        });
        let id = summary.id.clone();
        let view = until(&tasks, &id, |v| v.task.pending_request.is_some()).await;
        let wrong = AnswerTask {
            task: id.clone(),
            request_id: view.task.pending_request.clone().unwrap(),
            answer: Answer::Confirm { confirmed: true },
        };
        assert!(matches!(tasks.answer(wrong), Err(TaskError::Refused(_))));
        tasks.cancel(&id).unwrap();
        let view = until(&tasks, &id, |v| v.task.state.is_terminal()).await;
        assert_eq!(view.task.state, JobState::Cancelled);
        assert_eq!(view.task.pending_request, None);
    }

    #[tokio::test]
    async fn a_declined_confirmation_and_a_panic_end_the_task() {
        let (tasks, _) = tasks();
        let summary = tasks.start("confirm", "Confirm", BTreeMap::new(), |ctx| async move {
            let yes = ctx
                .confirm(RequestForm::Confirm {
                    title: "Erase?".into(),
                    message: String::new(),
                    severity: Severity::Destructive,
                    confirm_label: None,
                    cancel_label: None,
                    confirmation_text: None,
                    context: None,
                    details: Vec::new(),
                })
                .await?;
            if yes {
                Ok(())
            } else {
                Err(Stop::failed("declined"))
            }
        });
        let id = summary.id.clone();
        let view = until(&tasks, &id, |v| v.task.pending_request.is_some()).await;
        tasks
            .answer(AnswerTask {
                task: id.clone(),
                request_id: view.task.pending_request.unwrap(),
                answer: Answer::Confirm { confirmed: false },
            })
            .unwrap();
        let view = until(&tasks, &id, |v| v.task.state.is_terminal()).await;
        assert_eq!(view.task.state, JobState::Failed);
        assert_eq!(view.task.message.as_deref(), Some("declined"));

        let panicking = tasks.start("boom", "Boom", BTreeMap::new(), |_ctx| async move {
            panic!("boom");
        });
        let view = until(&tasks, &panicking.id, |v| v.task.state.is_terminal()).await;
        assert_eq!(view.task.state, JobState::Failed);
    }
}
