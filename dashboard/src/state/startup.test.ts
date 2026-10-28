import { describe, expect, it } from "vitest";
import { useStore } from "./store";
import { setBackend } from "@/lib/api";
import { createMockBackend } from "@/lib/mock/backend";
import { STARTUP_SOURCE_IDS } from "@/protocol/state";

describe("startup", () => {
  it("reads backups, replication, guests, disks, storage, and hosts together as soon as it is ready", async () => {
    const backend = createMockBackend();
    const started: string[] = [];
    const start = backend.startWorkflow;
    backend.startWorkflow = (id, values) => {
      started.push(id);
      return start(id, values);
    };
    setBackend(backend);
    await useStore.getState().bootstrap();
    // All six are under way at once, before any of them has finished.
    expect([...started].sort()).toEqual([...STARTUP_SOURCE_IDS].sort());
    for (const id of STARTUP_SOURCE_IDS) expect(useStore.getState().sources[id].status).toBe("loading");
  });

  it("reads them too when it opens on Settings as a first launch with a complete configuration", async () => {
    const backend = createMockBackend();
    const appInfo = backend.getAppInfo;
    backend.getAppInfo = async () => ({ ...(await appInfo()), first_launch: true });
    const started: string[] = [];
    const start = backend.startWorkflow;
    backend.startWorkflow = (id, values) => {
      started.push(id);
      return start(id, values);
    };
    useStore.setState((s) => ({
      page: "dashboard",
      sources: Object.fromEntries(Object.entries(s.sources).map(([id, entry]) => [id, { ...entry, status: "idle" }])) as never,
    }));
    setBackend(backend);
    await useStore.getState().bootstrap();
    expect(useStore.getState().page).toBe("settings");
    expect([...started].sort()).toEqual([...STARTUP_SOURCE_IDS].sort());
  });
});
