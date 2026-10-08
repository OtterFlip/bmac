import { useMemo, useState } from "react";
import {
  AlertOctagon,
  Ban,
  CheckCircle2,
  ChevronRight,
  Copy,
  Eye,
  FlaskConical,
  Loader2,
  OctagonX,
  RotateCcw,
  ShieldAlert,
  SquareTerminal,
  X,
  XCircle,
} from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Dialog } from "@/components/ui/dialog";
import { Tooltip } from "@/components/ui/tooltip";
import { RunStatusBadge, useNow } from "@/components/status";
import { RunSwitcher } from "@/components/RunSwitcher";
import { RequestView } from "./Requests";
import { MessageList, NextSteps, PhaseList, PlanView, ResultView, SectionTitle } from "./RunParts";
import { useStore } from "@/state/store";
import { elapsed, dateTime } from "@/lib/format";
import { cn } from "@/lib/utils";
import { isTerminal, type PlanEvent, type ProgressEvent, type RunRecord, type Workflow } from "@/protocol/types";

export function launchValuesFromRecord(record: RunRecord, workflow?: Workflow): Record<string, unknown> {
  const values: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(record.launch_values)) {
    const param = workflow?.params.find((p) => p.id === k);
    values[k] = param?.type === "flag" ? v === "yes" : v;
  }
  return values;
}

