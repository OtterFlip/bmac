// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The Deployer's Tauri shell. Every command is narrow: the window can read
//! the workstation checks and settings, follow and answer tasks, list and
//! remove the managed SSH entries, and open plain https links. There is no shell, general file access, or network
//! API; the work itself is in the `bmac-deployer` crate.

#[cfg(target_os = "linux")]
mod desktop_entry;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bmac_core::job::JobState;
use bmac_deployer::download::HttpFetch;
use bmac_deployer::flows::{self, Env, Tools};
use bmac_deployer::preflight::{self, Workstation};
use bmac_deployer::settings;
use bmac_deployer::tasks::{TaskSink, Tasks};
use bmac_deployer::workstation_ssh;
use bmac_exec::RealRunner;
use bmac_proto::deployer::{
    AnswerTask, Check, DeployerInfo, DeployerSettings, DeployerSettingsView, SshEntry, TaskEvent,
    TaskKind, TaskSummary, TaskView, events,
};
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_opener::OpenerExt;

type Reply<T> = Result<T, String>;

struct AppState {
    tasks: Tasks,
    settings: Arc<settings::Shared>,
    dirs: Dirs,
    first_launch: bool,
    quitting: Mutex<bool>,
}

/// Where the workflows keep their files. All are private to the operator.
struct Dirs {
    cache: PathBuf,
    state: PathBuf,
    isos: PathBuf,
    ssh: PathBuf,
    /// The static `bmac-inventory` and the `bmacd` packages the ISOs carry.
    bundle: PathBuf,
}

impl Dirs {
    fn find(app: &tauri::App) -> tauri::Result<Dirs> {
        let data = app.path().app_data_dir()?;
        let bundle = match std::env::var_os("BMAC_DEPLOYER_BUNDLE") {
            Some(dir) => PathBuf::from(dir),
            None => app.path().resource_dir()?.join("bundle"),
        };
        Ok(Dirs {
            cache: app.path().app_cache_dir()?,
            state: data.join("state"),
            isos: data.join("isos"),
            ssh: app.path().home_dir()?.join(".ssh"),
            bundle,
        })
    }

    fn create(&self) -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        for dir in [&self.cache, &self.state, &self.isos] {
            std::fs::create_dir_all(dir)?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
}

struct TauriSink {
    app: AppHandle,
    /// What was last notified for each task, so a summary update repeated
    /// for the same request or ending doesn't notify again.
    notified: Mutex<HashMap<String, String>>,
}

impl TauriSink {
    fn notify(&self, summary: &TaskSummary, key: String, body: &str) {
        {
            let mut notified = self.notified.lock().unwrap_or_else(|p| p.into_inner());
            if notified.get(&summary.id) == Some(&key) {
                return;
            }
            notified.insert(summary.id.clone(), key);
        }
        let Some(state) = self.app.try_state::<AppState>() else {
            return;
        };
        if !state.settings.get().notifications {
            return;
        }
        let focused = self
            .app
            .get_webview_window("main")
            .and_then(|w| w.is_focused().ok())
            .unwrap_or(false);
        if !focused {
            let _ = self
                .app
                .notification()
                .builder()
                .title(&summary.title)
                .body(body)
                .show();
        }
    }
}

impl TaskSink for TauriSink {
    fn event(&self, event: &TaskEvent) {
        let _ = self.app.emit(events::TASK_EVENT, event);
    }

