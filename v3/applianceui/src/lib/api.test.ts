// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

import { describe, expect, it, vi } from "vitest";
import { ApiFailure, createApi } from "./api";

const TOKEN = "A".repeat(43);

function reply(status: number, body: unknown) {
  return new Response(body === undefined ? "" : JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } });
}

describe("the API module", () => {
  it("keeps the token in memory only and sends it as a bearer header", async () => {
    const calls: [string, RequestInit][] = [];
    const fetcher = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      calls.push([String(input), init ?? {}]);
      if (String(input).endsWith("/login")) return reply(200, { token: TOKEN, idle_timeout_seconds: 1800, expires_at_ms: 1 });
      return reply(200, { entries: [] });
    });
    const api = createApi(fetcher as typeof fetch);
    await api.login("correct horse battery");
    await api.audit(5);
    const [url, init] = calls[1];
    expect(url).toBe("/api/v1/audit?limit=5");
    expect((init.headers as Record<string, string>).Authorization).toBe(`Bearer ${TOKEN}`);
    expect(init.credentials).toBe("omit");
    expect(calls[0][1].body).toBe(JSON.stringify({ password: "correct horse battery" }));
    expect((calls[0][1].headers as Record<string, string>)["Content-Type"]).toBe("application/json");
    expect(document.cookie).toBe("");
    expect(localStorage.length).toBe(0);
    expect(sessionStorage.length).toBe(0);
    expect(url).not.toContain(TOKEN);
  });

  it("forgets the token and says so when the server ends the session", async () => {
    let n = 0;
    const fetcher = vi.fn(async () => (n++ === 0 ? reply(200, { token: TOKEN, idle_timeout_seconds: 1800, expires_at_ms: 1 }) : reply(401, { code: "unauthenticated", message: "your session has ended; log in again" })));
    const api = createApi(fetcher as unknown as typeof fetch);
    const ended = vi.fn();
    api.onSessionEnded(ended);
    await api.login("pw");
    expect(api.loggedIn()).toBe(true);
    await expect(api.status()).rejects.toBeInstanceOf(ApiFailure);
    expect(api.loggedIn()).toBe(false);
    expect(ended).toHaveBeenCalledWith("your session has ended; log in again");
  });

  it("carries a rate limit's wait", async () => {
    const api = createApi((async () => reply(429, { code: "rate_limited", message: "too many login attempts.", retry_after_seconds: 42 })) as unknown as typeof fetch);
    const failure = await api.login("pw").catch((e: unknown) => e);
    expect(failure).toBeInstanceOf(ApiFailure);
    expect((failure as ApiFailure).retryAfterSeconds).toBe(42);
    expect(api.loggedIn()).toBe(false);
  });
});
