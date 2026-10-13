// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// Demo records for the Logs page and Log replication: three hosts, a
// failed job, journal entries, audit events, and one peer behind.

import type { LogKind, LogQuery, LogRecord, LogSeverity, ReplicationView } from "@bmac/ui";

const MINUTE = 60_000;
const JOB = "7d2f1c9e3b8a4f60a1c2d3e4f5a6b7c8";

function record(origin: string, seq: number, time_ms: number, kind: LogKind, severity: LogSeverity, summary: string, body: Record<string, unknown>, extra: Partial<LogRecord> = {}): LogRecord {
  return { origin, stream: `${origin}-19a2b3c4d5e`, seq, time_ms, kind, severity, job: null, unit: null, summary, body, evicted: false, ...extra };
}

export function demoLogs(now = Date.now()): LogRecord[] {
  const out: LogRecord[] = [
    record("mox1", 1, now - 50 * MINUTE, "audit", "info", "login by alice@pve: signed in", { kind: "login", user: "alice@pve", message: "signed in" }),
    record("mox2", 1, now - 40 * MINUTE, "journal", "error", "corosync.service: link 0 down", { unit: "corosync.service", priority: 3, message: "link 0 down" }, { unit: "corosync.service" }),
    record("mox3", 1, now - 35 * MINUTE, "journal", "warning", "zfs-zed.service: pool tank degraded", { unit: "zfs-zed.service", priority: 4, message: "pool tank degraded" }, { unit: "zfs-zed.service" }),
    record("mox2", 2, now - 30 * MINUTE, "log", "warning", "peer mox3 unreachable", { level: "warning", message: "peer mox3 unreachable" }),
  ];
  for (let i = 1; i <= 120; i++) {
    out.push(record("mox1", 1 + i, now - 30 * MINUTE + i * 1000, "job_event", "info", `output: copying block ${i}`, { type: "output", level: "info", text: `copying block ${i}` }, { job: JOB }));
  }
  out.push(record("mox1", 122, now - 20 * MINUTE, "job_event", "error", "failed: Disk Vanished", { type: "failed", message: "Disk Vanished" }, { job: JOB }));
  out.push({ ...record("mox3", 2, now - 25 * 24 * 60 * MINUTE, "job_event", "info", "(evicted for space)", {}), body: null, evicted: true });
  return out.sort((a, b) => b.time_ms - a.time_ms || (a.origin < b.origin ? 1 : -1) || b.seq - a.seq);
}

const RANK: Record<LogSeverity, number> = { info: 0, warning: 1, error: 2 };

/** The mock's search: the server's filters over `records`, newest first, paged by index. */
export function searchDemo(records: LogRecord[], query: LogQuery): { records: LogRecord[]; next: string | null } {
  const text = query.text?.trim().toLowerCase();
  const matching = records.filter(
    (r) =>
      (!query.host || r.origin === query.host) &&
      (!query.job || r.job === query.job) &&
      (!query.kinds?.length || query.kinds.includes(r.kind)) &&
      (!query.min_severity || RANK[r.severity] >= RANK[query.min_severity]) &&
      (!query.unit || r.unit === query.unit) &&
      (query.from_ms == null || r.time_ms >= query.from_ms) &&
      (query.to_ms == null || r.time_ms <= query.to_ms) &&
      (!text || JSON.stringify(r.body ?? "").toLowerCase().includes(text)),
  );
  const start = query.before ? Number(query.before) : 0;
  const limit = Math.min(query.limit ?? 100, 500);
  const page = matching.slice(start, start + limit);
  return { records: page, next: start + limit < matching.length ? String(start + limit) : null };
}

export function demoReplication(now = Date.now()): ReplicationView {
  const origin = (name: string, records: number, bytes: number, gaps = 0) => ({
    origin: name,
    streams: 2,
    records,
    bytes,
    oldest_ms: now - 29 * 24 * 60 * MINUTE,
    newest_ms: now - MINUTE,
    latest_stream: `${name}-19a2b3c4d5e`,
    last_seq: records,
    gaps,
  });
  return {
    host: "mox1",
    origins: [origin("mox1", 18_204, 41_000_000), origin("mox2", 9_120, 12_500_000), origin("mox3", 7_310, 9_800_000, 12)],
    peers: [
      {
        peer: "mox2",
        checked_ms: now - 20_000,
        error: null,
        origins: [
          { origin: "mox1", last_seq: 18_204, behind_records: 0, behind_ms: null, gaps: 0 },
          { origin: "mox2", last_seq: 9_120, behind_records: 0, behind_ms: null, gaps: 0 },
          { origin: "mox3", last_seq: 7_310, behind_records: 0, behind_ms: null, gaps: 0 },
        ],
      },
      {
        peer: "mox3",
        checked_ms: now - 9 * MINUTE,
        error: "mox3 is unreachable: connection refused",
        origins: [
          { origin: "mox1", last_seq: 17_950, behind_records: 254, behind_ms: 9 * MINUTE, gaps: 0 },
          { origin: "mox2", last_seq: 9_001, behind_records: 119, behind_ms: 9 * MINUTE, gaps: 0 },
          { origin: "mox3", last_seq: 7_310, behind_records: 0, behind_ms: null, gaps: 0 },
        ],
      },
    ],
    retention: { keep_days: 30, budget_bytes: 8 * 1024 ** 3, used_bytes: 63_300_000, audit_evicted_ms: null },
  };
}
