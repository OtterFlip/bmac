// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// What the window asks of the Deployer's Rust side. The Tauri backend calls
// its commands; the mock backend (lib/mock.ts) stands in for tests and for
// `pnpm dev` in a plain browser.

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import {
  DEPLOYER_CLOSE_REQUESTED,
  DEPLOYER_TASK_EVENT,
  DEPLOYER_TASK_UPDATED,
  type AnswerTask,
  type Check,
  type DeployerInfo,
  type DeployerSettings,
  type DeployerSettingsView,
  type SshEntry,
  type TaskEvent,
  type TaskKind,
  type TaskSummary,
  type TaskView,
} from "@bmac/ui";

export interface BackendEvents {
  event(event: TaskEvent): void;
  updated(summary: TaskSummary): void;
  /** The window was asked to close while tasks run. */
  closeRequested(running: number): void;
}

export interface Backend {
  info(): Promise<DeployerInfo>;
  preflight(): Promise<Check[]>;
  settings(): Promise<DeployerSettingsView>;
  saveSettings(next: DeployerSettings): Promise<DeployerSettingsView>;
  deleteIso(path: string): Promise<DeployerSettingsView>;
  /** Starts a workflow; it asks for everything else through its requests. */
  startTask(kind: TaskKind): Promise<TaskSummary>;
  /** The managed entries in ~/.ssh/config, read without connecting anywhere. */
  sshEntries(): Promise<SshEntry[]>;
  /** Removes one entry and its pinned key; returns the entries left. */
  removeSshEntry(alias: string): Promise<SshEntry[]>;
  tasks(): Promise<TaskSummary[]>;
  task(id: string): Promise<TaskView>;
  answer(answer: AnswerTask): Promise<void>;
  cancel(id: string): Promise<void>;
  openExternal(url: string): Promise<void>;
  /** Closes the Deployer; `cancel` cancels running tasks first. */
  quit(cancel: boolean): Promise<void>;
  subscribe(handlers: BackendEvents): () => void;
}

/** Tauri reports a command's error as the string the Rust side returned. */
export function errorText(error: unknown): string {
  if (typeof error === "string") return error;
  if (error instanceof Error) return error.message;
  return String(error);
}

export function createTauriBackend(): Backend {
  return {
    info: () => invoke("get_app_info"),
    preflight: () => invoke("run_preflight"),
    settings: () => invoke("get_settings"),
    saveSettings: (next) => invoke("update_settings", { next }),
    deleteIso: (path) => invoke("delete_iso", { path }),
    startTask: (kind) => invoke("start_task", { kind }),
    sshEntries: () => invoke("ssh_entries"),
    removeSshEntry: (alias) => invoke("remove_ssh_entry", { alias }),
    tasks: () => invoke("list_tasks"),
    task: (id) => invoke("get_task", { id }),
    answer: (answer) => invoke("answer_task", { answer }),
    cancel: (id) => invoke("cancel_task", { id }),
    openExternal: (url) => invoke("open_external", { url }),
    quit: (cancel) => invoke("quit_app", { cancel }),
    subscribe(handlers) {
      const stops = [
        listen<TaskEvent>(DEPLOYER_TASK_EVENT, (e) => handlers.event(e.payload)),
        listen<TaskSummary>(DEPLOYER_TASK_UPDATED, (e) => handlers.updated(e.payload)),
        listen<number>(DEPLOYER_CLOSE_REQUESTED, (e) => handlers.closeRequested(e.payload)),
      ];
      return () => stops.forEach((stop) => void stop.then((unlisten) => unlisten()));
    },
  };
}
