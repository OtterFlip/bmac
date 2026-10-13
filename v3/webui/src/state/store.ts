// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// The Web Dashboard's state: the session, the operations this user can see,
// jobs kept current by the event stream, and the cluster status snapshots.
// Snapshots come from the plain GET endpoints; the stream carries the
// changes after them. Everything here is memory only and is dropped when
// the session ends.

import { create } from "zustand";
import type {
  Answer,
  ClusterSnapshot,
  HostsSnapshot,
  JobEvent,
  JobRecord,
  JobSummary,
  NameOption,
  OperationView,
  SetupView,
  StorageSnapshot,
  StreamMessage,
  VmsSnapshot,
} from "@bmac/ui";
import { applyEvent, applyView, isTerminal, logs, newRecord, recordStatus, withSummary } from "@bmac/ui";
import { errorMessage, type Backend, type SessionInfo, type StreamState, type Unlisten } from "../lib/backend";
import { toast, useToasts } from "./toast";

export type Page =
  | "overview"
  | "hosts"
  | "production"
  | "staging"
  | "storage"
  | "qdevice"
  | "backups"
  | "alerts"
  | "activity"
  | "logs"
  | "diagnostics"
  | "settings";

export interface LaunchTarget {
  operation: string;
  values?: Record<string, unknown>;
  /** Made when the dialog opens, so a repeated POST returns the job already admitted. */
  idempotencyKey: string;
}

/** Names the launch dialog offers, from the collectors. */
export interface KnownNames {
  host: NameOption[];
  production: NameOption[];
  staging: NameOption[];
  freeHostSlots: { options: NameOption[]; suggested?: string };
}

export interface StatusData {
  cluster: ClusterSnapshot;
  hosts: HostsSnapshot;
  vms: VmsSnapshot;
  storage: StorageSnapshot;
}

export type StatusKind = keyof StatusData;

export const STATUS_KINDS: StatusKind[] = ["cluster", "hosts", "vms", "storage"];

export interface StatusEntry<T> {
  data: T | null;
  error: string | null;
  loading: boolean;
  /** When the last fetch succeeded. */
  updatedAt: number | null;
}

type StatusEntries = { [K in StatusKind]: StatusEntry<StatusData[K]> };

/** The snapshots each collector's observations change. */
const AFFECTED: Record<string, StatusKind[]> = {
  host: ["hosts", "cluster"],
  cluster: ["cluster", "hosts"],
  guests: ["vms", "cluster"],
  storage: ["storage", "cluster"],
};

/** Changes arriving together are fetched once. */
export const STATUS_DEBOUNCE_MS = 1000;

interface State {
  phase: "login" | "loading" | "ready";
  session: SessionInfo | null;
  /** Why the user is back at the login page, such as an expired session. */
  loginNotice: string | null;
  loadError: string | null;
  operations: Record<string, OperationView>;
  operationOrder: string[];
  /** While `active`, this host is being set up and the Dashboard shows only its setup job. */
  setup: SetupView | null;
  jobs: Record<string, JobRecord>;
  stream: StreamState;
  page: Page;
  focusedJobId: string | null;
  panelOpen: boolean;
  consoleOpen: boolean;
  consoleJobId: string | null;
  consolePinned: boolean;
  launch: LaunchTarget | null;
  known: KnownNames;
  status: StatusEntries;

  /**
   * A page shows this snapshot: it is fetched now if it is missing or
   * changed, and kept current while wanted. Returns the release.
   */
  wantStatus(kind: StatusKind): () => void;
  refreshStatus(kinds: StatusKind[]): Promise<void>;
  /** Call after a login succeeds: loads the snapshots and opens the stream. */
  begin(): Promise<void>;
  logout(): Promise<void>;
  setPage(page: Page): void;
  openLaunch(operation: string, values?: Record<string, unknown>): void;
  /** Starts a read-only operation without options at once; anything else opens the launch dialog. */
  startOperation(operation: string, values?: Record<string, unknown>): void;
  closeLaunch(): void;
  launchJob(operation: string, parameters: Record<string, unknown>, dryRun: boolean): Promise<JobSummary | null>;
  /** Starts the setup job, or starts it again after it failed or was cancelled. */
  startSetup(): Promise<void>;
  /** Reads the setup view again, such as a joining host's progress. */
  refreshSetup(): Promise<void>;
  answer(jobId: string, requestId: string, answer: Answer): Promise<boolean>;
  cancel(jobId: string, force?: boolean): Promise<boolean>;
  resume(jobId: string): Promise<boolean>;
  reconcile(jobId: string): Promise<boolean>;
  focusJob(jobId: string | null): void;
  showInPanel(jobId: string): void;
  setPanelOpen(open: boolean): void;
  setConsoleOpen(open: boolean): void;
  showInConsole(jobId: string, pin?: boolean): void;
  /** Fetches a job's events after the ones already held. */
  loadJob(jobId: string): Promise<void>;
}

