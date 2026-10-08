import { useEffect, useState, type ReactNode } from "react";
import {
  AlertTriangle,
  Ban,
  CheckCircle2,
  CircleDashed,
  Clock,
  Hand,
  Info,
  Loader2,
  MinusCircle,
  RefreshCw,
  XCircle,
} from "lucide-react";
import { Badge, type Tone } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Tooltip } from "@/components/ui/tooltip";
import { ago } from "@/lib/format";
import { cn } from "@/lib/utils";
import type { RunStatus } from "@/protocol/types";

export type Health = "ok" | "warn" | "danger" | "info" | "run" | "offline" | "unknown";

const healthMeta: Record<Health, { tone: Tone; icon: ReactNode }> = {
  ok: { tone: "ok", icon: <CheckCircle2 /> },
  warn: { tone: "warn", icon: <AlertTriangle /> },
  danger: { tone: "danger", icon: <XCircle /> },
  info: { tone: "info", icon: <Info /> },
  run: { tone: "run", icon: <Loader2 className="animate-spin" /> },
  offline: { tone: "neutral", icon: <MinusCircle /> },
  unknown: { tone: "neutral", icon: <CircleDashed /> },
};

/** A status label that never relies on color alone: icon + text. */
export function StatusPill({ health, children, className }: { health: Health; children: ReactNode; className?: string }) {
  const meta = healthMeta[health];
  return (
    <Badge tone={meta.tone} className={className}>
      {meta.icon}
      {children}
    </Badge>
  );
}

export const runStatusMeta: Record<RunStatus, { label: string; health: Health; icon: ReactNode }> = {
  starting: { label: "Starting", health: "run", icon: <Loader2 className="animate-spin" /> },
  running: { label: "Running", health: "run", icon: <Loader2 className="animate-spin" /> },
  waiting_input: { label: "Waiting for input", health: "info", icon: <Hand /> },
  waiting_confirmation: { label: "Waiting for confirmation", health: "warn", icon: <Hand /> },
  waiting_manual_action: { label: "Waiting for manual action", health: "warn", icon: <Hand /> },
  cancelling: { label: "Cancelling", health: "warn", icon: <Loader2 className="animate-spin" /> },
  succeeded: { label: "Succeeded", health: "ok", icon: <CheckCircle2 /> },
  failed: { label: "Failed", health: "danger", icon: <XCircle /> },
  cancelled: { label: "Cancelled", health: "offline", icon: <Ban /> },
  interrupted: { label: "Interrupted", health: "warn", icon: <AlertTriangle /> },
};

export function RunStatusBadge({ status, className }: { status: RunStatus; className?: string }) {
  const meta = runStatusMeta[status];
  return (
    <Badge tone={healthMeta[meta.health].tone} className={className}>
      {meta.icon}
      {meta.label}
    </Badge>
  );
}

export function RunStatusIcon({ status, className }: { status: RunStatus; className?: string }) {
  const meta = runStatusMeta[status];
  const color = {
    ok: "text-ok",
    warn: "text-warn",
    danger: "text-danger",
    info: "text-info",
    run: "text-run",
    offline: "text-fg-subtle",
    unknown: "text-fg-subtle",
  }[meta.health];
  return <span className={cn("inline-flex [&_svg]:size-3.5", color, className)} aria-label={meta.label}>{meta.icon}</span>;
}

export function Meter({ fraction, className, invert = false }: { fraction: number | null | undefined; className?: string; invert?: boolean }) {
  const f = fraction === null || fraction === undefined ? null : Math.min(1, Math.max(0, fraction));
  const used = f === null ? 0 : invert ? 1 - f : f;
  const tone = used >= 0.9 ? "bg-danger" : used >= 0.75 ? "bg-warn" : "bg-accent";
  return (
    <div className={cn("h-1.5 w-full overflow-hidden rounded-full bg-surface-3", className)}>
      {f !== null && <div className={cn("h-full rounded-full transition-[width] duration-500", tone)} style={{ width: `${Math.max(2, used * 100)}%` }} />}
    </div>
  );
}

/** Re-render every `ms` so relative times stay current. */
export function useNow(ms = 15000) {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), ms);
    return () => clearInterval(t);
  }, [ms]);
  return now;
}

export function RefreshControl({
  updatedAt,
  loading,
  onRefresh,
  error,
  label = "Refresh",
  compact = false,
}: {
  updatedAt: number | null;
  loading: boolean;
  onRefresh: () => void;
  error?: string | null;
  label?: string;
  compact?: boolean;
}) {
  const now = useNow(10000);
  return (
    <div className="flex items-center gap-2">
      {error && !loading ? (
        <Tooltip content={error}>
          <span className="flex max-w-[260px] items-center gap-1 truncate text-[12px] text-danger">
            <XCircle className="size-3.5 shrink-0" />
            <span className="truncate">Refresh failed</span>
          </span>
        </Tooltip>
      ) : (
        <span className="flex items-center gap-1 text-[12px] text-fg-subtle tabular-nums">
          <Clock className="size-3" />
          {loading ? "Refreshing…" : updatedAt ? `Updated ${ago(updatedAt, now)}` : "Not loaded yet"}
        </span>
      )}
      <Tooltip content={compact ? label : null}>
        <Button size={compact ? "icon" : "sm"} variant="secondary" onClick={onRefresh} disabled={loading} aria-label={label}>
          <RefreshCw className={cn(loading && "animate-spin")} />
          {!compact && label}
        </Button>
      </Tooltip>
    </div>
  );
}

export function EmptyState({ icon, title, body, action, className }: { icon?: ReactNode; title: ReactNode; body?: ReactNode; action?: ReactNode; className?: string }) {
  return (
    <div className={cn("flex flex-col items-center justify-center px-6 py-10 text-center", className)}>
      {icon && <div className="mb-3 flex size-10 items-center justify-center rounded-xl border border-line-strong bg-surface-2 text-fg-subtle [&_svg]:size-5">{icon}</div>}
      <div className="text-[13.5px] font-medium text-fg">{title}</div>
      {body && <div className="mt-1 max-w-[440px] text-[12.5px] leading-relaxed text-fg-subtle">{body}</div>}
      {action && <div className="mt-4">{action}</div>}
    </div>
  );
}

export function Skeleton({ className }: { className?: string }) {
  return <div className={cn("skeleton h-4", className)} />;
}

export function KV({ items, className }: { items: [ReactNode, ReactNode][]; className?: string }) {
  return (
    <dl className={cn("grid grid-cols-[minmax(110px,auto)_1fr] gap-x-4 gap-y-2 text-[12.5px]", className)}>
      {items.map(([k, v], i) => (
        <div key={i} className="contents">
          <dt className="text-fg-subtle">{k}</dt>
          <dd className="min-w-0 text-fg">{v}</dd>
        </div>
      ))}
    </dl>
  );
}

export function Mono({ children, className }: { children: ReactNode; className?: string }) {
  return <code className={cn("font-mono text-[12px] text-fg", className)}>{children}</code>;
}
