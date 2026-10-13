// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// One task: its phases, the question it is waiting on, warnings and the
// failure, the result, and its output. The shared job view's parts, without
// the bmacd-only actions (resume, reconcile, force cancel).

import { useMemo, useState } from "react";
import { ExternalLink, X } from "lucide-react";
import {
  Button,
  Dialog,
  MessageList,
  NextSteps,
  PhaseList,
  RequestView,
  ResultView,
  RunStatusBadge,
  SectionTitle,
  Terminal,
  recordStatus,
  type JobRecord,
} from "@bmac/ui";
import { useStore } from "../state/store";

/** The Dashboard link a host task ends with, once the certificate matched. */
function dashboardLink(result: unknown): string | null {
  if (typeof result !== "object" || result === null) return null;
  const url = (result as Record<string, unknown>).open_url;
  if (typeof url !== "string") return null;
  try {
    const parsed = new URL(url);
    return parsed.protocol === "https:" && !parsed.username && !parsed.password ? url : null;
  } catch {
    return null;
  }
}

export function TaskPanel({ record, onClose }: { record: JobRecord; onClose: () => void }) {
  const cancel = useStore((s) => s.cancel);
  const backend = useStore((s) => s.backend);
  const [confirmCancel, setConfirmCancel] = useState(false);
  const status = recordStatus(record);
  const ended = record.ended_at_ms !== null;

  const derived = useMemo(() => {
    const messages: { kind: "info" | "warning" | "error"; text: string; details?: string }[] = [];
    for (const event of record.events) {
      if (event.type === "warning") messages.push({ kind: "warning", text: event.message });
      else if (event.type === "failed") messages.push({ kind: "error", text: event.message, details: event.details });
    }
    const running = record.phases.find((p) => p.status === "running");
    const progress = record.progress && (!running || !record.progress.phase || record.progress.phase === running.id) ? record.progress : null;
    return { messages: messages.slice(-30), running, progress };
  }, [record.events, record.phases, record.progress]);

  const dashboard = dashboardLink(record.result);
  const cancelAllowed = !derived.running || derived.running.cancel_allowed;

  return (
    <aside aria-label={record.summary.title} className="flex w-[460px] shrink-0 flex-col border-l border-line bg-surface">
      <header className="flex items-center gap-2 border-b border-line px-4 py-3">
        <div className="min-w-0 flex-1">
          <h2 className="truncate text-[14px] font-semibold text-fg">{record.summary.title}</h2>
          <div className="mt-1 flex items-center gap-2">
            <RunStatusBadge status={status} />
            {record.summary.message && <span className="truncate text-[12px] text-fg-muted">{record.summary.message}</span>}
          </div>
        </div>
        <Button variant="ghost" size="icon" aria-label="Close task" onClick={onClose}>
          <X />
        </Button>
      </header>

      <div className="min-h-0 flex-1 space-y-5 overflow-y-auto px-4 py-4">
        {record.phases.length > 0 && (
          <section>
            <SectionTitle>Progress</SectionTitle>
            <PhaseList phases={record.phases} progress={derived.progress} />
          </section>
        )}
        <RequestView record={record} />
        {derived.messages.length > 0 && (
          <section>
            <SectionTitle>Messages</SectionTitle>
            <MessageList items={derived.messages} />
          </section>
        )}
        {record.result !== undefined && (
          <section>
            <SectionTitle>Result</SectionTitle>
            <ResultView data={record.result} />
            {dashboard && (
              <Button variant="primary" className="mt-3" onClick={() => void backend?.openExternal(dashboard)}>
                <ExternalLink />
                Open the Dashboard
              </Button>
            )}
          </section>
        )}
        {record.next_steps.length > 0 && (
          <section>
            <SectionTitle>Next</SectionTitle>
            <NextSteps steps={record.next_steps} />
          </section>
        )}
        {record.log_lines > 0 && (
          <section>
            <SectionTitle>Output</SectionTitle>
            <div className="h-[240px] overflow-hidden rounded-lg border border-line bg-sunken">
              <Terminal runId={record.summary.id} follow timestamps={false} protocol={false} />
            </div>
          </section>
        )}
      </div>

      {!ended && (
        <footer className="flex items-center gap-2 border-t border-line px-4 py-2.5">
          <span className="min-w-0 flex-1 truncate text-[12px] text-fg-subtle">{record.pending_request ? "Waiting for you" : derived.running?.label ?? "Running"}</span>
          <Button variant="danger-outline" disabled={record.summary.cancel_requested || !cancelAllowed} onClick={() => setConfirmCancel(true)}>
            {record.summary.cancel_requested ? "Cancelling…" : "Cancel"}
          </Button>
        </footer>
      )}

      <Dialog
        open={confirmCancel}
        onOpenChange={setConfirmCancel}
        tone="danger"
        title="Cancel this task?"
        description="It stops at the next safe point and cleans up what it made on this workstation."
        footer={
          <>
            <Button onClick={() => setConfirmCancel(false)}>Keep running</Button>
            <Button
              variant="danger"
              onClick={() => {
                setConfirmCancel(false);
                void cancel(record.summary.id);
              }}
            >
              Cancel task
            </Button>
          </>
        }
      />
    </aside>
  );
}