let backend: Backend | null = null;
let stopStream: Unlisten | null = null;
let stopEnded: Unlisten | null = null;
const loading = new Map<string, Promise<void>>();

const emptyEntry = { data: null, error: null, loading: false, updatedAt: null };
const emptyStatus = (): StatusEntries => ({ cluster: { ...emptyEntry }, hosts: { ...emptyEntry }, vms: { ...emptyEntry }, storage: { ...emptyEntry } });
const wanted: Record<StatusKind, number> = { cluster: 0, hosts: 0, vms: 0, storage: 0 };
/** Changed while nothing showed them; fetched when next wanted. */
const changed = new Set<StatusKind>();
const pending = new Set<StatusKind>();
let statusTimer: ReturnType<typeof setTimeout> | null = null;
const fetching = new Map<StatusKind, Promise<void>>();
/** A change arrived during a fetch, so fetch again after it. */
const again = new Set<StatusKind>();
/** Bumped on reset, so a fetch from an ended session is dropped. */
let generation = 0;

function resetStatus() {
  generation += 1;
  if (statusTimer) clearTimeout(statusTimer);
  statusTimer = null;
  changed.clear();
  pending.clear();
  fetching.clear();
  again.clear();
}

export function api(): Backend {
  if (!backend) throw new Error("the backend is not set");
  return backend;
}

const initial = {
  phase: "login" as const,
  session: null,
  loadError: null,
  operations: {},
  operationOrder: [],
  setup: null as SetupView | null,
  jobs: {},
  stream: "closed" as StreamState,
  page: "overview" as Page,
  focusedJobId: null,
  panelOpen: false,
  consoleOpen: false,
  consoleJobId: null,
  consolePinned: false,
  launch: null,
  known: { host: [], production: [], staging: [], freeHostSlots: { options: [] } } as KnownNames,
  status: emptyStatus(),
};

