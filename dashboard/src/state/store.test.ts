import { beforeEach, describe, expect, it } from "vitest";
import { useStore } from "./store";

describe("run selection", () => {
  beforeEach(() => useStore.setState({ focusedRunId: "a", panelOpen: true, consoleRunId: "b", consolePinned: false }));

  it("switches the workflow panel without moving the console", () => {
    useStore.getState().showInPanel("c");
    expect(useStore.getState()).toMatchObject({ focusedRunId: "c", panelOpen: true, consoleRunId: "b" });
  });

  it("switches and pins the console without moving the panel", () => {
    useStore.getState().showInConsole("c");
    expect(useStore.getState()).toMatchObject({ focusedRunId: "a", consoleRunId: "c", consolePinned: true });
  });
});

describe("cluster state sources", () => {
  const stale = { collected_at: 1, probe: "mox1", cluster: {}, qdevice: {}, hosts: [{ name: "mox1" }], problems: [] } as never;

  beforeEach(() =>
    useStore.setState((s) => ({
      workflows: { ...s.workflows, list_hosts: { id: "list_hosts", available: false, unavailable_reason: "No reachable mox jump host was found" } as never },
      sources: { ...s.sources, list_hosts: { data: stale, updatedAt: Date.now() - 600_000, status: "ok", error: null, runId: "old", nextSteps: [] } },
    })),
  );

  it("drops the previous data when a read fails, so stale state is never shown as current", async () => {
    await useStore.getState().refreshSource("list_hosts");
    expect(useStore.getState().sources.list_hosts).toMatchObject({
      data: null,
      updatedAt: null,
      status: "error",
      error: "No reachable mox jump host was found",
    });
  });
});
