// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// Sample data for `pnpm dev` with `?mock`: two operations, a finished job, a
// job waiting for an answer, and launches that play out a short script.

import type { LaunchJob, OperationView } from "@bmac/ui";
import type { MockBackend } from "./backend";
import { runScenario } from "./scenarios";

const base = {
  allowed: true,
  kind: "script",
  min_role: "operator",
  mutating: true,
  destructive: false,
  supports_dry_run: true,
  locks: [],
  cancel: { rule: "script" },
  resume: "never",
} as const;

export const DEMO_OPERATIONS = [
  {
    ...base,
    id: "lab.interactive",
    title: "Lab: interactive mock",
    summary: "Asks for input and confirmation, then finishes.",
    description: "A test script that exercises every request kind.",
    category: "diagnostics",
    params: [{ id: "host", label: "Host", type: "host", required: true, advanced: false, choices: [] }],
    target: { rule: "host", param: "host" },
  },
  {
    ...base,
    id: "lab.wipe",
    title: "Lab: destructive mock",
    summary: "Pretends to wipe a disk.",
    description: "Shows the destructive launch dialog.",
    category: "storage",
    destructive: true,
    min_role: "admin",
    params: [],
    target: { rule: "duty" },
  },
] as unknown as OperationView[];

function param(id: string, label: string, type: string) {
  return { id, label, type, required: true, advanced: false, choices: [] };
}

/** The cluster operations the status pages offer, as bmacd lists them. */
export const CLUSTER_OPERATIONS = (
  [
    ["prod.add", "Add production VM", "production", "operator", false, []],
    ["prod.remove", "Remove production VM", "production", "admin", true, [param("resource", "Production VM", "production")]],
    ["prod.change_owner", "Move production VM", "production", "operator", false, [param("resource", "Production VM", "production")]],
    ["prod.change_placement", "Change placement", "production", "operator", false, [param("resource", "Production VM", "production")]],
    ["prod.extend_disk", "Grow production disk", "production", "operator", false, [param("resource", "Production VM", "production")]],
    ["staging.add", "Add staging VM", "staging", "operator", false, [param("source", "Copy of", "production")]],
    ["staging.remove", "Remove staging VM", "staging", "admin", true, [param("resource", "Staging VM", "staging")]],
    ["hosts.remove", "Remove Proxmox host", "hosts", "admin", true, [param("host", "Host", "host")]],
    ["hosts.join", "Approve a host's join", "hosts", "admin", true, [{ ...param("host", "Host", "host"), suggest: "free_host_slots" }]],
    ["qdevice.add", "Add QDevice", "qdevice", "operator", false, []],
    ["qdevice.remove", "Remove QDevice", "qdevice", "admin", true, []],
  ] as const
)
  .map(([id, title, category, min_role, destructive, params]) => ({
    ...base,
    id,
    title,
    summary: `${title}.`,
    description: "",
    category,
    min_role,
    destructive,
    params,
    target: { rule: "duty" },
  }))
  .concat(
    (
      [
        ["diagnostics.cluster_health", "Cluster health check", []],
        ["diagnostics.cluster_state", "Full cluster state", []],
        ["diagnostics.host_state", "Inspect a host", [param("host", "Host", "host")]],
        ["diagnostics.prod_vm_state", "Inspect a production VM", [param("resource", "Production VM", "production")]],
        ["diagnostics.qdevice_state", "Inspect the QDevice", []],
      ] as const
    ).map(([id, title, params]) => ({
      ...base,
      id,
      title,
      summary: `${title}; read-only.`,
      description: "",
      category: "diagnostics",
      min_role: "viewer",
      mutating: false,
      destructive: false,
      supports_dry_run: false,
      params,
      target: { rule: "duty" },
    })) as never[],
  )
  .concat(
    (
      [
        ["storage.add_vdev", "Add a vdev", true],
        ["storage.attach_disk", "Attach a disk", true],
        ["storage.replace_disk", "Replace a disk", true],
        ["storage.remove_vdev", "Remove a vdev", true],
        ["storage.inventory", "Storage inventory", false],
        ["storage.simulate_failure", "Simulate a disk failure", true],
      ] as const
    ).map(([id, title, destructive]) => ({
      ...base,
      id,
      title,
      summary: `${title}.`,
      description: "",
      category: "storage",
      min_role: "admin",
      destructive,
      params: [param("host", "Host", "host")],
      target: { rule: "host", param: "host" },
    })) as never[],
  ) as unknown as OperationView[];

export function seedDemo(mock: MockBackend) {
  mock.operationList = [...DEMO_OPERATIONS, ...CLUSTER_OPERATIONS];
  const done = mock.addJob({ title: "Lab: interactive mock", host: "mox1", state: "RUNNING", created_at_ms: Date.now() - 600_000 });
  mock.addEvent(done.id, { type: "started", operation: done.operation, host: "mox1", user: "alice@pve", dry_run: false });
  mock.addEvent(done.id, { type: "phase", id: "check", label: "Check the host", status: "complete" });
  mock.addEvent(done.id, { type: "output", level: "info", text: "mox1 is a cluster member" });
  mock.addEvent(done.id, { type: "completed", message: "Everything checked out." });

  const waiting = mock.addJob({ title: "Lab: interactive mock", host: "mox2" });
  mock.addEvent(waiting.id, { type: "started", operation: waiting.operation, host: "mox2", user: "alice@pve", dry_run: false });
  mock.addEvent(waiting.id, { type: "phase", id: "ask", label: "Ask the operator", status: "running" });
  mock.addEvent(waiting.id, { type: "output", level: "info", text: "Waiting for an answer" });
  mock.addEvent(waiting.id, { type: "request", request_id: "r1", kind: "confirm", title: "Continue?", message: "The mock will finish when you confirm.", severity: "info" });

  const launch = mock.launch.bind(mock);
  mock.launch = async (request: LaunchJob) => {
    const host = typeof request.parameters.host === "string" ? request.parameters.host : "mox1";
    const existing = mock.jobsById.has(`key-${request.idempotency_key}`);
    const summary = await launch(request);
    const job = mock.jobsById.get(summary.id);
    const op = mock.operationList.find((o) => o.id === request.operation);
    if (!job || !op || existing) return summary;
    job.summary = { ...job.summary, host };
    runScenario(mock, summary.id, op, request, host);
    return job.summary;
  };
}
