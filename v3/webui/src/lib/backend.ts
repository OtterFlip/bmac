// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// Everything the Web Dashboard asks of bmacd, over the JSON API and the
// event stream (ARCHITECTURE.md, "API" and "Live updates").
//
// The session token lives in this module's closure and nowhere else: never
// in a cookie, Web Storage, IndexedDB, or a URL (SECURITY.md). A reload
// loses it, so the user logs in again.

import type {
  Alert,
  Answer,
  ApiError,
  CertificateView,
  ClusterSnapshot,
  ErrorCode,
  HostFactsRecord,
  HostsSnapshot,
  IssueJoin,
  IssuedJoin,
  JobList,
  JobSummary,
  JobView,
  JoinList,
  LaunchJob,
  LogPage,
  LogQuery,
  LoginRequest,
  LoginResponse,
  NotifyView,
  OperationList,
  OperationView,
  ReplicationView,
  Role,
  SaveNotifySettings,
  SaveSettings,
  SaveSilence,
  SettingsRecord,
  SettingsView,
  SetupView,
  Silence,
  StorageSnapshot,
  StreamMessage,
  TemplatesView,
  TestEmailResult,
  VmsSnapshot,
} from "@bmac/ui";
import { API_PREFIX, KEEPALIVE_SECONDS } from "@bmac/ui";

export type Unlisten = () => void;

/** Who is logged in; the token itself never leaves the backend. */
export interface SessionInfo {
  user: string;
  role: Role;
  idle_timeout_seconds: number;
  expires_at_ms: number;
}

export type StreamState = "connecting" | "open" | "reconnecting" | "closed";

export interface StreamHandlers {
  message(message: StreamMessage): void;
  state(state: StreamState): void;
}

export interface Backend {
  kind: "http" | "mock";
  /** A password step may return a second factor challenge; the last step starts the session. */
  login(request: LoginRequest): Promise<{ step: "second_factor"; challenge: string; methods: string[] } | { step: "session"; session: SessionInfo }>;
  logout(): Promise<void>;
  session(): SessionInfo | null;
  /** Called once when the session ends for any reason other than logout. */
  onSessionEnded(callback: (message: string) => void): Unlisten;
  operations(): Promise<OperationView[]>;
  /** Whether this host is still being set up; while it is, only the setup job is offered. */
  setup(): Promise<SetupView>;
  cluster(): Promise<ClusterSnapshot>;
  hosts(): Promise<HostsSnapshot>;
  vms(): Promise<VmsSnapshot>;
  storage(): Promise<StorageSnapshot>;
  settings(): Promise<SettingsView>;
  /** Fails with `conflict` when someone saved since `expected_revision`. */
  saveClusterSettings(request: SaveSettings): Promise<SettingsRecord>;
  saveHostFacts(host: string, request: SaveSettings): Promise<HostFactsRecord>;
  /** The certificate of the host serving this page, and a pending new one. */
  /** Join bundles this cluster issued, with the requests made with them. */
  joins(): Promise<JoinList>;
  /** Issues a bundle; its content holds a single-use token and is shown once. */
  issueJoin(request: IssueJoin): Promise<IssuedJoin>;
  revokeJoin(host: string): Promise<void>;
  certificate(): Promise<CertificateView>;
  rotateCertificate(): Promise<CertificateView>;
  /** Fails with `conflict` unless `fingerprint` is the pending certificate's. */
  switchCertificate(fingerprint: string): Promise<CertificateView>;
  discardCertificate(): Promise<CertificateView>;
  /** Alerts, silences, and the notification settings; never the Postmark token. */
  alerts(): Promise<NotifyView>;
  acknowledgeAlert(id: string): Promise<Alert>;
  saveSilence(request: SaveSilence): Promise<Silence>;
  deleteSilence(id: string): Promise<Silence>;
  /** Fails with `conflict` when someone saved since `expected_revision`. */
  saveNotifySettings(request: SaveNotifySettings): Promise<NotifyView>;
  /** Stores the token on the cluster; an empty token removes it. Nothing here keeps it. */
  setPostmarkToken(token: string): Promise<NotifyView>;
  /** `to` absent sends to the default recipients. */
  sendTestEmail(to: string | null): Promise<TestEmailResult[]>;
  emailTemplates(): Promise<TemplatesView>;
  createEmailTemplates(): Promise<TemplatesView>;
  /** Stored records from every host, newest first; pass `next` as `before` for older ones. */
  searchLogs(query: LogQuery): Promise<LogPage>;
  /** What this host holds of each origin, how far its peers have copied, and retention. */
  logReplication(): Promise<ReplicationView>;
  jobs(): Promise<JobSummary[]>;
  job(id: string, fromSeq: number): Promise<JobView>;
  launch(request: LaunchJob): Promise<JobSummary>;
  answer(job: string, request: string, answer: Answer): Promise<void>;
  cancel(job: string, force: boolean): Promise<void>;
  resume(job: string): Promise<void>;
  /** Marks an interrupted job failed after someone checked the live state. */
  reconcile(job: string): Promise<void>;
  /** The jobs whose events this tab's stream carries; replaces the set. */
  watch(jobs: string[]): Promise<void>;
  /** Opens the event stream; it reconnects until unlistened or the session ends. */
  subscribe(handlers: StreamHandlers): Unlisten;
}

