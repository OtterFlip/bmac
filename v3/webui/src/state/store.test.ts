// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

import { beforeEach, describe, expect, it } from "vitest";
import { logs, recordStatus, type OperationView } from "@bmac/ui";
import { ApiFailure } from "../lib/backend";
import { MockBackend } from "../lib/mock/backend";
import { STATUS_DEBOUNCE_MS, setBackend, useStore } from "./store";

const operation = { id: "lab.interactive", title: "Lab: interactive mock", allowed: true, params: [] } as unknown as OperationView;

let mock: MockBackend;

const settle = async () => {
  for (let i = 0; i < 20; i++) await new Promise((r) => setTimeout(r, 0));
};

async function loggedIn() {
  await mock.login({ step: "password", user: "alice@pve", password: "alice" });
  await mock.login({ step: "second_factor", challenge: "mock-challenge", method: "totp", response: "123456" });
  await useStore.getState().begin();
  await settle();
}

beforeEach(() => {
  mock = new MockBackend();
  mock.operationList = [operation];
  setBackend(mock);
});

describe("the store", () => {
  it("loads the snapshots after login and opens the stream", async () => {
    const running = mock.addJob();
    await loggedIn();
    const s = useStore.getState();
    expect(s.phase).toBe("ready");
    expect(s.session?.user).toBe("alice@pve");
    expect(Object.keys(s.operations)).toEqual(["lab.interactive"]);
    expect(s.jobs[running.id].summary.state).toBe("RUNNING");
    expect(s.consoleJobId).toBe(running.id);
    expect(s.stream).toBe("open");
  });

  it("watches the job in the panel and replays what the stream missed", async () => {
    const job = mock.addJob();
    mock.addEvent(job.id, { type: "started", operation: job.operation, host: "mox2", user: "alice@pve", dry_run: false });
    await loggedIn();
    useStore.getState().focusJob(job.id);
    await settle();
    expect(mock.watched).toEqual([job.id]);
    expect(useStore.getState().jobs[job.id].seq).toBe(1);

    mock.addEvent(job.id, { type: "output", level: "info", text: "line two" });
    await settle();
    expect(logs.get(job.id).map((l) => l.text)).toContain("line two");

    // The stream drops two events (as across a reconnect); the next one shows the gap.
    mock.addEvent(job.id, { type: "output", level: "info", text: "missed one" }, false);
    mock.addEvent(job.id, { type: "request", request_id: "r1", kind: "confirm", title: "Go?", message: "Sure?", severity: "normal" }, false);
    mock.addEvent(job.id, { type: "output", level: "info", text: "after the gap" });
    await settle();
    const record = useStore.getState().jobs[job.id];
    expect(record.seq).toBe(5);
    expect(logs.get(job.id).map((l) => l.text)).toEqual(expect.arrayContaining(["missed one", "after the gap"]));
    mock.pushSummary(job.id);
    await settle();
    expect(recordStatus(useStore.getState().jobs[job.id])).toBe("waiting_confirmation");
  });

  it("fetches fresh snapshots when told its copy is too old", async () => {
    const job = mock.addJob();
    await loggedIn();
    useStore.getState().focusJob(job.id);
    await settle();
    mock.addEvent(job.id, { type: "warning", message: "while away" }, false);
    const other = { ...mock.addJob(), state: "COMPLETED" as const };
    mock.jobsById.get(other.id)!.summary = other;
    const before = mock.calls.filter((c) => c.method === "jobs").length;
    mock.push({ type: "resnapshot", reason: "the gap is too old to replay" });
    await settle();
    expect(mock.calls.filter((c) => c.method === "jobs").length).toBe(before + 1);
    expect(useStore.getState().jobs[job.id].warnings).toEqual(["while away"]);
    expect(useStore.getState().jobs[other.id].summary.state).toBe("COMPLETED");
  });

  it("returns to the login page with nothing kept when the session expires", async () => {
    const job = mock.addJob();
    await loggedIn();
    useStore.getState().focusJob(job.id);
    await settle();
    mock.addEvent(job.id, { type: "output", level: "info", text: "secret-ish output" });
    await settle();
    expect(logs.get(job.id)).toHaveLength(1);
    mock.endSession("Your session expired.");
    await settle();
    const s = useStore.getState();
    expect(s.phase).toBe("login");
    expect(s.loginNotice).toBe("Your session expired.");
    expect(s.session).toBeNull();
    expect(s.jobs).toEqual({});
    expect(s.focusedJobId).toBeNull();
    expect(logs.get(job.id)).toEqual([]);
  });

  it("launches with the dialog's idempotency key and answers requests", async () => {
    await loggedIn();
    useStore.getState().openLaunch("lab.interactive", { host: "mox2" });
    const key = useStore.getState().launch!.idempotencyKey;
    const summary = await useStore.getState().launchJob("lab.interactive", { host: "mox2" }, false);
    expect(summary?.id).toBe(`key-${key}`);
    const s = useStore.getState();
    expect(s.launch).toBeNull();
    expect(s.focusedJobId).toBe(summary!.id);
    expect(s.panelOpen).toBe(true);
    expect(await s.answer(summary!.id, "r1", { kind: "acknowledge" })).toBe(true);
    expect(mock.calls.find((c) => c.method === "answer")?.args).toEqual([summary!.id, "r1", { kind: "acknowledge" }]);
  });

  it("starts read-only operations without options at once and opens the dialog for the rest", async () => {
    const check = { ...operation, id: "lab.check", title: "Lab: check", mutating: false } as OperationView;
    const change = { ...operation, id: "lab.change", title: "Lab: change", mutating: true } as OperationView;
    mock.operationList = [check, change];
    await loggedIn();
    useStore.getState().startOperation("lab.check");
    await settle();
    expect(useStore.getState().launch).toBeNull();
    expect(mock.calls.find((c) => c.method === "launch")?.args[0]).toMatchObject({ operation: "lab.check", parameters: {}, dry_run: false });
    useStore.getState().startOperation("lab.change", { host: "mox1" });
    expect(useStore.getState().launch).toMatchObject({ operation: "lab.change", values: { host: "mox1" } });
    expect(mock.calls.filter((c) => c.method === "launch")).toHaveLength(1);
  });

  it("logs out and forgets the session", async () => {
    await loggedIn();
    await useStore.getState().logout();
    expect(useStore.getState().phase).toBe("login");
    expect(useStore.getState().loginNotice).toBeNull();
    expect(mock.session()).toBeNull();
  });
});

