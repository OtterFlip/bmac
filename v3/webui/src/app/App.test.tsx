// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// The login rules of SECURITY.md, "Browser sessions": the token lives only
// in memory, nothing is written to cookies or browser storage, the token never
// appears in a URL, and a reload starts at the login page.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { OperationView } from "@bmac/ui";
import { createHttpBackend } from "../lib/backend";
import { MockBackend } from "../lib/mock/backend";
import { setBackend } from "../state/store";
import { App } from "./App";

const TOKEN = "s3cr3t-token-".padEnd(43, "x");

const operation: OperationView = {
  allowed: true,
  id: "lab.interactive",
  title: "Lab: interactive mock",
  summary: "Asks a question and waits.",
  description: "A script that asks for input.",
  category: "diagnostics",
  kind: "script",
  min_role: "operator",
  mutating: true,
  destructive: false,
  supports_dry_run: false,
  params: [],
  locks: [],
  target: { rule: "any" },
  cancel: { rule: "script" },
  resume: "never",
} as unknown as OperationView;

interface Call {
  url: string;
  method: string;
  authorization?: string;
  init: RequestInit;
}

const json = (status: number, body: unknown) => new Response(JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } });

/** A fake bmacd behind the real HTTP backend: one user with TOTP, one operation. */
function fakeBmacd() {
  const calls: Call[] = [];
  let jobs: unknown[] = [];
  const fetcher = vi.fn(async (input: RequestInfo | URL, init: RequestInit = {}) => {
    const url = String(input);
    const headers = (init.headers ?? {}) as Record<string, string>;
    calls.push({ url, method: init.method ?? "GET", authorization: headers.Authorization, init });
    const path = url.replace(/^.*\/api\/v1/, "");
    const body = init.body ? JSON.parse(String(init.body)) : undefined;
    if (path === "/login") {
      if (body.step === "password") return body.password === "pw" ? json(200, { step: "second_factor", challenge: "c1", methods: ["totp", "recovery"] }) : json(401, { code: "unauthenticated", message: "The user name or password is wrong." });
      return body.response === "123456"
        ? json(200, { step: "session", token: TOKEN, user: "alice@pve", role: "admin", idle_timeout_seconds: 1800, expires_at_ms: 9e12 })
        : json(401, { code: "unauthenticated", message: "That code is wrong." });
    }
    if (headers.Authorization !== `Bearer ${TOKEN}`) return json(401, { code: "unauthenticated", message: "Log in first." });
    if (path === "/events") {
      const encoder = new TextEncoder();
      const stream = new ReadableStream<Uint8Array>({
        start(controller) {
          controller.enqueue(encoder.encode(`id: e-1\ndata: ${JSON.stringify({ id: "e-1", type: "hello", stream: "st1" })}\n\n`));
          init.signal?.addEventListener("abort", () => controller.error(new DOMException("aborted", "AbortError")));
        },
      });
      return new Response(stream, { status: 200, headers: { "Content-Type": "text/event-stream" } });
    }
    if (path === "/operations") return json(200, { operations: [operation] });
    if (path === "/setup") return json(200, { active: false, operation: "hosts.setup", new_cluster: false });
    if (path === "/jobs" && init.method === "POST") {
      const now = Date.now();
      const summary = { id: "j1", operation: operation.id, title: operation.title, state: "ADMITTED", host: "mox1", user: "alice@pve", dry_run: body.dry_run, created_at_ms: now, updated_at_ms: now, cancel_requested: false, last_seq: 0 };
      jobs = [summary];
      return json(200, summary);
    }
    if (path === "/jobs") return json(200, { jobs });
    if (path.startsWith("/jobs/j1")) return json(200, { job: jobs[0], parameters: {}, locks: [], events: [], more: false });
    if (path === "/events/watch" || path === "/logout") return json(200, {});
    return json(404, { code: "not_found", message: path });
  });
  return { calls, fetcher: fetcher as unknown as typeof fetch };
}

/** Fails the test on any use of cookies or browser storage, whatever catches the error. */
function tripwire() {
  const touched: string[] = [];
  const restore: (() => void)[] = [];
  const guard = (target: object, name: string) => {
    const own = Object.getOwnPropertyDescriptor(target, name);
    const fail = () => {
      touched.push(name);
      throw new Error(`${name} must not be used`);
    };
    Object.defineProperty(target, name, { configurable: true, get: fail, set: fail });
    restore.push(() => (own ? Object.defineProperty(target, name, own) : delete (target as Record<string, unknown>)[name]));
  };
  guard(document, "cookie");
  for (const name of ["localStorage", "sessionStorage", "indexedDB", "caches"]) guard(window, name);
  return { touched, restore: () => restore.reverse().forEach((r) => r()) };
}

