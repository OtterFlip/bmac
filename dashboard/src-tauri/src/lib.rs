//! Tauri shell for the BMAC dashboard. Every command here is narrow: the
//! webview can start only allowlisted workflows with typed values, answer the
//! request a script is waiting on, and read run history. There is no shell,
//! SSH, or general file read/write API.

#[cfg(target_os = "linux")]
mod desktop_entry;

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bmac_engine::browse::{self, Listing};
use bmac_engine::history::{RunDetails, RunEvent, RunRecord, RunStatus};
use bmac_engine::platform::PlatformInfo;
use bmac_engine::preflight::{self, Check};
use bmac_engine::protocol::{ResponsePayload, WorkflowEvent, PROTOCOL_VERSION};
use bmac_engine::registry::{Registry, WorkflowInfo};
use bmac_engine::repo::{self, RepositoryInfo};
use bmac_engine::settings::Settings;
use bmac_engine::supervisor::{Attention, EventSink, Supervisor};
use bmac_engine::{EngineError, Result};
use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_opener::OpenerExt;

const EVENT_RUN_EVENTS: &str = "bmac://run-events";
const EVENT_RUN_UPDATED: &str = "bmac://run-updated";
const EVENT_CLOSE_REQUESTED: &str = "bmac://close-requested";

struct AppState {
    supervisor: Arc<Supervisor>,
    platform: PlatformInfo,
    settings: Mutex<Settings>,
    settings_path: PathBuf,
    repo: Mutex<Option<PathBuf>>,
    exported: Mutex<HashSet<PathBuf>>,
    quitting: Mutex<bool>,
}

impl AppState {
    fn repo(&self) -> Result<PathBuf> {
        self.repo
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| EngineError::Repository("select a BMAC checkout in Settings".into()))
    }
}

#[derive(Clone, Serialize)]
struct RunEventsPayload<'a> {
    run_id: &'a str,
    events: Vec<RunEvent>,
}

struct TauriSink {
    app: AppHandle,
}

impl EventSink for TauriSink {
    fn events(&self, run_id: &str, events: Vec<RunEvent>) {
        let _ = self.app.emit(EVENT_RUN_EVENTS, RunEventsPayload { run_id, events });
    }

    fn updated(&self, record: &RunRecord) {
        let _ = self.app.emit(EVENT_RUN_UPDATED, record);
    }

    fn attention(&self, record: &RunRecord, why: Attention) {
        let Some(state) = self.app.try_state::<AppState>() else { return };
        if !state.settings.lock().unwrap().notifications {
            return;
        }
        let focused = self
            .app
            .get_webview_window("main")
            .and_then(|w| w.is_focused().ok())
            .unwrap_or(false);
        if focused {
            return;
        }
        let body = match why {
            Attention::NeedsInput => match &record.pending_request {
                Some(WorkflowEvent::Confirm { title, .. })
                | Some(WorkflowEvent::ManualAction { title, .. })
                | Some(WorkflowEvent::InputGroup { title, .. }) => format!("Waiting for you: {title}"),
                _ => "Waiting for your input.".to_string(),
            },
            Attention::Finished => match record.status {
                RunStatus::Succeeded => "Finished successfully.".to_string(),
                RunStatus::Cancelled => "Cancelled.".to_string(),
                _ => format!("Failed: {}", record.message.clone().unwrap_or_default()),
            },
        };
        let _ = self.app.notification().builder().title(&record.workflow_title).body(body).show();
    }
}

#[derive(Serialize)]
struct AppInfo {
    name: &'static str,
    version: &'static str,
    protocol_version: u32,
    history_dir: String,
}

#[tauri::command]
fn get_app_info(state: State<'_, AppState>) -> AppInfo {
    AppInfo {
        name: "BMAC Dashboard",
        version: env!("CARGO_PKG_VERSION"),
        protocol_version: PROTOCOL_VERSION,
        history_dir: state.supervisor.history().dir().display().to_string(),
    }
}

#[tauri::command]
fn get_platform_info(state: State<'_, AppState>) -> PlatformInfo {
    state.platform.clone()
}