export function WorkflowPanel() {
  const runId = useStore((s) => s.focusedRunId);
  const record = useStore((s) => (s.focusedRunId ? s.records[s.focusedRunId] : undefined));
  const events = useStore((s) => (s.focusedRunId ? s.events[s.focusedRunId] : undefined));
  const workflow = useStore((s) => (record ? s.workflows[record.workflow_id] : undefined));
  const setPanelOpen = useStore((s) => s.setPanelOpen);
  const showInConsole = useStore((s) => s.showInConsole);
  const showInPanel = useStore((s) => s.showInPanel);
  const setConsoleOpen = useStore((s) => s.setConsoleOpen);
  const cancel = useStore((s) => s.cancel);
  const openLaunch = useStore((s) => s.openLaunch);
  const now = useNow(1000);
  const [confirmCancel, setConfirmCancel] = useState<null | "cancel" | "force">(null);
  const [copied, setCopied] = useState(false);
  const [showInputs, setShowInputs] = useState(false);

  const derived = useMemo(() => {
    const plans: PlanEvent[] = [];
    const messages: { kind: "info" | "warning" | "error"; text: string; details?: string }[] = [];
    let progress: ProgressEvent | null = null;
    for (const { event } of events ?? []) {
      if (event.type === "plan") plans.push(event);
      else if (event.type === "progress") progress = event;
      else if (event.type === "phase" && event.status === "running") progress = null;
      else if (event.type === "info") messages.push({ kind: "info", text: event.message });
      else if (event.type === "warning") messages.push({ kind: "warning", text: event.message });
      else if (event.type === "error") messages.push({ kind: "error", text: event.message, details: event.details });
    }
    return { plans, messages: messages.slice(-30), progress };
  }, [events]);

  if (!record || !runId) return null;
  const terminal = isTerminal(record.status);
  const cancelling = record.status === "cancelling";
  const cancelAge = record.cancel ? (now - Date.parse(record.cancel.requested_at)) / 1000 : 0;

  return (
    <aside className="flex w-[clamp(380px,36vw,540px)] shrink-0 flex-col border-l border-line bg-canvas animate-slide-in" aria-label="Workflow panel">
      <header className="shrink-0 border-b border-line bg-surface/70 px-4 pt-3.5 pb-3">
        <div className="flex items-start gap-2">
          <div className="min-w-0 flex-1">
            <div className="flex items-center gap-2 text-[11px] font-medium uppercase tracking-[0.08em] text-fg-subtle">
              {workflow?.category ?? "workflow"}
              <span>·</span>
              <span className="normal-case tracking-normal">started {dateTime(record.started_at)}</span>
            </div>
            <h2 className="mt-1 flex min-w-0 text-[16px] font-semibold tracking-[-0.015em] text-fg">
              <RunSwitcher currentId={runId} onSelect={showInPanel} tooltip="Show another running job in this panel">
                <span className="min-w-0 truncate">
                  {record.workflow_title}
                  {record.target && <span className="ml-2 font-mono text-[14px] font-medium text-accent">{record.target}</span>}
                </span>
              </RunSwitcher>
            </h2>
          </div>
          <Tooltip content="Hide panel (the run keeps going)">
            <Button size="icon" variant="ghost" onClick={() => setPanelOpen(false)} aria-label="Hide panel">
              <X />
            </Button>
          </Tooltip>
        </div>
        <div className="mt-2 flex flex-wrap items-center gap-1.5">
          <RunStatusBadge status={record.status} />
          {record.dry_run && (
            <Badge tone="info">
              <FlaskConical /> Dry run
            </Badge>
          )}
          {record.destructive && !record.dry_run && (
            <Badge tone="danger">
              <ShieldAlert /> Destructive
            </Badge>
          )}
          {record.mode === "read_only" && (
            <Badge>
              <Eye /> Read-only
            </Badge>
          )}
          <span className="ml-auto text-[12px] tabular-nums text-fg-subtle">{elapsed(record.started_at, record.ended_at, now)}</span>
        </div>
        <div className="mt-2.5 flex items-center gap-1 rounded-md border border-line bg-[#07090c] py-1 pl-2.5 pr-1">
          <span className="font-mono text-[11.5px] text-accent">$</span>
          <code className="selectable min-w-0 flex-1 truncate font-mono text-[11.5px] text-fg-muted" title={record.command_line}>
            {record.command_line}
          </code>
          <Tooltip content="Copy the equivalent terminal command (without --json)">
            <Button
              size="icon-sm"
              variant="ghost"
              onClick={() => {
                void navigator.clipboard.writeText(record.terminal_command);
                setCopied(true);
                setTimeout(() => setCopied(false), 1200);
              }}
              aria-label="Copy command"
            >
              {copied ? <CheckCircle2 /> : <Copy />}
            </Button>
          </Tooltip>
          <Tooltip content="Show live output in the console">
            <Button
              size="icon-sm"
              variant="ghost"
              onClick={() => {
                showInConsole(record.run_id);
                setConsoleOpen(true);
              }}
              aria-label="Show output"
            >
              <SquareTerminal />
            </Button>
          </Tooltip>
        </div>
      </header>

      <div className="min-h-0 flex-1 space-y-5 overflow-y-auto px-4 py-4">
        {terminal && <Outcome record={record} />}
        {record.pending_request && <RequestView runId={record.run_id} request={record.pending_request} />}
        {cancelling && (
          <div className="flex items-start gap-2 rounded-lg border border-warn/30 bg-warn/8 px-3 py-2.5 text-[12.5px] text-warn">
            <Loader2 className="mt-0.5 size-3.5 shrink-0 animate-spin" />
            <div>
              {record.cancel?.forced ? "Stopping the script (SIGTERM, then SIGKILL after 10 seconds)…" : "Cancel requested. The script is cleaning up and will stop on its own."}
            </div>
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
            <Loader2 className="size-3.5 animate-spin text-run" /> {record.current_activity ?? "Working"}
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
        {(record.responses.length > 0 || Object.keys(record.launch_values).length > 0) && (
          <section>
            <button onClick={() => setShowInputs(!showInputs)} className="flex items-center gap-1 text-[11px] font-semibold uppercase tracking-[0.08em] text-fg-subtle hover:text-fg">
              <ChevronRight className={cn("size-3 transition-transform", showInputs && "rotate-90")} />
              Inputs ({Object.keys(record.launch_values).length + record.responses.length})
            </button>
            {showInputs && (
              <div className="mt-2 space-y-1.5 text-[12.5px]">
                {Object.entries(record.launch_values).map(([k, v]) => (
                  <div key={k} className="flex gap-3 rounded-md bg-surface-2/60 px-3 py-1.5">
                    <span className="text-fg-subtle">{workflow?.params.find((p) => p.id === k)?.label ?? k}</span>
                    <span className="ml-auto font-mono text-[12px] text-fg">{v}</span>
                  </div>
                ))}
                {record.responses.map((r) => (
                  <div key={r.request_id + r.at} className="rounded-md bg-surface-2/60 px-3 py-1.5">
                    <div className="flex gap-2">
                      <span className="text-fg-muted">{r.title}</span>
                      <span className="ml-auto text-[11.5px] text-fg-subtle">{dateTime(r.at)}</span>
                    </div>
                    <div className="mt-0.5 font-mono text-[11.5px] text-fg-subtle">
                      {r.kind === "values"
                        ? Object.entries(r.values ?? {})
                            .map(([k, v]) => `${k}=${Array.isArray(v) ? v.join(",") : String(v)}`)
                            .join("  ")
                        : r.kind === "confirm"
                          ? r.confirmed
                            ? "confirmed"
                            : "declined"
                          : r.kind === "acknowledge"
                            ? "acknowledged"
                            : "cancelled"}
                    </div>
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
            <span className="min-w-0 flex-1 truncate text-[12px] text-fg-subtle">
              {record.pending_request ? "Waiting for you" : record.current_activity ?? "Running"}
            </span>
            {cancelling && cancelAge > 8 && !record.cancel?.forced && (
              <Button variant="danger-outline" onClick={() => setConfirmCancel("force")}>
                <OctagonX /> Force stop
              </Button>
            )}
            <Tooltip content={!record.cancel_allowed && !record.pending_request ? "The script is in a step that must not be interrupted." : "Interrupt the script like Ctrl-C"}>
              <span>
                <Button
                  variant="secondary"
                  disabled={cancelling || (!record.cancel_allowed && !record.pending_request)}
                  onClick={() => (record.pending_request ? void cancel(record.run_id) : setConfirmCancel("cancel"))}
                >
                  <Ban /> Cancel
                </Button>
              </span>
            </Tooltip>
          </>
        ) : (
          <>
            <span className="min-w-0 flex-1 truncate text-[12px] text-fg-subtle">
              Finished {dateTime(record.ended_at)} · exit {record.exit_code ?? (record.signal ? `signal ${record.signal}` : "?")}
            </span>
            {workflow && (
              <Button variant="secondary" onClick={() => openLaunch(workflow.id, launchValuesFromRecord(record, workflow))}>
                <RotateCcw /> Run again
              </Button>
            )}
            <Button variant="ghost" onClick={() => setPanelOpen(false)}>
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
        title={confirmCancel === "force" ? "Force-stop this script?" : `Cancel ${record.workflow_title}?`}
        description={
          confirmCancel === "force"
            ? "The script has not stopped after the interrupt. Forcing it sends SIGTERM and then SIGKILL, which skips any cleanup it has not finished and can leave the cluster part-way through a change."
            : "The dashboard sends the script the same interrupt as Ctrl-C. The script runs its own cleanup and stops; it may leave work it already did in place."
        }
        footer={
          <>
            <Button variant="ghost" onClick={() => setConfirmCancel(null)}>
              Keep running
            </Button>
            <Button
              variant={confirmCancel === "force" ? "danger" : "primary"}
              onClick={() => {
                void cancel(record.run_id, confirmCancel === "force");
                setConfirmCancel(null);
              }}
            >
              {confirmCancel === "force" ? "Force stop" : "Send interrupt"}
            </Button>
          </>
        }
      />
    </aside>
  );
}

function Outcome({ record }: { record: RunRecord }) {
  const meta = {
    succeeded: { icon: <CheckCircle2 />, tone: "border-ok/30 bg-ok/8 text-ok", title: record.dry_run ? "Dry run complete" : "Completed successfully" },
    failed: { icon: <XCircle />, tone: "border-danger/40 bg-danger/8 text-danger", title: "Failed" },
    cancelled: { icon: <Ban />, tone: "border-line-strong bg-surface-2 text-fg-muted", title: "Cancelled" },
    interrupted: { icon: <AlertOctagon />, tone: "border-warn/35 bg-warn/8 text-warn", title: "Interrupted" },
  }[record.status as "succeeded" | "failed" | "cancelled" | "interrupted"];
  if (!meta) return null;
  return (
    <div className={cn("rounded-xl border px-4 py-3", meta.tone)}>
      <div className="flex items-center gap-2 text-[14px] font-semibold [&_svg]:size-4">
        {meta.icon}
        {meta.title}
      </div>
      {record.message && <p className="selectable mt-1 text-[12.5px] leading-relaxed text-fg">{record.message}</p>}
      {record.status === "failed" && record.mode === "mutating" && !record.dry_run && (
        <p className="mt-1.5 text-[12px] leading-relaxed text-fg-muted">
          The workflow may have made partial changes. Review the output and the suggested next steps before retrying; many BMAC workflows detect and resume their own incomplete work.
        </p>
      )}
    </div>
  );
}
