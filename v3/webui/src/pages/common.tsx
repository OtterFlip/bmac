// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// Pieces the status pages share: tables, operation buttons and tiles, the
// conditions panel, and the refresh control for status snapshots.

import { useState, type ReactNode } from "react";
import { AlertTriangle, ChevronDown, ChevronRight, Eye, Play, ShieldAlert, Wrench, XCircle } from "lucide-react";
import { Button, NextSteps, RefreshControl, Tooltip, ago, cn, useNow, type Condition, type Health, type MenuItem, type NextStep } from "@bmac/ui";
import { useShallow } from "zustand/react/shallow";
import { useStore, type StatusEntry, type StatusKind } from "../state/store";

export function SectionTitle({ children, className }: { children: ReactNode; className?: string }) {
  return <h2 className={cn("mb-2.5 text-[12px] font-semibold uppercase tracking-[0.08em] text-fg-subtle", className)}>{children}</h2>;
}

export function Table({ head, children, className, label }: { head: ReactNode[]; children: ReactNode; className?: string; label?: string }) {
  return (
    <div className={cn("overflow-x-auto", className)}>
      <table className="w-full border-collapse text-[12.5px]" aria-label={label}>
        <thead>
          <tr className="border-b border-line">
            {head.map((h, i) => (
              <th key={i} className="h-9 px-3 text-left text-[11px] font-semibold uppercase tracking-[0.06em] whitespace-nowrap text-fg-subtle first:pl-4 last:pr-4">
                {h}
              </th>
            ))}
          </tr>
        </thead>
        <tbody className="[&_tr]:border-b [&_tr]:border-line/70 [&_tr:last-child]:border-0 [&_td]:px-3 [&_td]:py-2.5 [&_td:first-child]:pl-4 [&_td:last-child]:pr-4 [&_tr]:transition-colors [&_tr:hover]:bg-surface-2/60">
          {children}
        </tbody>
      </table>
    </div>
  );
}

export function SkeletonRows({ rows = 3, cols }: { rows?: number; cols: number }) {
  return (
    <>
      {Array.from({ length: rows }, (_, r) => (
        <tr key={r}>
          {Array.from({ length: cols }, (_, c) => (
            <td key={c}>
              <div className="skeleton h-3.5" style={{ width: `${45 + ((r * 7 + c * 13) % 40)}%` }} />
            </td>
          ))}
        </tr>
      ))}
    </>
  );
}

/** The refresh control for the snapshots a page shows. */
export function StatusRefresh({ kinds, label = "Refresh" }: { kinds: StatusKind[]; label?: string }) {
  const entries = useStore(useShallow((s) => kinds.map((k) => s.status[k] as StatusEntry<unknown>)));
  const refreshStatus = useStore((s) => s.refreshStatus);
  const loading = entries.some((e) => e.loading);
  const updated = entries.every((e) => e.updatedAt) ? Math.min(...entries.map((e) => e.updatedAt!)) : null;
  const error = entries.find((e) => e.error)?.error ?? null;
  return <RefreshControl updatedAt={updated} loading={loading} error={error} onRefresh={() => void refreshStatus(kinds)} label={label} />;
}

export const vmHealth = (status: string): Health =>
  status === "running" ? "ok" : status === "stopped" ? "offline" : status === "absent" ? "danger" : status === "unknown" ? "unknown" : "warn";

function operationIcon(op: { mutating: boolean; destructive: boolean }) {
  return !op.mutating ? <Eye /> : op.destructive ? <ShieldAlert /> : <Wrench />;
}

/** Starts an operation with prefilled values; hidden when this user can't see it. */
export function OperationButton({
  id,
  values,
  label,
  variant,
  size = "sm",
  icon,
  className,
}: {
  id: string;
  values?: Record<string, unknown>;
  label?: ReactNode;
  variant?: "primary" | "secondary" | "ghost" | "outline" | "danger-outline";
  size?: "sm" | "md";
  icon?: ReactNode;
  className?: string;
}) {
  const op = useStore((s) => s.operations[id]);
  const startOperation = useStore((s) => s.startOperation);
  if (!op) return null;
  return (
    <Tooltip content={op.allowed ? op.summary : `Needs the ${op.min_role} role`}>
      <span className={className}>
        <Button
          variant={variant ?? (op.destructive ? "danger-outline" : op.mutating ? "secondary" : "ghost")}
          size={size}
          disabled={!op.allowed}
          onClick={(e) => {
            e.stopPropagation();
            startOperation(id, values);
          }}
        >
          {icon ?? operationIcon(op)}
          {label ?? op.title}
        </Button>
      </span>
    </Tooltip>
  );
}

