// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// Each status page against the mock backend: it shows what the snapshots
// say, offers the operations for what it shows, and follows changes the
// event stream announces.

import { beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { App } from "../app/App";
import { ApiFailure } from "../lib/backend";
import { MockBackend } from "../lib/mock/backend";
import { CLUSTER_OPERATIONS } from "../lib/mock/demo";
import { setBackend } from "../state/store";

type User = ReturnType<typeof userEvent.setup>;

let mock: MockBackend;

beforeEach(() => {
  history.replaceState(null, "", "/");
  mock = new MockBackend();
  mock.operationList = CLUSTER_OPERATIONS;
});

async function open(page: string, role: "admin" | "viewer" = "admin"): Promise<User> {
  mock.users["alice@pve"].role = role;
  history.replaceState(null, "", page);
  setBackend(mock);
  const user = userEvent.setup();
  render(<App />);
  await user.type(await screen.findByLabelText(/Proxmox user/), "alice@pve");
  await user.type(screen.getByLabelText(/Password/), "alice");
  await user.click(screen.getByRole("button", { name: /Log in/ }));
  await user.type(await screen.findByLabelText(/authenticator app/), "123456");
  await user.click(screen.getByRole("button", { name: /Continue/ }));
  await screen.findByRole("button", { name: "Log out" });
  return user;
}

const calls = (method: string) => mock.calls.filter((c) => c.method === method).length;

describe("the Overview page", () => {
  it("shows quorum, hosts, VMs, and what needs attention", async () => {
    await open("/");
    expect(await screen.findByText("Quorate")).toBeInTheDocument();
    expect(screen.getByText("3 of 3 votes · needs 2")).toBeInTheDocument();
    expect(screen.getByText("duty host mox1")).toBeInTheDocument();
    expect(screen.getByText("1 failing")).toBeInTheDocument();
    const attention = screen.getByRole("region", { name: "Needs attention" });
    expect(within(attention).getByText(/102-1 from mox2 to mox3 is failing/)).toBeInTheDocument();
    expect(within(attention).getByText(/mox2\/rpool has 15% free/)).toBeInTheDocument();
  });

  it("opens a condition's next step with its values filled in", async () => {
    const user = await open("/");
    const attention = await screen.findByRole("region", { name: "Needs attention" });
    await user.click(within(attention).getByRole("button", { name: /Inspect a production VM/ }));
    const dialog = await screen.findByRole("dialog");
    expect(within(dialog).getByRole("combobox")).toHaveTextContent("prod2");
  });

  it("fetches the snapshots it shows, and nothing for pages not open", async () => {
    await open("/hosts");
    await screen.findByRole("table", { name: "Hosts" });
    expect(calls("hosts")).toBe(1);
    expect(calls("vms")).toBe(0);
    expect(calls("storage")).toBe(0);
  });
});

describe("the sidebar", () => {
  it("marks the pages whose resources need attention", async () => {
    await open("/hosts");
    const nav = screen.getByRole("navigation");
    await waitFor(() => expect(within(nav).getByRole("button", { name: /^Storage/ })).toHaveTextContent("Storage!"));
    expect(within(nav).getByRole("button", { name: /^Production VMs/ })).toHaveTextContent("Production VMs!");
    expect(within(nav).getByRole("button", { name: /^Hosts/ })).toHaveTextContent(/^Hosts$/);
  });
});

describe("the Hosts page", () => {
  it("lists each host with its load, role, and freshness", async () => {
    await open("/hosts");
    const table = await screen.findByRole("table", { name: "Hosts" });
    const rows = within(table).getAllByRole("row").slice(1);
    expect(rows.map((r) => within(r).getAllByRole("cell")[0].textContent)).toEqual(["mox110.10.0.1", "mox210.10.0.2", "mox310.10.0.3"]);
    expect(within(rows[0]).getByText("duty")).toBeInTheDocument();
    expect(within(rows[0]).getByText("this host")).toBeInTheDocument();
    expect(within(rows[1]).getByText("41% of 16 cores")).toBeInTheDocument();
    expect(within(rows[1]).getByText(/observed/)).toBeInTheDocument();
  });

  it("marks a host whose stream is down as unreachable", async () => {
    mock.statusData.hosts.hosts[2].freshness = { observed_at_ms: Date.now() - 300_000, reachable: false };
    await open("/hosts");
    const table = await screen.findByRole("table", { name: "Hosts" });
    const row = within(table).getAllByRole("row")[3];
    expect(within(row).getByText("unreachable")).toBeInTheDocument();
    expect(within(row).getByText(/seen 5m ago/)).toBeInTheDocument();
  });

  it("offers host operations for the host on its row", async () => {
    const user = await open("/hosts");
    const table = await screen.findByRole("table", { name: "Hosts" });
    await user.click(within(table).getByRole("button", { name: "mox2 actions" }));
    await user.click(await screen.findByRole("menuitem", { name: /Remove host/ }));
    const dialog = await screen.findByRole("dialog");
    expect(within(dialog).getByRole("combobox")).toHaveTextContent("mox2");
  });

  it("says why host status is missing", async () => {
    mock.statusFailures.hosts = new ApiFailure(503, "unavailable", "cluster status is not available on this host");
    await open("/hosts");
    expect(await screen.findByText("Host status isn't available")).toBeInTheDocument();
    expect(screen.getAllByText("cluster status is not available on this host").length).toBeGreaterThan(0);
  });

  it("follows the changes the event stream announces", async () => {
    await open("/hosts");
    const table = await screen.findByRole("table", { name: "Hosts" });
    expect(within(table).getAllByText("online")).toHaveLength(3);
    mock.statusData.hosts.hosts[1].online = false;
    mock.push({ type: "host", host: "mox2", reachable: false, last_seen_ms: Date.now() });
    await waitFor(() => expect(within(table).getAllByText("offline").length).toBeGreaterThan(0), { timeout: 3000 });
    expect(calls("hosts")).toBe(2);
  });
});

describe("adding hosts", () => {
  it("issues a bundle for a free slot and downloads it", async () => {
    const created = vi.fn((_: Blob) => "blob:bundle");
    URL.createObjectURL = created;
    URL.revokeObjectURL = vi.fn();
    const clicked = vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(() => {});
    const user = await open("/hosts");
    await user.click(await screen.findByRole("button", { name: /Add a host/ }));
    const dialog = await screen.findByRole("dialog");
    expect(within(dialog).getByRole("combobox")).toHaveTextContent("mox4");
    await user.click(within(dialog).getByRole("button", { name: "Issue and download" }));
    await waitFor(() => expect(clicked).toHaveBeenCalled());
    expect(mock.calls.find((c) => c.method === "issueJoin")?.args[0]).toMatchObject({ host: "mox4", max_prod_vms: 4, max_staging_vms: 8, ttl_hours: 168 });
    const table = await screen.findByRole("table", { name: "Join bundles" });
    expect(within(table).getByText("waiting for the host")).toBeInTheDocument();
    clicked.mockRestore();
  });

  it("shows a pending request's fingerprints and approves it with hosts.join", async () => {
    const now = Date.now();
    mock.joinData.joins = [
      {
        host: "mox4",
        issued_by: "root@pam",
        issued_at_ms: now - 3_600_000,
        expires_at_ms: now + 3_600_000,
        used_at_ms: now - 60_000,
        request: { request_id: "req1", address: "100.64.0.9", ssh_host_key_fingerprint: "SHA256:abc", https_fingerprint: "AA:BB", received_by: "mox1", created_at_ms: now - 60_000 },
      },
    ];
    const user = await open("/hosts");
    const table = await screen.findByRole("table", { name: "Join bundles" });
    expect(within(table).getByText("SSH SHA256:abc")).toBeInTheDocument();
    expect(within(table).getByText("HTTPS AA:BB")).toBeInTheDocument();
    await user.click(within(table).getByRole("button", { name: /Approve/ }));
    const dialog = await screen.findByRole("dialog");
    expect(within(dialog).getByText("Approve a host's join")).toBeInTheDocument();
    expect(within(dialog).getByDisplayValue("mox4")).toBeInTheDocument();
  });

  it("offers nothing to change to a viewer", async () => {
    mock.joinData.joins = [{ host: "mox4", issued_by: "root@pam", issued_at_ms: 1, expires_at_ms: Date.now() + 1000, used_at_ms: null, request: null }];
    await open("/hosts", "viewer");
    const table = await screen.findByRole("table", { name: "Join bundles" });
    expect(within(table).queryByRole("button")).toBeNull();
    expect(screen.queryByRole("button", { name: /Add a host/ })).toBeNull();
  });
});

describe("the Production page", () => {
  it("shows each VM's placement, HA, and replication", async () => {
    await open("/production");
    const table = await screen.findByRole("table", { name: "Production VMs" });
    const prod2 = within(table).getByText("prod2").closest("tr")!;
    expect(within(prod2).getByText("1/1 failing")).toBeInTheDocument();
    expect(within(prod2).getByText("placement mox2,mox3")).toBeInTheDocument();
    expect(screen.getByText(/Not in the BMAC registry/).parentElement).toHaveTextContent("scratch (VM 900 on mox3)");
    const jobs = screen.getByRole("table", { name: "Replication jobs" });
    expect(within(jobs).getByText("mox2 → mox3").closest("tr")).toHaveTextContent("failing ×25");
  });

  it("keeps staging conditions off the page", async () => {
    mock.statusData.vms.conditions.push({ id: "staging.cleanup", resource: "stage1prod1", severity: "warning", message: "stage1prod1 is left over", since_ms: Date.now(), next_steps: [] });
    await open("/production");
    await screen.findByRole("region", { name: "Needs attention" });
    expect(screen.queryByText("stage1prod1 is left over")).toBeNull();
  });

  it("shows a viewer the destructive operations as needing admin", async () => {
    await open("/production", "viewer");
    await screen.findByRole("table", { name: "Production VMs" });
    const tile = screen.getByRole("button", { name: /Remove production VM/ });
    expect(tile).toBeDisabled();
    expect(tile).toHaveTextContent("Needs the admin role");
  });
});

describe("the Staging page", () => {
  it("shows each staging VM's source and snapshot", async () => {
    await open("/staging");
    const table = await screen.findByRole("table", { name: "Staging VMs" });
    const row = within(table).getByText("stage1prod1").closest("tr")!;
    expect(within(row).getByText("prod1")).toBeInTheDocument();
    expect(within(row).getByText("bmac-staging-prod1-261011T081500Z")).toBeInTheDocument();
    expect(within(row).getByText(/taken .* · 2h ago/)).toBeInTheDocument();
    expect(within(row).getByRole("link")).toHaveAttribute("rel", "noopener noreferrer");
  });

  it("invites creating one when there are none", async () => {
    mock.statusData.vms.staging = [];
    await open("/staging");
    expect(await screen.findByText("No staging VMs")).toBeInTheDocument();
  });
});

describe("the Storage page", () => {
  it("shows pools, vdevs, and disks outside pools on every host", async () => {
    await open("/storage");
    await screen.findByRole("table", { name: "mox1 vdevs" });
    expect(screen.getAllByText("15% free")).toHaveLength(1);
    const vdevs = screen.getByRole("table", { name: "mox2 vdevs" });
    expect(within(vdevs).getByText("S6MOX2A0042")).toBeInTheDocument();
    expect(within(vdevs).getAllByText("LUKS")).toHaveLength(1);
    const disks = screen.getByRole("table", { name: "mox3 disks outside pools" });
    expect(within(disks).getByText("Available")).toBeInTheDocument();
    expect(screen.getAllByText("Every disk on this host is in a pool.")).toHaveLength(2);
  });

  it("says when a host's storage hasn't been observed", async () => {
    mock.statusData.storage.hosts[1].observation = null;
    await open("/storage");
    await screen.findByRole("table", { name: "mox1 vdevs" });
    expect(screen.getAllByText("Not observed yet").length).toBeGreaterThan(0);
  });
});

describe("the QDevice page", () => {
  it("offers the setup guide ahead of Add QDevice", async () => {
    await open("/qdevice");
    const operations = (await screen.findByText("QDevice operations")).nextElementSibling as HTMLElement;
    const titles = [...operations.querySelectorAll("a, button")].map((b) => b.textContent ?? "");
    const prereq = titles.findIndex((t) => t.includes("QDevice Setup Prereq"));
    expect(prereq).toBeGreaterThanOrEqual(0);
    expect(prereq).toBeLessThan(titles.findIndex((t) => t.includes("Add QDevice")));
  });

  it("says a QDevice isn't needed with an odd number of hosts", async () => {
    await open("/qdevice");
    expect(await screen.findByText("Not needed")).toBeInTheDocument();
    expect(screen.getByText("3 (odd)")).toBeInTheDocument();
  });

  it("asks for one with an even number of hosts", async () => {
    Object.assign(mock.statusData.hosts.cluster, { host_count: 4, online_count: 4 });
    mock.statusData.hosts.qdevice.needed = true;
    const user = await open("/qdevice");
    expect(await screen.findByText("Needed but not registered")).toBeInTheDocument();
    await user.click(screen.getAllByRole("button", { name: /Add QDevice/ })[0]);
    expect(await screen.findByRole("dialog")).toBeInTheDocument();
  });

  it("warns when the QDevice is registered but not voting", async () => {
    Object.assign(mock.statusData.hosts.qdevice, { registered: true, needed: true, voting: false, address: "100.64.0.9" });
    await open("/qdevice");
    expect(await screen.findByText("Registered but not voting")).toBeInTheDocument();
    expect(screen.getByText("100.64.0.9")).toBeInTheDocument();
  });
});

describe("the Diagnostics page", () => {
  it("offers the full reports", async () => {
    await open("/diagnostics");
    for (const title of ["Cluster health check", "Full cluster state", "Inspect a host", "Inspect a production VM", "Inspect the QDevice"]) {
      expect(await screen.findByRole("button", { name: new RegExp(title) })).toBeEnabled();
    }
  });
});

describe("pages not built yet", () => {
  it("say when they arrive", async () => {
    await open("/backups");
    expect(await screen.findByText("Backups isn't built yet")).toBeInTheDocument();
    expect(screen.getByText(/arrive with the backup appliance/)).toBeInTheDocument();
  });
});
