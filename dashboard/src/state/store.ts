import { create } from "zustand";
import { api, errorMessage, initBackend } from "@/lib/api";
import type {
  AppInfo,
  ConfigFile,
  ConfigListing,
  NextStep,
  PlatformInfo,
  PreflightCheck,
  RepositoryInfo,
  ResponsePayload,
  RunEvent,
  RunRecord,
  Settings,
  WorkflowInfo,
} from "@/protocol/types";
import { isTerminal } from "@/protocol/types";
import { SOURCE_IDS, type SourceId, type StateSources } from "@/protocol/state";
import { logs } from "./logs";
import { toast } from "./toast";

export type Page =
  | "dashboard"
  | "hosts"
  | "production"
  | "staging"
  | "storage"
  | "qdevice"
  | "diagnostics"
  | "operations"
  | "config"
  | "settings";

export interface SourceEntry<T> {
  data: T | null;
  updatedAt: number | null;
  status: "idle" | "loading" | "ok" | "error";
  error: string | null;
  runId: string | null;
  nextSteps: NextStep[];
}

type Sources = { [K in SourceId]: SourceEntry<StateSources[K]> };

const emptySource = (): SourceEntry<never> => ({
  data: null,
  updatedAt: null,
  status: "idle",
  error: null,
  runId: null,
  nextSteps: [],
});

export interface PreflightState {
  checks: PreflightCheck[] | null;
  loading: boolean;
  updatedAt: number | null;
  error: string | null;
}

export interface LaunchTarget {
  workflowId: string;
  values?: Record<string, unknown>;
}

interface State {
  ready: boolean;
  bootError: string | null;
  backendKind: "tauri" | "mock" | null;
  appInfo: AppInfo | null;
  platform: PlatformInfo | null;
  repo: RepositoryInfo | null;
  settings: Settings | null;
  config: ConfigListing | null;
  /** The file the config page should show, set when another page links to it. */
  configFocus: string | null;
  workflows: Record<string, WorkflowInfo>;
  workflowOrder: string[];

  page: Page;
  records: Record<string, RunRecord>;
  events: Record<string, RunEvent[]>;
  focusedRunId: string | null;
  panelOpen: boolean;
  consoleOpen: boolean;
  consoleRunId: string | null;
  consolePinned: boolean;
  launch: LaunchTarget | null;
  closeRequested: number;
  sources: Sources;
  sourceRuns: Record<string, SourceId>;
  historyVersion: number;
  preflight: PreflightState;

  setPage(page: Page): void;
  runPreflight(): Promise<void>;
  bootstrap(): Promise<void>;
  reloadCatalog(): Promise<void>;
  refreshRepo(): Promise<void>;
  refreshConfig(): Promise<void>;
  /** Show the config page, optionally with one file selected. */
  openConfig(name?: string | null): void;
  openLaunch(workflowId: string, values?: Record<string, unknown>): void;
  closeLaunch(): void;
  startRun(workflowId: string, values: Record<string, unknown>, opts?: { focus?: boolean }): Promise<RunRecord | null>;
  respond(runId: string, requestId: string, payload: ResponsePayload): Promise<boolean>;
  cancel(runId: string, force?: boolean): Promise<void>;
  focusRun(runId: string | null): void;
  /** Show a run in the workflow panel without changing the console. */
  showInPanel(runId: string): void;
  setPanelOpen(open: boolean): void;
  setConsoleOpen(open: boolean): void;
  showInConsole(runId: string, pin?: boolean): void;
  loadRun(runId: string): Promise<void>;
  refreshSource(id: SourceId, opts?: { ifOlderThanMs?: number }): Promise<void>;
  refreshSources(ids?: SourceId[], opts?: { ifOlderThanMs?: number }): void;
  setCloseRequested(n: number): void;
  setSettings(settings: Settings): void;
  /** Starts parameterless read-only workflows directly; everything else goes through the launch dialog. */
  launchWorkflow(workflowId: string, values?: Record<string, unknown>): void;
}

const recordEvents = (set: (fn: (s: State) => Partial<State>) => void, runId: string, items: RunEvent[]) => {
  const structured = items.filter((e) => e.event.type !== "log");
  logs.append(runId, items);
  if (!structured.length) return;
  set((s) => {
    const existing = s.events[runId] ?? [];
    const last = existing.length ? existing[existing.length - 1].seq : 0;
    const fresh = structured.filter((e) => e.seq > last);
    if (!fresh.length) return {};
    return { events: { ...s.events, [runId]: [...existing, ...fresh] } };
  });
};