#[tauri::command]
fn get_repository_info(state: State<'_, AppState>) -> Option<RepositoryInfo> {
    state.repo.lock().unwrap().as_deref().map(repo::info)
}

#[tauri::command]
fn set_repository(state: State<'_, AppState>, path: String) -> Result<RepositoryInfo> {
    let root = repo::validate(std::path::Path::new(&path))?;
    let info = repo::info(&root);
    if !info.valid {
        return Err(EngineError::Repository(info.problems.join("; ")));
    }
    let mut settings = state.settings.lock().unwrap();
    settings.repository = Some(root.display().to_string());
    settings.save(&state.settings_path)?;
    *state.repo.lock().unwrap() = Some(root);
    Ok(info)
}

#[tauri::command]
fn get_settings(state: State<'_, AppState>) -> Settings {
    state.settings.lock().unwrap().clone()
}

#[tauri::command]
fn update_settings(state: State<'_, AppState>, settings: Settings) -> Result<Settings> {
    let mut current = state.settings.lock().unwrap();
    let repository = current.repository.clone();
    let mut next = settings.sanitized();
    next.repository = repository;
    next.save(&state.settings_path)?;
    *current = next.clone();
    Ok(next)
}

#[tauri::command]
fn list_workflows(state: State<'_, AppState>) -> Vec<WorkflowInfo> {
    let repo = state.repo.lock().unwrap().clone();
    state.supervisor.registry().describe(&state.platform, repo.as_deref())
}

#[tauri::command]
async fn run_preflight(state: State<'_, AppState>) -> Result<Vec<Check>> {
    let platform = state.platform.clone();
    let repo = state.repo.lock().unwrap().clone();
    tokio::task::spawn_blocking(move || preflight::run(&platform, repo.as_deref()))
        .await
        .map_err(|e| EngineError::Io(e.to_string()))
}

// Run-control commands are async so Tauri runs them on its Tokio runtime
// rather than the GUI thread.
#[tauri::command]
async fn start_workflow(state: State<'_, AppState>, workflow_id: String, values: BTreeMap<String, Value>) -> Result<RunRecord> {
    if *state.quitting.lock().unwrap() {
        return Err(EngineError::Unavailable("The dashboard is closing.".into()));
    }
    let root = state.repo()?;
    state.supervisor.start(&root, &state.platform, &workflow_id, &values)
}

#[tauri::command]
async fn respond_to_workflow(
    state: State<'_, AppState>,
    run_id: String,
    request_id: String,
    payload: ResponsePayload,
) -> Result<RunRecord> {
    state.supervisor.respond(&run_id, &request_id, payload)
}

#[tauri::command]
async fn cancel_workflow(state: State<'_, AppState>, run_id: String, force: bool) -> Result<RunRecord> {
    state.supervisor.cancel(&run_id, force)
}

#[tauri::command]
fn get_active_runs(state: State<'_, AppState>) -> Vec<RunRecord> {
    state.supervisor.active_runs()
}

#[tauri::command]
fn get_run_history(state: State<'_, AppState>, limit: Option<usize>) -> Vec<RunRecord> {
    state.supervisor.list_history(limit.unwrap_or(200).min(1000))
}

#[tauri::command]
fn get_run_details(state: State<'_, AppState>, run_id: String) -> Result<RunDetails> {
    state.supervisor.details(&run_id)
}

#[tauri::command]
fn delete_run(state: State<'_, AppState>, run_id: String) -> Result<()> {
    state.supervisor.delete_history(&run_id)
}

#[tauri::command]
fn browse_directory(state: State<'_, AppState>, path: Option<String>) -> Result<Listing> {
    let repo = state.repo.lock().unwrap().clone();
    browse::list(path.as_deref(), repo.as_deref())
}

