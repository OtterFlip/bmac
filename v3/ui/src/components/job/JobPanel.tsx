// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// One job: its steps, plan, request, result, and next steps. Both apps show
// jobs with this panel.

import { useMemo, useState, type ReactNode } from "react";
import { AlertOctagon, Ban, CheckCircle2, ChevronRight, Eye, FlaskConical, Loader2, OctagonX, RotateCcw, Server, ShieldAlert, SquareTerminal, X, XCircle } from "lucide-react";
import { Badge } from "../ui/badge";
import { Button } from "../ui/button";
import { Dialog } from "../ui/dialog";
import { Tooltip } from "../ui/tooltip";
import { RunStatusBadge, useNow } from "../status";
import { RequestView } from "./Requests";
import { MessageList, NextSteps, PhaseList, PlanView, ResultView, SectionTitle } from "./RunParts";
import { useJobActions } from "../../job/actions";
import { dateTimeMs, elapsedMs } from "../../lib/format";
import { cn } from "../../lib/utils";
import { isTerminal, recordStatus, type JobRecord, type PlanEvent } from "../../job/model";

/** Seconds after a cancel request before Force stop is offered. */
const FORCE_AFTER_SECONDS = 8;

export function JobPanel({
  record,
  onClose,
  onShowOutput,
  title,
}: {
  record: JobRecord;
  onClose: () => void;
  onShowOutput?: () => void;
  /** Replaces the plain title, as with a switcher between running jobs. */
  title?: (content: ReactNode) => ReactNode;
}) {
  const { operations, cancel, resume, reconcile, openLaunch } = useJobActions();
  const operation = operations[record.summary.operation];
  const now = useNow(1000);
  const [confirmCancel, setConfirmCancel] = useState<null | "cancel" | "force">(null);
  const [confirmReconcile, setConfirmReconcile] = useState(false);
  const [showInputs, setShowInputs] = useState(false);
  const job = record.summary;
  const status = recordStatus(record);

  const derived = useMemo(() => {
    const plans: PlanEvent[] = [];
    const messages: { kind: "info" | "warning" | "error"; text: string; details?: string }[] = [];
    for (const event of record.events) {
      if (event.type === "plan") plans.push(event);
      else if (event.type === "warning") messages.push({ kind: "warning", text: event.message });
      else if (event.type === "failed") messages.push({ kind: "error", text: event.message, details: event.details });
    }
    const running = record.phases.find((p) => p.status === "running");
    const progress = record.progress && (!running || !record.progress.phase || record.progress.phase === running.id) ? record.progress : null;
    return { plans, messages: messages.slice(-30), progress, running };
  }, [record.events, record.phases, record.progress]);

  const terminal = isTerminal(status);
  const cancelling = status === "cancelling";
  const cancelAge = job.cancel_requested ? (now - job.updated_at_ms) / 1000 : 0;
  const running = derived.running;
  const cancelAllowed = !running || (running.cancel_allowed && !(operation?.cancel.rule === "phases" && operation.cancel.not_during.includes(running.id)));
  const heading = (
    <span className="min-w-0 truncate">
      {job.title}
      {record.parameters.resource && <span className="ml-2 font-mono text-[14px] font-medium text-accent">{record.parameters.resource}</span>}
    </span>
  );

  return (
    <aside className="flex w-[clamp(380px,36vw,540px)] shrink-0 flex-col border-l border-line bg-canvas animate-slide-in" aria-label="Job panel">
      <header className="shrink-0 border-b border-line bg-surface/70 px-4 pt-3.5 pb-3">
        <div className="flex items-start gap-2">
          <div className="min-w-0 flex-1">
            <div className="flex items-center gap-2 text-[11px] font-medium uppercase tracking-[0.08em] text-fg-subtle">
              {operation?.category ?? "job"}
              <span>·</span>
              <span className="normal-case tracking-normal">started {dateTimeMs(record.started_at_ms)} by {job.user}</span>
            </div>
            <h2 className="mt-1 flex min-w-0 text-[16px] font-semibold tracking-[-0.015em] text-fg">{title ? title(heading) : heading}</h2>
          </div>
          <Tooltip content="Hide panel (the job keeps going)">
            <Button size="icon" variant="ghost" onClick={onClose} aria-label="Hide panel">
              <X />
            </Button>
          </Tooltip>
        </div>
        <div className="mt-2 flex flex-wrap items-center gap-1.5">
          <RunStatusBadge status={status} />
          <Badge>
            <Server /> {job.host}
          </Badge>
          {job.dry_run && (
            <Badge tone="info">
              <FlaskConical /> Dry run
            </Badge>
          )}
          {operation?.destructive && !job.dry_run && (
            <Badge tone="danger">
              <ShieldAlert /> Destructive
            </Badge>
          )}
          {operation && !operation.mutating && (
            <Badge>
              <Eye /> Read-only
            </Badge>
          )}
          <span className="ml-auto text-[12px] tabular-nums text-fg-subtle">{elapsedMs(record.started_at_ms, record.ended_at_ms, now)}</span>
          {onShowOutput && (
            <Tooltip content="Show live output in the console">
              <Button size="icon-sm" variant="ghost" onClick={onShowOutput} aria-label="Show output">
                <SquareTerminal />
              </Button>
            </Tooltip>
          )}
        </div>
      </header>

      <div className="min-h-0 flex-1 space-y-5 overflow-y-auto px-4 py-4">
        {terminal && <Outcome record={record} />}
        <RequestView record={record} />
        {cancelling && (
          <div className="flex items-start gap-2 rounded-lg border border-warn/30 bg-warn/8 px-3 py-2.5 text-[12.5px] text-warn">
            <Loader2 className="mt-0.5 size-3.5 shrink-0 animate-spin" />
            <div>Cancel requested. The script is cleaning up and will stop on its own.</div>
          </div>
        )}
        {record.phases.length > 0 && (
          <section>
            <SectionTitle>Steps</SectionTitle>
            <PhaseList phases={record.phases} progress={derived.progress} />
          </section>
        )}
        {!terminal && !record.phases.length && !record.pending_request && (
          <div className="flex items-center gap-2 text-[12.5px] text-fg-muted">
            <Loader2 className="size-3.5 animate-spin text-run" /> {status === "starting" ? `Starting on ${job.host}` : "Working"}
            {record.last_log && <span className="min-w-0 truncate font-mono text-[11.5px] text-fg-subtle">— {record.last_log}</span>}
          </div>
        )}
        {derived.plans.length > 0 && (
          <section className="space-y-2">
            <SectionTitle>Plan</SectionTitle>
            {derived.plans.map((plan, i) => (
              <PlanView key={i} plan={plan} />
            ))}
          </section>
        )}
        {derived.messages.length > 0 && (
          <section>
            <SectionTitle>Messages</SectionTitle>
            <MessageList items={derived.messages} />
          </section>
        )}
        {record.result !== null && record.result !== undefined && (
          <section>
            <SectionTitle>Result</SectionTitle>
            <ResultView data={record.result} />
          </section>
        )}
        {record.next_steps.length > 0 && (
          <section>
            <SectionTitle>Suggested next steps</SectionTitle>
            <NextSteps steps={record.next_steps} />
          </section>
        )}
        {(record.responses.length > 0 || Object.keys(record.parameters).length > 0) && (
          <section>
            <button onClick={() => setShowInputs(!showInputs)} className="flex items-center gap-1 text-[11px] font-semibold uppercase tracking-[0.08em] text-fg-subtle hover:text-fg">
              <ChevronRight className={cn("size-3 transition-transform", showInputs && "rotate-90")} />
              Inputs ({Object.keys(record.parameters).length + record.responses.length})
            </button>
            {showInputs && (
              <div className="mt-2 space-y-1.5 text-[12.5px]">
                {Object.entries(record.parameters).map(([k, v]) => (
                  <div key={k} className="flex gap-3 rounded-md bg-surface-2/60 px-3 py-1.5">
                    <span className="text-fg-subtle">{operation?.params.find((p) => p.id === k)?.label ?? k}</span>
                    <span className="ml-auto font-mono text-[12px] text-fg">{v}</span>
                  </div>
                ))}
                {record.responses.map((r) => (
                  <div key={r.request_id + r.at_ms} className="rounded-md bg-surface-2/60 px-3 py-1.5">
                    <div className="flex gap-2">
                      <span className="text-fg-muted">{r.title}</span>
                      <span className="ml-auto text-[11.5px] text-fg-subtle">
                        {r.by} · {dateTimeMs(r.at_ms)}
                      </span>
                    </div>
                    {r.values && (
                      <div className="mt-0.5 font-mono text-[11.5px] text-fg-subtle">
                        {Object.entries(r.values)
                          .map(([k, v]) => `${k}=${Array.isArray(v) ? v.join(",") : String(v)}`)
                          .join("  ")}
                      </div>
                    )}
                  </div>
                ))}
              </div>
            )}
          </section>
        )}
      </div>

      <footer className="flex shrink-0 items-center gap-2 border-t border-line bg-surface/70 px-4 py-3">
        {!terminal ? (
          <>
            <span className="min-w-0 flex-1 truncate text-[12px] text-fg-subtle">{record.pending_request ? "Waiting for you" : derived.running?.label ?? "Running"}</span>
            {cancelling && cancelAge > FORCE_AFTER_SECONDS && (
              <Button variant="danger-outline" onClick={() => setConfirmCancel("force")}>
                <OctagonX /> Force stop
              </Button>
            )}
            <Tooltip content={!cancelAllowed && !record.pending_request ? "The script is in a step that must not be interrupted." : "Interrupt the script like Ctrl-C"}>
              <span>
                <Button
                  variant="secondary"
                  disabled={cancelling || (!cancelAllowed && !record.pending_request)}
                  onClick={() => (record.pending_request ? void cancel(job.id, false) : setConfirmCancel("cancel"))}
                >
                  <Ban /> Cancel
                </Button>
              </span>
            </Tooltip>
          </>
        ) : status === "interrupted" ? (
          <>
            <span className="min-w-0 flex-1 truncate text-[12px] text-fg-subtle">Still holds what it locked</span>
            {operation?.allowed && operation.resume === "checkpoint" && (
              <Button variant="secondary" onClick={() => void resume(job.id)}>
                <RotateCcw /> Resume
              </Button>
            )}
            {operation?.allowed && (
              <Button variant="danger-outline" onClick={() => setConfirmReconcile(true)}>
                <XCircle /> Mark failed
              </Button>
            )}
          </>
        ) : (
          <>
            <span className="min-w-0 flex-1 truncate text-[12px] text-fg-subtle">Finished {dateTimeMs(record.ended_at_ms)}</span>
            {operation?.allowed && (
              <Button variant="secondary" onClick={() => openLaunch(operation.id, record.parameters)}>
                <RotateCcw /> Run again
              </Button>
            )}
            <Button variant="ghost" onClick={onClose}>
              Close
            </Button>
          </>
        )}
      </footer>

      <Dialog
        open={confirmCancel !== null}
        onOpenChange={(o) => !o && setConfirmCancel(null)}
        tone={confirmCancel === "force" ? "danger" : "warn"}
        icon={confirmCancel === "force" ? <OctagonX /> : <Ban />}
        title={confirmCancel === "force" ? "Force-stop this script?" : `Cancel ${job.title}?`}
        description={
          confirmCancel === "force"
            ? "The script has not stopped after the interrupt. Forcing it sends SIGTERM and then SIGKILL, which skips any cleanup it has not finished and can leave the cluster part-way through a change."
            : "The Dashboard sends the script the same interrupt as Ctrl-C. The script runs its own cleanup and stops; it may leave work it already did in place."
        }
        footer={
          <>
            <Button variant="ghost" onClick={() => setConfirmCancel(null)}>
              Keep running
            </Button>
            <Button
              variant={confirmCancel === "force" ? "danger" : "primary"}
              onClick={() => {
                void cancel(job.id, confirmCancel === "force");
                setConfirmCancel(null);
              }}
            >
              {confirmCancel === "force" ? "Force stop" : "Send interrupt"}
            </Button>
          </>
        }
      />

      <Dialog
        open={confirmReconcile}
        onOpenChange={setConfirmReconcile}
        tone="warn"
        icon={<XCircle />}
        title={`Mark ${job.title} failed?`}
        description={`Its host stopped while it ran, so it may have left a change half done on ${job.host}. Check the live state first. Marking it failed releases what it locked, so other jobs can use those resources again.`}
        footer={
          <>
            <Button variant="ghost" onClick={() => setConfirmReconcile(false)}>
              Not yet
            </Button>
            <Button
              variant="primary"
              onClick={() => {
                void reconcile(job.id);
                setConfirmReconcile(false);
              }}
            >
              I checked; mark it failed
            </Button>
          </>
        }
      />
    </aside>
  );
}

