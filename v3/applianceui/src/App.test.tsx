// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { ApplianceHello, ApplianceStatusView, PitView } from "@bmac/ui";
import { App } from "./App";
import { ApiFailure, type Api } from "./lib/api";

const HELLO: ApplianceHello = { name: "vault1", appliance_uuid: "a-1", version: "3.0.0", locked: false, no_password: false, fingerprint: "AB:CD" };

function pit(id: string, taken_s: number, parent: string | null): PitView {
  return {
    cluster_uuid: "c1",
    cluster_name: "prod",
    vm_uuid: "vm-0001",
    branch_uuid: "main",
    pit_uuid: id,
    parent,
    name: `bmac-pit-${id}`,
    taken_s,
    published_at_ms: 0,
    bytes: 1024,
    volumes: 1,
    received_from: parent,
    held: false,
    suspect: false,
    granted: false,
    time_zone: "America/Denver",
  };
}

const STATUS: ApplianceStatusView = {
  hello: HELLO,
  pool: { name: "bk", guid: "123", health: "ONLINE", size_bytes: 1000, free_bytes: 400 },
  clusters: [{ cluster_uuid: "c1", cluster_name: "prod", state: "ACTIVE", last_checkin_ms: 1, last_error: null, offers: 0, pulling: null, hosts: ["mox1"], time_zone: "America/Denver" }],
  transfers: [],
  pits: 2,
  scrub: null,
  watch: [],
  notify_error: null,
  pending_retention: null,
};

function fakeApi(hello: ApplianceHello = HELLO) {
  let ended: ((message: string) => void) | null = null;
  let loggedIn = false;
  const api = {
    hello: vi.fn(async () => hello),
    login: vi.fn(async (password: string) => {
      if (password !== "right password") throw new ApiFailure(401, "unauthenticated", "that isn't the administrator's password");
      loggedIn = true;
    }),
    logout: vi.fn(async () => {
      loggedIn = false;
    }),
    loggedIn: () => loggedIn,
    onSessionEnded: (callback: (message: string) => void) => {
      ended = callback;
      return () => (ended = null);
    },
    status: vi.fn(async () => STATUS),
    pits: vi.fn(async () => ({ pits: [pit("p1", 100, null), pit("p2", 200, "p1")], receiving: [], quarantined: [] })),
    issueGrant: vi.fn(async () => ({
      grant: { grant_id: "g1", cluster_uuid: "c1", pits: ["p2"], created_at_ms: 0, expires_at_ms: 1, by: "ui", state: "live", last_used_ms: null, kind: "restore", host_fingerprint: null },
      token: "bmac-grant-secret",
    })),
    grants: vi.fn(async () => []),
    retention: vi.fn(async () => ({
      policy: { daily: 7, weekly: 4, monthly: 12, month_days: 30, yearly: true },
      floor: { daily: 1, weekly: 1, monthly: 1, month_days: 30, yearly: true },
      delay_hours: 72,
      pending: null,
      holds: [],
      prune: [],
      reclaim_bytes: 0,
      coverage: [{ cluster_uuid: "c1", vm_uuid: "vm-0001", pits: 2, oldest_s: 100, newest_s: 200, time_zone: "America/Denver" }],
    })),
    setRetention: vi.fn(async () => ({ applied: false, effective_at_ms: 1 })),
    issueRecoveryGrant: vi.fn(async () => ({
      grant: { grant_id: "g2", cluster_uuid: "c1", pits: [], created_at_ms: 0, expires_at_ms: 1, by: "ui", state: "live", last_used_ms: null, kind: "recovery", host_fingerprint: "EF:01" },
      token: "bmac-grant-recovery-secret",
    })),
    recovery: vi.fn(async () => ({
      clusters: [
        {
          cluster_uuid: "c1",
          cluster_name: "prod",
          manifests: [{ generated_at_ms: 1_700_000_000_000, revision: "0123456789abcdef" }],
          headers: [{ host: "mox1", file: "bmac-rpool-s1.img", newest_at_ms: 1_700_000_000_000, versions: 2 }],
          restore_tests: [
            { restore_uuid: "r1", vm_uuid: "v1", pit_uuid: "p2", host: "mox2", started_at_ms: 1_700_000_000_000, finished_at_ms: 1_700_000_060_000, booted: true, agent: true, agent_after_s: 21, error: null },
            { restore_uuid: "r2", vm_uuid: "v1", pit_uuid: "p1", host: "mox2", started_at_ms: 1_690_000_000_000, finished_at_ms: null, booted: false, agent: false, agent_after_s: null, error: "the scratch VM didn't start" },
          ],
        },
      ],
    })),
  } as unknown as Api;
  return { api, end: (message: string) => ended?.(message) };
}

