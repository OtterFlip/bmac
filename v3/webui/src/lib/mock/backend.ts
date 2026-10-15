// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// An in-memory bmacd for development and tests. Tests drive it directly:
// add jobs and events, push stream messages, drop the stream, end the session.

import type {
  Alert,
  Answer,
  ApplianceList,
  BackupsView,
  CertificateView,
  HostFactsRecord,
  HostSlot,
  IssueEnroll,
  IssuedEnroll,
  IssueJoin,
  IssuedJoin,
  JobEvent,
  JobSummary,
  JobView,
  JoinList,
  LaunchJob,
  LogPage,
  LogQuery,
  LogRecord,
  LoginRequest,
  NotifyView,
  OperationView,
  ReplicationView,
  SaveBackupSettings,
  SaveNotifySettings,
  SaveSettings,
  SaveSilence,
  SettingKey,
  SettingsRecord,
  SettingsView,
  SetupView,
  Silence,
  StreamMessage,
  TemplatesView,
  TestEmailResult,
} from "@bmac/ui";
import { ApiFailure, type Backend, type PolicyChange, type SessionInfo, type StreamHandlers, type Unlisten } from "../backend";
import { demoAlerts } from "./alerts";
import { demoAppliances, demoBackups } from "./backups";
import { demoLogs, demoReplication, searchDemo } from "./logs";
import type { ScenarioHooks } from "./scenarios";
import { demoCertificate, demoSettings, fingerprint } from "./settings";
import { demoStatus, type DemoStatus } from "./status";

export interface MockUser {
  password: string;
  totp?: string;
  role: SessionInfo["role"];
}

/** A stream message before the server gives it an id. */
export type StreamBody = StreamMessage extends infer M ? (M extends StreamMessage ? Omit<M, "id"> : never) : never;

interface MockJob {
  summary: JobSummary;
  parameters: Record<string, string>;
  locks: string[];
  events: JobEvent[];
}

export class MockBackend implements Backend {
  readonly kind = "mock" as const;
  users: Record<string, MockUser> = { "alice@pve": { password: "alice", totp: "123456", role: "admin" } };
  operationList: OperationView[] = [];
  /** What the status endpoints return; tests change it, then push an observation. */
  statusData: DemoStatus = demoStatus();
  statusFailures: Partial<Record<keyof DemoStatus, ApiFailure>> = {};
  /** What `settings()` returns; `can_edit` follows the session's role. */
  settingsData: SettingsView = demoSettings();
  /** What `certificate()` returns; `can_rotate` follows the session's role. */
  certificateData: CertificateView = demoCertificate();
  private certificateSeed = 100;
  /** What `joins()` returns; `can_manage` follows the session's role. */
  joinData: JoinList = { joins: [], can_manage: false };
  /** What `backups()` returns; `can_manage` follows the session's role. */
  backupsData: BackupsView = demoBackups();
  /** What `appliances()` returns; `can_manage` follows the session's role. */
  applianceData: ApplianceList = demoAppliances();
  /** What `alerts()` returns; the `can_` flags follow the session's role. */
  alertsData: NotifyView = demoAlerts();
  /** The Postmark token as bmacd would store it; never returned. */
  postmarkToken = "";
  /** Template aliases and whether each is on the Postmark server. */
  templatesData: TemplatesView = {
    server_name: "bmac_lab",
    delivery_type: "Sandbox",
    templates: ["bmac-layout-v1", "bmac-alert-opened-v1", "bmac-alert-resolved-v1", "bmac-digest-v1", "bmac-test-v1", "bmac-watchdog-down-v1", "bmac-watchdog-up-v1", "bmac-url-down-v1", "bmac-url-up-v1"].map((alias) => ({
      alias,
      presence: "missing" as const,
    })),
  };
  /** The stored records `searchLogs()` searches, newest first. */
  logsData: LogRecord[] = demoLogs();
  /** What `logReplication()` returns. */
  replicationData: ReplicationView = demoReplication();
  private nextSilence = 1;
  jobsById = new Map<string, MockJob>();
  /** Scripted jobs still running, by job id (`scenarios.ts`). */
  scenarios = new Map<string, ScenarioHooks>();
  /** Scripted jobs pause at their next step while set, or once they have this many events, for screenshots. */
  holdJobs: boolean | number = false;

