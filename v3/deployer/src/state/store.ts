// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

import { create } from "zustand";
import { logs, type Answer, type Check, type DeployerInfo, type DeployerSettings, type DeployerSettingsView, type JobRecord, type TaskEvent, type TaskKind, type TaskSummary } from "@bmac/ui";
import { errorText, type Backend } from "../lib/backend";
import { foldEvent, foldSummary, foldView, toJobEvent } from "../lib/tasks";
import { toast } from "./toast";

export type Page = "start" | "discovery" | "hosts" | "appliances" | "ssh" | "settings";

interface State {
  backend: Backend | null;
  info: DeployerInfo | null;
  checks: Check[] | null;
  checking: boolean;
  settings: DeployerSettingsView | null;
  /** Every task this session, by id; `order` is newest first. */
  tasks: Record<string, JobRecord>;
  order: string[];
  focused: string | null;
  page: Page;
  /** Tasks still running when the window was asked to close. */
  closeRequest: number | null;

  setBackend(backend: Backend): () => void;
  recheck(): Promise<void>;
  saveSettings(next: DeployerSettings): Promise<boolean>;
  deleteIso(path: string): Promise<boolean>;
  setPage(page: Page): void;
  focus(id: string | null): void;
  startTask(kind: TaskKind): Promise<boolean>;
  answer(id: string, requestId: string, answer: Answer): Promise<boolean>;
  cancel(id: string): Promise<boolean>;
  dismissClose(): void;
  quit(cancel: boolean): Promise<void>;
}

const need = (backend: Backend | null) => {
  if (!backend) throw new Error("the Deployer backend isn't connected");
  return backend;
};

export const useStore = create<State>((set, get) => {
  /** Fetches a task's whole history; used the first time the window hears of it. */
  const load = async (id: string) => {
    try {
      const view = await need(get().backend).task(id);
      const output = view.events.filter((e) => e.type === "output").map(toJobEvent);
      if (output.length) logs.append(id, output);
      set((s) => ({
        tasks: { ...s.tasks, [id]: foldView(s.tasks[id] ?? null, view) },
        order: s.order.includes(id) ? s.order : [id, ...s.order],
      }));
    } catch (error) {
      toast.error("Couldn't load the task", errorText(error));
    }
  };

  const onEvent = (event: TaskEvent) => {
    const record = get().tasks[event.task];
    if (!record) return void load(event.task);
    if (event.seq <= record.seq) return;
    if (event.type === "output") logs.append(event.task, [toJobEvent(event)]);
    set((s) => ({ tasks: { ...s.tasks, [event.task]: foldEvent(s.tasks[event.task], event) } }));
  };

  const onUpdated = (summary: TaskSummary) => {
    const record = get().tasks[summary.id];
    if (!record) {
      void load(summary.id).then(() => set({ focused: summary.id }));
      return;
    }
    set((s) => ({ tasks: { ...s.tasks, [summary.id]: foldSummary(s.tasks[summary.id], summary) } }));
  };

  return {
    backend: null,
    info: null,
    checks: null,
    checking: false,
    settings: null,
    tasks: {},
    order: [],
    focused: null,
    page: "start",
    closeRequest: null,

    setBackend(backend) {
      set({ backend });
      const stop = backend.subscribe({ event: onEvent, updated: onUpdated, closeRequested: (running) => set({ closeRequest: running }) });
      void backend.info().then((info) => set({ info }), (error) => toast.error("Couldn't start the Deployer", errorText(error)));
      void backend.settings().then((settings) => set({ settings }), (error) => toast.error("Couldn't read the settings", errorText(error)));
      void backend.tasks().then((tasks) => tasks.forEach((t) => void load(t.id)), () => {});
      void get().recheck();
      return stop;
    },

    async recheck() {
      set({ checking: true });
      try {
        set({ checks: await need(get().backend).preflight() });
      } catch (error) {
        toast.error("Couldn't check this workstation", errorText(error));
      } finally {
        set({ checking: false });
      }
    },

    async saveSettings(next) {
      try {
        set({ settings: await need(get().backend).saveSettings(next) });
        return true;
      } catch (error) {
        toast.error("Couldn't save the settings", errorText(error));
        return false;
      }
    },

    async deleteIso(path) {
      try {
        set({ settings: await need(get().backend).deleteIso(path) });
        return true;
      } catch (error) {
        toast.error("Couldn't delete the ISO", errorText(error));
        return false;
      }
    },

    setPage: (page) => set({ page }),
    focus: (focused) => set({ focused }),

    async startTask(kind) {
      try {
        const summary = await need(get().backend).startTask(kind);
        if (!get().tasks[summary.id]) await load(summary.id);
        set({ focused: summary.id });
        return true;
      } catch (error) {
        toast.error("Couldn't start the task", errorText(error));
        return false;
      }
    },

    async answer(id, requestId, answer) {
      try {
        await need(get().backend).answer({ task: id, request_id: requestId, answer });
        return true;
      } catch (error) {
        toast.error("Couldn't send the answer", errorText(error));
        return false;
      }
    },

    async cancel(id) {
      try {
        await need(get().backend).cancel(id);
        return true;
      } catch (error) {
        toast.error("Couldn't cancel the task", errorText(error));
        return false;
      }
    },

    dismissClose: () => set({ closeRequest: null }),

    async quit(cancel) {
      set({ closeRequest: null });
      try {
        await need(get().backend).quit(cancel);
      } catch (error) {
        toast.error("Couldn't close the Deployer", errorText(error));
      }
    },
  };
});