describe("the appliance UI", () => {
  it("asks for the console unlock while the pool is locked", async () => {
    const { api } = fakeApi({ ...HELLO, locked: true });
    render(<App api={api} />);
    expect(await screen.findByText("The backup pool is locked")).toBeInTheDocument();
    expect(screen.getByText("bmac-backupd unlock")).toBeInTheDocument();
    expect(screen.getByText("AB:CD")).toBeInTheDocument();
    expect(screen.queryByLabelText(/password/i)).toBeNull();
  });

  it("logs in with the password, and back out when the session ends", async () => {
    const user = userEvent.setup();
    const { api, end } = fakeApi();
    render(<App api={api} />);
    const field = await screen.findByLabelText(/Administrator password/);
    await user.type(field, "wrong password");
    await user.click(screen.getByRole("button", { name: /Log in/ }));
    expect(await screen.findByRole("alert")).toHaveTextContent("isn't the administrator's password");
    expect(field).toHaveValue("");
    await user.type(field, "right password");
    await user.click(screen.getByRole("button", { name: /Log in/ }));
    expect(await screen.findByText("Clusters")).toBeInTheDocument();
    expect(screen.getByText("prod")).toBeInTheDocument();
    end("your session has ended; log in again");
    expect(await screen.findByText("your session has ended; log in again")).toBeInTheDocument();
  });

  it("issues a restore grant for selected PITs and shows its token once", async () => {
    const user = userEvent.setup();
    const { api } = fakeApi();
    render(<App api={api} />);
    await user.type(await screen.findByLabelText(/Administrator password/), "right password");
    await user.click(screen.getByRole("button", { name: /Log in/ }));
    await user.click(await screen.findByRole("button", { name: "PITs" }));
    expect(await screen.findByRole("img", { name: /PIT graph/ })).toBeInTheDocument();
    expect(screen.getByText(/times in America\/Denver/)).toBeInTheDocument();
    await user.click(screen.getByRole("checkbox", { name: "select bmac-pit-p2" }));
    await user.click(screen.getByRole("button", { name: /Grant to a restore/ }));
    const dialog = await screen.findByRole("dialog");
    await user.click(within(dialog).getByRole("button", { name: "Issue grant" }));
    expect(await within(dialog).findByText("bmac-grant-secret")).toBeInTheDocument();
    expect(api.issueGrant).toHaveBeenCalledWith({ cluster_uuid: "c1", hours: 24, pits: ["p2"] });
    await user.click(within(dialog).getByRole("button", { name: "Done" }));
    await waitFor(() => expect(screen.queryByText("bmac-grant-secret")).toBeNull());
  });

  it("sets the month length and yearly consolidation, and shows days in the cluster's zone", async () => {
    const user = userEvent.setup();
    const { api } = fakeApi();
    render(<App api={api} />);
    await user.type(await screen.findByLabelText(/Administrator password/), "right password");
    await user.click(screen.getByRole("button", { name: /Log in/ }));
    await user.click(await screen.findByRole("button", { name: "Retention" }));
    const days = await screen.findByLabelText("days in a month");
    expect(screen.getByRole("cell", { name: "America/Denver" })).toBeInTheDocument();
    await user.clear(days);
    await user.type(days, "32");
    expect(screen.getByRole("button", { name: "Save" })).toBeDisabled();
    await user.clear(days);
    await user.type(days, "31");
    await user.click(screen.getByRole("checkbox", { name: "yearly" }));
    await user.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(api.setRetention).toHaveBeenCalledWith({ daily: 7, weekly: 4, monthly: 12, month_days: 31, yearly: false }));
  });

  it("issues a recovery grant for a new host and shows its token once", async () => {
    const user = userEvent.setup();
    const { api } = fakeApi();
    render(<App api={api} />);
    await user.type(await screen.findByLabelText(/Administrator password/), "right password");
    await user.click(screen.getByRole("button", { name: /Log in/ }));
    await user.click(await screen.findByRole("button", { name: "Grants" }));
    await user.click(await screen.findByRole("button", { name: /Grant a recovery/ }));
    const dialog = await screen.findByRole("dialog");
    const issue = within(dialog).getByRole("button", { name: "Issue grant" });
    expect(issue).toBeDisabled();
    await user.type(within(dialog).getByLabelText(/certificate fingerprint/), " EF:01 ");
    await user.click(issue);
    expect(await within(dialog).findByText("bmac-grant-recovery-secret")).toBeInTheDocument();
    expect(api.issueRecoveryGrant).toHaveBeenCalledWith({ cluster_uuid: "c1", hours: 24, host_fingerprint: "EF:01" });
    await user.click(within(dialog).getByRole("button", { name: "Done" }));
    await waitFor(() => expect(screen.queryByText("bmac-grant-recovery-secret")).toBeNull());
  });

  it("shows each cluster's recovery manifests, LUKS header backups, and restore tests", async () => {
    const user = userEvent.setup();
    const { api } = fakeApi();
    render(<App api={api} />);
    await user.type(await screen.findByLabelText(/Administrator password/), "right password");
    await user.click(screen.getByRole("button", { name: /Log in/ }));
    await user.click(await screen.findByRole("button", { name: "Recovery" }));
    expect(await screen.findByText(/1 kept/)).toBeInTheDocument();
    expect(within(screen.getByRole("list", { name: "prod LUKS header backups" })).getByText(/bmac-rpool-s1\.img \(2 kept/)).toBeInTheDocument();
    const tests = within(screen.getByRole("list", { name: "prod restore tests" })).getAllByRole("listitem");
    expect(tests[0]).toHaveTextContent("booted");
    expect(tests[0]).toHaveTextContent("agent after 21 s");
    expect(tests[1]).toHaveTextContent("failed");
    expect(tests[1]).toHaveTextContent("the scratch VM didn't start");
  });
});
