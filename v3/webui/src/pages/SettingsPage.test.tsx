// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// The Settings page against the mock backend: everyone reads, admins save
// with revision checks, and each host's keys can be compared and copied.

import { beforeEach, describe, expect, it } from "vitest";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { App } from "../app/App";
import { MockBackend } from "../lib/mock/backend";
import { setBackend } from "../state/store";

type User = ReturnType<typeof userEvent.setup>;

let mock: MockBackend;

beforeEach(() => {
  history.replaceState(null, "", "/");
  mock = new MockBackend();
});

async function open(role: "admin" | "operator" | "viewer" = "admin"): Promise<User> {
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

const clusterForm = () => screen.findByRole("form", { name: "Cluster settings" });
const saves = (method: string) => mock.calls.filter((c) => c.method === method).map((c) => c.args);

// v2's SettingsPage.test.tsx: the banner says what to do first.
describe("settings banner", () => {
  it("asks an admin to fill in the cluster's values before running workflows", async () => {
    mock.settingsData.cluster = null;
    await open();
    const banner = await screen.findByRole("region", { name: "Needs attention" });
    expect(within(banner).getByText("Fill in your cluster's values before running workflows.")).toBeInTheDocument();
    expect(within(await clusterForm()).getByText("Not saved yet")).toBeInTheDocument();
  });

  it("tells anyone else that an administrator needs to", async () => {
    mock.settingsData.cluster = null;
    await open("operator");
    const banner = await screen.findByRole("region", { name: "Needs attention" });
    expect(within(banner).getByText(/An administrator needs to fill in the cluster's values/)).toBeInTheDocument();
  });

  it("is gone once the cluster settings are saved", async () => {
    await open();
    await clusterForm();
    expect(screen.queryByRole("region", { name: "Needs attention" })).not.toBeInTheDocument();
  });
});

describe("cluster settings", () => {
  it("are read only for viewers", async () => {
    await open("viewer");
    const form = await clusterForm();
    expect(within(form).getByText(/Revision 4, saved by/)).toBeInTheDocument();
    expect(within(form).getByLabelText(/^MAX_MOX_HOSTS/)).toHaveValue("10");
    expect(within(form).getByLabelText(/^MAX_MOX_HOSTS/)).toHaveAttribute("readonly");
    expect(within(form).getAllByText(/changing settings needs the admin role/).length).toBeGreaterThan(0);
    expect(within(form).queryByRole("button", { name: /Save/ })).not.toBeInTheDocument();
  });

  it("save every value with the revision they were read at", async () => {
    const user = await open();
    const form = await clusterForm();
    const field = within(form).getByLabelText(/^PROD_VM_CORES/);
    await user.clear(field);
    await user.type(field, "48");
    await user.click(within(form).getByRole("button", { name: /Save/ }));
    await waitFor(() => expect(within(form).getByText(/Revision 5, saved by/)).toBeInTheDocument());
    const [[request]] = saves("saveClusterSettings") as [[{ values: Record<string, string>; expected_revision: number }]];
    expect(request.expected_revision).toBe(4);
    expect(request.values.PROD_VM_CORES).toBe("48");
    expect(request.values.MAX_MOX_HOSTS).toBe("10");
    expect(await screen.findByText("Cluster settings saved")).toBeInTheDocument();
    expect(within(form).getByRole("button", { name: /Save/ })).toBeDisabled();
  });

  it("leave out values that were cleared", async () => {
    const user = await open();
    const form = await clusterForm();
    await user.clear(within(form).getByLabelText(/^PROXMOX_QDEVICE_HOST/));
    await user.click(within(form).getByRole("button", { name: /Save/ }));
    await waitFor(() => expect(saves("saveClusterSettings")).toHaveLength(1));
    const [[request]] = saves("saveClusterSettings") as [[{ values: Record<string, string> }]];
    expect("PROXMOX_QDEVICE_HOST" in request.values).toBe(false);
  });

  it("keep the address plan fixed once the cluster exists", async () => {
    await open();
    const form = await clusterForm();
    for (const name of ["MAX_MOX_HOSTS", "PRIVATE_SUBNET_CIDR", "MOX_IP_START", "STAGING_IP_END"]) {
      const field = within(form).getByLabelText(new RegExp(`^${name}`));
      expect(field).toHaveAttribute("readonly");
      expect(field).toHaveAccessibleDescription("Part of the cluster's address plan, fixed once the cluster exists");
    }
    expect(within(form).getByLabelText(/^GUEST_EGRESS_VIP/)).not.toHaveAttribute("readonly");
    expect(within(form).getByLabelText(/^PROD_VM_CORES/)).not.toHaveAttribute("readonly");
  });

  it("show the server's reason beside each field it refused", async () => {
    const user = await open();
    const form = await clusterForm();
    const field = within(form).getByLabelText(/^PROXMOX_INTERNAL_DOMAIN/);
    await user.clear(field);
    await user.click(within(form).getByRole("button", { name: /Save/ }));
    expect(await within(form).findByText("Fix the settings marked below, then save again.")).toBeInTheDocument();
    expect(field).toHaveAttribute("aria-invalid", "true");
    expect(field).toHaveAccessibleDescription("required");
  });

  it("refuse to overwrite someone else's save, and reload theirs", async () => {
    const user = await open();
    const form = await clusterForm();
    mock.settingsData.cluster = { ...mock.settingsData.cluster!, revision: 5, values: { ...mock.settingsData.cluster!.values, PROD_VM_CORES: "32" } };
    const field = within(form).getByLabelText(/^PROD_VM_CORES/);
    await user.clear(field);
    await user.type(field, "48");
    await user.click(within(form).getByRole("button", { name: /Save/ }));
    const alert = await within(form).findByRole("alert");
    expect(alert).toHaveTextContent(/Someone saved these settings after you opened them/);
    expect(mock.settingsData.cluster.values.PROD_VM_CORES).toBe("32");
    await user.click(within(alert).getByRole("button", { name: /Reload/ }));
    await waitFor(() => expect(within(form).getByLabelText(/^PROD_VM_CORES/)).toHaveValue("32"));
    expect(within(form).getByText(/Revision 5, saved by/)).toBeInTheDocument();
    expect(within(form).queryByRole("alert")).not.toBeInTheDocument();
  });

  it("hide unset optional settings until asked", async () => {
    const user = await open();
    const form = await clusterForm();
    expect(within(form).queryByLabelText(/^TAILSCALE_TAG/)).not.toBeInTheDocument();
    await user.click(within(form).getByRole("button", { name: /Show \d+ unset optional settings/ }));
    expect(within(form).getByLabelText(/^TAILSCALE_TAG/)).toHaveValue("");
  });
});

describe("host facts", () => {
  it("are shown and saved per host", async () => {
    const user = await open();
    expect(await screen.findByRole("form", { name: "mox1's facts" })).toBeInTheDocument();
    await user.click(screen.getByRole("combobox", { name: "Host" }));
    await user.click(await screen.findByRole("option", { name: "mox2 · revision 3" }));
    const form = await screen.findByRole("form", { name: "mox2's facts" });
    expect(within(form).getByLabelText(/^PROXMOX_IP/)).toHaveValue("203.0.113.12");
    const field = within(form).getByLabelText(/^MAX_PROD_VM_COUNT_ON_THIS_HOST/);
    await user.clear(field);
    await user.type(field, "3");
    await user.click(within(form).getByRole("button", { name: /Save/ }));
    await waitFor(() => expect(within(form).getByText(/Revision 4, saved by/)).toBeInTheDocument());
    const [[host, request]] = saves("saveHostFacts") as [[string, { values: Record<string, string>; expected_revision: number }]];
    expect(host).toBe("mox2");
    expect(request.expected_revision).toBe(3);
    expect(request.values.MAX_PROD_VM_COUNT_ON_THIS_HOST).toBe("3");
  });

  it("can be created for a host that has none", async () => {
    const user = await open();
    await screen.findByRole("form", { name: "mox1's facts" });
    await user.click(screen.getByRole("combobox", { name: "Host" }));
    await user.click(await screen.findByRole("option", { name: "mox4 · no facts yet" }));
    const form = await screen.findByRole("form", { name: "mox4's facts" });
    expect(within(form).getByText("Not saved yet")).toBeInTheDocument();
    for (const [name, value] of [
      ["PROXMOX_IP", "203.0.113.14"],
      ["PROXMOX_GATEWAY", "203.0.113.1"],
      ["PROXMOX_PREFIX", "24"],
      ["PROXMOX_PUBLIC_MAC", "aa:bb:cc:dd:ee:04"],
      ["PROXMOX_SECONDARY_MAC", "auto"],
      ["MAX_PROD_VM_COUNT_ON_THIS_HOST", "2"],
      ["MAX_STAGING_VM_COUNT_ON_THIS_HOST", "4"],
    ]) {
      await user.type(within(form).getByLabelText(new RegExp(`^${name}`)), value);
    }
    await user.click(within(form).getByRole("button", { name: /Save/ }));
    await waitFor(() => expect(within(form).getByText(/Revision 1, saved by/)).toBeInTheDocument());
    expect((saves("saveHostFacts")[0] as [string, { expected_revision: number | null }])[1].expected_revision).toBeNull();
    expect(mock.settingsData.hosts.map((h) => h.host)).toEqual(["mox1", "mox2", "mox3", "mox4"]);
  });
});

describe("certificates and SSH host keys", () => {
  it("lists each host's fingerprints and copies its known_hosts line", async () => {
    const user = await open("viewer");
    const table = await screen.findByRole("table", { name: "Host keys" });
    const rows = within(table).getAllByRole("row").slice(1);
    expect(rows).toHaveLength(3);
    expect(within(rows[0]).getByText("SHA256:6pyryPWb/Q2XEYAJxJkDp9mclGwSmIvrNn6QEmRg9gY")).toBeInTheDocument();
    expect(within(rows[0]).getByText(mock.settingsData.identities[0].https_fingerprint!)).toBeInTheDocument();
    await user.click(within(rows[0]).getByRole("button", { name: "Copy mox1's known_hosts line" }));
    expect(await navigator.clipboard.readText()).toBe("mox1 ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBhE1+iRU5AP8b2hmfLfAHJQ/BVRAR0n4ddx0DFCnkg4");
  });

  it("says when no host has published its keys", async () => {
    mock.settingsData.identities = [];
    await open();
    expect(await screen.findByText(/No host has published its keys yet/)).toBeInTheDocument();
  });
});

describe("certificate rotation", () => {
  const card = () => screen.findByText("mox1's Dashboard certificate");

  it("shows the current certificate to everyone, with no way to rotate it unless admin", async () => {
    await open("viewer");
    await card();
    expect(screen.getAllByText(mock.certificateData.current!.fingerprint), "the card and the published host keys").toHaveLength(2);
    expect(screen.getByText(/mox1\.example\.ts\.net/)).toBeInTheDocument();
    expect(screen.getByText(/days left/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /Make a new certificate/ })).not.toBeInTheDocument();
  });

  it("shows the new fingerprint before switching, and switches to exactly that one", async () => {
    const user = await open();
    await card();
    const old = mock.certificateData.current!.fingerprint;
    await user.click(screen.getByRole("button", { name: /Make a new certificate/ }));
    const fresh = await screen.findByRole("region", { name: "New certificate" });
    const shown = mock.certificateData.pending!.fingerprint;
    expect(shown).not.toBe(old);
    expect(within(fresh).getByText(shown)).toBeInTheDocument();
    expect(mock.certificateData.current!.fingerprint).toBe(old);

    await user.click(within(fresh).getByRole("button", { name: "Switch to the new certificate" }));
    const dialog = await screen.findByRole("dialog", { name: "Switch to the new certificate?" });
    expect(within(dialog).getByText(shown)).toBeInTheDocument();
    await user.click(within(dialog).getByRole("button", { name: "Switch" }));

    const done = await screen.findByRole("status", { name: "Certificate switched" });
    expect(within(done).getByText(shown)).toBeInTheDocument();
    expect(within(done).getByText(/compare the SHA-256 fingerprint/)).toBeInTheDocument();
    expect(within(done).getByRole("button", { name: /Reload the page/ })).toBeInTheDocument();
    expect(saves("switchCertificate")).toEqual([[shown]]);
    expect(mock.certificateData.current!.fingerprint).toBe(shown);
  });

  it("refuses to switch to a certificate that changed since it was shown", async () => {
    const user = await open();
    await card();
    await user.click(screen.getByRole("button", { name: /Make a new certificate/ }));
    const fresh = await screen.findByRole("region", { name: "New certificate" });
    const shown = mock.certificateData.pending!.fingerprint;
    await mock.rotateCertificate();
    const replaced = mock.certificateData.pending!.fingerprint;
    await user.click(within(fresh).getByRole("button", { name: "Switch to the new certificate" }));
    await user.click(within(await screen.findByRole("dialog")).getByRole("button", { name: "Switch" }));
    expect(await screen.findByText(/changed since you saw it/)).toBeInTheDocument();
    expect(saves("switchCertificate")).toEqual([[shown]]);
    expect(await within(await screen.findByRole("region", { name: "New certificate" })).findByText(replaced)).toBeInTheDocument();
    expect(screen.queryByRole("status", { name: "Certificate switched" })).not.toBeInTheDocument();
  });

  it("can discard a new certificate", async () => {
    const user = await open();
    await card();
    await user.click(screen.getByRole("button", { name: /Make a new certificate/ }));
    const fresh = await screen.findByRole("region", { name: "New certificate" });
    await user.click(within(fresh).getByRole("button", { name: "Discard" }));
    await waitFor(() => expect(screen.queryByRole("region", { name: "New certificate" })).not.toBeInTheDocument());
    expect(mock.certificateData.pending).toBeNull();
    expect(screen.getByRole("button", { name: /Make a new certificate/ })).toBeInTheDocument();
  });
});
