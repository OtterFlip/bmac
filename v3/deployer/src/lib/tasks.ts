// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// A Deployer task has the same phases, requests, output, and result as a
// bmacd job, so the window folds it into the shared job record and shows it
// with the shared job view. Only the summary differs: a task runs on this
// workstation, for whoever is at it.

import { applyEvent, applyView, withSummary, type JobEvent, type JobRecord, type JobSummary, type TaskEvent, type TaskSummary, type TaskView } from "@bmac/ui";

export const WORKSTATION = "this workstation";

export function toJobSummary(task: TaskSummary): JobSummary {
  return {
    id: task.id,
    operation: task.kind,
    title: task.title,
    state: task.state,
    host: WORKSTATION,
    user: "you",
    dry_run: false,
    created_at_ms: task.created_at_ms,
    updated_at_ms: task.updated_at_ms,
    pending_request: task.pending_request ?? undefined,
    message: task.message ?? undefined,
    cancel_requested: task.cancel_requested,
    last_seq: task.last_seq,
  };
}

export function toJobEvent(event: TaskEvent): JobEvent {
  const { task, ...rest } = event;
  return { ...rest, job: task } as JobEvent;
}

export function foldView(record: JobRecord | null, view: TaskView): JobRecord {
  return applyView(record, {
    job: toJobSummary(view.task),
    parameters: view.parameters,
    locks: [],
    events: view.events.map(toJobEvent),
    more: false,
  });
}

export function foldEvent(record: JobRecord, event: TaskEvent): JobRecord {
  return event.seq <= record.seq ? record : applyEvent(record, toJobEvent(event));
}

export function foldSummary(record: JobRecord, summary: TaskSummary): JobRecord {
  return withSummary(record, toJobSummary(summary));
}
