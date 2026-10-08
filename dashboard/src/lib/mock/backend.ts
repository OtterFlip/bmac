// An in-browser stand-in for the Rust engine, used by `npm run dev` outside
// Tauri. It replays plausible protocol events so the UI can be developed and
// demonstrated without a cluster. It never runs anything.

import registry from "@registry";
import type { Backend } from "@/lib/api";
import type {
  DirListing,
  PreflightCheck,
  RequestEvent,
  ResponsePayload,
  RunDetails,
  RunEvent,
  RunRecord,
  RunStatus,
  Settings,
  Workflow,
  WorkflowEvent,
  WorkflowInfo,
} from "@/protocol/types";
import { disksState, guestsState, hostsState, replicationState, storageState } from "./fixtures";

const workflows = (registry as { workflows: Workflow[] }).workflows;

class Cancelled extends Error {}

interface MockRun {
  record: RunRecord;
  events: RunEvent[];
  logs: RunEvent[];
  seq: number;
  resolve: ((payload: ResponsePayload) => void) | null;
  cancelled: boolean;
  awaiting: RequestEvent | null;
}

const quote = (arg: string) => (/^[A-Za-z0-9_./=:,@%+-]+$/.test(arg) ? arg : `'${arg.replace(/'/g, "'\\''")}'`);

function buildArgs(workflow: Workflow, values: Record<string, unknown>) {
  const options: string[] = [];
  const positional: string[] = [];
  const summary: Record<string, string> = {};
  let target: string | null = null;
  for (const param of workflow.params) {
    const value = values[param.id];
    if (value === undefined || value === null || value === "" || value === false) {
      if (param.required) throw { code: "invalid_argument", message: `${param.label} is required` };
      continue;
    }
    if (param.type === "flag") {
      options.push(param.flag!);
      summary[param.id] = "yes";
    } else if (param.type === "flag_choice") {
      const choice = param.choices?.find((c) => c.value === value);
      if (choice?.flag) options.push(choice.flag);
      summary[param.id] = String(value);
    } else {
      if (param.flag) options.push(param.flag, String(value));
      else positional.push(String(value));
      summary[param.id] = String(value);
      if (!target && ["host", "production", "staging", "guest", "hostname"].includes(param.type)) target = String(value);
    }
  }
  return { args: [...options, ...positional], summary, target, dryRun: values.dry_run === true };
}

const tableLines: Record<string, string[]> = {
  list_hosts: [
    "",
    "CLUSTER bmac - read through mox1",
    "=================================",
    "  Quorate:       yes",
    "  Votes:         3 of 3 expected (quorum 2)",
    "  Hosts online:  3 of 3",
    "  Control node:  mox1",
    "  QDevice:       not registered; not needed (odd host count)",
    "",
    "HOSTS",
    "=====",
    "  HOST  STATE   SSH FROM HERE  UPTIME   CPU        MEMORY              SLOT    ROLE",
    "  ----  ------  -------------  -------  ---------  ------------------  ------  --------------",
    "  mox1  online  ok             15d 1h   18% of 48  92.0 GiB / 256 GiB  active  control, probe",
    "  mox2  online  ok             18d 2h   7% of 48   61.0 GiB / 256 GiB  active  -",
    "  mox3  online  ok             21d 3h   31% of 64  141 GiB / 384 GiB   active  -",
    "",
    "OK: nothing needs attention.",
  ],
};

