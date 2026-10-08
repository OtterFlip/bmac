import { useState } from "react";
import {
  AlertTriangle,
  ArrowRight,
  Ban,
  Check,
  CheckCircle2,
  ChevronRight,
  Circle,
  Copy,
  Info,
  Loader2,
  Lock,
  Minus,
  Pencil,
  Play,
  Plus,
  ShieldCheck,
  XCircle,
} from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Tooltip } from "@/components/ui/tooltip";
import { useNow } from "@/components/status";
import { useStore } from "@/state/store";
import { elapsed } from "@/lib/format";
import { cn } from "@/lib/utils";
import type { NextStep, PhaseState, PlanEvent, PlanKind, ProgressEvent } from "@/protocol/types";

export function PhaseList({ phases, progress }: { phases: PhaseState[]; progress?: ProgressEvent | null }) {
  const now = useNow(1000);
  if (!phases.length) return null;
  return (
    <ol className="relative space-y-0.5">
      {phases.map((p, i) => {
        const last = i === phases.length - 1;
        const icon =
          p.status === "complete" ? (
            <CheckCircle2 className="size-4 text-ok" />
          ) : p.status === "running" ? (
            <Loader2 className="size-4 animate-spin text-run" />
          ) : p.status === "failed" ? (
            <XCircle className="size-4 text-danger" />
          ) : p.status === "skipped" ? (
            <Ban className="size-4 text-fg-subtle" />
          ) : (
            <Circle className="size-4 text-fg-subtle/60" />
          );
        const showProgress = p.status === "running" && progress && (!progress.phase || progress.phase === p.id);
        return (
          <li key={p.id} className="relative flex gap-3 pb-2">
            {!last && <span className="absolute left-[7.5px] top-5 bottom-0 w-px bg-line-strong" />}
            <span className="relative z-10 mt-0.5 bg-surface">{icon}</span>
            <div className="min-w-0 flex-1">
              <div className="flex items-center gap-2">
                <span className={cn("text-[13px]", p.status === "running" ? "font-medium text-fg" : p.status === "pending" ? "text-fg-subtle" : "text-fg-muted")}>{p.label}</span>
                {!p.cancel_allowed && p.status === "running" && (
                  <Tooltip content="The script marked this step as unsafe to interrupt.">
                    <span className="flex items-center gap-1 text-[11px] text-warn">
                      <Lock className="size-3" /> do not interrupt
                    </span>
                  </Tooltip>
                )}
                <span className="ml-auto text-[11.5px] tabular-nums text-fg-subtle">
                  {p.started_at && (p.status === "running" || p.ended_at) ? elapsed(p.started_at, p.ended_at, now) : ""}
                </span>
              </div>
              {showProgress && <ProgressLine progress={progress!} />}
            </div>
          </li>
        );
      })}
    </ol>
  );
}

function ProgressLine({ progress }: { progress: ProgressEvent }) {
  const fraction = progress.total ? Math.min(1, (progress.current ?? 0) / progress.total) : null;
  return (
    <div className="mt-1">
      <div className="truncate text-[12px] text-fg-subtle">{progress.message}</div>
      {fraction !== null && (
        <div className="mt-1 flex items-center gap-2">
          <div className="h-1 flex-1 overflow-hidden rounded-full bg-surface-3">
            <div className="h-full rounded-full bg-run transition-[width] duration-300" style={{ width: `${fraction * 100}%` }} />
          </div>
          <span className="text-[11px] tabular-nums text-fg-subtle">{Math.round(fraction * 100)}%</span>
        </div>
      )}
    </div>
  );
}

const planMeta: Record<PlanKind, { icon: React.ReactNode; tone: string; label: string }> = {
  create: { icon: <Plus />, tone: "text-ok bg-ok/12 border-ok/25", label: "Create" },
  update: { icon: <Pencil />, tone: "text-info bg-info/12 border-info/25", label: "Update" },
  remove: { icon: <Minus />, tone: "text-danger bg-danger/12 border-danger/30", label: "Remove" },
  keep: { icon: <ShieldCheck />, tone: "text-fg-muted bg-surface-3 border-line-strong", label: "Keep" },
  check: { icon: <Check />, tone: "text-accent bg-accent/12 border-accent/25", label: "Check" },
};

export function PlanView({ plan }: { plan: PlanEvent }) {
  return (
    <div className="overflow-hidden rounded-xl border border-line bg-surface">
      <div className="flex items-center gap-2 border-b border-line px-4 py-2.5">
        <span className="text-[13px] font-semibold text-fg">{plan.title}</span>
        {plan.dry_run && <Badge tone="info">Dry run — nothing changed</Badge>}
        <span className="ml-auto text-[11.5px] text-fg-subtle">{plan.items.length} item{plan.items.length === 1 ? "" : "s"}</span>
      </div>
      {plan.description && <p className="border-b border-line px-4 py-2 text-[12.5px] text-fg-muted">{plan.description}</p>}
      <ul className="divide-y divide-line/70">
        {plan.items.map((item, i) => {
          const meta = planMeta[item.kind];
          return (
            <li key={i} className="flex items-start gap-3 px-4 py-2">
              <Tooltip content={meta.label}>
                <span className={cn("mt-0.5 flex size-5 shrink-0 items-center justify-center rounded-md border [&_svg]:size-3", meta.tone)}>{meta.icon}</span>
              </Tooltip>
              <div className="min-w-0">
                <div className="font-mono text-[12px] text-fg">{item.target}</div>
                <div className="text-[12.5px] text-fg-muted">{item.description}</div>
              </div>
            </li>
          );
        })}
      </ul>
    </div>
  );
}

