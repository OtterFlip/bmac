// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// The Logs page and Settings, then Log replication, against the mock
// backend: searching with filters, paging, a record's details, exporting a
// search as a file, and how far each peer has copied.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { App } from "../app/App";
import { MockBackend } from "../lib/mock/backend";
import { setBackend } from "../state/store";

type User = ReturnType<typeof userEvent.setup>;

let mock: MockBackend;

beforeEach(() => {
  history.replaceState(null, "", "/");
  localStorage.clear();
  sessionStorage.clear();
  mock = new MockBackend();
});

afterEach(() => {
  vi.restoreAllMocks();
});

async function open(path: string): Promise<User> {
  mock.users["alice@pve"].role = "viewer";
  history.replaceState(null, "", path);
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

const table = () => screen.findByRole("table", { name: "Log records" });
const rows = (t: HTMLElement) => within(t).getAllByRole("row").slice(1);
const searches = () => mock.calls.filter((c) => c.method === "searchLogs").map((c) => c.args[0] as Record<string, unknown>);

describe("logs page", () => {
  it("shows every host's records newest first and pages back", async () => {
    const user = await open("/logs");
    const t = await table();
    expect(rows(t)).toHaveLength(100);
    expect(rows(t)[0]).toHaveTextContent("failed: Disk Vanished");
    expect(screen.getByText(/100 records so far/)).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Older records" }));
    await waitFor(() => expect(rows(t)).toHaveLength(mock.logsData.length));
    expect(screen.queryByRole("button", { name: "Older records" })).not.toBeInTheDocument();
    expect(searches()[1].before).toBe("100");
    expect(t).toHaveTextContent("(evicted for space)");
  });

  it("filters by severity, host, unit, and text", async () => {
    const user = await open("/logs");
    await table();
    await user.click(screen.getByRole("combobox", { name: "Severity" }));
    await user.click(await screen.findByRole("option", { name: "Error" }));
    await user.click(screen.getByRole("combobox", { name: "Host" }));
    await user.click(await screen.findByRole("option", { name: "mox2" }));
    await user.type(screen.getByLabelText("Unit"), "corosync.service");
    await user.click(screen.getByRole("button", { name: "Search" }));
    await waitFor(() => expect(rows(screen.getByRole("table", { name: "Log records" }))).toHaveLength(1));
    expect(screen.getByRole("table", { name: "Log records" })).toHaveTextContent("corosync.service: link 0 down");
    expect(searches().at(-1)).toMatchObject({ host: "mox2", min_severity: "error", unit: "corosync.service", kinds: [], limit: 100 });

    await user.click(screen.getByRole("button", { name: "Clear" }));
    await user.type(screen.getByLabelText("Contains"), "nothing like this");
    await user.click(screen.getByRole("button", { name: "Search" }));
    expect(await screen.findByText("No records")).toBeInTheDocument();
  });

  it("shows a record's body as text", async () => {
    const user = await open("/logs");
    const t = await table();
    await user.click(within(t).getByRole("button", { name: /Show the record mox1 .* #122/ }));
    const details = rows(t)[1];
    expect(details).toHaveTextContent('"message": "Disk Vanished"');
    expect(details).toHaveTextContent("7d2f1c9e3b8a4f60a1c2d3e4f5a6b7c8");
    expect(details.querySelector("pre")?.textContent).toContain('"type": "failed"');
  });

  it("exports the whole search as a file", async () => {
    const blobs: Blob[] = [];
    vi.spyOn(URL, "createObjectURL").mockImplementation((blob) => {
      blobs.push(blob as Blob);
      return "blob:logs";
    });
    vi.spyOn(URL, "revokeObjectURL").mockImplementation(() => {});
    const clicked = vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(() => {});
    const user = await open("/logs");
    await table();
    await user.click(screen.getByRole("button", { name: "Export" }));
    await waitFor(() => expect(clicked).toHaveBeenCalled());
    const lines = (await blobs[0].text()).trim().split("\n");
    expect(lines).toHaveLength(mock.logsData.length);
    expect(JSON.parse(lines[0]).summary).toBe("failed: Disk Vanished");
    expect(searches().at(-1)).toMatchObject({ limit: 500 });
  });
});

describe("log replication", () => {
  it("shows what this host holds and how far each peer copied", async () => {
    await open("/settings");
    const region = await screen.findByRole("region", { name: "Log replication" });
    const held = await within(region).findByRole("table", { name: "Records held" });
    expect(held).toHaveTextContent("mox3");
    expect(within(region).getByText(/Keeps 30 days within/)).toBeInTheDocument();
    const peers = within(region).getByRole("table", { name: "Peer copies" });
    expect(peers).toHaveTextContent("254 records, 9 min");
    expect(within(peers).getByText("unreachable")).toBeInTheDocument();
    expect(peers).toHaveTextContent("up to date");
  });
});
