// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

import { describe, expect, it, vi } from "vitest";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { ReactNode } from "react";
import type { Answer, JobEvent, JobSummary, OperationView } from "../../generated";
import { JobActionsProvider, type JobActions } from "../../job/actions";
import { applyEvent, applyView, withSummary, type JobRecord } from "../../job/model";
import { TooltipProvider } from "../ui/tooltip";
import { JobPanel } from "./JobPanel";

const operation: OperationView = {
  allowed: true,
  id: "lab.interactive",
  title: "Lab: interactive mock",
  summary: "",
  description: "",
  category: "diagnostics",
  kind: "script",
  min_role: "operator",
  mutating: false,
  destructive: false,
  supports_dry_run: false,
  params: [{ id: "host", label: "Host", type: "host", required: true, advanced: false, choices: [] }],
  locks: [],
  target: { rule: "host", param: "host" },
  cancel: { rule: "script" },
  resume: "never",
} as unknown as OperationView;

const summary = (over: Partial<JobSummary> = {}): JobSummary => ({
  id: "j1",
  operation: "lab.interactive",
  title: "Lab: interactive mock",
  state: "RUNNING",
  host: "mox2",
  user: "alice@pve",
  dry_run: false,
  created_at_ms: Date.now(),
  updated_at_ms: Date.now(),
  cancel_requested: false,
  last_seq: 0,
  ...over,
});

let seq = 0;
const ev = (body: Record<string, unknown>): JobEvent => ({ job: "j1", seq: ++seq, at_ms: Date.now(), ...body }) as JobEvent;

function withRequest(request: Record<string, unknown>): JobRecord {
  seq = 0;
  const event = ev({ type: "request", request_id: "r1", ...request });
  const id = String(request.request_id ?? "r1");
  return applyView(null, { job: summary({ pending_request: id, last_seq: 1 }), parameters: { host: "mox2" }, locks: [], events: [event], more: false });
}

function setup(record: JobRecord, op: OperationView = operation) {
  const answers: Answer[] = [];
  const actions: JobActions = {
    operations: { [op.id]: op },
    answer: vi.fn(async (_job: string, _request: string, answer: Answer) => {
      answers.push(answer);
      return true;
    }),
    cancel: vi.fn(async () => true),
    resume: vi.fn(async () => true),
    reconcile: vi.fn(async () => true),
    openLaunch: vi.fn(),
    notify: vi.fn(),
  };
  const wrap = (r: JobRecord): ReactNode => (
    <JobActionsProvider value={actions}>
      <TooltipProvider>
        <JobPanel record={r} onClose={() => {}} />
      </TooltipProvider>
    </JobActionsProvider>
  );
  const view = render(wrap(record));
  return { actions, answers, rerender: (r: JobRecord) => view.rerender(wrap(r)) };
}

const field = (over: Record<string, unknown>) => ({ type: "string", required: true, sensitive: false, disabled: false, ...over });

