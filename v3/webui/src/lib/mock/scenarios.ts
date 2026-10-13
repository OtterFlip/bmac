// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// Scripts the mock plays for launched jobs, from v2's mock backend: a
// read-only check, a dry run, and mutating runs that ask for input,
// validate it, confirm, show progress, wait at the console, and pass a
// phase that can't be interrupted. They react to answers and cancels.

import type { Answer, LaunchJob, OperationView } from "@bmac/ui";
import type { MockBackend } from "./backend";

class Cancelled extends Error {}

/** Where a scenario's answers and cancels arrive. */
export interface ScenarioHooks {
  answer(request: string, answer: Answer): void;
  cancel(): void;
}

type Body = Record<string, unknown>;

export function runScenario(mock: MockBackend, jobId: string, op: OperationView, request: LaunchJob, host: string) {
  let cancelled = false;
  let waiting: { id: string; resolve: (a: Answer) => void } | null = null;
  let stepNo = 0;
  const phase: { running: { id: string; label: string } | null } = { running: null };

  const emit = (body: Body) => {
    if (cancelled) throw new Cancelled();
    mock.addEvent(jobId, body);
    mock.pushSummary(jobId);
  };
  const sleep = async (ms: number) => {
    await new Promise((r) => setTimeout(r, ms));
    while (mock.holding(jobId) && !cancelled) await new Promise((r) => setTimeout(r, 50));
    if (cancelled) throw new Cancelled();
  };
  const log = async (lines: string[], gap = 25) => {
    for (const text of lines) {
      const level = /^\s*(ERROR|FAIL)/.test(text) ? "error" : /^\s*(WARNING|ATTENTION)/.test(text) ? "warning" : "info";
      emit({ type: "output", stream: "stdout", level, text });
      if (gap) await sleep(gap);
    }
  };
  const ask = (body: Body) =>
    new Promise<Answer>((resolve) => {
      waiting = { id: String(body.request_id), resolve };
      emit({ type: "request", ...body });
    });
  const step = (label: string) => {
    if (phase.running) emit({ type: "phase", ...phase.running, status: "complete" });
    stepNo += 1;
    phase.running = { id: `step-${stepNo}`, label };
    emit({ type: "phase", ...phase.running, status: "running" });
  };

  const hooks: ScenarioHooks = {
    answer(id, answer) {
      if (waiting?.id !== id) return;
      const { resolve } = waiting;
      waiting = null;
      mock.addEvent(jobId, { type: "answered", request_id: id, by: mock.session()?.user ?? "alice@pve", ...(answer.kind === "values" ? { values: answer.values } : {}) });
      mock.pushSummary(jobId);
      resolve(answer);
    },
    cancel() {
      cancelled = true;
      if (waiting) {
        const { resolve } = waiting;
        waiting = null;
        resolve({ kind: "cancel" });
      }
    },
  };
  mock.scenarios.set(jobId, hooks);

  const target = typeof request.parameters.resource === "string" ? request.parameters.resource : "prod2";

  async function play() {
    if (!op.mutating) {
      for (const section of ["Cluster membership", "Corosync links", "ZFS pools", "SMART health"]) {
        step(section);
        await log([`== ${section} ==`, ...Array.from({ length: 14 }, (_, i) => `  mox${(i % 3) + 1}  ${section.toLowerCase()} check ${i + 1}: ok`)], 40);
      }
      await log(["", "WARNING: mox2 rpool has 15% free", "", "Next steps:", "  - Inspect mox2's pool members and disk serials."], 30);
      emit({ type: "warning", message: "mox2 rpool has 15% free" });
      emit({ type: "next_step", text: "Inspect mox2's pool members and disk serials.", operation: "diagnostics.host_state", args: { host: "mox2" } });
      return;
    }

    step("Validate the cluster");
    await log(["==> Loading the cluster settings", "==> Checking the cluster", "    3 of 3 hosts online, quorate"], 120);

    let source = "prod1";
    if (op.id === "staging.add" && !request.dry_run) {
      const answer = await ask({
        request_id: "req-1-source",
        kind: "input",
        title: "Source production VM",
        context: "Production VMs that can be cloned:\n  prod1  mox1  running  placement mox1,mox2\n  prod2  mox2  running  placement mox2,mox3",
        field: {
          id: "source",
          type: "select",
          label: "Clone from",
          required: true,
          sensitive: false,
          disabled: false,
          options: [
            { value: "prod1", label: "prod1 — shop.example.com", help: "Standby on mox2" },
            { value: "prod2", label: "prod2 — blog.example.com", help: "Standby on mox3" },
          ],
        },
      });
      if (answer.kind !== "values") throw new Cancelled();
      source = String(answer.values.source ?? "prod1");
      if (source === "prod1") {
        step("Check existing staging VMs");
        await log(
          [
            "Existing staging VMs based on prod1:",
            "  #   SNAPSHOT                                 TAKEN                    STAGING VMS",
            "  1   bmac-staging-prod1-261011T081500Z        2026-10-11 08:15:00 UTC  stage1prod1",
          ],
          40,
        );
        const snapshot = await ask({
          request_id: "req-1b-snapshot",
          kind: "input",
          title: "Snapshot for the new staging VM",
          field: {
            id: "choice",
            type: "select",
            label: "Snapshot for the new staging VM",
            required: true,
            sensitive: false,
            disabled: false,
            default: "new",
            options: [
              { value: "new", label: "Take a new snapshot of prod1 now" },
              { value: "1", label: "Share the snapshot of stage1prod1 (bmac-staging-prod1-261011T081500Z, taken 2026-10-11 08:15:00 UTC)" },
            ],
          },
        });
        if (snapshot.kind !== "values") throw new Cancelled();
      }
      step("Choose resources");
      const group = {
        request_id: "req-2-resources",
        kind: "input_group",
        title: "Staging VM resources",
        layout: "form",
        description: `The clone starts with ${source}'s configuration; these override it.`,
        fields: [
          { id: "cores", type: "integer", label: "vCPU cores", default: 4, min: 1, max: 32, required: true, sensitive: false, disabled: false },
          { id: "memory_gib", type: "integer", label: "Memory", suffix: "GiB", default: 16, min: 2, max: 256, required: true, sensitive: false, disabled: false },
          { id: "start", type: "boolean", label: "Start the VM after creating it", default: true, required: false, sensitive: false, disabled: false },
          { id: "link_down", type: "boolean", label: "Keep its network link down", help: "Start isolated from the network until you have checked it.", default: false, required: false, sensitive: false, disabled: false },
        ],
      };
      for (;;) {
        const answer = await ask(group);
        if (answer.kind !== "values") throw new Cancelled();
        if (answer.values === null || typeof answer.values !== "object") throw new Error("the answer had no values");
        if (Number(answer.values.memory_gib) > 64) {
          emit({ type: "validation_error", request_id: group.request_id, field_errors: { memory_gib: "mox3 has only 64 GiB free for staging." } });
          continue;
        }
        break;
      }
    }

    step("Build the plan");
    await sleep(500);
    emit({
      type: "plan",
      title: `${op.title}${request.parameters.resource ? `: ${target}` : ""}`,
      description: request.dry_run ? "Dry run: nothing was changed." : "These are the changes the operation will make.",
      dry_run: request.dry_run,
      items: op.destructive
        ? [
            { kind: "remove", target: "HAProxy routes", description: `Unpublish ${target}'s routes` },
            { kind: "remove", target: "VM", description: `Destroy ${target} and its volumes` },
            { kind: "update", target: "BMAC registry", description: `Release the ${target} allocation` },
          ]
        : [
            { kind: "check", target: "cluster", description: "3 of 3 hosts online, quorate" },
            { kind: "create", target: "VM 201", description: "Linked clone on mox3" },
            { kind: "update", target: "BMAC registry", description: "Reserve stage2prod1" },
            { kind: "keep", target: "production", description: "Production is never modified" },
          ],
    });
    if (request.dry_run) {
      await log(["", "Dry run complete. Nothing was changed."], 30);
      emit({ type: "next_step", text: "Run it again without dry run to apply this plan.", operation: op.id, args: Object.fromEntries(Object.entries(request.parameters).map(([k, v]) => [k, String(v)])) });
      return;
    }

    const confirm = await ask({
      request_id: "req-3-confirm",
      kind: "confirm",
      title: op.destructive ? `Permanently destroy ${target}?` : `${op.title}?`,
      message: op.destructive ? `This removes ${target}, its replicas, routes, and registry allocation. It cannot be undone.` : "The operation is ready to apply this plan.",
      severity: op.destructive ? "destructive" : "normal",
      confirm_label: op.destructive ? `Destroy ${target}` : "Apply",
      ...(op.destructive ? { confirmation_text: `DESTROY ${target}` } : {}),
    });
    if (confirm.kind !== "confirm" || !confirm.confirmed) throw new Cancelled();

    step("Apply changes");
    for (let i = 1; i <= 12; i += 1) {
      emit({ type: "progress", message: "Replicating snapshot to mox3", current: i * 512 * 1024 ** 2, total: 6 * 1024 ** 3, unit: "bytes" });
      await log([`  zfs send: ${(i * 0.5).toFixed(1)} GiB of 6.0 GiB`], 250);
    }
    if (op.category === "hosts" || op.category === "storage") {
      const ack = await ask({
        request_id: "req-4-console",
        kind: "manual_action",
        title: "Unlock the new disks at the console",
        instructions: [
          `Open the iDRAC virtual console for ${host}.`,
          { text: "Log in as root and run:", copy: "bmac luks-enter", run_on: host },
          "Type the LUKS passphrase for the new disks when asked, twice.",
          "Return here once both disks show as unlocked.",
        ],
      });
      if (ack.kind !== "acknowledge") throw new Cancelled();
    }
    step("Verify");
    emit({ type: "phase", id: "commit", label: "Commit registry", status: "running", cancel_allowed: false });
    await log(["==> Committing the registry (do not interrupt)"], 1800);
    emit({ type: "phase", id: "commit", label: "Commit registry", status: "complete" });
    await log(["    verified", "", "Done."], 80);
    emit({ type: "result", data: op.destructive ? { removed: target } : { name: "stage2prod1", vmid: 201, node: "mox3", ip: "10.20.0.202" } });
    if (op.category === "staging" && !op.destructive) emit({ type: "next_step", text: "Set up SSH to the new VM in the Deployer's Workstation SSH page." });
  }

  void (async () => {
    try {
      await sleep(200);
      emit({ type: "started", operation: op.id, host, user: mock.session()?.user ?? "alice@pve", dry_run: request.dry_run });
      await play();
      if (phase.running) emit({ type: "phase", ...phase.running, status: "complete" });
      emit({ type: "completed", message: request.dry_run ? "Dry run finished; nothing changed." : undefined });
    } catch (error) {
      cancelled = false;
      if (phase.running) emit({ type: "phase", ...phase.running, status: error instanceof Cancelled ? "skipped" : "failed" });
      if (error instanceof Cancelled) {
        await log(["ERROR: Cancelled by the operator."], 0);
        emit({ type: "cancelled", by: mock.session()?.user ?? "alice@pve", message: "Cancelled by the operator." });
      } else {
        emit({ type: "failed", message: error instanceof Error ? error.message : String(error) });
      }
    } finally {
      mock.scenarios.delete(jobId);
    }
  })();
}
