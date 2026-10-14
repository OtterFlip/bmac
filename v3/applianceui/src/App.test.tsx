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
  };
}

const STATUS: ApplianceStatusView = {
  hello: HELLO,
  pool: { name: "bk", guid: "123", health: "ONLINE", size_bytes: 1000, free_bytes: 400 },
  clusters: [{ cluster_uuid: "c1", cluster_name: "prod", state: "ACTIVE", last_checkin_ms: 1, last_error: null, offers: 0, pulling: null, hosts: ["mox1"] }],
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
      grant: { grant_id: "g1", cluster_uuid: "c1", pits: ["p2"], created_at_ms: 0, expires_at_ms: 1, by: "ui", state: "live", last_used_ms: null },
      token: "bmac-grant-secret",
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
    await user.click(screen.getByRole("checkbox", { name: "select bmac-pit-p2" }));
    await user.click(screen.getByRole("button", { name: /Grant to a restore/ }));
    const dialog = await screen.findByRole("dialog");
    await user.click(within(dialog).getByRole("button", { name: "Issue grant" }));
    expect(await within(dialog).findByText("bmac-grant-secret")).toBeInTheDocument();
    expect(api.issueGrant).toHaveBeenCalledWith({ cluster_uuid: "c1", hours: 24, pits: ["p2"] });
    await user.click(within(dialog).getByRole("button", { name: "Done" }));
    await waitFor(() => expect(screen.queryByText("bmac-grant-secret")).toBeNull());
  });
});
