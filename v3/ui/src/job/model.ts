// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// A job as the job view shows it: the record from bmacd with its events
// folded in. The UI never computes cluster state; this only arranges what
// the server sent.

import type { JobEvent, JobSummary, JobView, JsonValue, NextStep, PhaseStatus } from "../generated";

type Of<T extends JobEvent["type"]> = Extract<JobEvent, { type: T }>;
export type RequestEvent = Of<"request">;
export type PlanEvent = Of<"plan">;
export type ProgressEvent = Of<"progress">;
export type OutputEvent = Of<"output">;

/** v2's run statuses, which the job view's components are written against. */
export type JobStatus =
  | "starting"
  | "running"
  | "waiting_input"
  | "waiting_confirmation"
  | "waiting_manual_action"
  | "cancelling"
  | "succeeded"
  | "failed"
  | "cancelled"
  | "interrupted";

export const TERMINAL_STATUSES: JobStatus[] = ["succeeded", "failed", "cancelled", "interrupted"];
export const isTerminal = (status: JobStatus) => TERMINAL_STATUSES.includes(status);
export const isWaiting = (status: JobStatus) =>
  status === "waiting_input" || status === "waiting_confirmation" || status === "waiting_manual_action";

export interface PhaseState {
  id: string;
  label: string;
  status: PhaseStatus;
  cancel_allowed: boolean;
  started_at_ms: number | null;
  ended_at_ms: number | null;
}

export interface ResponseRecord {
  request_id: string;
  at_ms: number;
  title: string;
  by: string;
  values?: Record<string, JsonValue>;
}

export interface JobRecord {
  summary: JobSummary;
  parameters: Record<string, string>;
  locks: string[];
  /** Every event but output, which goes to the log view. */
  events: JobEvent[];
  /** The last sequence folded in; later fetches start after it. */
  seq: number;
  started_at_ms: number;
  ended_at_ms: number | null;
  phases: PhaseState[];
  plan: PlanEvent | null;
  progress: ProgressEvent | null;
  pending_request: RequestEvent | null;
  /** The server's objections to the last answer, by field, and overall. */
  field_errors: Record<string, string>;
  validation_message: string | null;
  /** The sequence of the last validation error, so a form knows a new one arrived. */
  validation_seq: number;
  result: JsonValue | undefined;
  next_steps: NextStep[];
  errors: string[];
  warnings: string[];
  responses: ResponseRecord[];
  log_lines: number;
  last_log: string | null;
}

const ENDED = new Set(["COMPLETED", "FAILED", "CANCELLED", "INTERRUPTED"]);

export function jobStatus(summary: JobSummary, pending: RequestEvent | null = null): JobStatus {
  switch (summary.state) {
    case "ADMITTED":
      return summary.cancel_requested ? "cancelling" : "starting";
    case "RUNNING":
      if (summary.cancel_requested) return "cancelling";
      if (pending && summary.pending_request === pending.request_id) {
        if (pending.kind === "confirm") return "waiting_confirmation";
        if (pending.kind === "manual_action") return "waiting_manual_action";
        return "waiting_input";
      }
      return summary.pending_request ? "waiting_input" : "running";
    case "COMPLETED":
      return "succeeded";
    case "FAILED":
      return "failed";
    case "CANCELLED":
      return "cancelled";
    case "INTERRUPTED":
      return "interrupted";
  }
}

export const recordStatus = (record: JobRecord) => jobStatus(record.summary, record.pending_request);

export function newRecord(view: Pick<JobView, "job" | "parameters" | "locks">): JobRecord {
  return {
    summary: view.job,
    parameters: view.parameters,
    locks: view.locks,
    events: [],
    seq: 0,
    started_at_ms: view.job.created_at_ms,
    ended_at_ms: ENDED.has(view.job.state) ? view.job.updated_at_ms : null,
    phases: [],
    plan: null,
    progress: null,
    pending_request: null,
    field_errors: {},
    validation_message: null,
    validation_seq: 0,
    result: undefined,
    next_steps: [],
    errors: [],
    warnings: [],
    responses: [],
    log_lines: 0,
    last_log: null,
  };
}