describe("the job panel", () => {
  it("brings a rejected answer back with the script's errors and the values filled in", async () => {
    const record = withRequest({ request_id: "rejected", kind: "input_group", title: "Size the VM", fields: [field({ id: "cores", type: "integer", label: "Cores" })] });
    const { answers, rerender } = setup(record);
    const user = userEvent.setup();
    await user.type(screen.getByLabelText(/cores/i), "3");
    await user.click(screen.getByRole("button", { name: /continue/i }));
    expect(answers).toEqual([{ kind: "values", values: { cores: 3 } }]);
    let next = applyEvent(record, ev({ type: "answered", request_id: "rejected", by: "alice@pve", values: { cores: 3 } }));
    rerender(next);
    expect(screen.queryByLabelText(/cores/i)).toBeNull();
    next = applyEvent(next, ev({ type: "validation_error", request_id: "rejected", field_errors: { cores: "Use an even number of cores." } }));
    rerender(next);
    expect(screen.getByText("Use an even number of cores.")).toBeInTheDocument();
    expect(screen.getByLabelText(/cores/i)).toHaveValue("3");
  });

  it("offers resume and mark-failed for an interrupted job, after a check", async () => {
    seq = 0;
    const record = applyView(null, { job: summary({ state: "INTERRUPTED", message: "mox2 stopped" }), parameters: { host: "mox2" }, locks: ["host:mox2"], events: [], more: false });
    const { actions } = setup(record, { ...operation, resume: "checkpoint" } as OperationView);
    const user = userEvent.setup();
    expect(screen.queryByRole("button", { name: /run again/i })).toBeNull();
    await user.click(screen.getByRole("button", { name: /resume/i }));
    expect(actions.resume).toHaveBeenCalledWith("j1");
    await user.click(screen.getByRole("button", { name: /mark failed/i }));
    expect(actions.reconcile).not.toHaveBeenCalled();
    await user.click(within(screen.getByRole("dialog")).getByRole("button", { name: /i checked/i }));
    expect(actions.reconcile).toHaveBeenCalledWith("j1");
  });

  it("answers an input group with typed values and clears secrets once sent", async () => {
    const record = withRequest({
      kind: "input_group",
      title: "Size the VM",
      fields: [field({ id: "cores", type: "integer", label: "Cores", min: 1, max: 8 }), field({ id: "secret", type: "password", label: "Passphrase", sensitive: true })],
    });
    const { answers } = setup(record);
    expect(screen.getByText("Size the VM")).toBeInTheDocument();
    const user = userEvent.setup();
    await user.click(screen.getByRole("button", { name: /continue/i }));
    expect(answers).toEqual([]);
    expect(screen.getAllByText("Required.").length).toBe(2);
    await user.type(screen.getByLabelText(/cores/i), "4");
    await user.type(screen.getByLabelText(/passphrase/i), "hunter2");
    await user.click(screen.getByRole("button", { name: /continue/i }));
    expect(answers).toEqual([{ kind: "values", values: { cores: 4, secret: "hunter2" } }]);
    expect(screen.getByLabelText(/passphrase/i)).toHaveValue("");
  });

  it("keeps the form open with the server's corrections", async () => {
    let record = withRequest({ kind: "input", field: field({ id: "name", label: "Name" }) });
    const { answers, rerender } = setup(record);
    const user = userEvent.setup();
    await user.type(screen.getByLabelText(/name/i), "bad name");
    await user.click(screen.getByRole("button", { name: /continue/i }));
    expect(answers).toHaveLength(1);
    expect(screen.getByRole("button", { name: /sending/i })).toBeDisabled();
    record = applyEvent(record, ev({ type: "validation_error", request_id: "r1", field_errors: { name: "no spaces" }, message: "Fix the name" }));
    rerender(record);
    expect(screen.getByText("no spaces")).toBeInTheDocument();
    expect(screen.getByText("Fix the name")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /continue/i })).toBeEnabled();
    expect(screen.getByLabelText(/name/i)).toHaveValue("bad name");
  });

  it("requires a typed phrase for a destructive confirmation", async () => {
    const { answers } = setup(
      withRequest({ kind: "confirm", title: "Remove prod2?", message: "This deletes it", severity: "destructive", confirmation_text: "remove prod2" }),
    );
    const user = userEvent.setup();
    const confirm = screen.getByRole("button", { name: "Confirm" });
    expect(confirm).toBeDisabled();
    await user.type(screen.getByLabelText(/to confirm/i), "remove prod");
    expect(confirm).toBeDisabled();
    await user.type(screen.getByLabelText(/to confirm/i), "2");
    await user.click(confirm);
    expect(answers).toEqual([{ kind: "confirm", confirmed: true }]);
  });

  it("acknowledges a manual action and shows where to run it", async () => {
    const { answers } = setup(
      withRequest({ kind: "manual_action", title: "Run the formatter", instructions: ["Open a console", { text: "Run this", copy: "/root/format.sh", run_on: "mox3" }] }),
    );
    expect(screen.getByText(/run a script manually on mox3/i)).toBeInTheDocument();
    await userEvent.setup().click(screen.getByRole("button", { name: /i ran it manually/i }));
    expect(answers).toEqual([{ kind: "acknowledge" }]);
  });

  it("shows how a job ended and offers its next steps and a rerun", async () => {
    seq = 0;
    let record = applyView(null, { job: summary(), parameters: { host: "mox2" }, locks: [], events: [], more: false });
    record = applyEvent(record, ev({ type: "result", data: { vmid: 100, name: "prod9" } }));
    record = applyEvent(record, ev({ type: "next_step", text: "Check it", operation: "lab.interactive", args: { host: "mox3" } }));
    record = applyEvent(record, ev({ type: "completed" }));
    record = withSummary(record, summary({ state: "COMPLETED", last_seq: record.seq }));
    const { actions } = setup(record);
    expect(screen.getByText("Completed successfully")).toBeInTheDocument();
    expect(screen.getByText("prod9")).toBeInTheDocument();
    const user = userEvent.setup();
    await user.click(screen.getByRole("button", { name: /lab: interactive mock/i }));
    expect(actions.openLaunch).toHaveBeenCalledWith("lab.interactive", { host: "mox3" });
    await user.click(screen.getByRole("button", { name: /run again/i }));
    expect(actions.openLaunch).toHaveBeenCalledWith("lab.interactive", { host: "mox2" });
  });

  it("cancels a waiting job at once and asks first while it runs", async () => {
    const waiting = withRequest({ kind: "confirm", title: "Go?", message: "Sure?", severity: "normal" });
    const first = setup(waiting);
    const user = userEvent.setup();
    const footer = () => within(document.querySelector("footer")!);
    await user.click(footer().getByRole("button", { name: /^cancel$/i }));
    expect(first.actions.cancel).toHaveBeenCalledWith("j1", false);
    first.rerender(withSummary(waiting, summary({ last_seq: 2 })));
    expect(first.actions.cancel).toHaveBeenCalledTimes(1);
    await user.click(footer().getByRole("button", { name: /^cancel$/i }));
    expect(first.actions.cancel).toHaveBeenCalledTimes(1);
    await user.click(screen.getByRole("button", { name: /send interrupt/i }));
    expect(first.actions.cancel).toHaveBeenCalledTimes(2);
  });
});
