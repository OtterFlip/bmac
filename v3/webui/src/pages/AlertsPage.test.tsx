// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// The Alerts page and the Notifications part of Settings against the mock
// backend: viewers look, operators acknowledge and silence, admins set up
// email, and the Postmark token is never shown or kept in the browser.

import { beforeEach, describe, expect, it } from "vitest";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { App } from "../app/App";
import { MockBackend } from "../lib/mock/backend";
import { setBackend } from "../state/store";

type User = ReturnType<typeof userEvent.setup>;

const TOKEN = "8c1f7a52-0d4e-4b7b-9b60-2f3c1d9e6a10";

let mock: MockBackend;

beforeEach(() => {
  history.replaceState(null, "", "/");
  localStorage.clear();
  sessionStorage.clear();
  mock = new MockBackend();
});

async function open(path: string, role: "admin" | "operator" | "viewer" = "admin"): Promise<User> {
  mock.users["alice@pve"].role = role;
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

const alertList = () => screen.findByRole("list", { name: "Alerts" });
const calls = (method: string) => mock.calls.filter((c) => c.method === method).map((c) => c.args);

describe("alerts page", () => {
  it("shows viewers the alerts and history without actions", async () => {
    await open("/alerts", "viewer");
    const list = await alertList();
    expect(within(list).getByText(/Replication job 102-1/)).toBeInTheDocument();
    expect(within(list).getByText(/acknowledged by/)).toBeInTheDocument();
    expect(within(list).queryByRole("button", { name: /Acknowledge/ })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /New silence/ })).not.toBeInTheDocument();
    expect(screen.getByRole("table", { name: "Alert history" })).toHaveTextContent("corosync link 0");
    expect(screen.getByRole("region", { name: "Email delivery" })).toHaveTextContent("An administrator sets up email");
  });

  it("lets operators acknowledge an alert", async () => {
    const user = await open("/alerts", "operator");
    const list = await alertList();
    await user.click(within(list).getByRole("button", { name: /Acknowledge/ }));
    await waitFor(() => expect(within(list).queryByRole("button", { name: /Acknowledge/ })).not.toBeInTheDocument());
    expect(calls("acknowledgeAlert")).toEqual([["a1b2c3d4e5f6"]]);
    expect(within(list).getAllByText(/acknowledged by/)).toHaveLength(2);
  });

  it("silences an alert's condition from the alert, then removes the silence", async () => {
    const user = await open("/alerts", "operator");
    const list = await alertList();
    const replication = within(list).getByText(/Replication job 102-1/).closest("li")!;
    await user.click(within(replication as HTMLElement).getByRole("button", { name: /Silence/ }));
    const dialog = await screen.findByRole("dialog", { name: "Silence notifications" });
    expect(within(dialog).getByLabelText("Condition")).toHaveValue("replication.failing");
    expect(within(dialog).getByLabelText("Resource")).toHaveValue("prod2");
    const save = within(dialog).getByRole("button", { name: "Silence" });
    expect(save).toBeDisabled();
    await user.type(within(dialog).getByLabelText(/Reason/), "rebuilding mox3's pool");
    await user.click(save);
    const table = await screen.findByRole("table", { name: "Silences" });
    expect(table).toHaveTextContent("replication.failing");
    expect(table).toHaveTextContent("rebuilding mox3's pool");
    const [[request]] = calls("saveSilence") as [[{ condition: string; resource: string; host: string | null; until_ms: number }]];
    expect(request).toMatchObject({ condition: "replication.failing", resource: "prod2", host: null });
    expect(request.until_ms - Date.now()).toBeGreaterThan(3 * 3_600_000);
    expect(within(replication as HTMLElement).getByText("silenced")).toBeInTheDocument();

    await user.click(within(table).getByRole("button", { name: /Remove the silence/ }));
    await waitFor(() => expect(screen.queryByRole("table", { name: "Silences" })).not.toBeInTheDocument());
  });

  it("warns before silencing everything", async () => {
    const user = await open("/alerts", "operator");
    await user.click(await screen.findByRole("button", { name: /New silence/ }));
    const dialog = await screen.findByRole("dialog", { name: "Silence notifications" });
    expect(within(dialog).getByRole("note")).toHaveTextContent("silences every alert in the cluster");
  });
});

