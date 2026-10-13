// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";
import { App } from "./App";
import { MockBackend, defaultSettings } from "../lib/mock";
import { useStore } from "../state/store";

let backend: MockBackend;

beforeEach(() => {
  useStore.setState({ backend: null, info: null, checks: null, settings: null, tasks: {}, order: [], focused: null, page: "start", closeRequest: null });
  backend = new MockBackend();
});

const nav = () => within(screen.getByRole("navigation", { name: "Deployer" }));

describe("Start", () => {
  it("lists the workstation checks with the fix and a command to copy", async () => {
    render(<App backend={backend} />);
    expect(await screen.findByText("Tailscale")).toBeInTheDocument();
    expect(screen.getByText("Ready, with 1 warning.")).toBeInTheDocument();
    expect(screen.getByText("Connect this workstation to the tailnet the cluster uses.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Copy sudo tailscale up" })).toBeInTheDocument();
  });

  it("says what is missing when a required command is", async () => {
    backend.checks = [{ id: "xorriso", label: "xorriso", status: "error", detail: "not found on PATH", fix: "Install xorriso.", command: "sudo apt install xorriso", purpose: null, link: null }];
    render(<App backend={backend} />);
    expect(await screen.findByText(/1 required item is missing/)).toBeInTheDocument();
    expect(screen.getByText("Missing")).toBeInTheDocument();
  });

  it("opens a check's link through the backend", async () => {
    backend.checks = [{ id: "tailscale", label: "Tailscale", status: "error", detail: "not found", fix: null, command: null, purpose: null, link: { label: "Install Tailscale", url: "https://tailscale.com/download/linux" } }];
    render(<App backend={backend} />);
    await userEvent.setup().click(await screen.findByRole("button", { name: /install tailscale/i }));
    expect(backend.opened).toEqual(["https://tailscale.com/download/linux"]);
  });
});

describe("tasks", () => {
  it("shows a new task, raises its question, and sends the answer", async () => {
    render(<App backend={backend} />);
    const user = userEvent.setup();
    await screen.findByText("Tailscale");
    act(() => {
      backend.start("t1", "Clear old rpool label");
      backend.emit("t1", { type: "phase", id: "read", label: "Read the disks", status: "running", cancel_allowed: true });
    });
    const panel = await screen.findByRole("complementary", { name: "Clear old rpool label" });
    expect(within(panel).getAllByText("Read the disks").length).toBeGreaterThan(0);
    expect(nav().getByRole("button", { name: /clear old rpool label/i })).toBeInTheDocument();

    act(() =>
      backend.emit("t1", {
        type: "request",
        request_id: "r1",
        kind: "confirm",
        title: "Clear the label on sdb?",
        message: "It holds an old rpool.",
        severity: "destructive",
        confirmation_text: "sdb",
      }),
    );
    expect(await within(panel).findByText("Clear the label on sdb?")).toBeInTheDocument();
    await user.type(within(panel).getByLabelText(/to confirm/i), "sdb");
    await user.click(within(panel).getByRole("button", { name: "Confirm" }));
    await waitFor(() => expect(backend.answers).toEqual([{ task: "t1", request_id: "r1", answer: { kind: "confirm", confirmed: true } }]));
  });

  it("asks before cancelling a running task", async () => {
    render(<App backend={backend} />);
    const user = userEvent.setup();
    await screen.findByText("Tailscale");
    act(() => {
      backend.start("t1", "Build discovery ISO");
      backend.emit("t1", { type: "phase", id: "build", label: "Build", status: "running", cancel_allowed: true });
    });
    const panel = await screen.findByRole("complementary", { name: "Build discovery ISO" });
    await user.click(within(panel).getByRole("button", { name: "Cancel" }));
    await user.click(screen.getByRole("button", { name: "Cancel task" }));
    expect(backend.cancelled).toEqual(["t1"]);
    expect(await within(panel).findByRole("button", { name: "Cancelling…" })).toBeDisabled();
  });

  it("shows the failure and drops the cancel button once a task ends", async () => {
    render(<App backend={backend} />);
    await screen.findByText("Tailscale");
    act(() => {
      backend.start("t1", "Build discovery ISO");
      backend.emit("t1", { type: "failed", message: "xorriso exited with status 5" });
    });
    const panel = await screen.findByRole("complementary", { name: "Build discovery ISO" });
    expect(await within(panel).findByText("xorriso exited with status 5")).toBeInTheDocument();
    expect(within(panel).queryByRole("button", { name: "Cancel" })).toBeNull();
  });

  it("picks up tasks already running when the window loads", async () => {
    backend.start("t0", "Wait for host");
    backend.emit("t0", { type: "phase", id: "wait", label: "Wait for the host on the tailnet", status: "running", cancel_allowed: true });
    render(<App backend={backend} />);
    const user = userEvent.setup();
    await user.click(await nav().findByRole("button", { name: /wait for host/i }));
    const panel = await screen.findByRole("complementary", { name: "Wait for host" });
    expect(within(panel).getAllByText("Wait for the host on the tailnet").length).toBeGreaterThan(0);
  });
});