  holding(jobId: string) {
    return this.holdJobs === true || (typeof this.holdJobs === "number" && (this.jobsById.get(jobId)?.events.length ?? 0) >= this.holdJobs);
  }
  calls: { method: string; args: unknown[] }[] = [];
  /** Events per page of `job()`, to exercise paging. */
  pageSize = 1000;
  private current: SessionInfo | null = null;
  private pendingUser: string | null = null;
  private ended = new Set<(message: string) => void>();
  private streams = new Set<StreamHandlers>();
  private nextId = 1;
  private nextJob = 1;
  watched: string[] = [];

  private record(method: string, ...args: unknown[]) {
    this.calls.push({ method, args });
  }

  private requireSession() {
    if (!this.current) throw new ApiFailure(401, "unauthenticated", "Log in first.");
  }

  async login(request: LoginRequest) {
    this.record("login", request.step);
    if (request.step === "password") {
      const user = this.users[request.user];
      if (!user || user.password !== request.password) throw new ApiFailure(401, "unauthenticated", "The user name or password is wrong.");
      if (user.totp) {
        this.pendingUser = request.user;
        return { step: "second_factor" as const, challenge: "mock-challenge", methods: ["totp", "recovery"] };
      }
      return this.start(request.user);
    }
    const user = this.pendingUser ? this.users[this.pendingUser] : undefined;
    if (!user || request.challenge !== "mock-challenge" || request.response !== user.totp) throw new ApiFailure(401, "unauthenticated", "That code is wrong; try again.");
    return this.start(this.pendingUser!);
  }

  private start(user: string) {
    this.pendingUser = null;
    this.current = { user, role: this.users[user].role, idle_timeout_seconds: 1800, expires_at_ms: Date.now() + 8 * 3600_000 };
    return { step: "session" as const, session: this.current };
  }

  async logout() {
    this.record("logout");
    this.current = null;
    this.closeStreams();
  }

  session = () => this.current;

  onSessionEnded(callback: (message: string) => void): Unlisten {
    this.ended.add(callback);
    return () => this.ended.delete(callback);
  }

  /** The server ends the session: the stream says so, then calls fail with 401. */
  endSession(message = "Your session expired.") {
    if (!this.current) return;
    this.push({ type: "session_ended", message });
    this.current = null;
    this.closeStreams();
    this.ended.forEach((cb) => cb(message));
  }

  async operations() {
    this.record("operations");
    this.requireSession();
    const rank = { viewer: 0, operator: 1, admin: 2 } as Record<string, number>;
    const role = rank[this.current!.role] ?? 0;
    return this.operationList.map((o) => ({ ...o, allowed: o.allowed && role >= (rank[o.min_role] ?? 0) }));
  }

  /** Tests turn setup mode on by setting `active`. */
  setupView: SetupView = { active: false, operation: "hosts.setup", new_cluster: true };

  async setup() {
    this.record("setup");
    this.requireSession();
    const job = [...this.jobsById.values()]
      .map((j) => j.summary)
      .filter((j) => j.operation === this.setupView.operation)
      .sort((a, b) => b.created_at_ms - a.created_at_ms)[0];
    return structuredClone({ ...this.setupView, job });
  }

  private snapshot<K extends keyof DemoStatus>(kind: K): DemoStatus[K] {
    this.record(kind);
    this.requireSession();
    const failure = this.statusFailures[kind];
    if (failure) throw failure;
    return structuredClone(this.statusData[kind]);
  }

  async cluster() {
    return this.snapshot("cluster");
  }

  async hosts() {
    return this.snapshot("hosts");
  }

  async vms() {
    return this.snapshot("vms");
  }

  async storage() {
    return this.snapshot("storage");
  }