#[tauri::command]
fn export_run_log(state: State<'_, AppState>, run_id: String) -> Result<String> {
    let details = state.supervisor.details(&run_id)?;
    let record = &details.record;
    let mut lines = vec![
        format!("# {} ({})", record.workflow_title, record.workflow_id),
        format!("# run {}", record.run_id),
        format!("# command: {}", record.command_line),
        format!("# started {} ended {}", record.started_at, record.ended_at.clone().unwrap_or_else(|| "-".into())),
        format!("# status {:?} {}", record.status, record.message.clone().unwrap_or_default()),
        String::new(),
    ];
    let logs = if record.status.is_terminal() {
        state.supervisor.history().all_logs(&run_id)
    } else {
        details.logs.clone()
    };
    for item in logs {
        if let WorkflowEvent::Log { stream, text, .. } = item.event {
            let tag = if stream.as_deref() == Some("stderr") { "ERR" } else { "OUT" };
            lines.push(format!("{} {tag} {text}", item.at));
        }
    }
    let path = browse::export_log(&record.workflow_id, lines.into_iter())?;
    state.exported.lock().unwrap().insert(path.clone());
    Ok(path.display().to_string())
}

#[tauri::command]
fn reveal_exported(app: AppHandle, state: State<'_, AppState>, path: String) -> Result<()> {
    let path = PathBuf::from(path);
    if !state.exported.lock().unwrap().contains(&path) {
        return Err(EngineError::InvalidArgument("Only exported logs can be revealed.".into()));
    }
    app.opener().reveal_item_in_dir(&path).map_err(|e| EngineError::Io(e.to_string()))
}

/// Open an https URL in the operator's normal browser.
#[tauri::command]
fn open_external(app: AppHandle, url: String) -> Result<()> {
    let ok = url.len() <= 2048
        && url.starts_with("https://")
        && !url[8..].split('/').next().unwrap_or("").contains('@')
        && !url.chars().any(|c| c.is_control() || c.is_whitespace());
    if !ok {
        return Err(EngineError::InvalidArgument("Only plain https links can be opened.".into()));
    }
    app.opener().open_url(url, None::<&str>).map_err(|e| EngineError::Io(e.to_string()))
}

/// Close the dashboard. With active runs, `interrupt` first sends them the
/// same interrupt as Ctrl-C and waits up to 20 seconds for them to stop.
#[tauri::command]
async fn quit_app(app: AppHandle, state: State<'_, AppState>, interrupt: bool) -> Result<()> {
    *state.quitting.lock().unwrap() = true;
    if interrupt {
        state.supervisor.interrupt_all();
        for _ in 0..200 {
            if state.supervisor.active_runs().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    app.exit(0);
    Ok(())
}

fn initial_repo(settings: &Settings) -> Option<PathBuf> {
    settings
        .repository
        .as_ref()
        .and_then(|p| repo::validate(std::path::Path::new(p)).ok())
        .or_else(repo::discover)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(target_os = "linux")]
    desktop_entry::register();
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            let config_dir = app.path().app_config_dir()?;
            let data_dir = app.path().app_data_dir()?;
            let settings_path = config_dir.join("settings.json");
            let settings = Settings::load(&settings_path);
            let repo = initial_repo(&settings);
            let history = bmac_engine::history::History::open(data_dir.join("runs"))?;
            let sink = Arc::new(TauriSink { app: app.handle().clone() });
            let supervisor = Supervisor::new(
                Registry::builtin(),
                history,
                sink,
                tauri::async_runtime::handle().inner().clone(),
            );
            app.manage(AppState {
                supervisor,
                platform: PlatformInfo::detect(),
                settings: Mutex::new(settings),
                settings_path,
                repo: Mutex::new(repo),
                exported: Mutex::new(HashSet::new()),
                quitting: Mutex::new(false),
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let state = window.state::<AppState>();
                let active = state.supervisor.active_runs();
                if !active.is_empty() && !*state.quitting.lock().unwrap() {
                    api.prevent_close();
                    let _ = window.emit(EVENT_CLOSE_REQUESTED, active.len());
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_app_info,
            get_platform_info,
            get_repository_info,
            set_repository,
            get_settings,
            update_settings,
            list_workflows,
            run_preflight,
            start_workflow,
            respond_to_workflow,
            cancel_workflow,
            get_active_runs,
            get_run_history,
            get_run_details,
            delete_run,
            browse_directory,
            export_run_log,
            reveal_exported,
            open_external,
            quit_app,
        ])
        .run(tauri::generate_context!())
        .expect("error while running the BMAC dashboard");
}