export class ApiFailure extends Error {
  constructor(
    readonly status: number,
    readonly code: ErrorCode | "network",
    message: string,
    readonly fieldErrors: Record<string, string> = {},
    readonly retryAfterSeconds?: number,
  ) {
    super(message);
  }
}

export function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  return String(error);
}

/** Splits Server-Sent Events text into complete frames and what is left over. */
export function parseSse(text: string): { frames: { id?: string; data: string }[]; rest: string } {
  const frames: { id?: string; data: string }[] = [];
  const normalized = text.replace(/\r\n?/g, "\n");
  const parts = normalized.split("\n\n");
  const rest = parts.pop() ?? "";
  for (const part of parts) {
    let id: string | undefined;
    const data: string[] = [];
    for (const line of part.split("\n")) {
      if (!line || line.startsWith(":")) continue;
      const colon = line.indexOf(":");
      const field = colon < 0 ? line : line.slice(0, colon);
      const value = colon < 0 ? "" : line.slice(colon + 1).replace(/^ /, "");
      if (field === "id") id = value;
      else if (field === "data") data.push(value);
    }
    if (data.length) frames.push({ id, data: data.join("\n") });
  }
  return { frames, rest };
}

/** Reconnect delays: quick at first, then at most every 15 seconds. */
const RETRY_MS = [500, 1000, 2000, 5000, 10000, 15000];
/** A stream silent this long (no data, no keep-alive) is reconnected. */
const SILENCE_MS = (KEEPALIVE_SECONDS * 2 + 5) * 1000;