  async settings(): Promise<SettingsView> {
    this.record("settings");
    this.requireSession();
    return { ...structuredClone(this.settingsData), can_edit: this.current!.role === "admin" };
  }

  /** Checks a save the way bmacd does: admins only, current revision, required keys set. */
  private checkSave(current: { revision: number; values: Record<string, string> } | null, keys: SettingKey[], request: SaveSettings) {
    this.requireSession();
    if (this.current!.role !== "admin") throw new ApiFailure(403, "forbidden", "changing settings needs the admin role");
    if ((current?.revision ?? null) !== request.expected_revision)
      throw new ApiFailure(409, "conflict", current ? `these settings are at revision ${current.revision}; reload them and try again` : "these settings don't exist yet; reload them and try again");
    const missing = Object.fromEntries(keys.filter((k) => k.required && !request.values[k.name]).map((k) => [k.name, "required"]));
    if (Object.keys(missing).length) throw new ApiFailure(422, "invalid", "some settings are missing or wrong", missing);
    const previous = current?.values;
    const changed = previous && keys.find((k) => k.locked && previous[k.name] !== request.values[k.name]);
    if (changed)
      throw new ApiFailure(422, "invalid", "some settings are missing or wrong", {
        [changed.name]: `is part of the cluster's address plan and can't change once the cluster exists (it is ${previous[changed.name] ?? "unset"})`,
      });
    return { revision: (current?.revision ?? 0) + 1, values: { ...request.values }, updated_by: this.current!.user, updated_at_ms: Date.now() };
  }

  async saveClusterSettings(request: SaveSettings): Promise<SettingsRecord> {
    this.record("saveClusterSettings", request);
    const saved = this.checkSave(this.settingsData.cluster, this.settingsData.cluster_keys, request);
    this.settingsData.cluster = saved;
    return structuredClone(saved);
  }

  async saveHostFacts(host: string, request: SaveSettings): Promise<HostFactsRecord> {
    this.record("saveHostFacts", host, request);
    const hosts = this.settingsData.hosts;
    const saved = { host: host as HostSlot, ...this.checkSave(hosts.find((h) => h.host === host) ?? null, this.settingsData.host_keys, request) };
    this.settingsData.hosts = [...hosts.filter((h) => h.host !== host), saved].sort((a, b) => a.host.localeCompare(b.host, undefined, { numeric: true }));
    return structuredClone(saved);
  }

  async joins(): Promise<JoinList> {
    this.record("joins");
    this.requireSession();
    return { ...structuredClone(this.joinData), can_manage: this.current!.role === "admin" };
  }

  async issueJoin(request: IssueJoin): Promise<IssuedJoin> {
    this.record("issueJoin", request);
    this.requireAdmin("managing join bundles");
    const now = Date.now();
    const ttl = (request.ttl_hours ?? 168) * 3_600_000;
    this.joinData.joins = [
      ...this.joinData.joins.filter((j) => j.host !== request.host),
      { host: request.host, issued_by: this.current!.user, issued_at_ms: now, expires_at_ms: now + ttl, used_at_ms: null, request: null },
    ];
    return { file_name: `demo-${request.host}-join.json`, content: JSON.stringify({ version: 1, slot: request.host }) };
  }

  async revokeJoin(host: string): Promise<void> {
    this.record("revokeJoin", host);
    this.requireAdmin("managing join bundles");
    if (!this.joinData.joins.some((j) => j.host === host)) throw new ApiFailure(404, "not_found", `${host} has no outstanding join bundle`);
    this.joinData.joins = this.joinData.joins.filter((j) => j.host !== host);
  }

  async backups(): Promise<BackupsView> {
    this.record("backups");
    this.requireSession();
    return { ...structuredClone(this.backupsData), can_manage: this.current!.role === "admin" };
  }

  async appliances(): Promise<ApplianceList> {
    this.record("appliances");
    this.requireSession();
    return { ...structuredClone(this.applianceData), can_manage: this.current!.role === "admin" };
  }

  async issueEnroll(request: IssueEnroll): Promise<IssuedEnroll> {
    this.record("issueEnroll", request);
    this.requireAdmin("managing backup appliances");
    return { file_name: "bmac-appliance-enroll.json", content: JSON.stringify({ version: 1, token: "demo" }) };
  }