export const useStore = create<State>((set, get) => ({
  ready: false,
  bootError: null,
  backendKind: null,
  appInfo: null,
  platform: null,
  repo: null,
  settings: null,
  config: null,
  configFocus: null,
  workflows: {},
  workflowOrder: [],
  page: "dashboard",
  records: {},
  events: {},
  focusedRunId: null,
  panelOpen: false,
  consoleOpen: false,
  consoleRunId: null,
  consolePinned: false,
  launch: null,
  closeRequested: 0,
  sources: Object.fromEntries(SOURCE_IDS.map((id) => [id, emptySource()])) as unknown as Sources,
  sourceRuns: {},
  historyVersion: 0,
  preflight: { checks: null, loading: false, updatedAt: null, error: null },

  setPage: (page) => set({ page }),

  async runPreflight() {
    if (get().preflight.loading) return;
    set((s) => ({ preflight: { ...s.preflight, loading: true } }));
    try {
      const checks = await api().runPreflight();
      set({ preflight: { checks, loading: false, updatedAt: Date.now(), error: null } });
    } catch (error) {
      set((s) => ({ preflight: { ...s.preflight, loading: false, error: errorMessage(error) } }));
    }
  },

  async bootstrap() {
    try {
      const backend = await initBackend();
      set({ backendKind: backend.kind });
      backend.onRunEvents((runId, items) => recordEvents(set, runId, items));
      backend.onRunUpdated((record) => onRecord(record));
      backend.onCloseRequested((n) => set({ closeRequested: n }));
      const [appInfo, platform, settings] = await Promise.all([
        backend.getAppInfo(),
        backend.getPlatformInfo(),
        backend.getSettings(),
      ]);
      set({ appInfo, platform, settings });
      await get().reloadCatalog();
      await get().refreshConfig();
      void get().runPreflight();
      // Start on Settings until the essential config files are filled in.
      const config = get().config;
      if (appInfo.first_launch || (config && configAttention(config).length > 0)) set({ page: "settings" });
      const [active, history] = await Promise.all([backend.getActiveRuns(), backend.getRunHistory(200)]);
      const records: Record<string, RunRecord> = {};
      for (const r of [...history, ...active]) records[r.run_id] = r;
      set({ records, ready: true });
      for (const r of active) void get().loadRun(r.run_id);
      const latestActive = active[active.length - 1];
      if (latestActive) set({ consoleRunId: latestActive.run_id });
      // Seed each state source from its last successful run.
      for (const id of SOURCE_IDS) {
        const last = history.find((r) => r.workflow_id === id && r.status === "succeeded" && r.result);
        if (last) {
          set((s) => ({
            sources: {
              ...s.sources,
              [id]: {
                ...s.sources[id],
                data: last.result as never,
                updatedAt: Date.parse(last.ended_at ?? last.started_at),
                status: "ok",
                runId: last.run_id,
                nextSteps: last.next_steps,
              },
            },
          }));
        }
      }
    } catch (error) {
      set({ bootError: errorMessage(error), ready: true });
    }
  },

  async reloadCatalog() {
    const backend = api();
    const [repo, list] = await Promise.all([backend.getRepositoryInfo(), backend.listWorkflows()]);
    set({
      repo,
      workflows: Object.fromEntries(list.map((w) => [w.id, w])),
      workflowOrder: list.map((w) => w.id),
    });
  },

  async refreshRepo() {
    try {
      set({ repo: await api().getRepositoryInfo() });
    } catch {
      /* keep the last known repository info */
    }
  },

  async refreshConfig() {
    try {
      set({ config: await api().getConfigListing() });
    } catch {
      set({ config: null });
    }
  },

  openConfig: (name = null) => set({ page: "config", configFocus: name }),

  openLaunch: (workflowId, values) => set({ launch: { workflowId, values } }),
  closeLaunch: () => set({ launch: null }),

  async startRun(workflowId, values, opts = {}) {
    const focus = opts.focus ?? true;
    try {
      const record = await api().startWorkflow(workflowId, values);
      onRecord(record);
      set((s) => ({
        focusedRunId: focus ? record.run_id : s.focusedRunId,
        panelOpen: focus ? true : s.panelOpen,
        consoleRunId: s.consolePinned && s.consoleRunId && !isTerminal(s.records[s.consoleRunId]?.status ?? "succeeded") ? s.consoleRunId : record.run_id,
        consolePinned: focus ? false : s.consolePinned,
        launch: null,
      }));
      return record;
    } catch (error) {
      toast.error(`Could not start ${get().workflows[workflowId]?.title ?? workflowId}`, errorMessage(error));
      return null;
    }
  },

  async respond(runId, requestId, payload) {
    try {
      onRecord(await api().respond(runId, requestId, payload));
      return true;
    } catch (error) {
      toast.error("The response was not accepted", errorMessage(error));
      return false;
    }
  },

  async cancel(runId, force = false) {
    try {
      onRecord(await api().cancel(runId, force));
    } catch (error) {
      toast.error(force ? "Could not stop the script" : "Could not cancel", errorMessage(error));
    }
  },

  focusRun: (runId) => {
    set({ focusedRunId: runId, panelOpen: runId !== null });
    if (runId) {
      set({ consoleRunId: runId });
      void get().loadRun(runId);
    }
  },
  showInPanel: (runId) => {
    set({ focusedRunId: runId, panelOpen: true });
    void get().loadRun(runId);
  },
  setPanelOpen: (open) => set({ panelOpen: open }),
  setConsoleOpen: (open) => set({ consoleOpen: open }),
  showInConsole: (runId, pin = true) => {
    set({ consoleRunId: runId, consolePinned: pin });
    void get().loadRun(runId);
  },

  async loadRun(runId) {
    if (logs.isLoaded(runId)) return;
    try {
      const details = await api().getRunDetails(runId);
      logs.load(runId, [...details.events, ...details.logs]);
      set((s) => {
        const live = s.events[runId] ?? [];
        const last = details.events.length ? details.events[details.events.length - 1].seq : 0;
        return {
          events: { ...s.events, [runId]: [...details.events, ...live.filter((e) => e.seq > last)] },
          records: { ...s.records, [runId]: s.records[runId] && !isTerminal(details.record.status) ? s.records[runId] : details.record },
        };
      });
    } catch {
      /* the run may have been pruned */
    }
  },

  async refreshSource(id, opts = {}) {
    const entry = get().sources[id];
    if (entry.status === "loading") return;
    if (opts.ifOlderThanMs && entry.updatedAt && Date.now() - entry.updatedAt < opts.ifOlderThanMs) return;
    const workflow = get().workflows[id];
    if (!workflow?.available) {
      set((s) => ({
        sources: { ...s.sources, [id]: { ...s.sources[id], status: "error", error: workflow?.unavailable_reason ?? "Unavailable." } },
      }));
      return;
    }
    set((s) => ({ sources: { ...s.sources, [id]: { ...s.sources[id], status: "loading", error: null } } }));
    try {
      const record = await api().startWorkflow(id, {});
      set((s) => ({
        sourceRuns: { ...s.sourceRuns, [record.run_id]: id },
        sources: { ...s.sources, [id]: { ...s.sources[id], runId: record.run_id } },
        consoleRunId: s.consolePinned && s.consoleRunId && !isTerminal(s.records[s.consoleRunId]?.status ?? "succeeded") ? s.consoleRunId : record.run_id,
      }));
      onRecord(record);
    } catch (error) {
      set((s) => ({ sources: { ...s.sources, [id]: { ...s.sources[id], status: "error", error: errorMessage(error) } } }));
    }
  },

  refreshSources(ids = SOURCE_IDS, opts) {
    for (const id of ids) void get().refreshSource(id, opts);
  },

  setCloseRequested: (n) => set({ closeRequested: n }),
  setSettings: (settings) => set({ settings }),

  launchWorkflow(workflowId, values) {
    const workflow = get().workflows[workflowId];
    if (workflow && workflow.available && workflow.mode === "read_only" && workflow.params.length === 0) {
      void get().startRun(workflowId, {}, { focus: true });
    } else {
      get().openLaunch(workflowId, values);
    }
  },
}));