describe("notification settings", () => {
  const section = () => screen.findByRole("region", { name: "Email notifications" });

  it("are read only below admin, with no token entry", async () => {
    await open("/settings", "operator");
    const form = await screen.findByRole("form", { name: "Notification settings" });
    expect(within(form).getByLabelText("Sender")).toHaveAttribute("readonly");
    expect(within(form).queryByRole("button", { name: /Save/ })).not.toBeInTheDocument();
    expect(screen.queryByLabelText("Postmark server token")).not.toBeInTheDocument();
    expect(screen.getByText(/changing notifications needs the admin role/)).toBeInTheDocument();
  });

  it("store the token without showing it again or keeping it in the browser", async () => {
    const user = await open("/settings");
    const card = await section();
    expect(within(card).getByText("not set")).toBeInTheDocument();
    const field = within(card).getByLabelText("Postmark server token");
    expect(field).toHaveAttribute("type", "password");
    await user.type(field, TOKEN);
    await user.click(within(card).getByRole("button", { name: /Store token/ }));
    await waitFor(() => expect(within(card).getByText("stored on the cluster")).toBeInTheDocument());
    expect(field).toHaveValue("");
    expect(mock.postmarkToken).toBe(TOKEN);
    expect(JSON.stringify(mock.calls)).not.toContain(TOKEN);
    expect(document.body.textContent).not.toContain(TOKEN);
    expect([...document.querySelectorAll("input")].map((i) => i.value)).not.toContain(TOKEN);
    expect(localStorage.length + sessionStorage.length).toBe(0);
    expect(document.cookie).toBe("");
  });

  it("save recipients with the revision they were read at", async () => {
    const user = await open("/settings");
    const form = await screen.findByRole("form", { name: "Notification settings" });
    const recipients = within(form).getByLabelText("Recipients");
    await user.clear(recipients);
    await user.type(recipients, "ops@example.com, oncall@example.com");
    await user.click(within(form).getByRole("button", { name: /Save/ }));
    expect(await screen.findByText("Notification settings saved")).toBeInTheDocument();
    const [[request]] = calls("saveNotifySettings") as [[{ settings: { recipients: string[]; digest_hour: number | null }; expected_revision: number }]];
    expect(request.expected_revision).toBe(1);
    expect(request.settings.recipients).toEqual(["ops@example.com", "oncall@example.com"]);
    expect(request.settings.digest_hour).toBe(7);
    await waitFor(() => expect(within(form).getByRole("button", { name: /Save/ })).toBeDisabled());
  });

  it("offer a reload when someone else saved first", async () => {
    const user = await open("/settings");
    const form = await screen.findByRole("form", { name: "Notification settings" });
    mock.alertsData.settings_revision = 2;
    await user.type(within(form).getByLabelText("Recipients"), ", more@example.com");
    await user.click(within(form).getByRole("button", { name: /Save/ }));
    expect(await within(form).findByRole("alert")).toHaveTextContent("Someone saved these settings");
  });

  it("check and create the templates, then send a test email", async () => {
    const user = await open("/settings");
    const card = await section();
    expect(within(card).queryByRole("button", { name: /Check the Postmark server/ })).not.toBeInTheDocument();
    await user.type(within(card).getByLabelText("Postmark server token"), TOKEN);
    await user.click(within(card).getByRole("button", { name: /Store token/ }));
    await user.click(await within(card).findByRole("button", { name: /Check the Postmark server/ }));
    const table = await within(card).findByRole("table", { name: "Email templates" });
    expect(within(table).getAllByText("missing")).toHaveLength(9);
    expect(within(card).getByText(/A sandbox server accepts email and delivers none/)).toBeInTheDocument();
    await user.click(within(card).getByRole("button", { name: /Create 9 missing/ }));
    await waitFor(() => expect(within(table).getAllByText("present")).toHaveLength(9));

    await user.click(within(card).getByRole("button", { name: /^Send$/ }));
    expect(await within(card).findByRole("list", { name: "Test email results" })).toHaveTextContent("ops@example.com: accepted by Postmark");
    expect(calls("sendTestEmail")).toEqual([[null]]);
  });
});