  async approveAppliance(requestId: string, fingerprint: string): Promise<void> {
    this.record("approveAppliance", requestId, fingerprint);
    this.requireAdmin("managing backup appliances");
    const request = this.applianceData.requests.find((r) => r.request_id === requestId);
    if (!request) throw new ApiFailure(404, "not_found", "no such enrollment request");
    if (request.fingerprint !== fingerprint) throw new ApiFailure(409, "conflict", "the fingerprint doesn't match the request's certificate");
    this.applianceData.requests = this.applianceData.requests.filter((r) => r.request_id !== requestId);
  }

  async rejectAppliance(requestId: string): Promise<void> {
    this.record("rejectAppliance", requestId);
    this.requireAdmin("managing backup appliances");
    this.applianceData.requests = this.applianceData.requests.filter((r) => r.request_id !== requestId);
  }

  async revokeAppliance(applianceUuid: string): Promise<void> {
    this.record("revokeAppliance", applianceUuid);
    this.requireAdmin("managing backup appliances");
    for (const a of this.backupsData.appliances) if (a.appliance.appliance_uuid === applianceUuid) a.appliance.enrollment = "revoked";
  }

  async setApplianceAddress(applianceUuid: string, address: string | null): Promise<void> {
    this.record("setApplianceAddress", applianceUuid, address);
    this.requireAdmin("managing backup appliances");
    for (const a of this.backupsData.appliances) if (a.appliance.appliance_uuid === applianceUuid) a.appliance.address = address;
  }

  async saveBackupPolicy(change: PolicyChange): Promise<void> {
    this.record("saveBackupPolicy", change);
    this.requireAdmin("changing backup policies");
    const vm = this.backupsData.vms.find((v) => v.vm === change.vm);
    if (!vm) throw new ApiFailure(422, "invalid", `vm: no production VM ${change.vm}`, { vm: `no production VM ${change.vm}` });
    if ((vm.policy?.revision ?? null) !== change.revision) throw new ApiFailure(409, "conflict", "the backup policy changed since it was read; reload and try again");
    const every = change.every as { kind: string; at?: string; minutes?: number };
    vm.policy = {
      revision: (vm.policy?.revision ?? 0) + 1,
      enabled: change.enabled,
      appliances: [...change.appliances],
      local_newest: change.local_retention.newest,
      local_days: change.local_retention.days,
      local_weeks: change.local_retention.weeks,
      every: every.kind === "interval" ? `every ${every.minutes} minutes` : `daily at ${every.at}`,
      every_json: JSON.stringify(change.every),
      freeze: change.freeze,
      accept_multi_pool: change.accept_multi_pool,
      updated_by: this.current!.user,
      updated_at_ms: Date.now(),
    };
  }

  async deleteBackupPolicy(vmUuid: string, revision: number): Promise<void> {
    this.record("deleteBackupPolicy", vmUuid, revision);
    this.requireAdmin("changing backup policies");
    const vm = this.backupsData.vms.find((v) => v.vm_uuid === vmUuid);
    if (!vm?.policy) throw new ApiFailure(404, "not_found", `no backup policy for ${vmUuid}`);
    if (vm.policy.revision !== revision) throw new ApiFailure(409, "conflict", "the backup policy changed since it was read; reload and try again");
    vm.policy = null;
  }

  async saveBackupSettings(request: SaveBackupSettings): Promise<SaveBackupSettings> {
    this.record("saveBackupSettings", request);
    this.requireAdmin("changing backup policies and settings");
    if (request.revision !== this.backupsData.settings_revision) throw new ApiFailure(409, "conflict", "the backup settings changed since they were read; reload and try again");
    const s = request.settings;
    if (s.late_after_s <= s.checkin_interval_s) throw new ApiFailure(422, "invalid", "an appliance must be allowed more than one check-in interval before it is late");
    if (s.delinquency_s <= s.late_after_s) throw new ApiFailure(422, "invalid", "the delinquency period must be longer than the late threshold");
    this.backupsData.settings = { ...s };
    this.backupsData.settings_revision += 1;
    return { settings: { ...s }, revision: this.backupsData.settings_revision };
  }

