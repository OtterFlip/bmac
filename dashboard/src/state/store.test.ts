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