export function NextSteps({ steps, compact = false }: { steps: NextStep[]; compact?: boolean }) {
  const workflows = useStore((s) => s.workflows);
  const openLaunch = useStore((s) => s.openLaunch);
  const [copied, setCopied] = useState<number | null>(null);
  if (!steps.length) return null;
  return (
    <div className={cn("space-y-1.5", compact && "space-y-1")}>
      {steps.map((step, i) => {
        const workflow = step.workflow ? workflows[step.workflow] : undefined;
        return (
          <div key={i} className="group flex items-start gap-3 rounded-lg border border-line bg-surface-2/60 px-3 py-2.5">
            <ArrowRight className="mt-0.5 size-3.5 shrink-0 text-accent" />
            <div className="min-w-0 flex-1">
              <div className="text-[13px] leading-snug text-fg">{step.text}</div>
              {step.command && (
                <div className="mt-1 flex items-center gap-1.5">
                  <code className="selectable min-w-0 truncate font-mono text-[11.5px] text-fg-subtle">{step.command}</code>
                  <Tooltip content="Copy command">
                    <button
                      className="shrink-0 rounded p-0.5 text-fg-subtle opacity-0 hover:text-fg group-hover:opacity-100"
                      onClick={() => {
                        void navigator.clipboard.writeText(step.command!);
                        setCopied(i);
                        setTimeout(() => setCopied(null), 1200);
                      }}
                      aria-label="Copy command"
                    >
                      {copied === i ? <Check className="size-3" /> : <Copy className="size-3" />}
                    </button>
                  </Tooltip>
                </div>
              )}
            </div>
            {workflow && (
              <Tooltip content={workflow.available ? `Open ${workflow.title}` : workflow.unavailable_reason}>
                <span>
                  <Button size="sm" variant="secondary" disabled={!workflow.available} onClick={() => openLaunch(workflow.id, step.args ?? {})}>
                    <Play /> {workflow.title}
                  </Button>
                </span>
              </Tooltip>
            )}
          </div>
        );
      })}
    </div>
  );
}

/** A generic, readable rendering of a script's structured result. */
export function ResultView({ data }: { data: unknown }) {
  const [raw, setRaw] = useState(false);
  if (data === null || data === undefined) return null;
  const scalars: [string, string][] = [];
  const nested: string[] = [];
  if (typeof data === "object" && !Array.isArray(data)) {
    for (const [k, v] of Object.entries(data as Record<string, unknown>)) {
      if (v === null || ["string", "number", "boolean"].includes(typeof v)) scalars.push([k, v === null ? "—" : String(v)]);
      else nested.push(k);
    }
  }
  return (
    <div className="overflow-hidden rounded-xl border border-line bg-surface">
      {scalars.length > 0 && !raw && (
        <dl className="grid grid-cols-[minmax(110px,auto)_1fr] gap-x-4 gap-y-1.5 px-4 py-3 text-[12.5px]">
          {scalars.map(([k, v]) => (
            <div key={k} className="contents">
              <dt className="text-fg-subtle">{k.replace(/_/g, " ")}</dt>
              <dd className="selectable min-w-0 truncate font-mono text-[12px] text-fg">{v}</dd>
            </div>
          ))}
        </dl>
      )}
      {nested.length > 0 && !raw && (
        <div className="border-t border-line px-4 py-2 text-[12px] text-fg-subtle">
          Also includes {nested.map((n) => n.replace(/_/g, " ")).join(", ")}.
        </div>
      )}
      <button onClick={() => setRaw(!raw)} className="flex w-full items-center gap-1 border-t border-line px-4 py-1.5 text-left text-[11.5px] text-fg-subtle hover:text-fg">
        <ChevronRight className={cn("size-3 transition-transform", raw && "rotate-90")} /> {raw ? "Hide" : "Show"} raw result
      </button>
      {raw && <pre className="selectable max-h-80 overflow-auto bg-[#07090c] px-4 py-3 font-mono text-[11.5px] leading-relaxed text-fg-muted">{JSON.stringify(data, null, 2)}</pre>}
    </div>
  );
}

export function MessageList({ items }: { items: { kind: "info" | "warning" | "error"; text: string; details?: string }[] }) {
  if (!items.length) return null;
  return (
    <div className="space-y-1.5">
      {items.map((m, i) => (
        <div
          key={i}
          className={cn(
            "flex items-start gap-2 rounded-lg border px-3 py-2 text-[12.5px]",
            m.kind === "error" && "border-danger/35 bg-danger/8 text-danger",
            m.kind === "warning" && "border-warn/30 bg-warn/8 text-warn",
            m.kind === "info" && "border-info/25 bg-info/6 text-fg-muted",
          )}
        >
          {m.kind === "error" ? <XCircle className="mt-0.5 size-3.5 shrink-0" /> : m.kind === "warning" ? <AlertTriangle className="mt-0.5 size-3.5 shrink-0" /> : <Info className="mt-0.5 size-3.5 shrink-0 text-info" />}
          <div className="min-w-0">
            <div className="selectable">{m.text}</div>
            {m.details && <div className="selectable mt-0.5 text-fg-subtle">{m.details}</div>}
          </div>
        </div>
      ))}
    </div>
  );
}

export function SectionTitle({ children, aside }: { children: React.ReactNode; aside?: React.ReactNode }) {
  return (
    <div className="mb-2 flex items-center gap-2">
      <h4 className="text-[11px] font-semibold uppercase tracking-[0.08em] text-fg-subtle">{children}</h4>
      {aside && <div className="ml-auto">{aside}</div>}
    </div>
  );
}
