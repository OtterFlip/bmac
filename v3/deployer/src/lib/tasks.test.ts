// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

import { describe, expect, it } from "vitest";
import { recordStatus, type TaskEvent, type TaskSummary } from "@bmac/ui";
import { WORKSTATION, foldEvent, foldSummary, foldView } from "./tasks";
import { parseLinkWait } from "../pages/SettingsPage";

const summary = (change: Partial<TaskSummary> = {}): TaskSummary => ({
  id: "t1",
  kind: "discovery",
  title: "Discovery",
  state: "RUNNING",
  created_at_ms: 1,
  updated_at_ms: 1,
  pending_request: null,
  message: null,
  cancel_requested: false,
  last_seq: 0,
  ...change,
});

const event = (seq: number, body: object) => ({ task: "t1", seq, at_ms: seq, ...body }) as TaskEvent;

describe("task records", () => {
  it("folds a task's history into the shared job record", () => {
    const record = foldView(null, {
      task: summary({ last_seq: 2, pending_request: "r1" }),
      parameters: { iso: "/tmp/d.iso" },
      events: [
        event(1, { type: "phase", id: "boot", label: "Boot", status: "running", cancel_allowed: true }),
        event(2, { type: "request", request_id: "r1", kind: "confirm", title: "Go?", message: "Sure?", severity: "normal" }),
      ],
    });
    expect(record.summary.host).toBe(WORKSTATION);
    expect(record.summary.operation).toBe("discovery");
    expect(record.summary.pending_request).toBe("r1");
    expect(record.parameters).toEqual({ iso: "/tmp/d.iso" });
    expect(record.phases.map((p) => p.label)).toEqual(["Boot"]);
    expect(recordStatus(record)).toBe("waiting_confirmation");
  });

  it("ignores repeated events and stale summaries", () => {
    let record = foldView(null, { task: summary(), parameters: {}, events: [] });
    const phase = event(1, { type: "phase", id: "boot", label: "Boot", status: "running", cancel_allowed: true });
    record = foldEvent(record, phase);
    expect(foldEvent(record, phase)).toBe(record);
    record = foldSummary(record, summary({ last_seq: 1, state: "COMPLETED" }));
    expect(record.summary.state).toBe("COMPLETED");
    expect(foldSummary(record, summary({ last_seq: 0 })).summary.state).toBe("COMPLETED");
  });
});

describe("link wait limits", () => {
  it("matches the Rust side's limits", () => {
    expect(parseLinkWait("10", "3")).toEqual({ ok: true, value: { attempts: 10, interval_seconds: 3 } });
    expect(parseLinkWait("101", "3").ok).toBe(false);
    expect(parseLinkWait("1.5", "3").ok).toBe(false);
    expect(parseLinkWait("5", "61").ok).toBe(false);
    expect(parseLinkWait("5", "")).toMatchObject({ ok: false, field: "interval" });
  });
});