describe("closing", () => {
  it("asks before cancelling running tasks, and can keep working", async () => {
    render(<App backend={backend} />);
    const user = userEvent.setup();
    await screen.findByText("Tailscale");
    act(() => backend.requestClose(2));
    expect(await screen.findByText("2 tasks are still running")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Keep working" }));
    expect(backend.quits).toEqual([]);
    act(() => backend.requestClose(1));
    await user.click(await screen.findByRole("button", { name: "Cancel tasks and close" }));
    expect(backend.quits).toEqual([true]);
  });
});

describe("Settings", () => {
  it("validates and saves the link wait", async () => {
    render(<App backend={backend} />);
    const user = userEvent.setup();
    await user.click(nav().getByRole("button", { name: "Settings" }));
    const attempts = await screen.findByLabelText("Checks");
    expect(attempts).toHaveValue("10");
    await user.clear(attempts);
    await user.type(attempts, "0");
    expect(screen.getByText("Checks must be a whole number from 1 to 100.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Save" })).toBeDisabled();
    await user.clear(attempts);
    await user.type(attempts, "20");
    await user.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(backend.stored.link_wait).toEqual({ attempts: 20, interval_seconds: 3 }));
  });

  it("turns notifications off", async () => {
    render(<App backend={backend} />);
    const user = userEvent.setup();
    await user.click(nav().getByRole("button", { name: "Settings" }));
    await user.click(await screen.findByRole("switch", { name: "Desktop notifications" }));
    await waitFor(() => expect(backend.stored.notifications).toBe(false));
  });

  it("deletes a built ISO after asking", async () => {
    backend.stored = {
      ...defaultSettings(),
      isos: [{ path: "/home/you/bmac/mox4.iso", kind: "host", target: "mox4", built_ms: Date.now() - 60_000 }],
    };
    backend.present.add("/home/you/bmac/mox4.iso");
    render(<App backend={backend} />);
    const user = userEvent.setup();
    await user.click(nav().getByRole("button", { name: "Settings" }));
    expect(await screen.findByText("Proxmox host: mox4")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Delete" }));
    const dialog = await screen.findByRole("dialog");
    await user.click(within(dialog).getByRole("button", { name: "Delete" }));
    expect(await screen.findByText("No ISOs")).toBeInTheDocument();
    expect(backend.stored.isos).toEqual([]);
  });
});

