// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// Everything the appliance UI asks of bmac-backupd. The session token lives
// in this module's closure and nowhere else: never in a cookie, Web Storage,
// IndexedDB, or a URL (SECURITY.md, "Tokens: memory only"). A reload loses
// it, so the administrator logs in again.

import type {
  ApiError,
  ApplianceHello,
  ApplianceRetentionView,
  ApplianceSession,
  ApplianceStatusView,
  ApplianceTestEmail,
  AuditView,
  GrantView,
  IssueGrant,
  IssueRecoveryGrant,
  IssuedGrant,
  PeersView,
  RecoveryView,
  PitsView,
  PlatformView,
  PruneResult,
  RetentionChange,
  RetentionCounts,
  SavePlatform,
  ScrubView,
  ThroughputView,
  TrustPeer,
  TrustResult,
} from "@bmac/ui";
import { APPLIANCE_API } from "@bmac/ui";

export class ApiFailure extends Error {
  readonly code: ApiError["code"] | "network";
  readonly status: number;
  readonly retryAfterSeconds: number | null;

  constructor(status: number, code: ApiError["code"] | "network", message: string, retryAfterSeconds: number | null = null) {
    super(message);
    this.status = status;
    this.code = code;
    this.retryAfterSeconds = retryAfterSeconds;
  }
}

export function errorMessage(error: unknown): string {
  if (error instanceof ApiFailure) {
    return error.code === "rate_limited" && error.retryAfterSeconds ? `${error.message} Try again in ${error.retryAfterSeconds} seconds.` : error.message;
  }
  return error instanceof Error ? error.message : String(error);
}

export interface Api {
  hello(): Promise<ApplianceHello>;
  login(password: string): Promise<void>;
  logout(): Promise<void>;
  loggedIn(): boolean;
  /** Called once when the server ends the session (expiry, a password change elsewhere). */
  onSessionEnded(callback: (message: string) => void): () => void;
  status(): Promise<ApplianceStatusView>;
  pits(): Promise<PitsView>;
  throughput(): Promise<ThroughputView>;
  retention(): Promise<ApplianceRetentionView>;
  setRetention(policy: RetentionCounts): Promise<RetentionChange>;
  cancelRetention(): Promise<void>;
  hold(pitUuid: string, hold: boolean): Promise<void>;
  prune(): Promise<PruneResult>;
  grants(): Promise<GrantView[]>;
  issueGrant(request: IssueGrant): Promise<IssuedGrant>;
  issueRecoveryGrant(request: IssueRecoveryGrant): Promise<IssuedGrant>;
  revokeGrant(grantId: string): Promise<void>;
  peers(): Promise<PeersView>;
  /** Recovery manifests and LUKS header backups kept per cluster, and its restore tests. */
  recovery(): Promise<RecoveryView>;
  trust(request: TrustPeer): Promise<TrustResult>;
  untrust(applianceUuid: string, password: string): Promise<void>;
  scrub(): Promise<ScrubView | null>;
  startScrub(): Promise<void>;
  settings(): Promise<PlatformView>;
  saveSettings(settings: SavePlatform): Promise<PlatformView>;
  setToken(token: string | null): Promise<void>;
  testEmail(): Promise<ApplianceTestEmail>;
  changePassword(current: string, next: string): Promise<void>;
  audit(limit?: number): Promise<AuditView>;
}

export function createApi(fetcher: typeof fetch = (input, init) => fetch(input, init)): Api {
  let token: string | null = null;
  const ended = new Set<(message: string) => void>();

  const call = async <T>(method: "GET" | "POST", path: string, body?: unknown): Promise<T> => {
    const headers: Record<string, string> = {};
    if (token) headers.Authorization = `Bearer ${token}`;
    if (method === "POST") headers["Content-Type"] = "application/json";
    let response: Response;
    try {
      response = await fetcher(`${APPLIANCE_API}${path}`, {
        method,
        headers,
        body: method === "POST" ? JSON.stringify(body ?? {}) : undefined,
        credentials: "omit",
        cache: "no-store",
        referrerPolicy: "no-referrer",
      });
    } catch (error) {
      throw new ApiFailure(0, "network", `The appliance can't be reached: ${error instanceof Error ? error.message : String(error)}`);
    }
    const text = await response.text();
    const parsed: unknown = text ? JSON.parse(text) : null;
    if (!response.ok) {
      const error = (parsed ?? {}) as Partial<ApiError>;
      const failure = new ApiFailure(response.status, error.code ?? "internal", error.message ?? `HTTP ${response.status}`, error.retry_after_seconds ?? null);
      if (failure.code === "unauthenticated" && token && path !== "/login") {
        token = null;
        for (const callback of ended) callback(failure.message);
      }
      throw failure;
    }
    return parsed as T;
  };

  return {
    hello: () => call("GET", "/hello"),
    async login(password) {
      const session = await call<ApplianceSession>("POST", "/login", { password });
      token = session.token;
    },
    async logout() {
      try {
        if (token) await call("POST", "/logout");
      } finally {
        token = null;
      }
    },
    loggedIn: () => token !== null,
    onSessionEnded(callback) {
      ended.add(callback);
      return () => ended.delete(callback);
    },
    status: () => call("GET", "/status"),
    pits: () => call("GET", "/pits"),
    throughput: () => call("GET", "/throughput"),
    retention: () => call("GET", "/retention"),
    setRetention: (policy) => call("POST", "/retention", { policy }),
    cancelRetention: () => call("POST", "/retention/cancel"),
    hold: (pit_uuid, hold) => call("POST", "/holds", { pit_uuid, hold }),
    prune: () => call("POST", "/prune"),
    grants: () => call("GET", "/grants"),
    issueGrant: (request) => call("POST", "/grants", request),
    issueRecoveryGrant: (request) => call("POST", "/grants/recovery", request),
    revokeGrant: (grant_id) => call("POST", "/grants/revoke", { grant_id }),
    peers: () => call("GET", "/peers"),
    recovery: () => call("GET", "/recovery"),
    trust: (request) => call("POST", "/peers/trust", request),
    untrust: (appliance_uuid, password) => call("POST", "/peers/untrust", { appliance_uuid, password }),
    scrub: () => call("GET", "/scrub"),
    startScrub: () => call("POST", "/scrub/start"),
    settings: () => call("GET", "/settings"),
    saveSettings: (settings) => call("POST", "/settings", settings),
    setToken: (value) => call("POST", "/settings/token", { token: value }),
    testEmail: () => call("POST", "/settings/test-email"),
    changePassword: (current, next) => call("POST", "/password", { current, new: next }),
    audit: (limit = 200) => call("GET", `/audit?limit=${limit}`),
  };
}