  async certificate(): Promise<CertificateView> {
    this.record("certificate");
    this.requireSession();
    return { ...structuredClone(this.certificateData), can_rotate: this.current!.role === "admin" };
  }

  private requireAdmin(what: string) {
    this.requireSession();
    if (this.current!.role !== "admin") throw new ApiFailure(403, "forbidden", `${what} needs the admin role`);
  }

  private requireAdminForCertificate() {
    this.requireSession();
    if (this.current!.role !== "admin") throw new ApiFailure(403, "forbidden", "rotating the certificate needs the admin role");
  }

  async rotateCertificate(): Promise<CertificateView> {
    this.record("rotateCertificate");
    this.requireAdminForCertificate();
    const current = this.certificateData.current;
    if (!current) throw new ApiFailure(503, "unavailable", "this host's certificate can't be read, so there are no names to copy");
    const now = Date.now();
    this.certificateData.pending = { fingerprint: fingerprint(this.certificateSeed++), names: [...current.names], not_before_ms: now, not_after_ms: now + 3650 * 86_400_000 };
    return this.certificate();
  }

  async switchCertificate(shown: string): Promise<CertificateView> {
    this.record("switchCertificate", shown);
    this.requireAdminForCertificate();
    const pending = this.certificateData.pending;
    if (!pending) throw new ApiFailure(409, "conflict", "there is no new certificate to switch to; make one first");
    if (pending.fingerprint.toUpperCase() !== shown.trim().toUpperCase())
      throw new ApiFailure(409, "conflict", "the new certificate changed since you saw it; check its fingerprint again");
    this.certificateData = { ...this.certificateData, current: pending, pending: null };
    return this.certificate();
  }

  async discardCertificate(): Promise<CertificateView> {
    this.record("discardCertificate");
    this.requireAdminForCertificate();
    this.certificateData.pending = null;
    return this.certificate();
  }

  private notifyView(): NotifyView {
    const role = this.current!.role;
    return { ...structuredClone(this.alertsData), token_set: this.postmarkToken !== "", can_acknowledge: role !== "viewer", can_edit: role === "admin" };
  }

  private requireRole(role: "operator" | "admin") {
    this.requireSession();
    const ok = role === "admin" ? this.current!.role === "admin" : this.current!.role !== "viewer";
    if (!ok) throw new ApiFailure(403, "forbidden", `this needs the ${role} role`);
  }

  async alerts(): Promise<NotifyView> {
    this.record("alerts");
    this.requireSession();
    return this.notifyView();
  }

  async acknowledgeAlert(id: string): Promise<Alert> {
    this.record("acknowledgeAlert", id);
    this.requireRole("operator");
    const alert = this.alertsData.alerts.find((a) => a.id === id);
    if (!alert) throw new ApiFailure(404, "not_found", "no such alert");
    if (alert.state === "open") Object.assign(alert, { state: "acknowledged", acknowledged_by: this.current!.user, acknowledged_ms: Date.now() });
    return structuredClone(alert);
  }

  async saveSilence(request: SaveSilence): Promise<Silence> {
    this.record("saveSilence", request);
    this.requireRole("operator");
    const now = Date.now();
    if (request.until_ms <= now || request.until_ms - now > 90 * 86_400_000) throw new ApiFailure(422, "invalid", "a silence ends in the future, within 90 days");
    if (!request.reason.trim()) throw new ApiFailure(422, "invalid", "give a reason of up to 200 characters");
    const silence: Silence = { id: `5110000000${String(this.nextSilence++).padStart(2, "0")}`, ...request, reason: request.reason.trim(), created_by: this.current!.user, created_ms: now };
    this.alertsData.silences.push(silence);
    return structuredClone(silence);
  }