describe("status snapshots", () => {
  const fetched = (kind: string) => mock.calls.filter((c) => c.method === kind).length;
  const debounce = () => new Promise((r) => setTimeout(r, STATUS_DEBOUNCE_MS + 100));

  it("are fetched once wanted, and give the launch dialog its names", async () => {
    await loggedIn();
    expect(fetched("hosts")).toBe(0);
    const release = useStore.getState().wantStatus("hosts");
    await settle();
    expect(fetched("hosts")).toBe(1);
    expect(useStore.getState().status.hosts.data?.hosts.map((h) => h.name)).toEqual(["mox1", "mox2", "mox3"]);
    expect(useStore.getState().known.host.map((n) => n.value)).toEqual(["mox1", "mox2", "mox3"]);
    release();
  });

  it("fetch a burst of changes once", async () => {
    await loggedIn();
    const release = useStore.getState().wantStatus("hosts");
    await settle();
    for (const host of ["mox1", "mox2", "mox3"]) mock.push({ type: "observation", host, collector: "host", data: {} });
    await debounce();
    expect(fetched("hosts")).toBe(2);
    release();
  });

  it("wait to be shown before fetching a change", async () => {
    await loggedIn();
    useStore.getState().wantStatus("storage")();
    await settle();
    mock.push({ type: "observation", host: "mox2", collector: "storage", data: {} });
    await debounce();
    expect(fetched("storage")).toBe(1);
    const release = useStore.getState().wantStatus("storage");
    await settle();
    expect(fetched("storage")).toBe(2);
    release();
  });

  it("are fetched again after a job that changes the cluster succeeds", async () => {
    const change = { ...operation, id: "lab.change", mutating: true } as OperationView;
    mock.operationList = [change];
    const job = mock.addJob({ operation: "lab.change" });
    await loggedIn();
    const release = useStore.getState().wantStatus("vms");
    await settle();
    mock.addEvent(job.id, { type: "completed" });
    mock.pushSummary(job.id);
    await debounce();
    expect(fetched("vms")).toBe(2);
    release();
  });

  it("keep the last snapshot and say why a fetch failed", async () => {
    await loggedIn();
    const release = useStore.getState().wantStatus("cluster");
    await settle();
    mock.statusFailures.cluster = new ApiFailure(503, "unavailable", "no status here");
    await useStore.getState().refreshStatus(["cluster"]);
    const entry = useStore.getState().status.cluster;
    expect(entry.error).toBe("no status here");
    expect(entry.data?.cluster.name).toBe("bmac");
    release();
  });

  it("are dropped at logout, with any fetch still running", async () => {
    await loggedIn();
    const release = useStore.getState().wantStatus("hosts");
    await useStore.getState().logout();
    release();
    await settle();
    expect(useStore.getState().status.hosts.data).toBeNull();
  });
});