export function createMockBackend(): Backend {
  const runs = new Map<string, MockRun>();
  const finished: RunRecord[] = [];
  const eventListeners = new Set<(runId: string, events: RunEvent[]) => void>();
  const recordListeners = new Set<(record: RunRecord) => void>();
  let settings: Settings = { repository: "/home/operator/src/bmac", refresh_interval_seconds: 0, notifications: true, default_dry_run: true };

  const notify = (run: MockRun) => {
    const copy = structuredClone(run.record);
    recordListeners.forEach((cb) => cb(copy));
  };

  const emit = (run: MockRun, event: WorkflowEvent) => {
    if (run.cancelled && event.type !== "completed" && event.type !== "log") throw new Cancelled();
    run.seq += 1;
    const item: RunEvent = { seq: run.seq, at: new Date().toISOString(), event };
    const r = run.record;
    r.event_count = run.seq;
    if (event.type === "log") {
      run.logs.push(item);
      r.log_lines += 1;
      if (event.text.trim()) r.last_log = event.text;
    } else {
      run.events.push(item);
    }
    switch (event.type) {
      case "protocol":
        r.status = "running";
        break;
      case "phase": {
        const existing = r.phases.find((p) => p.id === event.id);
        const at = item.at;
        if (existing) {
          existing.status = event.status;
          if (event.status !== "running") existing.ended_at = at;
        } else {
          r.phases.push({ id: event.id, label: event.label, status: event.status, cancel_allowed: event.cancel_allowed ?? true, started_at: at, ended_at: null });
        }
        if (event.status === "running") r.current_activity = event.label;
        r.cancel_allowed = !r.phases.some((p) => p.status === "running" && !p.cancel_allowed);
        break;
      }
      case "progress":
        r.current_activity = event.message;
        break;
      case "result":
        r.result = event.data;
        break;
      case "next_step":
        r.next_steps.push({ text: event.text, command: event.command, workflow: event.workflow, args: event.args });
        break;
      case "error":
        r.errors.push(event.message);
        break;
      case "warning":
        r.warnings += 1;
        break;
      case "input":
      case "input_group":
        r.pending_request = event;
        r.status = "waiting_input";
        break;
      case "confirm":
        r.pending_request = event;
        r.status = "waiting_confirmation";
        break;
      case "manual_action":
        r.pending_request = event;
        r.status = "waiting_manual_action";
        break;
      case "validation_error":
        r.pending_request = run.awaiting;
        r.status = "waiting_input";
        break;
      default:
        break;
    }
    eventListeners.forEach((cb) => cb(r.run_id, [item]));
    if (event.type !== "log") notify(run);
  };

  const sleep = (run: MockRun, ms: number) =>
    new Promise<void>((resolve, reject) => {
      const timer = setTimeout(() => (run.cancelled ? reject(new Cancelled()) : resolve()), ms);
      if (run.cancelled) {
        clearTimeout(timer);
        reject(new Cancelled());
      }
    });

  const log = async (run: MockRun, lines: string[], gap = 25, stream = "stdout") => {
    for (const line of lines) {
      const level = /^\s*(ERROR|FAIL)/.test(line) ? "error" : /^\s*(WARNING|ATTENTION)/.test(line) ? "warning" : "info";
      emit(run, { type: "log", stream, level, text: line });
      if (gap) await sleep(run, gap);
    }
  };

  const ask = (run: MockRun, event: RequestEvent) =>
    new Promise<ResponsePayload>((resolve) => {
      run.awaiting = event;
      run.resolve = resolve;
      emit(run, event);
    });

  let stepNo = 0;
  const step = (run: MockRun, label: string) => {
    const previous = run.record.phases.find((p) => p.status === "running");
    if (previous) emit(run, { type: "phase", id: previous.id, label: previous.label, status: "complete" });
    stepNo += 1;
    emit(run, { type: "phase", id: `step-${stepNo}`, label, status: "running" });
  };

  async function scenario(run: MockRun, workflow: Workflow) {
    const r = run.record;
    const id = workflow.id;
    const target = r.target ?? "prod2";
    if (id.startsWith("list_")) {
      await sleep(run, 300);
      await log(run, [`Reading cluster state through mox1 ...`], 400);
      await log(run, tableLines[id] ?? ["", "(table output)", ""], 12);
      const data = { list_hosts: hostsState, list_guests: guestsState, list_replication: replicationState, list_storage: storageState, list_disks: disksState }[
        id as "list_hosts"
      ]();
      emit(run, { type: "result", data });
      if (id === "list_guests" || id === "list_replication") {
        emit(run, { type: "next_step", text: "Inspect prod2's replication and HA state.", command: "diagnostics/show_prod_vm_state.sh prod2", workflow: "show_prod_vm_state", args: { resource: "prod2" } });
      }
      if (id === "list_storage") {
        emit(run, { type: "next_step", text: "Inspect mox3's pool members and disk serials.", command: "diagnostics/show_proxmox_host_state.sh --host mox3", workflow: "show_proxmox_host_state", args: { host: "mox3" } });
      }
      if (id === "list_disks") {
        emit(run, { type: "next_step", text: "Finalize the retirement of the decommissioned disks on mox3.", command: "hosts/inventory_disks.sh --host mox3", workflow: "inventory_disks", args: { host: "mox3" } });
      }
      return;
    }
    if (workflow.mode === "read_only") {
      for (const section of ["Cluster membership", "Corosync links", "ZFS pools", "SMART health"]) {
        step(run, section);
        await log(run, [`== ${section} ==`, ...Array.from({ length: 14 }, (_, i) => `  mox${(i % 3) + 1}  ${section.toLowerCase()} check ${i + 1}: ok`)], 40);
      }
      await log(run, ["", "WARNING: mox3 rpool is 91% allocated", "", "Next steps:", "  - Inspect mox3's pool members and disk serials."], 30);
      emit(run, { type: "next_step", text: "Inspect mox3's pool members and disk serials.", command: "diagnostics/show_proxmox_host_state.sh --host mox3", workflow: "show_proxmox_host_state", args: { host: "mox3" } });
      return;
    }

    step(run, "Validate the cluster");
    await log(run, ["==> Loading env/cluster.conf", "==> Finding a reachable cluster member", "    using mox1 (quorate, 3 of 3 votes)"], 120);

    if (id === "add_staging_vm" && !r.dry_run) {
      const source = await ask(run, {
        type: "input", request_id: "req-1-source", title: "Source production VM",
        context: "Production VMs that can be cloned:\n  prod1  mox1  running  placement mox1,mox2\n  prod2  mox3  running  placement mox3,mox1,mox2",
        field: { id: "source", type: "select", label: "Clone from", required: true, sensitive: false, disabled: false,
          options: [{ value: "prod1", label: "prod1 — app.example.com", help: "Standby on mox2" }, { value: "prod2", label: "prod2 — api.example.com", help: "Standbys on mox1, mox2" }] },
      });
      if (source.kind === "cancel") throw new Cancelled();
      const values = source.kind === "values" ? String(source.values.source) : "prod1";
      step(run, "Choose resources");
      const group: RequestEvent = {
        type: "input_group", request_id: "req-2-resources", title: "Staging VM resources", layout: "form",
        description: `The clone starts with ${values}'s configuration; these override it.`,
        fields: [
          { id: "cores", type: "integer", label: "vCPU cores", default: 4, min: 1, max: 32, required: true, sensitive: false, disabled: false },
          { id: "memory_gib", type: "integer", label: "Memory", suffix: "GiB", default: 16, min: 2, max: 256, required: true, sensitive: false, disabled: false },
          { id: "start", type: "boolean", label: "Start the VM after creating it", default: true, required: false, sensitive: false, disabled: false },
          { id: "link_down", type: "boolean", label: "Keep its network link down", help: "Start isolated from the network until you have checked it.", default: false, required: false, sensitive: false, disabled: false },
        ],
      };
      for (;;) {
        const answer = await ask(run, group);
        if (answer.kind !== "values") throw new Cancelled();
        if (Number(answer.values.memory_gib) > 64) {
          emit(run, { type: "validation_error", request_id: group.request_id, field_errors: { memory_gib: "mox2 has only 64 GiB free for staging." } });
          continue;
        }
        break;
      }
    }

    step(run, "Build the plan");
    await sleep(run, 500);
    const destructive = workflow.destructive;
    emit(run, {
      type: "plan",
      title: `${workflow.title}${r.target ? `: ${r.target}` : ""}`,
      description: r.dry_run ? "Dry run: nothing was changed." : "These are the changes the script will make.",
      dry_run: r.dry_run,
      items: destructive
        ? [
            { kind: "remove", target: "HAProxy routes", description: `Unpublish ${target}'s 2 routes` },
            { kind: "remove", target: "HA resource vm:101", description: "Remove the HA resource and its node-affinity rule" },
            { kind: "remove", target: "replication 101-0, 101-1", description: "Remove both jobs and their target copies on mox1 and mox2" },
            { kind: "remove", target: "VM 101", description: "Destroy the VM and its volumes on mox3" },
            { kind: "update", target: "BMAC registry", description: `Release the ${target} allocation` },
          ]
        : [
            { kind: "check", target: "cluster", description: "3 of 3 hosts online, quorate" },
            { kind: "create", target: "VM 200", description: "Linked clone on mox2" },
            { kind: "update", target: "BMAC registry", description: "Reserve stage1prod1" },
            { kind: "keep", target: "production", description: "Production is never modified" },
          ],
    });
    if (r.dry_run) {
      await log(run, ["", "Dry run complete. Nothing was changed.", "", "Next steps:", `  - Run it again without --dry-run to apply this plan.`], 30);
      emit(run, { type: "next_step", text: "Run it again without dry run to apply this plan.", workflow: workflow.id, args: Object.fromEntries(Object.entries(r.launch_values).filter(([k]) => k !== "dry_run")) });
      return;
    }

    const confirm = await ask(run, {
      type: "confirm",
      request_id: "req-3-confirm",
      title: destructive ? `Permanently destroy ${target}?` : `${workflow.title}?`,
      message: destructive
        ? `This removes ${target}, its replicas, HA configuration, routes, and registry allocation. It cannot be undone.`
        : "The script is ready to apply this plan.",
      severity: destructive ? "destructive" : "normal",
      confirm_label: destructive ? `Destroy ${target}` : "Apply",
      confirmation_text: destructive ? `DESTROY ${target}` : undefined,
    });
    if (confirm.kind !== "confirm" || !confirm.confirmed) throw new Cancelled();

    step(run, "Apply changes");
    for (let i = 1; i <= 12; i += 1) {
      emit(run, { type: "progress", message: `Replicating snapshot to mox2`, current: i * 512 * 1024 ** 2, total: 6 * 1024 ** 3, unit: "bytes" });
      await log(run, [`  zfs send: ${(i * 0.5).toFixed(1)} GiB of 6.0 GiB`], 250);
    }
    if (workflow.category === "hosts" || workflow.category === "storage") {
      const ack = await ask(run, {
        type: "manual_action", request_id: "req-4-console", title: "Unlock the new disks at the console",
        instructions: ["Open the iDRAC virtual console for mox3.", "Type the LUKS passphrase for each new disk when prompted.", "Return here once both disks show as unlocked."],
        acknowledge_label: "Disks are unlocked",
      });
      if (ack.kind !== "acknowledge") throw new Cancelled();
    }
    step(run, "Verify");
    emit(run, { type: "phase", id: "commit", label: "Commit registry", status: "running", cancel_allowed: false });
    await log(run, ["==> Committing the registry (do not interrupt)"], 1800);
    emit(run, { type: "phase", id: "commit", label: "Commit registry", status: "complete" });
    await log(run, ["    verified", "", "Done."], 80);
    emit(run, { type: "result", data: destructive ? { removed: target, vmid: 101 } : { name: "stage1prod1", vmid: 200, node: "mox2", ip: "10.213.0.201", url: "https://stage1.app.example.com" } });
    emit(run, { type: "next_step", text: "Refresh the guest list to confirm the change.", workflow: "list_guests", args: {} });
    if (!destructive) emit(run, { type: "next_step", text: "Set up SSH to the new VM.", command: "guests/setup_jump_ssh_access.sh stage1prod1", workflow: "setup_jump_ssh_access", args: { resource: "stage1prod1" } });
  }

  const finish = (run: MockRun, status: RunStatus, message: string | null, exit: number) => {
    const r = run.record;
    for (const phase of r.phases) {
      if (phase.status === "running") phase.status = status === "succeeded" ? "complete" : status === "cancelled" ? "skipped" : "failed";
    }
    run.cancelled = false;
    emit(run, { type: "completed", status: status === "succeeded" ? "success" : status === "cancelled" ? "cancelled" : "failed", message: message ?? undefined, exit_code: exit });
    r.status = status;
    r.message = message;
    r.exit_code = exit;
    r.ended_at = new Date().toISOString();
    r.pending_request = null;
    r.cancel_allowed = false;
    r.current_activity = null;
    runs.delete(r.run_id);
    finished.unshift(structuredClone(r));
    runStore.set(r.run_id, run);
    notify(run);
  };
  const runStore = new Map<string, MockRun>();

  const start = (workflowId: string, values: Record<string, unknown>): RunRecord => {
    const workflow = workflows.find((w) => w.id === workflowId);
    if (!workflow) throw { code: "unknown_workflow", message: workflowId };
    if (workflow.mode === "mutating" && [...runs.values()].some((r) => r.record.mode === "mutating")) {
      throw { code: "busy", message: "another operation is already changing the cluster" };
    }
    const { args, summary, target, dryRun } = buildArgs(workflow, values);
    const argv = ["--json", ...args];
    const runId = crypto.randomUUID();
    const record: RunRecord = {
      run_id: runId, workflow_id: workflow.id, workflow_title: workflow.title, mode: workflow.mode, destructive: workflow.destructive,
      script: workflow.script, argv, command_line: `${workflow.script} ${argv.map(quote).join(" ")}`,
      terminal_command: `${workflow.script} ${args.map(quote).join(" ")}`.trim(), launch_values: summary, target, dry_run: dryRun,
      started_at: new Date().toISOString(), ended_at: null, status: "starting", message: null, exit_code: null, signal: null,
      pid: 40000 + Math.floor(Math.random() * 20000), cancel: null, cancel_allowed: true, phases: [], current_activity: "Starting",
      pending_request: null, result: null, next_steps: [], errors: [], warnings: 0, responses: [], log_lines: 0, last_log: null, event_count: 0,
    };
    const run: MockRun = { record, events: [], logs: [], seq: 0, resolve: null, cancelled: false, awaiting: null };
    runs.set(runId, run);
    runStore.set(runId, run);
    setTimeout(async () => {
      try {
        emit(run, { type: "protocol", protocol: "bmac-ui", version: 1 });
        emit(run, { type: "workflow_started", workflow: workflow.id, run_id: runId, script: workflow.script, argv: args, pid: record.pid ?? undefined, started_at: record.started_at });
        await scenario(run, workflow);
        const running = record.phases.find((p) => p.status === "running");
        if (running) emit(run, { type: "phase", id: running.id, label: running.label, status: "complete" });
        finish(run, "succeeded", null, 0);
      } catch (error) {
        if (error instanceof Cancelled) {
          await log(run, ["ERROR: Cancelled by the operator."], 0, "stderr").catch(() => {});
          finish(run, "cancelled", "Cancelled by the operator.", 3);
        } else {
          finish(run, "failed", String(error), 1);
        }
      }
    }, 120);
    return structuredClone(record);
  };

  const fakeDirs: Record<string, string[]> = {
    "/home/operator": ["Desktop/", "Documents/", "Downloads/", "src/", ".ssh/", "notes.txt"],
    "/home/operator/Downloads": ["proxmox-ve_9.0-1.iso", "sanitize_staging.sh", "origin-cert.pem", "origin-key.pem"],
    "/home/operator/src": ["bmac/"],
    "/home/operator/src/bmac": ["app/", "diagnostics/", "guests/", "hosts/", "lib/", "README.md"],
  };

  return {
    kind: "mock",
    getAppInfo: async () => ({ name: "BMAC Dashboard", version: "1.1.0", protocol_version: 1, history_dir: "~/.local/share/com.beentherevc.bmac.dashboard/runs" }),
    getPlatformInfo: async () => ({ os: "linux", arch: "x86_64", os_family: "debian", os_name: "Ubuntu 24.04.3 LTS", hostname: "workstation" }),
    getRepositoryInfo: async () => ({
      root: settings.repository ?? "", valid: true, problems: [], git_commit: "4b5847a1c0de", git_describe: "v0.2.0-14-g4b5847a", git_branch: "main",
      git_dirty: false, protocol_version: 1, cluster_conf_present: true, secrets_env_present: true,
      cluster_settings: [
        { key: "PROXMOX_CLUSTER_NAME", value: "bmac" }, { key: "PROXMOX_QDEVICE_HOST", value: "qdevice" },
        { key: "PROXMOX_CONTROL_NODE", value: "mox1" }, { key: "MAX_MOX_HOSTS", value: "8" },
      ],
      host_configs: ["mox1", "mox2", "mox3", "mox4", "mox6"],
    }),
    setRepository: async (path) => {
      settings = { ...settings, repository: path };
      return (await createMockBackend().getRepositoryInfo())!;
    },
    getSettings: async () => settings,
    updateSettings: async (next) => (settings = { ...next, repository: settings.repository }),
    listWorkflows: async () =>
      workflows.map((w): WorkflowInfo => ({ ...w, available: true })),
    runPreflight: async () => {
      await new Promise((r) => setTimeout(r, 500));
      const checks: PreflightCheck[] = [
        { id: "platform", label: "Workstation", status: "ok", detail: "Ubuntu 24.04.3 LTS on x86_64" },
        { id: "repository", label: "BMAC repository", status: "ok", detail: settings.repository ?? "" },
        { id: "bash", label: "Bash 4.4+", status: "ok", detail: "/usr/bin/bash (5.2.21(1)-release)" },
        { id: "python3", label: "Python 3.9+", status: "ok", detail: "python3 3.12" },
        { id: "ssh", label: "OpenSSH client", status: "ok", detail: "/usr/bin/ssh" },
        { id: "ssh_agent", label: "SSH agent", status: "ok", detail: "SSH_AUTH_SOCK is set." },
        { id: "flock", label: "flock", status: "ok", detail: "Available for host and QDevice workflows." },
        { id: "tailscale", label: "Tailscale", status: "ok", detail: "Backend state: Running" },
        { id: "cluster_conf", label: "env/cluster.conf", status: "ok", detail: "Present." },
        { id: "secrets_env", label: "env/secrets.env", status: "ok", detail: "Present (its contents are never read by the dashboard)." },
      ];
      return checks;
    },
    startWorkflow: async (id, values) => start(id, values),
    respond: async (runId, requestId, payload) => {
      const run = runs.get(runId);
      if (!run || run.record.pending_request?.request_id !== requestId || !run.resolve) {
        throw { code: "invalid_response", message: "That request is no longer waiting for an answer." };
      }
      const req = run.record.pending_request;
      run.record.responses.push({
        request_id: requestId, at: new Date().toISOString(), title: "title" in req && req.title ? req.title : "Input", kind: payload.kind,
        values: payload.kind === "values" ? payload.values : undefined, confirmed: payload.kind === "confirm" ? payload.confirmed : undefined,
      });
      run.record.pending_request = null;
      run.record.status = payload.kind === "cancel" ? "cancelling" : "running";
      const resolve = run.resolve;
      run.resolve = null;
      notify(run);
      setTimeout(() => resolve(payload), 150);
      return structuredClone(run.record);
    },
    cancel: async (runId, force) => {
      const run = runs.get(runId);
      if (!run) throw { code: "unknown_run", message: runId };
      if (!force && !run.record.cancel_allowed && !run.record.pending_request) {
        throw { code: "not_cancellable", message: `The script marked "${run.record.current_activity}" as unsafe to interrupt. Wait for it to finish.` };
      }
      run.record.cancel = { requested_at: new Date().toISOString(), forced: force };
      run.record.status = "cancelling";
      notify(run);
      if (run.resolve) {
        const resolve = run.resolve;
        run.resolve = null;
        resolve({ kind: "cancel" });
      } else {
        run.cancelled = true;
      }
      return structuredClone(run.record);
    },
    getActiveRuns: async () => [...runs.values()].map((r) => structuredClone(r.record)),
    getRunHistory: async () => [...[...runs.values()].map((r) => r.record), ...finished].map((r) => structuredClone(r)),
    getRunDetails: async (runId): Promise<RunDetails> => {
      const run = runStore.get(runId);
      if (!run) throw { code: "unknown_run", message: runId };
      return { record: structuredClone(run.record), events: [...run.events], logs: [...run.logs], logs_truncated: false };
    },
    deleteRun: async (runId) => {
      const index = finished.findIndex((r) => r.run_id === runId);
      if (index >= 0) finished.splice(index, 1);
      runStore.delete(runId);
    },
    browseDirectory: async (path): Promise<DirListing> => {
      const dir = path && fakeDirs[path.replace(/\/$/, "")] ? path.replace(/\/$/, "") : "/home/operator";
      return {
        path: dir,
        parent: dir.split("/").slice(0, -1).join("/") || "/",
        truncated: false,
        entries: fakeDirs[dir].map((name) => ({
          name: name.replace(/\/$/, ""), path: `${dir}/${name.replace(/\/$/, "")}`, kind: name.endsWith("/") ? "directory" : "file",
          size: name.endsWith("/") ? null : name.endsWith(".iso") ? 1_468_006_400 : 2_480, modified: new Date(Date.now() - 86400e3 * 3).toISOString(),
          symlink: false, hidden: name.startsWith("."),
        })),
        shortcuts: [
          { label: "Home", path: "/home/operator" }, { label: "BMAC repository", path: "/home/operator/src/bmac" },
          { label: "Downloads", path: "/home/operator/Downloads" }, { label: "Computer", path: "/" },
        ],
      };
    },
    exportRunLog: async (runId) => `/home/operator/Downloads/bmac-${runStore.get(runId)?.record.workflow_id ?? "run"}.log`,
    revealExported: async () => {},
    openExternal: async (url) => {
      window.open(url, "_blank", "noopener");
    },
    quit: async () => {},
    onRunEvents: (cb) => {
      eventListeners.add(cb);
      return () => eventListeners.delete(cb);
    },
    onRunUpdated: (cb) => {
      recordListeners.add(cb);
      return () => recordListeners.delete(cb);
    },
    onCloseRequested: () => () => {},
  };
}