  async deleteSilence(id: string): Promise<Silence> {
    this.record("deleteSilence", id);
    this.requireRole("operator");
    const silence = this.alertsData.silences.find((s) => s.id === id);
    if (!silence) throw new ApiFailure(404, "not_found", "no such silence");
    this.alertsData.silences = this.alertsData.silences.filter((s) => s.id !== id);
    return silence;
  }

  async saveNotifySettings(request: SaveNotifySettings): Promise<NotifyView> {
    this.record("saveNotifySettings", request);
    this.requireRole("admin");
    if (request.expected_revision !== this.alertsData.settings_revision)
      throw new ApiFailure(409, "conflict", "the notification settings changed since you opened them; reload them and try again");
    const bad = [request.settings.sender ?? "", ...request.settings.recipients].filter((a) => a && !/^[^@\s]+@[^@\s]+\.[^@\s]+$/.test(a));
    if (bad.length) throw new ApiFailure(422, "invalid", `not an email address: ${bad.join(", ")}`);
    this.alertsData.settings = structuredClone(request.settings);
    this.alertsData.settings_revision = (this.alertsData.settings_revision ?? 0) + 1;
    return this.notifyView();
  }

  async setPostmarkToken(token: string): Promise<NotifyView> {
    this.record("setPostmarkToken", token ? "(a token)" : "(empty)");
    this.requireRole("admin");
    this.postmarkToken = token.trim();
    return this.notifyView();
  }

  async sendTestEmail(to: string | null): Promise<TestEmailResult[]> {
    this.record("sendTestEmail", to);
    this.requireRole("admin");
    if (!this.postmarkToken) throw new ApiFailure(503, "unavailable", "no Postmark server token is set");
    const recipients = to ? [to] : this.alertsData.settings.recipients;
    if (!recipients.length) throw new ApiFailure(422, "invalid", "there are no recipients; add one or give an address");
    return recipients.map((recipient, i) => ({ recipient, message_id: `mock-message-${i + 1}`, error: null }));
  }

  async emailTemplates(): Promise<TemplatesView> {
    this.record("emailTemplates");
    this.requireRole("admin");
    if (!this.postmarkToken) throw new ApiFailure(503, "unavailable", "no Postmark server token is set");
    return structuredClone(this.templatesData);
  }

  async createEmailTemplates(): Promise<TemplatesView> {
    this.record("createEmailTemplates");
    this.requireRole("admin");
    if (!this.postmarkToken) throw new ApiFailure(503, "unavailable", "no Postmark server token is set");
    this.templatesData.templates = this.templatesData.templates.map((t) => (t.presence === "missing" ? { ...t, presence: "present" } : t));
    return structuredClone(this.templatesData);
  }

  async searchLogs(query: LogQuery): Promise<LogPage> {
    this.record("searchLogs", query);
    this.requireSession();
    if (query.before && !/^\d+$/.test(query.before)) throw new ApiFailure(422, "invalid", "the paging cursor is malformed");
    return structuredClone(searchDemo(this.logsData, query));
  }

  async logReplication(): Promise<ReplicationView> {
    this.record("logReplication");
    this.requireSession();
    return structuredClone(this.replicationData);
  }

  async jobs() {
    this.record("jobs");
    this.requireSession();
    return [...this.jobsById.values()].map((j) => j.summary).sort((a, b) => b.created_at_ms - a.created_at_ms);
  }

  async job(id: string, fromSeq: number): Promise<JobView> {
    this.record("job", id, fromSeq);
    this.requireSession();
    const job = this.jobsById.get(id);
    if (!job) throw new ApiFailure(404, "not_found", "No such job.");
    const events = job.events.filter((e) => e.seq >= Math.max(1, fromSeq));
    return { job: job.summary, parameters: job.parameters, locks: job.locks, events: events.slice(0, this.pageSize), more: events.length > this.pageSize };
  }