function requestTitle(request: RequestEvent): string {
  return ("title" in request && request.title) || ("field" in request ? request.field.label : "") || "Request";
}

/** Folds one event in; events at or before the record's sequence are ignored. */
export function applyEvent(record: JobRecord, event: JobEvent): JobRecord {
  if (event.seq <= record.seq) return record;
  const next: JobRecord = { ...record, seq: event.seq };
  if (event.type !== "output") next.events = [...record.events, event];
  switch (event.type) {
    case "started":
      next.started_at_ms = event.at_ms;
      break;
    case "phase": {
      const at = event.at_ms;
      const existing = record.phases.find((p) => p.id === event.id);
      const phase: PhaseState = {
        id: event.id,
        label: event.label,
        status: event.status,
        cancel_allowed: event.cancel_allowed ?? existing?.cancel_allowed ?? true,
        started_at_ms: existing?.started_at_ms ?? (event.status === "pending" ? null : at),
        ended_at_ms: event.status === "complete" || event.status === "failed" || event.status === "skipped" ? at : null,
      };
      next.phases = existing ? record.phases.map((p) => (p.id === event.id ? phase : p)) : [...record.phases, phase];
      break;
    }
    case "plan":
      next.plan = event;
      break;
    case "request":
      next.pending_request = event;
      if (record.pending_request?.request_id !== event.request_id) {
        next.field_errors = {};
        next.validation_message = null;
      }
      break;
    case "answered":
      if (record.pending_request?.request_id === event.request_id) {
        next.responses = [
          ...record.responses,
          { request_id: event.request_id, at_ms: event.at_ms, title: requestTitle(record.pending_request), by: event.by, values: event.values },
        ];
        next.pending_request = null;
        next.field_errors = {};
        next.validation_message = null;
      }
      break;
    case "validation_error": {
      // The script rejected the answer: the engine reopens the same request
      // without a new request event, so reopen it here too.
      const asked = record.pending_request ?? [...record.events].reverse().find((e): e is RequestEvent => e.type === "request" && e.request_id === event.request_id);
      if (asked?.request_id === event.request_id) {
        if (!record.pending_request) {
          const last = record.responses.at(-1);
          if (last?.request_id === event.request_id) next.responses = record.responses.slice(0, -1);
        }
        next.pending_request = asked;
        next.field_errors = event.field_errors;
        next.validation_message = event.message ?? null;
        next.validation_seq = event.seq;
      }
      break;
    }
    case "progress":
      next.progress = event;
      break;
    case "output":
      next.log_lines = record.log_lines + 1;
      next.last_log = event.text;
      if (event.level === "error") next.errors = [...record.errors, event.text];
      break;
    case "warning":
      next.warnings = [...record.warnings, event.message];
      break;
    case "result":
      next.result = event.data;
      break;
    case "next_step":
      next.next_steps = [...record.next_steps, { text: event.text, operation: event.operation ?? null, args: event.args ?? {} }];
      break;
    case "completed":
    case "cancelled":
    case "interrupted":
      next.ended_at_ms = event.at_ms;
      next.pending_request = null;
      break;
    case "failed":
      next.ended_at_ms = event.at_ms;
      next.pending_request = null;
      next.errors = [...record.errors, event.message];
      break;
  }
  return next;
}

/** Takes a newer summary (from a list or the event stream) without losing folded events. */
export function withSummary(record: JobRecord, summary: JobSummary): JobRecord {
  if (summary.last_seq < record.summary.last_seq) return record;
  const ended = ENDED.has(summary.state);
  const pending = record.pending_request;
  return {
    ...record,
    summary,
    ended_at_ms: ended ? (record.ended_at_ms ?? summary.updated_at_ms) : null,
    pending_request: pending && summary.pending_request === pending.request_id ? pending : null,
  };
}

/** Folds a fetched view: its summary, then its events in order. */
export function applyView(record: JobRecord | null, view: JobView): JobRecord {
  let next = record ? { ...record, parameters: view.parameters, locks: view.locks } : newRecord(view);
  for (const event of view.events) next = applyEvent(next, event);
  return withSummary(next, view.job);
}