/** Menu entries for operations this user can see. */
export function useOperationItems(entries: { id: string; values?: Record<string, unknown>; label?: string; icon?: ReactNode }[]): MenuItem[] {
  const operations = useStore((s) => s.operations);
  const startOperation = useStore((s) => s.startOperation);
  return entries.flatMap(({ id, values, label, icon }) => {
    const op = operations[id];
    if (!op) return [];
    return [
      {
        label: label ?? op.title,
        icon: icon ?? operationIcon(op),
        danger: op.destructive,
        disabled: !op.allowed,
        hint: op.allowed ? undefined : op.min_role,
        onSelect: () => startOperation(id, values),
      },
    ];
  });
}

export interface LinkTile {
  title: string;
  summary: string;
  url: string;
}

/** Tiles for a category's operations; ones this user may not run stay visible but disabled. */
export function OperationTiles({ ids, links = [], className }: { ids: string[]; links?: LinkTile[]; className?: string }) {
  const operations = useStore((s) => s.operations);
  const startOperation = useStore((s) => s.startOperation);
  const shown = ids.map((id) => operations[id]).filter(Boolean);
  if (!shown.length && !links.length) return <p className="text-[12.5px] text-fg-subtle">None of these operations are available to you.</p>;
  return (
    <div className={cn("grid grid-cols-[repeat(auto-fill,minmax(250px,1fr))] gap-2.5", className)}>
      {links.map((l) => (
        <a
          key={l.url}
          href={l.url}
          target="_blank"
          rel="noopener noreferrer"
          className="group flex flex-col items-start gap-1.5 rounded-xl border border-line bg-surface px-3.5 py-3 text-left transition-[border-color,background-color] hover:border-line-strong hover:bg-surface-2"
        >
          <span className="w-full truncate text-[13px] font-medium text-fg">{l.title}</span>
          <span className="line-clamp-2 text-[12px] leading-relaxed text-fg-subtle">{l.summary}</span>
        </a>
      ))}
      {shown.map((op) => (
        <button
          key={op.id}
          onClick={() => startOperation(op.id)}
          disabled={!op.allowed}
          className={cn(
            "group flex flex-col items-start gap-1.5 rounded-xl border bg-surface px-3.5 py-3 text-left transition-[border-color,background-color] enabled:hover:bg-surface-2",
            op.allowed ? "border-line hover:border-line-strong" : "border-dashed border-line opacity-60",
          )}
        >
          <div className="flex w-full items-center gap-2">
            <span
              className={cn(
                "flex size-6 items-center justify-center rounded-md border [&_svg]:size-3.5",
                op.destructive ? "border-danger/30 bg-danger/10 text-danger" : !op.mutating ? "border-accent/25 bg-accent/10 text-accent" : "border-line-strong bg-surface-3 text-fg-muted",
              )}
            >
              {operationIcon(op)}
            </span>
            <span className="flex-1 truncate text-[13px] font-medium text-fg">{op.title}</span>
            <Play className="size-3.5 text-fg-subtle opacity-0 transition-opacity group-hover:opacity-100" />
          </div>
          <p className="line-clamp-2 text-[12px] leading-relaxed text-fg-subtle">{op.summary}</p>
          {!op.allowed && <p className="text-[11.5px] font-medium text-warn">Needs the {op.min_role} role</p>}
        </button>
      ))}
    </div>
  );
}