describe("workflows", () => {
  it("starts discovery from its page and shows the task", async () => {
    render(<App backend={backend} />);
    const user = userEvent.setup();
    await user.click(nav().getByRole("button", { name: "Discovery" }));
    await user.click(screen.getByRole("button", { name: "Start discovery" }));
    expect(backend.started).toEqual(["discovery"]);
    expect(await screen.findByRole("complementary", { name: "Hardware discovery" })).toBeInTheDocument();
  });

  it("starts the workstation SSH tasks from their page", async () => {
    render(<App backend={backend} />);
    const user = userEvent.setup();
    await user.click(nav().getByRole("button", { name: "Workstation SSH" }));
    await user.click(screen.getByRole("button", { name: "Set up SSH to a host" }));
    expect(await screen.findByRole("complementary", { name: "Set up SSH to a host" })).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Set up SSH to a guest" }));
    await user.click(screen.getByRole("button", { name: "Deploy the hello app" }));
    expect(backend.started).toEqual(["host_ssh", "guest_ssh", "hello"]);
    const hello = screen.getByRole("region", { name: "The hello app" });
    expect(within(hello).getAllByRole("button", { name: /Deploy the hello app/ })).toHaveLength(2);
  });

  it("lists the managed SSH entries and removes one after confirming", async () => {
    backend.entries = [
      { kind: "host", alias: "bmac-prod-mox1", names: ["mox1"], address: "100.64.0.1" },
      { kind: "guest", alias: "bmac-prod-prod1", names: ["prod1"], address: "10.213.0.31", jump: "mox4", problem: "It jumps through mox4, which has no entry on this workstation." },
    ];
    render(<App backend={backend} />);
    const user = userEvent.setup();
    await user.click(nav().getByRole("button", { name: "Workstation SSH" }));
    expect(await screen.findByText("ssh prod1")).toBeInTheDocument();
    expect(screen.getByText("Needs repair")).toBeInTheDocument();
    expect(screen.getByText(/mox4, which has no entry/)).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Remove bmac-prod-prod1" }));
    const dialog = await screen.findByRole("dialog");
    await user.click(within(dialog).getByRole("button", { name: "Remove" }));
    await waitFor(() => expect(screen.queryByText("ssh prod1")).toBeNull());
    expect(backend.entries.map((e) => e.alias)).toEqual(["bmac-prod-mox1"]);
  });

  it("offers the Dashboard link only for a plain https result", async () => {
    render(<App backend={backend} />);
    const user = userEvent.setup();
    await user.click(nav().getByRole("button", { name: "Proxmox hosts" }));
    await user.click(screen.getByRole("button", { name: "Install a host" }));
    const panel = await screen.findByRole("complementary", { name: "Install a Proxmox host" });
    act(() => backend.emit("t1", { type: "result", data: { open_url: "javascript:alert(1)" } }));
    await within(panel).findByText("Result");
    expect(within(panel).queryByRole("button", { name: /open the dashboard/i })).toBeNull();
    act(() => backend.emit("t1", { type: "result", data: { open_url: "https://mox1.example.ts.net:8006/bmac/" } }));
    await user.click(await within(panel).findByRole("button", { name: /open the dashboard/i }));
    expect(backend.opened).toEqual(["https://mox1.example.ts.net:8006/bmac/"]);
  });
});

describe("discovery source", () => {
  it("shows the pinned default and saves a chosen Alpine ISO", async () => {
    render(<App backend={backend} />);
    const user = userEvent.setup();
    await user.click(nav().getByRole("button", { name: "Settings" }));
    expect(await screen.findByText("Using the pinned default:")).toBeInTheDocument();
    const use = screen.getByRole("button", { name: "Use this ISO" });
    await user.type(screen.getByLabelText("ISO URL"), "http://mirror.example/alpine.iso");
    await user.type(screen.getByLabelText("SHA-256"), "a".repeat(64));
    expect(screen.getByText("The URL must be a plain https URL.")).toBeInTheDocument();
    expect(use).toBeDisabled();
    await user.clear(screen.getByLabelText("ISO URL"));
    await user.type(screen.getByLabelText("ISO URL"), "https://mirror.example/alpine.iso");
    await user.click(use);
    expect(await screen.findByText("Using your choice:")).toBeInTheDocument();
    expect(backend.stored.discovery_iso).toEqual({ url: "https://mirror.example/alpine.iso", sha256: "a".repeat(64) });
    await user.click(screen.getByRole("button", { name: "Use the default" }));
    await waitFor(() => expect(backend.stored.discovery_iso).toBeUndefined());
  });
});
