// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

import { describe, expect, it } from "vitest";
import type { JobEvent, JobSummary, JobView } from "../generated";
import { applyEvent, applyView, jobStatus, newRecord, recordStatus, withSummary } from "./model";

const summary = (over: Partial<JobSummary> = {}): JobSummary => ({
  id: "j1",
  operation: "lab.interactive",
  title: "Lab: interactive mock",
  state: "RUNNING",
  host: "mox2",
  user: "alice@pve",
  dry_run: false,
  created_at_ms: 1000,
  updated_at_ms: 1000,
  cancel_requested: false,
  last_seq: 0,
  ...over,
});

let seq = 0;
const ev = (body: Record<string, unknown>): JobEvent => ({ job: "j1", seq: ++seq, at_ms: 1000 + seq, ...body }) as JobEvent;
const view = (job: JobSummary, events: JobEvent[]): JobView => ({ job, parameters: { host: "mox2" }, locks: ["host:mox2"], events, more: false });

const confirm = { type: "request", request_id: "r2", kind: "confirm", title: "Go?", message: "Really", severity: "warning" };
const group = {
  type: "request",
  request_id: "r1",
  kind: "input_group",
  title: "Size",
  fields: [{ id: "cores", type: "integer", label: "Cores", required: true, sensitive: false, disabled: false }],
};

describe("the job model", () => {
  it("folds phases, plan, progress, output, result, and next steps", () => {
    seq = 0;
    let r = newRecord(view(summary(), []));
    for (const e of [
      ev({ type: "started", operation: "lab.interactive", host: "mox2", user: "alice@pve", dry_run: false }),
      ev({ type: "phase", id: "check", label: "Checking", status: "running" }),
      ev({ type: "output", level: "info", text: "Checking the cluster" }),
      ev({ type: "phase", id: "check", label: "Checking", status: "complete", cancel_allowed: false }),
      ev({ type: "plan", title: "Plan", items: [{ kind: "create", target: "prod9", description: "new VM" }], dry_run: false }),
      ev({ type: "progress", message: "copying", current: 1, total: 4 }),
      ev({ type: "warning", message: "slow disk" }),
      ev({ type: "result", data: { vmid: 100 } }),
      ev({ type: "next_step", text: "Open it", operation: "prod.open", args: { name: "prod9" } }),
    ])
      r = applyEvent(r, e);
    expect(r.phases).toEqual([{ id: "check", label: "Checking", status: "complete", cancel_allowed: false, started_at_ms: 1002, ended_at_ms: 1004 }]);
    expect(r.plan?.items[0].target).toBe("prod9");
    expect(r.progress?.total).toBe(4);
    expect(r.log_lines).toBe(1);
    expect(r.last_log).toBe("Checking the cluster");
    expect(r.events.some((e) => e.type === "output")).toBe(false);
    expect(r.warnings).toEqual(["slow disk"]);
    expect(r.result).toEqual({ vmid: 100 });
    expect(r.next_steps).toEqual([{ text: "Open it", operation: "prod.open", args: { name: "prod9" } }]);
    expect(r.seq).toBe(9);
  });

  it("ignores events it has already folded", () => {
    seq = 0;
    const e = ev({ type: "warning", message: "once" });
    const r = applyEvent(applyEvent(newRecord(view(summary(), [])), e), e);
    expect(r.warnings).toEqual(["once"]);
  });

  it("reopens a request the script rejected, with its errors, until an answer is accepted", () => {
    seq = 0;
    let r = applyView(null, view(summary({ pending_request: "r1", last_seq: 1 }), [ev(group)]));
    expect(recordStatus(r)).toBe("waiting_input");
    r = applyEvent(r, ev({ type: "answered", request_id: "r1", by: "alice@pve", values: { cores: 3 } }));
    expect(r.pending_request).toBeNull();
    r = applyEvent(r, ev({ type: "validation_error", request_id: "r1", field_errors: { cores: "too many" }, message: "fix it" }));
    expect(r.pending_request?.request_id).toBe("r1");
    expect(r.field_errors).toEqual({ cores: "too many" });
    expect(r.validation_message).toBe("fix it");
    expect(r.responses).toEqual([]);
    r = applyEvent(r, ev({ type: "answered", request_id: "r1", by: "alice@pve", values: { cores: 4 } }));
    expect(r.pending_request).toBeNull();
    expect(r.field_errors).toEqual({});
    expect(r.responses).toEqual([{ request_id: "r1", at_ms: 1004, title: "Size", by: "alice@pve", values: { cores: 4 } }]);
    r = applyEvent(r, ev(confirm));
    r = withSummary(r, summary({ pending_request: "r2", last_seq: r.seq }));
    expect(recordStatus(r)).toBe("waiting_confirmation");
  });

  it("drops a request the server no longer has pending, and ignores stale summaries", () => {
    seq = 0;
    let r = applyView(null, view(summary({ pending_request: "r2", last_seq: 1 }), [ev(confirm)]));
    expect(withSummary(r, summary({ last_seq: 0 })).pending_request?.request_id).toBe("r2");
    r = withSummary(r, summary({ last_seq: 2, updated_at_ms: 2000 }));
    expect(r.pending_request).toBeNull();
    expect(recordStatus(r)).toBe("running");
  });

  it("records how a job ended", () => {
    seq = 0;
    let r = applyView(null, view(summary({ pending_request: "r2", last_seq: 1 }), [ev(confirm)]));
    r = applyEvent(r, ev({ type: "failed", message: "boom" }));
    r = withSummary(r, summary({ state: "FAILED", last_seq: 2, updated_at_ms: 5000 }));
    expect(r.pending_request).toBeNull();
    expect(r.errors).toEqual(["boom"]);
    expect(r.ended_at_ms).toBe(1002);
    expect(recordStatus(r)).toBe("failed");
  });

  it("maps job states to the view's statuses", () => {
    expect(jobStatus(summary({ state: "ADMITTED" }))).toBe("starting");
    expect(jobStatus(summary({ cancel_requested: true }))).toBe("cancelling");
    expect(jobStatus(summary())).toBe("running");
    expect(jobStatus(summary({ pending_request: "r9" }))).toBe("waiting_input");
    expect(jobStatus(summary({ state: "COMPLETED" }))).toBe("succeeded");
    expect(jobStatus(summary({ state: "CANCELLED" }))).toBe("cancelled");
    expect(jobStatus(summary({ state: "INTERRUPTED" }))).toBe("interrupted");
  });
});