export function createHttpBackend(fetcher: typeof fetch = (...args) => fetch(...args), base = ""): Backend {
  let token: string | null = null;
  let session: SessionInfo | null = null;
  const ended = new Set<(message: string) => void>();
  let streamName: string | null = null;
  let watched: string[] = [];

  const endSession = (message: string) => {
    if (!token) return;
    token = null;
    session = null;
    streamName = null;
    ended.forEach((cb) => cb(message));
  };

  async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
    const headers: Record<string, string> = { Accept: "application/json" };
    if (body !== undefined) headers["Content-Type"] = "application/json";
    if (token) headers.Authorization = `Bearer ${token}`;
    let response: Response;
    try {
      response = await fetcher(`${base}${API_PREFIX}${path}`, {
        method,
        headers,
        body: body === undefined ? undefined : JSON.stringify(body),
        cache: "no-store",
        credentials: "omit",
        redirect: "error",
      });
    } catch (error) {
      throw new ApiFailure(0, "network", `Could not reach this host: ${errorMessage(error)}`);
    }
    const text = await response.text();
    let parsed: unknown = undefined;
    if (text) {
      try {
        parsed = JSON.parse(text);
      } catch {
        parsed = undefined;
      }
    }
    if (!response.ok) {
      const error = (parsed ?? {}) as Partial<ApiError>;
      const failure = new ApiFailure(
        response.status,
        error.code ?? "internal",
        error.message ?? `HTTP ${response.status}`,
        error.field_errors ?? {},
        error.retry_after_seconds,
      );
      if (failure.code === "unauthenticated" && token) endSession(failure.message);
      throw failure;
    }
    return parsed as T;
  }

  const sendWatch = async () => {
    if (!streamName || !token) return;
    await request("POST", "/events/watch", { stream: streamName, jobs: watched });
  };

  return {
    kind: "http",

    async login(req) {
      const response = await request<LoginResponse>("POST", "/login", req);
      if (response.step === "second_factor") return { step: "second_factor", challenge: response.challenge, methods: response.methods };
      token = response.token;
      session = { user: response.user, role: response.role, idle_timeout_seconds: response.idle_timeout_seconds, expires_at_ms: response.expires_at_ms };
      return { step: "session", session };
    },

    async logout() {
      if (!token) return;
      try {
        await request("POST", "/logout", {});
      } finally {
        token = null;
        session = null;
        streamName = null;
      }
    },

    session: () => session,

    onSessionEnded(callback) {
      ended.add(callback);
      return () => ended.delete(callback);
    },

    operations: async () => (await request<OperationList>("GET", "/operations")).operations,
    setup: () => request<SetupView>("GET", "/setup"),
    cluster: () => request<ClusterSnapshot>("GET", "/cluster"),
    hosts: () => request<HostsSnapshot>("GET", "/hosts"),
    vms: () => request<VmsSnapshot>("GET", "/vms"),
    storage: () => request<StorageSnapshot>("GET", "/storage"),
    settings: () => request<SettingsView>("GET", "/settings"),
    saveClusterSettings: (req) => request<SettingsRecord>("PUT", "/settings/cluster", req),
    saveHostFacts: (host, req) => request<HostFactsRecord>("PUT", `/settings/hosts/${encodeURIComponent(host)}`, req),
    joins: () => request<JoinList>("GET", "/joins"),
    issueJoin: (req) => request<IssuedJoin>("POST", "/joins", req),
    revokeJoin: async (host) => {
      await request("POST", `/joins/${encodeURIComponent(host)}/revoke`, {});
    },
    certificate: () => request<CertificateView>("GET", "/certificate"),
    rotateCertificate: () => request<CertificateView>("POST", "/certificate/rotate", {}),
    switchCertificate: (fingerprint) => request<CertificateView>("POST", "/certificate/switch", { fingerprint }),
    discardCertificate: () => request<CertificateView>("POST", "/certificate/discard", {}),
    alerts: () => request<NotifyView>("GET", "/alerts"),
    acknowledgeAlert: (id) => request<Alert>("POST", `/alerts/${encodeURIComponent(id)}/acknowledge`, {}),
    saveSilence: (req) => request<Silence>("POST", "/silences", req),
    deleteSilence: (id) => request<Silence>("POST", `/silences/${encodeURIComponent(id)}/delete`, {}),
    saveNotifySettings: (req) => request<NotifyView>("PUT", "/notify/settings", req),
    setPostmarkToken: (token) => request<NotifyView>("PUT", "/notify/token", { token }),
    sendTestEmail: (to) => request<TestEmailResult[]>("POST", "/notify/test", { to }),
    emailTemplates: () => request<TemplatesView>("GET", "/notify/templates"),
    createEmailTemplates: () => request<TemplatesView>("POST", "/notify/templates", {}),
    searchLogs: (query) => request<LogPage>("POST", "/logs/search", query),
    logReplication: () => request<ReplicationView>("GET", "/logs/replication"),
    jobs: async () => (await request<JobList>("GET", "/jobs")).jobs,
    job: (id, fromSeq) => request<JobView>("GET", `/jobs/${encodeURIComponent(id)}${fromSeq > 0 ? `?from_seq=${fromSeq}` : ""}`),
    launch: (req) => request<JobSummary>("POST", "/jobs", req),
    answer: async (job, req, answer) => {
      await request("POST", `/jobs/${encodeURIComponent(job)}/requests/${encodeURIComponent(req)}`, answer);
    },
    cancel: async (job, force) => {
      await request("POST", `/jobs/${encodeURIComponent(job)}/cancel`, { force });
    },
    resume: async (job) => {
      await request("POST", `/jobs/${encodeURIComponent(job)}/resume`, {});
    },
    reconcile: async (job) => {
      await request("POST", `/jobs/${encodeURIComponent(job)}/reconcile`, {});
    },
    async watch(jobs) {
      watched = [...new Set(jobs)].sort();
      await sendWatch();
    },

    subscribe(handlers) {
      let stopped = false;
      let lastId: string | null = null;
      let attempt = 0;
      let controller: AbortController | null = null;
      let timer: ReturnType<typeof setTimeout> | null = null;

      const finish = () => {
        stopped = true;
        controller?.abort();
        if (timer) clearTimeout(timer);
        handlers.state("closed");
      };

      const connect = async () => {
        if (stopped) return;
        if (!token) return finish();
        handlers.state(attempt === 0 ? "connecting" : "reconnecting");
        controller = new AbortController();
        let silence: ReturnType<typeof setTimeout> | null = null;
        const quiet = () => {
          if (silence) clearTimeout(silence);
          silence = setTimeout(() => controller?.abort(), SILENCE_MS);
        };
        try {
          const headers: Record<string, string> = { Accept: "text/event-stream", Authorization: `Bearer ${token}` };
          if (lastId) headers["Last-Event-ID"] = lastId;
          const response = await fetcher(`${base}${API_PREFIX}/events`, {
            headers,
            cache: "no-store",
            credentials: "omit",
            redirect: "error",
            signal: controller.signal,
          });
          if (!response.ok || !response.body) {
            const error = (await response.json().catch(() => ({}))) as Partial<ApiError>;
            if (response.status === 401) return endSession(error.message ?? "Your session ended; log in again.");
            if (response.status === 400) lastId = null;
            throw new Error(error.message ?? `HTTP ${response.status}`);
          }
          const reader = response.body.getReader();
          const decoder = new TextDecoder();
          let buffer = "";
          quiet();
          for (;;) {
            const { value, done } = await reader.read();
            if (done) break;
            quiet();
            buffer += decoder.decode(value, { stream: true });
            const { frames, rest } = parseSse(buffer);
            buffer = rest;
            for (const frame of frames) {
              let message: StreamMessage;
              try {
                message = JSON.parse(frame.data) as StreamMessage;
              } catch {
                continue;
              }
              if (frame.id) lastId = frame.id;
              if (message.type === "hello") {
                attempt = 0;
                streamName = message.stream;
                handlers.state("open");
                void sendWatch().catch(() => {});
              }
              if (message.type === "session_ended") {
                handlers.message(message);
                return endSession(message.message);
              }
              handlers.message(message);
            }
          }
        } catch {
          // Dropped or aborted; reconnect below unless stopped.
        } finally {
          if (silence) clearTimeout(silence);
        }
        if (stopped || !token) return finish();
        const delay = RETRY_MS[Math.min(attempt, RETRY_MS.length - 1)];
        attempt += 1;
        handlers.state("reconnecting");
        timer = setTimeout(() => void connect(), delay);
      };

      const unsubscribeEnded = this.onSessionEnded(() => finish());
      void connect();
      return () => {
        unsubscribeEnded();
        finish();
      };
    },
  };
}