async function logIn(user: ReturnType<typeof userEvent.setup>) {
  await user.type(await screen.findByLabelText(/Proxmox user/), "alice@pve");
  await user.type(screen.getByLabelText(/Password/), "pw");
  await user.click(screen.getByRole("button", { name: /Log in/ }));
  await user.type(await screen.findByLabelText(/authenticator app/), "123456");
  await user.click(screen.getByRole("button", { name: /Continue/ }));
  await screen.findByRole("button", { name: "Log out" });
}

let wire: ReturnType<typeof tripwire>;

beforeEach(() => {
  history.replaceState(null, "", "/");
  wire = tripwire();
});

afterEach(() => {
  wire.restore();
});

describe("the Web Dashboard", () => {
  it("has a tripwire that notices storage use", () => {
    expect(() => localStorage.getItem("x")).toThrow();
    expect(() => (document.cookie = "a=b")).toThrow();
    expect(() => window.indexedDB).toThrow();
    expect(wire.touched).toEqual(["localStorage", "cookie", "indexedDB"]);
    wire.touched.length = 0;
  });

  it("logs in, runs a job, and logs out without touching cookies or browser storage", async () => {
    const server = fakeBmacd();
    setBackend(createHttpBackend(server.fetcher));
    const user = userEvent.setup();
    render(<App />);

    await logIn(user);
    await user.click(within(screen.getByRole("navigation")).getByRole("button", { name: /Activity/ }));
    expect(location.pathname).toBe("/activity");
    await user.click(await screen.findByRole("button", { name: operation.title }));
    const dialog = await screen.findByRole("dialog");
    await user.click(within(dialog).getByRole("button", { name: "Start" }));
    await waitFor(() => expect(server.calls.some((c) => c.method === "POST" && c.url.endsWith("/api/v1/jobs"))).toBe(true));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect((await screen.findAllByText(operation.title)).length).toBeGreaterThan(0);

    await user.click(screen.getByRole("button", { name: "Log out" }));
    await screen.findByLabelText(/Proxmox user/);
    expect(location.pathname).toBe("/");

    expect(wire.touched).toEqual([]);
    for (const call of server.calls) {
      expect(call.url).not.toContain(TOKEN);
      expect(call.init.credentials).toBe("omit");
    }
    expect(location.href).not.toContain(TOKEN);
    expect(server.calls.filter((c) => c.authorization).length).toBeGreaterThan(3);
  });

  it("starts at the login page after a reload, with no token from before", async () => {
    const server = fakeBmacd();
    setBackend(createHttpBackend(server.fetcher));
    const user = userEvent.setup();
    const first = render(<App />);
    await logIn(user);
    first.unmount();

    // A reload runs main.tsx again: a new backend, and only what the browser kept.
    const before = server.calls.length;
    history.replaceState(null, "", "/activity");
    setBackend(createHttpBackend(server.fetcher));
    render(<App />);
    expect(await screen.findByLabelText(/Proxmox user/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Log out" })).toBeNull();
    expect(server.calls.slice(before).every((c) => !c.authorization)).toBe(true);
    expect(wire.touched).toEqual([]);
  });

  it("asks before the tab closes while a job waits for an answer", async () => {
    const mock = new MockBackend();
    const job = mock.addJob();
    mock.addEvent(job.id, { type: "request", request_id: "r1", kind: "confirm", title: "Go on?", message: "Really?", severity: "info" });
    setBackend(mock);
    const user = userEvent.setup();
    render(<App />);
    await user.type(await screen.findByLabelText(/Proxmox user/), "alice@pve");
    await user.type(screen.getByLabelText(/Password/), "alice");
    await user.click(screen.getByRole("button", { name: /Log in/ }));
    await user.type(await screen.findByLabelText(/authenticator app/), "123456");
    await user.click(screen.getByRole("button", { name: /Continue/ }));
    await screen.findByRole("button", { name: "Log out" });

    const leave = () => {
      const event = new Event("beforeunload", { cancelable: true });
      window.dispatchEvent(event);
      return event.defaultPrevented;
    };
    expect(leave()).toBe(true);
    mock.addEvent(job.id, { type: "answered", request_id: "r1", by: "alice@pve" });
    mock.addEvent(job.id, { type: "completed" });
    mock.pushSummary(job.id);
    await waitFor(() => expect(leave()).toBe(false));
  });

  it("shows why a login failed and keeps nothing typed", async () => {
    setBackend(createHttpBackend(fakeBmacd().fetcher));
    const user = userEvent.setup();
    render(<App />);
    await user.type(await screen.findByLabelText(/Proxmox user/), "alice@pve");
    await user.type(screen.getByLabelText(/Password/), "wrong");
    await user.click(screen.getByRole("button", { name: /Log in/ }));
    expect(await screen.findByRole("alert")).toHaveTextContent("The user name or password is wrong.");
    expect(screen.getByLabelText(/Password/)).toHaveValue("");
    expect(wire.touched).toEqual([]);
  });
});