function onRecord(record: RunRecord) {
  const { records, sourceRuns } = useStore.getState();
  const previous = records[record.run_id];
  useStore.setState((s) => ({ records: { ...s.records, [record.run_id]: record } }));
  const justFinished = isTerminal(record.status) && (!previous || !isTerminal(previous.status));
  if (!justFinished) return;
  useStore.setState((s) => ({ historyVersion: s.historyVersion + 1 }));
  const background = sourceRuns[record.run_id];
  const unfiltered = Object.keys(record.launch_values).length === 0;
  const source =
    background ?? (unfiltered && (SOURCE_IDS as string[]).includes(record.workflow_id) ? (record.workflow_id as SourceId) : undefined);
  if (source) {
    useStore.setState((s) => {
      const prev = s.sources[source];
      const ok = record.result !== null && record.result !== undefined;
      return {
        sources: {
          ...s.sources,
          [source]: {
            ...prev,
            data: ok ? (record.result as never) : prev.data,
            updatedAt: ok ? Date.now() : prev.updatedAt,
            status: ok ? "ok" : "error",
            error: ok ? null : record.message ?? record.errors[0] ?? `The script finished as ${record.status}.`,
            nextSteps: record.next_steps,
          },
        },
      };
    });
    if (background) return;
  }
  const { focusedRunId, panelOpen } = useStore.getState();
  const visible = panelOpen && focusedRunId === record.run_id;
  if (!visible) {
    const title = record.workflow_title + (record.target ? ` · ${record.target}` : "");
    if (record.status === "succeeded") toast.success(`${title} finished`, record.dry_run ? "Dry run complete." : undefined, record.run_id);
    else if (record.status === "failed") toast.error(`${title} failed`, record.message ?? undefined, record.run_id);
    else if (record.status === "cancelled") toast.info(`${title} was cancelled`, undefined, record.run_id);
  }
  if (record.mode === "mutating" && !record.dry_run && record.status !== "cancelled") {
    useStore.getState().refreshSources();
  }
}

export const selectActiveRuns = (s: State) =>
  Object.values(s.records)
    .filter((r) => !isTerminal(r.status))
    .sort((a, b) => a.started_at.localeCompare(b.started_at));

/** Essential config files that are missing or still identical to their example. */
export const configAttention = (config: ConfigListing): ConfigFile[] =>
  config.files.filter((f) => f.essential && (f.state === "missing" || f.state === "unchanged"));