    fn updated(&self, summary: &TaskSummary) {
        let _ = self.app.emit(events::TASK_UPDATED, summary);
        if let Some(request) = &summary.pending_request {
            self.notify(summary, request.clone(), "Waiting for you.");
        } else if summary.state.is_terminal() {
            let ended = match summary.state {
                JobState::Completed => "Finished.",
                JobState::Cancelled => "Cancelled.",
                _ => "Failed.",
            };
            let body = summary.message.clone().unwrap_or_else(|| ended.into());
            self.notify(summary, "ended".into(), &body);
        }
    }
}

#[tauri::command]
fn get_app_info(state: State<'_, AppState>) -> DeployerInfo {
    DeployerInfo {
        name: "BMAC Deployer".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        first_launch: state.first_launch,
    }
}

#[tauri::command]
async fn run_preflight() -> Reply<Vec<Check>> {
    Ok(preflight::run(&Workstation::detect(), &RealRunner).await)
}

#[tauri::command]
fn get_settings(state: State<'_, AppState>) -> DeployerSettingsView {
    state.settings.view()
}

#[tauri::command]
fn update_settings(
    state: State<'_, AppState>,
    next: DeployerSettings,
) -> Reply<DeployerSettingsView> {
    state
        .settings
        .change(|current| {
            *current = settings::merge(current, next)?;
            Ok(())
        })
        .map_err(|e| e.to_string())
}

/// Shreds an ISO the Deployer built and forgets it. Overwriting a large
/// ISO takes seconds, so it runs off the window's thread.
#[tauri::command]
async fn delete_iso(state: State<'_, AppState>, path: String) -> Reply<DeployerSettingsView> {
    let shared = state.settings.clone();
    tauri::async_runtime::spawn_blocking(move || {
        shared
            .change(|current| settings::delete_iso(current, &path))
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Starts a workflow. The window chooses only which one; the workflow asks
/// for everything else through its requests.
#[tauri::command]
fn start_task(state: State<'_, AppState>, kind: TaskKind) -> Reply<TaskSummary> {
    let dirs = &state.dirs;
    dirs.create().map_err(|e| e.to_string())?;
    let present = |path: PathBuf| path.exists().then_some(path);
    let chosen = state.settings.get();
    let env = Env {
        runner: Arc::new(RealRunner),
        fetch: Arc::new(HttpFetch::new()?),
        tools: Tools::find(&Workstation::detect()),
        cache_dir: dirs.cache.clone(),
        state_dir: dirs.state.clone(),
        iso_dir: dirs.isos.clone(),
        ssh_dir: dirs.ssh.clone(),
        inventory_binary: present(dirs.bundle.join("bmac-inventory")),
        packages_dir: present(dirs.bundle.join("packages")),
        link_wait: chosen.link_wait,
        isos: state.settings.clone(),
        clock: Arc::new(bmac_deployer::now_ms),
        poll: Duration::from_secs(5),
        discovery_sources: flows::discovery_sources(chosen.discovery_iso.as_ref()),
        dashboard_port: bmac_deployer::hostiso::DASHBOARD_PORT,
        time_zone: flows::workstation_zone(),
    };
    Ok(flows::start(&state.tasks, kind, Arc::new(env)))
}

/// The managed entries in `~/.ssh/config`, read from the workstation's own
/// files without connecting anywhere.
#[tauri::command]
fn ssh_entries(state: State<'_, AppState>) -> Vec<SshEntry> {
    let read = |name: &str| std::fs::read_to_string(state.dirs.ssh.join(name)).unwrap_or_default();
    workstation_ssh::managed_entries(&read("config"), &read("known_hosts"))
}

#[tauri::command]
async fn remove_ssh_entry(state: State<'_, AppState>, alias: String) -> Reply<Vec<SshEntry>> {
    let dirs = &state.dirs;
    dirs.create().map_err(|e| e.to_string())?;
    workstation_ssh::remove_entry(
        &dirs.ssh,
        &dirs.state.join("ssh"),
        Duration::from_secs(30),
        &alias,
    )
    .await?;
    Ok(ssh_entries(state))
}

#[tauri::command]
fn list_tasks(state: State<'_, AppState>) -> Vec<TaskSummary> {
    state.tasks.list()
}

#[tauri::command]
fn get_task(state: State<'_, AppState>, id: String) -> Reply<TaskView> {
    state.tasks.view(&id).map_err(|e| e.to_string())
}

#[tauri::command]
fn answer_task(state: State<'_, AppState>, answer: AnswerTask) -> Reply<()> {
    state.tasks.answer(answer).map_err(|e| e.to_string())
}

#[tauri::command]
fn cancel_task(state: State<'_, AppState>, id: String) -> Reply<()> {
    state.tasks.cancel(&id).map_err(|e| e.to_string())
}

/// Opens a plain https URL in the operator's browser.
#[tauri::command]
fn open_external(app: AppHandle, url: String) -> Reply<()> {
    if !bmac_deployer::plain_https(&url) {
        return Err("Only plain https links can be opened.".into());
    }
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|e| e.to_string())
}

/// Closes the Deployer. With tasks running, `cancel` first cancels them and
/// waits up to 20 seconds for them to stop.
#[tauri::command]
async fn quit_app(app: AppHandle, state: State<'_, AppState>, cancel: bool) -> Reply<()> {
    *state.quitting.lock().unwrap_or_else(|p| p.into_inner()) = true;
    if cancel {
        state.tasks.cancel_all();
        for _ in 0..200 {
            if state.tasks.running() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    app.exit(0);
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(target_os = "linux")]
    desktop_entry::register();
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            let settings_path = settings::path_in(&app.path().app_config_dir()?);
            let (loaded, existed) = settings::load(&settings_path);
            let dirs = Dirs::find(app)?;
            let sink = Arc::new(TauriSink {
                app: app.handle().clone(),
                notified: Mutex::new(HashMap::new()),
            });
            app.manage(AppState {
                tasks: Tasks::new(sink, Arc::new(bmac_deployer::now_ms)),
                settings: Arc::new(settings::Shared {
                    path: settings_path,
                    current: Mutex::new(loaded),
                }),
                dirs,
                first_launch: !existed,
                quitting: Mutex::new(false),
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let state = window.state::<AppState>();
                let running = state.tasks.running();
                if running > 0 && !*state.quitting.lock().unwrap_or_else(|p| p.into_inner()) {
                    api.prevent_close();
                    let _ = window.emit(events::CLOSE_REQUESTED, running);
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_app_info,
            run_preflight,
            get_settings,
            update_settings,
            delete_iso,
            start_task,
            ssh_entries,
            remove_ssh_entry,
            list_tasks,
            get_task,
            answer_task,
            cancel_task,
            open_external,
            quit_app,
        ])
        .run(tauri::generate_context!())
        .expect("error while running the BMAC Deployer");
}