  async launch(request: LaunchJob) {
    this.record("launch", request);
    this.requireSession();
    const existing = [...this.jobsById.values()].find((j) => j.summary.id === `key-${request.idempotency_key}`);
    if (existing) return existing.summary;
    const operation = this.operationList.find((o) => o.id === request.operation);
    if (!operation) throw new ApiFailure(404, "not_found", "No such operation.");
    const parameters = Object.fromEntries(Object.entries(request.parameters).map(([k, v]) => [k, String(v)]));
    const summary = this.addJob({ id: `key-${request.idempotency_key}`, operation: operation.id, title: operation.title, dry_run: request.dry_run, state: "ADMITTED" }, parameters);
    return summary;
  }

  async answer(job: string, request: string, answer: Answer) {
    this.record("answer", job, request, answer);
    this.requireSession();
    this.scenarios.get(job)?.answer(request, answer);
  }

  async cancel(job: string, force: boolean) {
    this.record("cancel", job, force);
    this.requireSession();
    const scenario = this.scenarios.get(job);
    if (!scenario) return;
    const record = this.jobsById.get(job)!;
    record.summary = { ...record.summary, cancel_requested: true };
    this.pushSummary(job);
    scenario.cancel();
  }

  async resume(job: string) {
    this.record("resume", job);
    this.requireSession();
  }

  async reconcile(job: string) {
    this.record("reconcile", job);
    this.requireSession();
  }

  async watch(jobs: string[]) {
    this.record("watch", jobs);
    this.watched = [...jobs];
  }

  subscribe(handlers: StreamHandlers): Unlisten {
    this.record("subscribe");
    this.streams.add(handlers);
    handlers.state("connecting");
    queueMicrotask(() => {
      if (!this.streams.has(handlers)) return;
      handlers.state("open");
      handlers.message({ id: this.id(), type: "hello", stream: "mock-stream" });
    });
    return () => {
      if (this.streams.delete(handlers)) handlers.state("closed");
    };
  }

  // Test controls

  private id() {
    return `mock-${this.nextId++}`;
  }

  push(body: StreamBody) {
    const message = { id: this.id(), ...body } as StreamMessage;
    this.streams.forEach((s) => s.message(message));
  }

  private closeStreams() {
    for (const s of this.streams) s.state("closed");
    this.streams.clear();
  }

  addJob(over: Partial<JobSummary> = {}, parameters: Record<string, string> = {}): JobSummary {
    const now = Date.now();
    const summary: JobSummary = {
      id: `job-${this.nextJob++}`,
      operation: "lab.interactive",
      title: "Lab: interactive mock",
      state: "RUNNING",
      host: "mox2",
      user: this.current?.user ?? "alice@pve",
      dry_run: false,
      created_at_ms: now,
      updated_at_ms: now,
      cancel_requested: false,
      last_seq: 0,
      ...over,
    };
    this.jobsById.set(summary.id, { summary, parameters, locks: [], events: [] });
    this.push({ type: "job", job: summary });
    return summary;
  }

  /** Records an event; it is streamed only if the job is watched and `stream` is true. */
  addEvent(jobId: string, body: Record<string, unknown>, stream = true): JobEvent {
    const job = this.jobsById.get(jobId)!;
    const event = { job: jobId, seq: job.events.length + 1, at_ms: Date.now(), ...body } as JobEvent;
    job.events.push(event);
    const summary: JobSummary = { ...job.summary, last_seq: event.seq, updated_at_ms: event.at_ms };
    if (event.type === "request") summary.pending_request = event.request_id;
    if (event.type === "answered") delete summary.pending_request;
    if (event.type === "validation_error") summary.pending_request = event.request_id;
    if (["completed", "failed", "cancelled"].includes(event.type)) summary.cancel_requested = false;
    if (event.type === "started") summary.state = "RUNNING";
    if (event.type === "completed") summary.state = "COMPLETED";
    if (event.type === "failed") summary.state = "FAILED";
    if (event.type === "cancelled") summary.state = "CANCELLED";
    job.summary = summary;
    if (stream && this.watched.includes(jobId)) this.push({ type: "job_event", event });
    return event;
  }

  /** Sends the job's current summary on the stream. */
  pushSummary(jobId: string) {
    this.push({ type: "job", job: this.jobsById.get(jobId)!.summary });
  }
}