function Outcome({ record }: { record: JobRecord }) {
  const { operations } = useJobActions();
  const status = recordStatus(record);
  const meta = {
    succeeded: { icon: <CheckCircle2 />, tone: "border-ok/30 bg-ok/8 text-ok", title: record.summary.dry_run ? "Dry run complete" : "Completed successfully" },
    failed: { icon: <XCircle />, tone: "border-danger/40 bg-danger/8 text-danger", title: "Failed" },
    cancelled: { icon: <Ban />, tone: "border-line-strong bg-surface-2 text-fg-muted", title: "Cancelled" },
    interrupted: { icon: <AlertOctagon />, tone: "border-warn/35 bg-warn/8 text-warn", title: "Interrupted" },
  }[status as "succeeded" | "failed" | "cancelled" | "interrupted"];
  if (!meta) return null;
  const mutating = operations[record.summary.operation]?.mutating ?? true;
  return (
    <div className={cn("rounded-xl border px-4 py-3", meta.tone)}>
      <div className="flex items-center gap-2 text-[14px] font-semibold [&_svg]:size-4">
        {meta.icon}
        {meta.title}
      </div>
      {record.summary.message && <p className="selectable mt-1 text-[12.5px] leading-relaxed text-fg">{record.summary.message}</p>}
      {status === "interrupted" && (
        <p className="mt-1.5 text-[12px] leading-relaxed text-fg-muted">
          Its host stopped while it ran. It keeps what it locked until it is resumed or, after someone checks the live state, marked failed.
        </p>
      )}
      {status === "failed" && mutating && !record.summary.dry_run && (
        <p className="mt-1.5 text-[12px] leading-relaxed text-fg-muted">
          The job may have made partial changes. Review the output and the suggested next steps before retrying; many BMAC operations detect and resume their own incomplete work.
        </p>
      )}
    </div>
  );
}
