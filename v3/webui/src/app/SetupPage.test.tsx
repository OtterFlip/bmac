// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// Setup mode (DEPLOYMENT.md, "Host setup job"): after logging in to a host
// that is still being set up, the Dashboard shows only the setup job, and
// the rest of it once the job has finished.

import { beforeEach, describe, expect, it } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { OperationView } from "@bmac/ui";
import { App } from "./App";
import { MockBackend } from "../lib/mock/backend";
import { CLUSTER_OPERATIONS } from "../lib/mock/demo";
import { setBackend } from "../state/store";

const SETUP = {
  allowed: true,
  id: "hosts.setup",
  title: "Set up this host",
  summary: "Finish a new host's setup after the Proxmox install.",
  description: "d",
  category: "hosts",
  kind: "native",
  min_role: "admin",
  mutating: true,
  destructive: true,
  supports_dry_run: false,
  params: [],
  locks: ["cluster-membership"],
  target: { rule: "any" },
  cancel: { rule: "phases", not_during: ["storage", "cluster"] },
  resume: "checkpoint",
} as unknown as OperationView;

let mock: MockBackend;

beforeEach(() => {
  history.replaceState(null, "", "/");
  mock = new MockBackend();
  mock.operationList = [SETUP];
  mock.setupView = { active: true, operation: "hosts.setup", host: "mox1", new_cluster: true, cluster_name: "bmac-lab" };
});

async function login() {
  setBackend(mock);
  const user = userEvent.setup();
  render(<App />);
  await user.type(await screen.findByLabelText(/Proxmox user/), "alice@pve");
  await user.type(screen.getByLabelText(/Password/), "alice");
  await user.click(screen.getByRole("button", { name: /Log in/ }));
  await user.type(await screen.findByLabelText(/authenticator app/), "123456");
  await user.click(screen.getByRole("button", { name: /Continue/ }));
  return user;
}

describe("setup mode", () => {
  it("on a joining host, says where to approve it and offers no start", async () => {
    mock.setupView = {
      active: true,
      operation: "hosts.setup",
      host: "mox4",
      new_cluster: false,
      cluster_name: "bmac-lab",
      join: { state: "waiting", host: "mox1", dashboard_url: "https://mox1.example.ts.net:8443", request_id: "req1", updated_at_ms: 1 },
    };
    await login();
    expect(await screen.findByRole("heading", { name: "Set up mox4" })).toBeInTheDocument();
    expect(screen.getByText("Waiting for an administrator to approve this host on mox1.")).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "https://mox1.example.ts.net:8443" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /Start setup/ })).toBeNull();
  });

  it("on a joining host being set up, says from where", async () => {
    mock.setupView = {
      active: true,
      operation: "hosts.setup",
      host: "mox4",
      new_cluster: false,
      cluster_name: "bmac-lab",
      join: { state: "waiting", host: "mox1", message: "Being set up from mox1.", updated_at_ms: 1 },
    };
    await login();
    expect(await screen.findByText("Being set up from mox1.")).toBeInTheDocument();
  });

  it("shows only the setup page and starts the setup job", async () => {
    const user = await login();
    expect(await screen.findByRole("heading", { name: "Set up mox1" })).toBeInTheDocument();
    expect(screen.getByText(/creates the cluster bmac-lab/)).toBeInTheDocument();
    expect(screen.queryByRole("navigation")).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: /Start setup/ }));
    await waitFor(() => expect(mock.calls.some((c) => c.method === "launch" && (c.args[0] as { operation: string }).operation === "hosts.setup")).toBe(true));
    expect(await screen.findByRole("complementary", { name: "Job panel" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /Start setup/ })).not.toBeInTheDocument();
  });

  it("opens a setup job that is already running", async () => {
    const job = mock.addJob({ operation: "hosts.setup", title: "Set up this host", host: "mox1" });
    await login();
    expect(await screen.findByRole("complementary", { name: "Job panel" })).toBeInTheDocument();
    expect(screen.getByText("Setup is running.")).toBeInTheDocument();
    expect(mock.calls.some((c) => c.method === "job" && c.args[0] === job.id)).toBe(true);
  });

  it("shows the whole Dashboard once setup has finished", async () => {
    const job = mock.addJob({ operation: "hosts.setup", title: "Set up this host", host: "mox1" });
    await login();
    await screen.findByRole("heading", { name: "Set up mox1" });
    mock.setupView = { ...mock.setupView, active: false };
    mock.operationList = CLUSTER_OPERATIONS;
    mock.addEvent(job.id, { type: "completed", status: "success" });
    mock.pushSummary(job.id);
    expect(await screen.findByRole("navigation")).toBeInTheDocument();
    expect(screen.queryByRole("heading", { name: "Set up mox1" })).not.toBeInTheDocument();
  });
});
