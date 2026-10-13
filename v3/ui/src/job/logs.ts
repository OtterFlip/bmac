// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// Per-job console lines live outside React state: a busy script can print
// thousands of lines, and only the terminal view needs each one. The other
// events are kept here too, summarized as `protocol` lines, so the console
// can show exactly what the script and the Dashboard exchanged.

import type { JobEvent } from "../generated";

export interface LogLine {
  seq: number;
  at_ms: number;
  /** stdout, stderr, or protocol */
  stream: string;
  level: "debug" | "info" | "warning" | "error";
  text: string;
  source?: string;
}

type Listener = (lines: LogLine[], reset: boolean) => void;

const MAX_LINES = 120_000;

export function summarize(event: JobEvent): { text: string; level: LogLine["level"] } {
  switch (event.type) {
    case "started":
      return { text: `started ${event.operation} on ${event.host} for ${event.user}${event.dry_run ? " (dry run)" : ""}`, level: "debug" };
    case "phase":
      return { text: `phase ${event.status}: ${event.label}${event.cancel_allowed === false ? " (cannot be cancelled)" : ""}`, level: "debug" };
    case "progress":
      return {
        text: `progress: ${event.message}${event.total ? ` (${event.current ?? 0}/${event.total}${event.unit ? ` ${event.unit}` : ""})` : ""}`,
        level: "debug",
      };
    case "request":
      switch (event.kind) {
        case "input":
          return { text: `waiting for input: ${event.title ?? event.field.label} [${event.request_id}]`, level: "info" };
        case "input_group":
          return { text: `waiting for input: ${event.title} (${event.fields.length} fields) [${event.request_id}]`, level: "info" };
        case "confirm":
          return { text: `waiting for confirmation (${event.severity}): ${event.title} [${event.request_id}]`, level: "info" };
        case "manual_action":
          return { text: `waiting for manual action: ${event.title} [${event.request_id}]`, level: "info" };
      }
      break;
    case "answered":
      return { text: `answered by ${event.by} [${event.request_id}]`, level: "info" };
    case "validation_error":
      return {
        text: `validation error: ${[event.message, ...Object.entries(event.field_errors).map(([k, v]) => `${k}: ${v}`)].filter(Boolean).join("; ")}`,
        level: "warning",
      };
    case "plan":
      return { text: `plan${event.dry_run ? " (dry run)" : ""}: ${event.title}, ${event.items.length} item(s)`, level: "info" };
    case "warning":
      return { text: `warning: ${event.message}`, level: "warning" };
    case "result":
      return { text: "result received", level: "debug" };
    case "next_step":
      return { text: `next step: ${event.text}`, level: "info" };
    case "completed":
      return { text: `completed${event.message ? `: ${event.message}` : ""}`, level: "info" };
    case "failed":
      return { text: `failed${event.code ? ` [${event.code}]` : ""}: ${event.message}`, level: "error" };
    case "cancelled":
      return { text: `cancelled${event.by ? ` by ${event.by}` : ""}${event.message ? `: ${event.message}` : ""}`, level: "warning" };
    case "interrupted":
      return { text: `interrupted: ${event.message}`, level: "warning" };
    case "output":
      return { text: event.text, level: event.level };
  }
  return { text: "", level: "debug" };
}

function convert(events: JobEvent[], after: number): LogLine[] {
  const out: LogLine[] = [];
  for (const e of events) {
    if (e.seq <= after) continue;
    if (e.type === "output") {
      out.push({ seq: e.seq, at_ms: e.at_ms, stream: e.stream ?? "stdout", level: e.level, text: e.text, source: e.source });
    } else {
      const { text, level } = summarize(e);
      out.push({ seq: e.seq, at_ms: e.at_ms, stream: "protocol", level, text });
    }
  }
  return out;
}

export class LogBuffers {
  private lines = new Map<string, LogLine[]>();
  private listeners = new Map<string, Set<Listener>>();

  get(jobId: string): LogLine[] {
    return this.lines.get(jobId) ?? [];
  }

  /** Adds events in sequence order; ones already held are skipped. */
  append(jobId: string, events: JobEvent[]) {
    const list = this.lines.get(jobId) ?? [];
    const last = list.length ? list[list.length - 1].seq : 0;
    const added = convert([...events].sort((a, b) => a.seq - b.seq), last);
    if (!added.length) return;
    list.push(...added);
    if (list.length > MAX_LINES) list.splice(0, list.length - MAX_LINES);
    this.lines.set(jobId, list);
    this.listeners.get(jobId)?.forEach((cb) => cb(added, false));
  }

  /** Forgets a job's lines, as when the browser's copy is too far behind to resume. */
  reset(jobId: string) {
    this.lines.delete(jobId);
    this.listeners.get(jobId)?.forEach((cb) => cb([], true));
  }

  /** Forgets every job's lines, as when the session ends. */
  clear() {
    const held = [...this.lines.keys()];
    this.lines.clear();
    for (const jobId of held) this.listeners.get(jobId)?.forEach((cb) => cb([], true));
  }

  subscribe(jobId: string, listener: Listener) {
    const set = this.listeners.get(jobId) ?? new Set();
    set.add(listener);
    this.listeners.set(jobId, set);
    return () => {
      set.delete(listener);
    };
  }

  toText(jobId: string, withProtocol: boolean) {
    return this.get(jobId)
      .filter((l) => withProtocol || l.stream !== "protocol")
      .map((l) => (l.stream === "protocol" ? `◆ ${l.text}` : l.text))
      .join("\n");
  }
}

export const logs = new LogBuffers();