/** The warning and critical conditions bmacd reported, critical first, with their next steps. */
export function ConditionsBanner({ conditions, title = "Needs attention" }: { conditions: Condition[]; title?: string }) {
  const now = useNow(15000);
  const [open, setOpen] = useState(true);
  if (!conditions.length) return null;
  const critical = conditions.some((c) => c.severity === "critical");
  const seen = new Set<string>();
  const steps: NextStep[] = conditions
    .flatMap((c) => c.next_steps)
    .filter((s) => {
      const key = `${s.text}\n${s.operation ?? ""}\n${JSON.stringify(s.args)}`;
      return seen.has(key) ? false : (seen.add(key), true);
    });
  return (
    <section
      aria-label={title}
      className={cn("mb-5 overflow-hidden rounded-xl border bg-gradient-to-br to-transparent", critical ? "border-danger/35 from-danger/[0.07]" : "border-warn/30 from-warn/[0.07]")}
    >
      <button onClick={() => setOpen(!open)} aria-expanded={open} className="flex w-full items-center gap-2 px-4 py-3 text-left">
        <span className={cn("flex items-center gap-2 text-[13px] font-semibold", critical ? "text-danger" : "text-warn")}>
          {critical ? <XCircle className="size-4" /> : <AlertTriangle className="size-4" />} {title}
        </span>
        <span className="text-[13px] text-fg-subtle">· {conditions.length}</span>
        <span className="ml-auto text-fg-subtle">{open ? <ChevronDown className="size-4" /> : <ChevronRight className="size-4" />}</span>
      </button>
      {open && (
        <>
          <ul className="space-y-1.5 px-4 pb-3">
            {conditions.map((c) => (
              <li key={`${c.id}/${c.resource}/${c.message}`} className="selectable flex items-start gap-2 text-[12.5px] text-fg">
                <span className={cn("mt-1 size-1.5 shrink-0 rounded-full", c.severity === "critical" ? "bg-danger" : c.severity === "warning" ? "bg-warn" : "bg-info")} aria-hidden />
                <span className="sr-only">{c.severity}: </span>
                <span className="min-w-0 flex-1">{c.message}</span>
                <span className="shrink-0 text-[11.5px] text-fg-subtle tabular-nums" title={new Date(c.since_ms).toLocaleString()}>
                  since {ago(c.since_ms, now)}
                </span>
              </li>
            ))}
          </ul>
          {steps.length > 0 && (
            <div className={cn("border-t px-4 pt-3 pb-3", critical ? "border-danger/15" : "border-warn/15")}>
              <div className="mb-2 text-[11px] font-semibold uppercase tracking-[0.08em] text-fg-subtle">Suggested next steps</div>
              <NextSteps steps={steps} compact />
            </div>
          )}
        </>
      )}
    </section>
  );
}

/** Says the snapshot couldn't be read, or that it is on its way. */
export function StatusMissing<T>({ entry, what, icon }: { entry: StatusEntry<T>; what: string; icon: ReactNode }) {
  if (entry.data) return null;
  return entry.loading || (!entry.error && !entry.updatedAt) ? (
    <div className="space-y-3 p-4" aria-busy="true">
      <div className="skeleton h-4 w-1/3" />
      <div className="skeleton h-4 w-full" />
      <div className="skeleton h-4 w-2/3" />
    </div>
  ) : (
    <div className="flex flex-col items-center px-6 py-10 text-center">
      <div className="mb-3 flex size-10 items-center justify-center rounded-xl border border-line-strong bg-surface-2 text-fg-subtle [&_svg]:size-5">{icon}</div>
      <div className="text-[13.5px] font-medium text-fg">{`${what} isn't available`}</div>
      <div className="mt-1 max-w-[440px] text-[12.5px] leading-relaxed text-fg-subtle">{entry.error ?? "Refresh to try again."}</div>
    </div>
  );
}

/** "observed 20s ago", or why a host's part is missing or old. */
export function Freshness({ observedAt, reachable, online = true }: { observedAt: number | null; reachable: boolean; online?: boolean }) {
  const now = useNow(10000);
  if (!online) return <span className="text-fg-subtle">offline</span>;
  if (!reachable)
    return (
      <Tooltip content="This host's stream to the host answering is down, so what's shown may be old.">
        <span className="text-warn">unreachable{observedAt ? ` · seen ${ago(observedAt, now)}` : ""}</span>
      </Tooltip>
    );
  return <span className="text-fg-subtle">{observedAt ? `observed ${ago(observedAt, now)}` : "not observed yet"}</span>;
}
