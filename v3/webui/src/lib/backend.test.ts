// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

import { describe, expect, it, vi } from "vitest";
import type { StreamMessage } from "@bmac/ui";
import { ApiFailure, createHttpBackend, parseSse, type StreamState } from "./backend";

const TOKEN = "t".repeat(43);

interface Call {
  url: string;
  method: string;
  headers: Record<string, string>;
  body?: unknown;
  init: RequestInit;
}

const json = (status: number, body: unknown) => new Response(JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } });

/** A fake bmacd: `routes` answers JSON calls; `streams` hands out event-stream bodies in order. */
function fakeServer(routes: (call: Call) => Response | undefined, streams: (() => Response)[] = []) {
  const calls: Call[] = [];
  const fetcher = vi.fn(async (input: RequestInfo | URL, init: RequestInit = {}) => {
    const call: Call = {
      url: String(input),
      method: init.method ?? "GET",
      headers: (init.headers ?? {}) as Record<string, string>,
      body: init.body ? JSON.parse(String(init.body)) : undefined,
      init,
    };
    calls.push(call);
    if (call.url.endsWith("/api/v1/events")) {
      const next = streams.shift();
      if (!next) return new Promise<Response>(() => {});
      return next();
    }
    return routes(call) ?? json(404, { code: "not_found", message: "no route" });
  });
  return { calls, fetcher: fetcher as unknown as typeof fetch };
}

function streamOf(messages: StreamMessage[], keepOpen = false): () => Response {
  return () => {
    const encoder = new TextEncoder();
    const body = new ReadableStream<Uint8Array>({
      start(controller) {
        controller.enqueue(encoder.encode(": keep-alive\n\n"));
        for (const m of messages) controller.enqueue(encoder.encode(`id: ${m.id}\ndata: ${JSON.stringify(m)}\n\n`));
        if (!keepOpen) controller.close();
      },
    });
    return new Response(body, { status: 200, headers: { "Content-Type": "text/event-stream" } });
  };
}

const session = { step: "session", token: TOKEN, user: "alice@pve", role: "admin", idle_timeout_seconds: 1800, expires_at_ms: 9e12 };

function loginRoutes(call: Call): Response | undefined {
  if (call.url.endsWith("/login")) {
    const body = call.body as { step: string };
    return body.step === "password" ? json(200, { step: "second_factor", challenge: "c1", methods: ["totp", "recovery"] }) : json(200, session);
  }
  if (call.url.endsWith("/logout")) return json(200, {});
  if (call.url.endsWith("/operations")) return call.headers.Authorization === `Bearer ${TOKEN}` ? json(200, { operations: [] }) : json(401, { code: "unauthenticated", message: "log in" });
  if (call.url.endsWith("/events/watch")) return json(200, {});
  return undefined;
}

async function loggedIn(server: ReturnType<typeof fakeServer>) {
  const backend = createHttpBackend(server.fetcher);
  expect(await backend.login({ step: "password", user: "alice@pve", password: "pw" })).toEqual({ step: "second_factor", challenge: "c1", methods: ["totp", "recovery"] });
  const done = await backend.login({ step: "second_factor", challenge: "c1", method: "totp", response: "123456" });
  expect(done).toEqual({ step: "session", session: { user: "alice@pve", role: "admin", idle_timeout_seconds: 1800, expires_at_ms: 9e12 } });
  return backend;
}

const until = async (check: () => boolean) => {
  for (let i = 0; i < 600 && !check(); i++) await new Promise((r) => setTimeout(r, 10));
  expect(check()).toBe(true);
};

describe("parseSse", () => {
  it("splits complete frames, skips comments, and keeps the partial tail", () => {
    const { frames, rest } = parseSse(': keep-alive\n\nid: 1-1\ndata: {"a":1}\n\nid: 1-2\r\ndata: x\r\ndata: y\r\n\r\nid: 1-3\ndata: {"b"');
    expect(frames).toEqual([
      { id: "1-1", data: '{"a":1}' },
      { id: "1-2", data: "x\ny" },
    ]);
    expect(rest).toBe('id: 1-3\ndata: {"b"');
  });
});

describe("the HTTP backend", () => {
  it("logs in with a second factor and sends the token only in a header", async () => {
    const server = fakeServer(loginRoutes);
    const backend = await loggedIn(server);
    expect(await backend.operations()).toEqual([]);
    await backend.logout();
    expect(backend.session()).toBeNull();
    await expect(backend.operations()).rejects.toBeInstanceOf(ApiFailure);
    for (const call of server.calls) {
      expect(call.url).not.toContain(TOKEN);
      expect(call.init.credentials).toBe("omit");
      expect(call.init.cache).toBe("no-store");
    }
    expect(server.calls.filter((c) => c.headers.Authorization).map((c) => c.url.replace(/.*\/api\/v1/, ""))).toEqual(["/operations", "/logout"]);
  });

  it("ends the session on a 401 and says why", async () => {
    let expired = false;
    const server = fakeServer((call) => (expired && call.url.endsWith("/jobs") ? json(401, { code: "unauthenticated", message: "Your session expired" }) : loginRoutes(call)));
    const backend = await loggedIn(server);
    const ended = vi.fn();
    backend.onSessionEnded(ended);
    expired = true;
    await expect(backend.jobs()).rejects.toMatchObject({ code: "unauthenticated" });
    expect(ended).toHaveBeenCalledWith("Your session expired");
    expect(backend.session()).toBeNull();
  });

  it("names the watched jobs after hello, resumes from the last id, and stops when the session ends", async () => {
    const server = fakeServer(loginRoutes, [
      streamOf([
        { id: "7-1", type: "hello", stream: "s1" },
        { id: "7-2", type: "quorum", quorate: true },
      ]),
      streamOf([
        { id: "7-3", type: "hello", stream: "s2" },
        { id: "7-4", type: "session_ended", message: "logged out elsewhere" },
      ]),
    ]);
    const backend = await loggedIn(server);
    await backend.watch(["j2", "j1", "j2"]);
    const messages: StreamMessage[] = [];
    const states: StreamState[] = [];
    const ended = vi.fn();
    backend.onSessionEnded(ended);
    backend.subscribe({ message: (m) => messages.push(m), state: (s) => states.push(s) });
    await until(() => ended.mock.calls.length === 1);
    expect(messages.map((m) => m.id)).toEqual(["7-1", "7-2", "7-3", "7-4"]);
    const streams = server.calls.filter((c) => c.url.endsWith("/events"));
    expect(streams).toHaveLength(2);
    expect(streams[0].headers["Last-Event-ID"]).toBeUndefined();
    expect(streams[1].headers["Last-Event-ID"]).toBe("7-2");
    const watches = server.calls.filter((c) => c.url.endsWith("/events/watch")).map((c) => c.body);
    expect(watches).toEqual([{ stream: "s1", jobs: ["j1", "j2"] }, { stream: "s2", jobs: ["j1", "j2"] }]);
    expect(states[0]).toBe("connecting");
    expect(states).toContain("open");
    expect(states.at(-1)).toBe("closed");
    expect(ended).toHaveBeenCalledWith("logged out elsewhere");
    expect(backend.session()).toBeNull();
  });
});
