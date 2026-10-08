//! Local operation history: one JSON record per run plus NDJSON files of its
//! structured events and its log lines. This is a usability and debugging
//! aid, not an audit log. Secrets never reach it: launch parameters carry no
//! secrets, responses are stored redacted, and the supervisor scrubs any
//! submitted secret from later output before it is recorded.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{EngineError, Result};
use crate::protocol::{PhaseStatus, WorkflowEvent};
use crate::registry::{is_identifier, WorkflowMode};

const KEEP_RUNS: usize = 300;
const MAX_LOG_LINES_RETURNED: usize = 50_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Starting,
    Running,
    WaitingInput,
    WaitingConfirmation,
    WaitingManualAction,
    Cancelling,
    Succeeded,
    Failed,
    Cancelled,
    Interrupted,
}

impl RunStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, RunStatus::Succeeded | RunStatus::Failed | RunStatus::Cancelled | RunStatus::Interrupted)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PhaseState {
    pub id: String,
    pub label: String,
    pub status: PhaseStatus,
    pub cancel_allowed: bool,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NextStepRecord {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseRecord {
    pub request_id: String,
    pub at: String,
    pub title: String,
    /// `values`, `confirm`, `acknowledge`, or `cancel`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub values: Option<BTreeMap<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmed: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CancelRecord {
    pub requested_at: String,
    pub forced: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunRecord {
    pub run_id: String,
    pub workflow_id: String,
    pub workflow_title: String,
    pub mode: WorkflowMode,
    pub destructive: bool,
    pub script: String,
    /// Every argument after the script path, including `--json`.
    pub argv: Vec<String>,
    /// The exact command as an operator would type it from the repository.
    pub command_line: String,
    /// The same command without `--json`, for running it by hand.
    pub terminal_command: String,
    pub launch_values: BTreeMap<String, String>,
    pub target: Option<String>,
    pub dry_run: bool,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub status: RunStatus,
    pub message: Option<String>,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub pid: Option<u32>,
    pub cancel: Option<CancelRecord>,
    pub cancel_allowed: bool,
    pub phases: Vec<PhaseState>,
    pub current_activity: Option<String>,
    pub pending_request: Option<WorkflowEvent>,
    pub result: Option<Value>,
    pub next_steps: Vec<NextStepRecord>,
    pub errors: Vec<String>,
    pub warnings: usize,
    pub responses: Vec<ResponseRecord>,
    pub log_lines: u64,
    pub last_log: Option<String>,
    pub event_count: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunEvent {
    pub seq: u64,
    pub at: String,
    pub event: WorkflowEvent,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunDetails {
    pub record: RunRecord,
    /// Structured events other than `log`.
    pub events: Vec<RunEvent>,
    /// `log` events, newest last, capped.
    pub logs: Vec<RunEvent>,
    pub logs_truncated: bool,
}

#[derive(Debug, Clone)]
pub struct History {
    dir: PathBuf,
}

/// Open append handles for one run's event and log files.
pub struct RunFiles {
    events: BufWriter<File>,
    logs: BufWriter<File>,
}

impl RunFiles {
    pub fn append(&mut self, item: &RunEvent) {
        let target = if matches!(item.event, WorkflowEvent::Log { .. }) { &mut self.logs } else { &mut self.events };
        if let Ok(line) = serde_json::to_string(item) {
            let _ = writeln!(target, "{line}");
        }
    }

    pub fn flush(&mut self) {
        let _ = self.events.flush();
        let _ = self.logs.flush();
    }
}

impl History {
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
        }
        Ok(Self { dir })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, run_id: &str, suffix: &str) -> Result<PathBuf> {
        if !is_identifier(run_id) {
            return Err(EngineError::UnknownRun(run_id.to_string()));
        }
        Ok(self.dir.join(format!("{run_id}{suffix}")))
    }

    pub fn create_files(&self, run_id: &str) -> Result<RunFiles> {
        let open = |suffix: &str| -> Result<BufWriter<File>> {
            Ok(BufWriter::new(OpenOptions::new().create(true).append(true).open(self.path(run_id, suffix)?)?))
        };
        Ok(RunFiles { events: open(".events.ndjson")?, logs: open(".log.ndjson")? })
    }

    pub fn save(&self, record: &RunRecord) -> Result<()> {
        let path = self.path(&record.run_id, ".json")?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(record).map_err(|e| EngineError::Io(e.to_string()))?)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn load(&self, run_id: &str) -> Result<RunRecord> {
        let text = fs::read(self.path(run_id, ".json")?).map_err(|_| EngineError::UnknownRun(run_id.to_string()))?;
        serde_json::from_slice(&text).map_err(|e| EngineError::Io(e.to_string()))
    }

    pub fn list(&self, limit: usize) -> Vec<RunRecord> {
        let mut records: Vec<RunRecord> = fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                name.ends_with(".json") && !name.ends_with(".tmp")
            })
            .filter_map(|entry| fs::read(entry.path()).ok())
            .filter_map(|bytes| serde_json::from_slice::<RunRecord>(&bytes).ok())
            .collect();
        records.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        records.truncate(limit);
        records
    }

    fn read_events(&self, run_id: &str, suffix: &str) -> Vec<RunEvent> {
        let Ok(path) = self.path(run_id, suffix) else { return vec![] };
        let Ok(file) = File::open(path) else { return vec![] };
        BufReader::new(file)
            .lines()
            .map_while(std::result::Result::ok)
            .filter_map(|line| serde_json::from_str(&line).ok())
            .collect()
    }

    pub fn details(&self, run_id: &str) -> Result<RunDetails> {
        let record = self.load(run_id)?;
        let events = self.read_events(run_id, ".events.ndjson");
        let mut logs = self.read_events(run_id, ".log.ndjson");
        let logs_truncated = logs.len() > MAX_LOG_LINES_RETURNED;
        if logs_truncated {
            logs.drain(..logs.len() - MAX_LOG_LINES_RETURNED);
        }
        Ok(RunDetails { record, events, logs, logs_truncated })
    }

    /// Every log line of a run, for export.
    pub fn all_logs(&self, run_id: &str) -> Vec<RunEvent> {
        self.read_events(run_id, ".log.ndjson")
    }

    pub fn delete(&self, run_id: &str) -> Result<()> {
        for suffix in [".json", ".events.ndjson", ".log.ndjson"] {
            let _ = fs::remove_file(self.path(run_id, suffix)?);
        }
        Ok(())
    }

    /// Mark runs that were active when the dashboard last exited as
    /// interrupted, and prune old runs.
    pub fn recover(&self) {
        for mut record in self.list(usize::MAX) {
            if !record.status.is_terminal() {
                record.status = RunStatus::Interrupted;
                record.pending_request = None;
                record.message = Some(
                    "The dashboard exited while this run was active. The script may have kept running or \
                     stopped part-way; inspect the cluster before retrying."
                        .into(),
                );
                let _ = self.save(&record);
            }
        }
        for record in self.list(usize::MAX).into_iter().skip(KEEP_RUNS) {
            let _ = self.delete(&record.run_id);
        }
    }
}
