import type {
  AppInfo,
  ConfigListing,
  ConfigText,
  DirListing,
  EngineError,
  PlatformInfo,
  PreflightCheck,
  RepositoryInfo,
  ResponsePayload,
  RunDetails,
  RunEvent,
  RunRecord,
  Settings,
  WorkflowInfo,
} from "@/protocol/types";

export type Unlisten = () => void;

/** Everything the frontend may ask of the native side. Deliberately narrow:
 *  there is no shell, SSH, or general file API. */
export interface Backend {
  kind: "tauri" | "mock";
  getAppInfo(): Promise<AppInfo>;
  getPlatformInfo(): Promise<PlatformInfo>;
  getRepositoryInfo(): Promise<RepositoryInfo | null>;
  setRepository(path: string): Promise<RepositoryInfo>;
  getSettings(): Promise<Settings>;
  updateSettings(settings: Settings): Promise<Settings>;
  listWorkflows(): Promise<WorkflowInfo[]>;
  runPreflight(): Promise<PreflightCheck[]>;
  startWorkflow(workflowId: string, values: Record<string, unknown>): Promise<RunRecord>;
  respond(runId: string, requestId: string, payload: ResponsePayload): Promise<RunRecord>;
  cancel(runId: string, force: boolean): Promise<RunRecord>;
  getActiveRuns(): Promise<RunRecord[]>;
  getRunHistory(limit?: number): Promise<RunRecord[]>;
  getRunDetails(runId: string): Promise<RunDetails>;
  deleteRun(runId: string): Promise<void>;
  browseDirectory(path?: string | null): Promise<DirListing>;
  getConfigListing(): Promise<ConfigListing>;
  readConfigFile(name: string): Promise<ConfigText>;
  readConfigExample(name: string): Promise<ConfigText>;
  /** `revision` is what the editor loaded, or null when creating the file. */
  writeConfigFile(name: string, text: string, revision: string | null): Promise<ConfigText>;
  createConfigFromExample(example: string): Promise<string>;
  /** Open the config directory, or one file in it, with the operator's own apps. */
  openConfigLocation(name?: string | null): Promise<void>;
  relocateConfig(path: string, moveFiles: boolean): Promise<ConfigListing>;
  exportRunLog(runId: string): Promise<string>;
  revealExported(path: string): Promise<void>;
  openExternal(url: string): Promise<void>;
  quit(interrupt: boolean): Promise<void>;
  onRunEvents(callback: (runId: string, events: RunEvent[]) => void): Unlisten;
  onRunUpdated(callback: (record: RunRecord) => void): Unlisten;
  onCloseRequested(callback: (activeRuns: number) => void): Unlisten;
}

export function errorMessage(error: unknown): string {
  if (error && typeof error === "object" && "message" in error) {
    return String((error as EngineError).message);
  }
  return String(error);
}

export function errorCode(error: unknown): string | null {
  if (error && typeof error === "object" && "code" in error) {
    return String((error as EngineError).code);
  }
  return null;
}

async function createTauriBackend(): Promise<Backend> {
  const { invoke } = await import("@tauri-apps/api/core");
  const { listen } = await import("@tauri-apps/api/event");

  const subscribe = <T,>(name: string, handler: (payload: T) => void): Unlisten => {
    let stop: (() => void) | null = null;
    let cancelled = false;
    void listen<T>(name, (event) => handler(event.payload)).then((unlisten) => {
      if (cancelled) unlisten();
      else stop = unlisten;
    });
    return () => {
      cancelled = true;
      stop?.();
    };
  };

  return {
    kind: "tauri",
    getAppInfo: () => invoke("get_app_info"),
    getPlatformInfo: () => invoke("get_platform_info"),
    getRepositoryInfo: () => invoke("get_repository_info"),
    setRepository: (path) => invoke("set_repository", { path }),
    getSettings: () => invoke("get_settings"),
    updateSettings: (settings) => invoke("update_settings", { settings }),
    listWorkflows: () => invoke("list_workflows"),
    runPreflight: () => invoke("run_preflight"),
    startWorkflow: (workflowId, values) => invoke("start_workflow", { workflowId, values }),
    respond: (runId, requestId, payload) => invoke("respond_to_workflow", { runId, requestId, payload }),
    cancel: (runId, force) => invoke("cancel_workflow", { runId, force }),
    getActiveRuns: () => invoke("get_active_runs"),
    getRunHistory: (limit) => invoke("get_run_history", { limit }),
    getRunDetails: (runId) => invoke("get_run_details", { runId }),
    deleteRun: (runId) => invoke("delete_run", { runId }),
    browseDirectory: (path) => invoke("browse_directory", { path: path ?? null }),
    getConfigListing: () => invoke("get_config_listing"),
    readConfigFile: (name) => invoke("read_config_file", { name }),
    readConfigExample: (name) => invoke("read_config_example", { name }),
    writeConfigFile: (name, text, revision) => invoke("write_config_file", { name, text, revision }),
    createConfigFromExample: (example) => invoke("create_config_from_example", { example }),
    openConfigLocation: (name) => invoke("open_config_location", { name: name ?? null }),
    relocateConfig: (path, moveFiles) => invoke("relocate_config", { path, moveFiles }),
    exportRunLog: (runId) => invoke("export_run_log", { runId }),
    revealExported: (path) => invoke("reveal_exported", { path }),
    openExternal: (url) => invoke("open_external", { url }),
    quit: (interrupt) => invoke("quit_app", { interrupt }),
    onRunEvents: (callback) =>
      subscribe<{ run_id: string; events: RunEvent[] }>("bmac://run-events", (p) => callback(p.run_id, p.events)),
    onRunUpdated: (callback) => subscribe<RunRecord>("bmac://run-updated", callback),
    onCloseRequested: (callback) => subscribe<number>("bmac://close-requested", callback),
  };
}

export const isTauri = () => typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

let backend: Backend | null = null;

export async function initBackend(): Promise<Backend> {
  if (backend) return backend;
  if (isTauri()) {
    backend = await createTauriBackend();
  } else {
    const { createMockBackend } = await import("./mock/backend");
    backend = createMockBackend();
  }
  return backend;
}

export function api(): Backend {
  if (!backend) throw new Error("backend not initialized");
  return backend;
}

/** For tests. */
export function setBackend(next: Backend) {
  backend = next;
}
