// Per-run console lines live outside React state: a busy script can print
// thousands of lines, and only the terminal view needs each one. Structured
// protocol events are kept here too, summarized as `protocol` lines, so the
// console can show exactly what the script and the dashboard exchanged.

import type { RunEvent, WorkflowEvent } from "@/protocol/types";

export interface LogLine {
  seq: number;
  at: string;
  /** stdout, stderr, or protocol */
  stream: string;
  level: "debug" | "info" | "warning" | "error";
  text: string;
  source?: string;
}

type Listener = (lines: LogLine[], reset: boolean) => void;

const MAX_LINES = 120_000;

export function summarize(event: WorkflowEvent): { text: string; level: LogLine["level"] } {
  switch (event.type) {
    case "protocol":
      return { text: `protocol ${event.protocol} v${event.version}`, level: "debug" };
    case "workflow_started":
      return { text: `started ${event.script ?? event.workflow} (pid ${event.pid ?? "?"})`, level: "debug" };
    case "workflow_metadata":
      return { text: `metadata ${event.title ?? event.workflow ?? ""}`, level: "debug" };
    case "phase":
      return { text: `phase ${event.status}: ${event.label}${event.cancel_allowed === false ? " (cannot be cancelled)" : ""}`, level: "debug" };
    case "progress":
      return {
        text: `progress: ${event.message}${event.total ? ` (${event.current ?? 0}/${event.total}${event.unit ? ` ${event.unit}` : ""})` : ""}`,
        level: "debug",
      };
    case "input":
      return { text: `waiting for input: ${event.title ?? event.field.label} [${event.request_id}]`, level: "info" };
    case "input_group":
      return { text: `waiting for input: ${event.title} (${event.fields.length} fields) [${event.request_id}]`, level: "info" };
    case "confirm":
      return { text: `waiting for confirmation (${event.severity}): ${event.title} [${event.request_id}]`, level: "info" };
    case "manual_action":
      return { text: `waiting for manual action: ${event.title} [${event.request_id}]`, level: "info" };
    case "validation_error":
      return {
        text: `validation error: ${[event.message, ...Object.entries(event.field_errors).map(([k, v]) => `${k}: ${v}`)].filter(Boolean).join("; ")}`,
        level: "warning",
      };
    case "plan":
      return { text: `plan${event.dry_run ? " (dry run)" : ""}: ${event.title}, ${event.items.length} item(s)`, level: "info" };
    case "info":
      return { text: `info: ${event.message}`, level: "info" };
    case "warning":
      return { text: `warning: ${event.message}`, level: "warning" };
    case "error":
      return { text: `error${event.code ? ` [${event.code}]` : ""}: ${event.message}`, level: "error" };
    case "result":
      return { text: "result received", level: "debug" };
    case "next_step":
      return { text: `next step: ${event.text}`, level: "info" };
    case "completed":
      return {
        text: `completed: ${event.status}${event.exit_code !== undefined ? ` (exit ${event.exit_code})` : ""}${event.message ? ` - ${event.message}` : ""}`,
        level: event.status === "success" ? "info" : event.status === "failed" ? "error" : "warning",
      };
    case "log":
      return { text: event.text, level: event.level };
  }
}

class LogBuffers {
  private lines = new Map<string, LogLine[]>();
  private listeners = new Map<string, Set<Listener>>();
  private loaded = new Set<string>();

  get(runId: string): LogLine[] {
    return this.lines.get(runId) ?? [];
  }

  isLoaded(runId: string) {
    return this.loaded.has(runId);
  }

  private convert(items: RunEvent[], after: number): LogLine[] {
    const out: LogLine[] = [];
    for (const item of items) {
      if (item.seq <= after) continue;
      const e = item.event;
      if (e.type === "log") {
        out.push({ seq: item.seq, at: item.at, stream: e.stream ?? "stdout", level: e.level, text: e.text, source: e.source });
      } else {
        const { text, level } = summarize(e);
        out.push({ seq: item.seq, at: item.at, stream: "protocol", level, text });
      }
    }
    return out;
  }

  append(runId: string, events: RunEvent[]) {
    const list = this.lines.get(runId) ?? [];
    const last = list.length ? list[list.length - 1].seq : 0;
    const added = this.convert(events, last);
    if (!added.length) return;
    list.push(...added);
    if (list.length > MAX_LINES) list.splice(0, list.length - MAX_LINES);
    this.lines.set(runId, list);
    this.listeners.get(runId)?.forEach((cb) => cb(added, false));
  }

  /** Replace a run's lines with the engine's full copy, keeping newer live lines. */
  load(runId: string, events: RunEvent[]) {
    const live = this.lines.get(runId) ?? [];
    const merged = this.convert([...events].sort((a, b) => a.seq - b.seq), 0);
    const last = merged.length ? merged[merged.length - 1].seq : 0;
    merged.push(...live.filter((l) => l.seq > last));
    this.lines.set(runId, merged);
    this.loaded.add(runId);
    this.listeners.get(runId)?.forEach((cb) => cb(merged, true));
  }

  subscribe(runId: string, listener: Listener) {
    const set = this.listeners.get(runId) ?? new Set();
    set.add(listener);
    this.listeners.set(runId, set);
    return () => {
      set.delete(listener);
    };
  }

  toText(runId: string, withProtocol: boolean) {
    return this.get(runId)
      .filter((l) => withProtocol || l.stream !== "protocol")
      .map((l) => (l.stream === "protocol" ? `◆ ${l.text}` : l.text))
      .join("\n");
  }
}

export const logs = new LogBuffers();