export const useStore = create<State>((set, get) => {
  /** The jobs whose events this tab needs: the panel's and the console's. */
  const watchedIds = () => {
    const { focusedJobId, panelOpen, consoleJobId, consoleOpen } = get();
    return [...new Set([panelOpen ? focusedJobId : null, consoleOpen ? consoleJobId : null].filter((id): id is string => !!id))];
  };

  const updateWatch = () => {
    if (!backend || get().phase !== "ready") return;
    const ids = watchedIds();
    void backend.watch(ids).catch(() => {});
    for (const id of ids) void get().loadJob(id);
  };

  const putStatus = <K extends StatusKind>(kind: K, entry: Partial<StatusEntry<StatusData[K]>>) =>
    set((s) => ({ status: { ...s.status, [kind]: { ...s.status[kind], ...entry } } }));

  /** The names the launch dialog offers, from the snapshots. */
  const learnNames = <K extends StatusKind>(kind: K, data: StatusData[K]) => {
    if (kind === "hosts") {
      const hosts = (data as HostsSnapshot).hosts;
      const host = hosts.map((h) => ({ value: h.name, label: h.name, help: h.online ? undefined : "offline" }));
      set((s) => ({ known: { ...s.known, host } }));
    } else if (kind === "vms") {
      const vms = data as VmsSnapshot;
      const production = vms.production.map((v) => ({ value: v.name, label: v.name, help: v.purpose || undefined }));
      const staging = vms.staging.map((v) => ({ value: v.name, label: v.name, help: v.source ? `copy of ${v.source}` : undefined }));
      set((s) => ({ known: { ...s.known, production, staging } }));
    }
  };

  const fetchStatus = (kind: StatusKind): Promise<void> => {
    const running = fetching.get(kind);
    if (running) {
      again.add(kind);
      return running;
    }
    const started = generation;
    const task = (async () => {
      putStatus(kind, { loading: true });
      try {
        const data = await api()[kind]();
        if (started !== generation) return;
        putStatus(kind, { data, error: null, loading: false, updatedAt: Date.now() });
        learnNames(kind, data);
      } catch (error) {
        if (started !== generation) return;
        putStatus(kind, { error: errorMessage(error), loading: false });
      } finally {
        if (started === generation) {
          fetching.delete(kind);
          if (again.delete(kind)) void fetchStatus(kind);
        }
      }
    })();
    fetching.set(kind, task);
    return task;
  };

  /** Something these snapshots show changed: fetch the shown ones soon, the others when next shown. */
  const statusChanged = (kinds: StatusKind[]) => {
    for (const kind of kinds) {
      if (wanted[kind] > 0) pending.add(kind);
      else changed.add(kind);
    }
    if (!pending.size || statusTimer) return;
    statusTimer = setTimeout(() => {
      statusTimer = null;
      const kinds = [...pending];
      pending.clear();
      if (get().phase === "ready") for (const kind of kinds) void fetchStatus(kind);
    }, STATUS_DEBOUNCE_MS);
  };

  const putSummary = (summary: JobSummary) => {
    const previous = get().jobs[summary.id];
    const record = previous ? withSummary(previous, summary) : newRecord({ job: summary, parameters: {}, locks: [] });
    set((s) => ({ jobs: { ...s.jobs, [summary.id]: record } }));
    if (previous && !isTerminal(recordStatus(previous)) && isTerminal(recordStatus(record))) {
      announce(record);
      if (summary.operation === get().setup?.operation) void refreshSetup();
      if (get().operations[summary.operation]?.mutating && !summary.dry_run) statusChanged(STATUS_KINDS);
    }
    if (watchedIds().includes(summary.id) && summary.last_seq > record.seq) void get().loadJob(summary.id);
  };

  const putEvent = (event: JobEvent) => {
    const record = get().jobs[event.job];
    if (!record) return;
    if (event.seq > record.seq + 1) {
      void get().loadJob(event.job);
      return;
    }
    logs.append(event.job, [event]);
    set((s) => ({ jobs: { ...s.jobs, [event.job]: applyEvent(s.jobs[event.job], event) } }));
  };

  const announce = (record: JobRecord) => {
    const { focusedJobId, panelOpen } = get();
    if (panelOpen && focusedJobId === record.summary.id) return;
    const title = record.summary.title;
    const status = recordStatus(record);
    if (status === "succeeded") toast.success(`${title} finished`, record.summary.dry_run ? "Dry run complete." : undefined, record.summary.id);
    else if (status === "failed") toast.error(`${title} failed`, record.summary.message, record.summary.id);
    else if (status === "cancelled") toast.info(`${title} was cancelled`, undefined, record.summary.id);
    else if (status === "interrupted") toast.error(`${title} was interrupted`, record.summary.message, record.summary.id);
  };

  /** When setup has just finished, everything else becomes available: load it all again. */
  const refreshSetup = async () => {
    const wasActive = get().setup?.active;
    try {
      const setup = await api().setup();
      if (get().phase !== "ready") return;
      set({ setup });
      if (wasActive && !setup.active) {
        stopStream?.();
        stopStream = null;
        await get().begin();
      }
    } catch {
      // The next login reads it again.
    }
  };

  const refreshJobs = async () => {
    const list = await api().jobs();
    for (const summary of list) putSummary(summary);
  };

  const onMessage = (message: StreamMessage) => {
    switch (message.type) {
      case "job":
        putSummary(message.job);
        break;
      case "job_event":
        putEvent(message.event);
        break;
      case "resnapshot":
        void refreshJobs().catch(() => {});
        for (const id of watchedIds()) void get().loadJob(id);
        statusChanged(STATUS_KINDS);
        break;
      case "observation":
        statusChanged(AFFECTED[message.collector] ?? STATUS_KINDS);
        break;
      case "host":
      case "quorum":
        statusChanged(STATUS_KINDS);
        break;
      default:
        break;
    }
  };

  const reset = (notice: string | null) => {
    stopStream?.();
    stopStream = null;
    logs.clear();
    loading.clear();
    resetStatus();
    useToasts.getState().clear();
    set({ ...initial, loginNotice: notice });
  };

  return {
    ...initial,
    loginNotice: null,

    wantStatus(kind) {
      wanted[kind] += 1;
      const entry = get().status[kind];
      if (changed.delete(kind) || (!entry.data && !entry.loading)) void fetchStatus(kind);
      let released = false;
      return () => {
        if (released) return;
        released = true;
        wanted[kind] -= 1;
      };
    },

    async refreshStatus(kinds) {
      await Promise.all(kinds.map((kind) => fetchStatus(kind)));
    },

    async begin() {
      const b = api();
      stopEnded?.();
      stopEnded = b.onSessionEnded((message) => reset(message || "Your session ended. Log in again."));
      set({ phase: "loading", session: b.session(), loginNotice: null, loadError: null });
      try {
        const [operations, jobs, setup] = await Promise.all([b.operations(), b.jobs(), b.setup()]);
        set({
          operations: Object.fromEntries(operations.map((o) => [o.id, o])),
          operationOrder: operations.map((o) => o.id),
          setup,
          jobs: Object.fromEntries(jobs.map((j) => [j.id, newRecord({ job: j, parameters: {}, locks: [] })])),
          phase: "ready",
        });
        stopStream = b.subscribe({ message: onMessage, state: (stream) => set({ stream }) });
        if (setup.active && setup.job) {
          get().focusJob(setup.job.id);
          return;
        }
        const active = jobs.filter((j) => !isTerminal(recordStatus(newRecord({ job: j, parameters: {}, locks: [] }))));
        const latest = active[0];
        if (latest) set({ consoleJobId: latest.id });
      } catch (error) {
        if (get().phase === "login") return;
        set({ loadError: errorMessage(error), phase: "ready" });
      }
    },

    async logout() {
      try {
        await backend?.logout();
      } finally {
        reset(null);
      }
    },

    setPage: (page) => set({ page }),

    openLaunch: (operation, values) => set({ launch: { operation, values, idempotencyKey: crypto.randomUUID() } }),
    closeLaunch: () => set({ launch: null }),

    startOperation(operation, values) {
      const op = get().operations[operation];
      if (op && op.allowed && !op.mutating && op.params.length === 0) void get().launchJob(operation, {}, false);
      else get().openLaunch(operation, values);
    },

    async launchJob(operation, parameters, dryRun) {
      const key = get().launch?.operation === operation ? get().launch!.idempotencyKey : crypto.randomUUID();
      try {
        const summary = await api().launch({ operation, parameters: parameters as never, idempotency_key: key, dry_run: dryRun });
        putSummary(summary);
        set((s) => ({
          focusedJobId: summary.id,
          panelOpen: true,
          consoleJobId: s.consolePinned && s.consoleJobId && !isTerminal(recordStatus(s.jobs[s.consoleJobId] ?? newRecord({ job: summary, parameters: {}, locks: [] }))) ? s.consoleJobId : summary.id,
          consolePinned: false,
          launch: null,
        }));
        updateWatch();
        return summary;
      } catch (error) {
        toast.error(`Could not start ${get().operations[operation]?.title ?? operation}`, errorMessage(error));
        return null;
      }
    },

    refreshSetup: () => refreshSetup(),

    async startSetup() {
      const setup = get().setup;
      if (!setup?.active) return;
      const summary = await get().launchJob(setup.operation, {}, false);
      if (summary) set((s) => ({ setup: s.setup && { ...s.setup, job: summary } }));
    },

    async answer(jobId, requestId, answer) {
      try {
        await api().answer(jobId, requestId, answer);
        return true;
      } catch (error) {
        toast.error("The answer was not accepted", errorMessage(error));
        return false;
      }
    },

    async cancel(jobId, force = false) {
      try {
        await api().cancel(jobId, force);
        return true;
      } catch (error) {
        toast.error(force ? "Could not stop the script" : "Could not cancel", errorMessage(error));
        return false;
      }
    },

    async resume(jobId) {
      try {
        await api().resume(jobId);
        return true;
      } catch (error) {
        toast.error("Could not resume", errorMessage(error));
        return false;
      }
    },

    async reconcile(jobId) {
      try {
        await api().reconcile(jobId);
        return true;
      } catch (error) {
        toast.error("Could not mark the job failed", errorMessage(error));
        return false;
      }
    },

    focusJob: (jobId) => {
      set({ focusedJobId: jobId, panelOpen: jobId !== null });
      if (jobId) set((s) => (s.consolePinned ? {} : { consoleJobId: jobId }));
      updateWatch();
    },
    showInPanel: (jobId) => {
      set({ focusedJobId: jobId, panelOpen: true });
      updateWatch();
    },
    setPanelOpen: (open) => {
      set({ panelOpen: open });
      updateWatch();
    },
    setConsoleOpen: (open) => {
      set({ consoleOpen: open });
      updateWatch();
    },
    showInConsole: (jobId, pin = true) => {
      set({ consoleJobId: jobId, consolePinned: pin, consoleOpen: true });
      updateWatch();
    },

    loadJob(jobId) {
      const running = loading.get(jobId);
      if (running) return running;
      const task = (async () => {
        try {
          for (;;) {
            const record = get().jobs[jobId];
            const view = await api().job(jobId, (record?.seq ?? 0) + 1);
            if (get().phase !== "ready") return;
            logs.append(jobId, view.events);
            set((s) => ({ jobs: { ...s.jobs, [jobId]: applyView(s.jobs[jobId] ?? null, view) } }));
            if (!view.more) return;
          }
        } catch {
          // The job may have been pruned; the stream will say if it changes.
        } finally {
          loading.delete(jobId);
        }
      })();
      loading.set(jobId, task);
      return task;
    },
  };
});

/** Points the store at a backend (the real one, or the mock in development and tests). */
export function setBackend(next: Backend) {
  stopStream?.();
  stopStream = null;
  stopEnded?.();
  stopEnded = null;
  backend = next;
  loading.clear();
  resetStatus();
  logs.clear();
  useStore.setState({ ...initial, loginNotice: null });
}

export const selectActiveJobs = (s: State) =>
  Object.values(s.jobs)
    .filter((r) => !isTerminal(recordStatus(r)))
    .sort((a, b) => a.summary.created_at_ms - b.summary.created_at_ms);
