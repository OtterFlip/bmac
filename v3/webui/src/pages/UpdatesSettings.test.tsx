// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// The Updates card on the Settings page against the mock backend: admins
// upload a bundle in chunks and start the update; viewers only read.

import { beforeEach, describe, expect, it } from "vitest";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { App } from "../app/App";
import { CLUSTER_OPERATIONS } from "../lib/mock/demo";
import { MockBackend } from "../lib/mock/backend";
import { setBackend } from "../state/store";

let mock: MockBackend;

beforeEach(() => {
  history.replaceState(null, "", "/");
  mock = new MockBackend();
  mock.operationList = CLUSTER_OPERATIONS;
});

async function open(role: "admin" | "viewer" = "admin") {
  mock.users["alice@pve"].role = role;
  history.replaceState(null, "", "/settings");
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

describe("updates", () => {
  it("uploads a bundle in order, chunk by chunk, and lists it", async () => {
    const user = await open();
    const versions = await screen.findByRole("table", { name: "Host versions" });
    expect(within(versions).getAllByText("3.0.0")).toHaveLength(3);
    const size = 512 * 1024 * 2 + 10;
    const file = new File([new Uint8Array(size)], "bmac-3.0.1.bundle");
    await user.upload(await screen.findByLabelText("Release bundle file"), file);
    const bundles = await screen.findByRole("table", { name: "Release bundles" });
    expect(within(bundles).getByText("3.0.1")).toBeInTheDocument();
    const chunks = mock.calls.filter((c) => c.method === "uploadChunk").map((c) => c.args[1]);
    expect(chunks).toEqual([0, 512 * 1024, 1024 * 1024]);
    await waitFor(() => expect(screen.getByRole("button", { name: /Update the cluster/ })).toBeEnabled());
  });

  it("is read only for viewers", async () => {
    await open("viewer");
    await screen.findByRole("table", { name: "Host versions" });
    expect(screen.queryByLabelText("Release bundle file")).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /Update the cluster/ })).not.toBeInTheDocument();
  });
});
