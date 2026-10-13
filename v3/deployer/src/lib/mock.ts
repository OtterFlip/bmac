// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// A stand-in for the Deployer's Rust side: canned workstation checks and
// settings, and tasks driven by the test (or the demo) through `emit`.

import type { AnswerTask, Check, DeployerInfo, DeployerSettings, DeployerSettingsView, EventBody, SshEntry, TaskEvent, TaskKind, TaskSummary, TaskView } from "@bmac/ui";
import type { Backend, BackendEvents } from "./backend";

export const demoChecks: Check[] = [
  { id: "platform", label: "Workstation", status: "ok", detail: "linux on x86_64", fix: null, command: null, purpose: "The Deployer runs on amd64 Linux.", link: null },
  { id: "xorriso", label: "xorriso", status: "ok", detail: "/usr/bin/xorriso", fix: null, command: null, purpose: "Builds the discovery, host, and appliance ISOs.", link: null },
  {
    id: "tailscale",
    label: "Tailscale",
    status: "warning",
    detail: "Backend state: NeedsLogin",
    fix: "Connect this workstation to the tailnet the cluster uses.",
    command: "sudo tailscale up",
    purpose: "New hosts and discovery sessions are found on your tailnet.",
    link: null,
  },
];

export function defaultSettings(): DeployerSettings {
  return { link_wait: { attempts: 10, interval_seconds: 3 }, isos: [], notifications: true };
}

export class MockBackend implements Backend {
  checks: Check[] = demoChecks;
  stored: DeployerSettings = defaultSettings();
  present = new Set<string>();
  summaries = new Map<string, TaskSummary>();
  events = new Map<string, TaskEvent[]>();
  answers: AnswerTask[] = [];
  cancelled: string[] = [];
  opened: string[] = [];
  quits: boolean[] = [];
  started: TaskKind[] = [];
  entries: SshEntry[] = [];
  private handlers: BackendEvents[] = [];

  async info(): Promise<DeployerInfo> {
    return { name: "BMAC Deployer", version: "3.0.0-dev.0", first_launch: false };
  }
  async preflight() {
    return this.checks;
  }
  private view(): DeployerSettingsView {
    return {
      settings: structuredClone(this.stored),
      isos: this.stored.isos.map((iso) => ({ iso, present: this.present.has(iso.path) })),
      path: "/home/you/.config/com.btvcorp.bmac.deployer/settings.json",
      default_discovery_iso: { url: "https://dl-cdn.alpinelinux.org/alpine/v3.24/releases/x86_64/alpine-extended-3.24.2-x86_64.iso", sha256: "a22115a46a1bcd876234a6cd43797fe19f4acfc4512fb41ea64e16cc2ffb4d90" },
    };
  }
  async settings() {
    return this.view();
  }
  async saveSettings(next: DeployerSettings) {
    this.stored = { ...this.stored, link_wait: next.link_wait, notifications: next.notifications, discovery_iso: next.discovery_iso };
    return this.view();
  }
  async deleteIso(path: string) {
    this.stored.isos = this.stored.isos.filter((iso) => iso.path !== path);
    this.present.delete(path);
    return this.view();
  }
  async startTask(kind: TaskKind) {
    this.started.push(kind);
    const titles: Record<TaskKind, string> = {
      discovery: "Hardware discovery",
      host: "Install a Proxmox host",
      host_ssh: "Set up SSH to a host",
      guest_ssh: "Set up SSH to a guest",
      hello: "Deploy the hello app",
    };
    const title = titles[kind];
    return this.start(`t${this.summaries.size + 1}`, title, kind);
  }
  async sshEntries() {
    return structuredClone(this.entries);
  }
  async removeSshEntry(alias: string) {
    this.entries = this.entries.filter((e) => e.alias !== alias);
    return this.sshEntries();
  }
  async tasks() {
    return [...this.summaries.values()].sort((a, b) => b.created_at_ms - a.created_at_ms);
  }
  async task(id: string): Promise<TaskView> {
    const task = this.summaries.get(id);
    if (!task) throw `there is no task ${id}`;
    return { task, parameters: {}, events: this.events.get(id) ?? [] };
  }
  async answer(answer: AnswerTask) {
    this.answers.push(answer);
    this.update(answer.task, { pending_request: null });
  }
  async cancel(id: string) {
    this.cancelled.push(id);
    this.update(id, { cancel_requested: true });
  }
  async openExternal(url: string) {
    this.opened.push(url);
  }
  async quit(cancel: boolean) {
    this.quits.push(cancel);
  }
  subscribe(handlers: BackendEvents) {
    this.handlers.push(handlers);
    return () => {
      this.handlers = this.handlers.filter((h) => h !== handlers);
    };
  }

  /** Starts a task, as the Rust side does when work begins. */
  start(id: string, title: string, kind = "demo"): TaskSummary {
    const summary: TaskSummary = {
      id,
      kind,
      title,
      state: "RUNNING",
      created_at_ms: Date.now(),
      updated_at_ms: Date.now(),
      pending_request: null,
      message: null,
      cancel_requested: false,
      last_seq: 0,
    };
    this.summaries.set(id, summary);
    this.events.set(id, []);
    this.handlers.forEach((h) => h.updated(summary));
    return summary;
  }

  /** Records an event on a task and sends it to the window. */
  emit(id: string, body: EventBody) {
    const summary = this.summaries.get(id)!;
    const event = { task: id, seq: summary.last_seq + 1, at_ms: Date.now(), ...body } as TaskEvent;
    this.events.get(id)!.push(event);
    const ended = { completed: "COMPLETED", failed: "FAILED", cancelled: "CANCELLED" } as const;
    const done = body.type in ended;
    const state = done ? ended[body.type as keyof typeof ended] : summary.state;
    const pending = body.type === "request" ? body.request_id : summary.pending_request;
    this.summaries.set(id, { ...summary, state, last_seq: event.seq, pending_request: done ? null : pending });
    this.handlers.forEach((h) => h.event(event));
    this.handlers.forEach((h) => h.updated(this.summaries.get(id)!));
  }

  update(id: string, change: Partial<TaskSummary>) {
    const next = { ...this.summaries.get(id)!, ...change };
    this.summaries.set(id, next);
    this.handlers.forEach((h) => h.updated(next));
  }

  requestClose(running: number) {
    this.handlers.forEach((h) => h.closeRequested(running));
  }
}
