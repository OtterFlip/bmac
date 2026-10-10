import { useEffect, type ReactNode } from "react";
import { BookOpen, ExternalLink, Eye, Play, ShieldAlert, Wrench } from "lucide-react";
import { useStore, type SourceEntry } from "@/state/store";
import { Button } from "@/components/ui/button";
import { Tooltip } from "@/components/ui/tooltip";
import { RefreshControl } from "@/components/status";
import { api, errorMessage } from "@/lib/api";
import { toast } from "@/state/toast";
import { cn } from "@/lib/utils";
import type { SourceId, StateSources } from "@/protocol/state";

const DEFAULT_STALE_MS = 2 * 60 * 1000;

export const QDEVICE_SETUP_URL = "https://github.com/OtterFlip/bmac/blob/main/docs/QDEVICE_MANUAL_SETUP.md";

/** A fast state source. Loads on mount when stale, and on the configured interval. */
export function useSource<K extends SourceId>(id: K): SourceEntry<StateSources[K]> & { refresh: () => void } {
  const entry = useStore((s) => s.sources[id]) as SourceEntry<StateSources[K]>;
  const ready = useStore((s) => s.ready && !!s.workflows[id]);
  const interval = useStore((s) => s.settings?.refresh_interval_seconds ?? 0);
  const refreshSource = useStore((s) => s.refreshSource);

  useEffect(() => {
    if (!ready) return;
    void refreshSource(id, { ifOlderThanMs: interval > 0 ? interval * 1000 : DEFAULT_STALE_MS });
    if (interval <= 0) return;
    const timer = setInterval(() => void refreshSource(id, { ifOlderThanMs: interval * 1000 - 1000 }), interval * 1000);
    return () => clearInterval(timer);
  }, [id, ready, interval, refreshSource]);

  return { ...entry, refresh: () => void refreshSource(id) };
}

export function SourceRefresh({ ids, label = "Refresh" }: { ids: SourceId[]; label?: string }) {
  const sources = useStore((s) => s.sources);
  const refreshSources = useStore((s) => s.refreshSources);
  const entries = ids.map((id) => sources[id]);
  const loading = entries.some((e) => e.status === "loading");
  const updated = entries.every((e) => e.updatedAt) ? Math.min(...entries.map((e) => e.updatedAt!)) : null;
  const error = entries.find((e) => e.status === "error")?.error ?? null;
  return <RefreshControl updatedAt={updated} loading={loading} error={error} onRefresh={() => refreshSources(ids)} label={label} />;
}

export function openExternal(url: string) {
  api()
    .openExternal(url)
    .catch((e) => toast.error("Could not open the link", errorMessage(e)));
}

export function proxmoxUrl(host: string) {
  return `https://${host}:8006/`;
}

export function ExternalButton({ url, children, className }: { url: string; children?: ReactNode; className?: string }) {
  return (
    <Tooltip content={url}>
      <Button variant="ghost" size={children ? "sm" : "icon-sm"} className={className} onClick={(e) => (e.stopPropagation(), openExternal(url))} aria-label={`Open ${url}`}>
        <ExternalLink />
        {children}
      </Button>
    </Tooltip>
  );
}

/** A button that launches a curated workflow (never an arbitrary command). */
export function WorkflowButton({
  id,
  values,
  label,
  variant,
  size = "sm",
  className,
  icon,
}: {
  id: string;
  values?: Record<string, unknown>;
  label?: ReactNode;
  variant?: "primary" | "secondary" | "ghost" | "outline" | "danger-outline";
  size?: "sm" | "md" | "lg" | "icon-sm" | "icon";
  className?: string;
  icon?: ReactNode;
}) {
  const workflow = useStore((s) => s.workflows[id]);
  const launchWorkflow = useStore((s) => s.launchWorkflow);
  if (!workflow) return null;
  const v = variant ?? (workflow.destructive ? "danger-outline" : workflow.mode === "read_only" ? "ghost" : "secondary");
  const button = (
    <Button
      variant={v}
      size={size}
      className={className}
      onClick={(e) => {
        e.stopPropagation();
        launchWorkflow(id, values);
      }}
      aria-disabled={!workflow.available}
      aria-label={typeof label === "string" ? label : workflow.title}
    >
      {icon ?? (workflow.mode === "read_only" ? <Eye /> : workflow.destructive ? <ShieldAlert /> : <Wrench />)}
      {size !== "icon" && size !== "icon-sm" && (label ?? workflow.title)}
    </Button>
  );
  const tip = workflow.available ? workflow.summary : `Unavailable here: ${workflow.unavailable_reason}`;
  return <Tooltip content={tip}>{button}</Tooltip>;
}

export interface LinkTile {
  title: string;
  summary: string;
  url: string;
}

/** A tile listing the workflows for one category. Unsupported ones stay visible but disabled. */
export function WorkflowTiles({ ids, links = [], className }: { ids: string[]; links?: LinkTile[]; className?: string }) {
  const workflows = useStore((s) => s.workflows);
  const launchWorkflow = useStore((s) => s.launchWorkflow);
  return (
    <div className={cn("grid grid-cols-[repeat(auto-fill,minmax(250px,1fr))] gap-2.5", className)}>
      {links.map((l) => (
        <Tooltip key={l.url} content={l.url}>
          <button
            onClick={() => openExternal(l.url)}
            className="group flex flex-col items-start gap-1.5 rounded-xl border border-line bg-surface px-3.5 py-3 text-left transition-[border-color,background-color] hover:border-line-strong hover:bg-surface-2"
          >
            <div className="flex w-full items-center gap-2">
              <span className="flex size-6 items-center justify-center rounded-md border border-accent/25 bg-accent/10 text-accent [&_svg]:size-3.5">
                <BookOpen />
              </span>
              <span className="flex-1 truncate text-[13px] font-medium text-fg">{l.title}</span>
              <ExternalLink className="size-3.5 text-fg-subtle opacity-0 transition-opacity group-hover:opacity-100" />
            </div>
            <p className="line-clamp-2 text-[12px] leading-relaxed text-fg-subtle">{l.summary}</p>
          </button>
        </Tooltip>
      ))}
      {ids
        .map((id) => workflows[id])
        .filter(Boolean)
        .map((w) => (
          <button
            key={w.id}
            onClick={() => launchWorkflow(w.id)}
            className={cn(
              "group flex flex-col items-start gap-1.5 rounded-xl border bg-surface px-3.5 py-3 text-left transition-[border-color,background-color] hover:bg-surface-2",
              w.available ? "border-line hover:border-line-strong" : "border-dashed border-line opacity-60",
            )}
          >
            <div className="flex w-full items-center gap-2">
              <span
                className={cn(
                  "flex size-6 items-center justify-center rounded-md border [&_svg]:size-3.5",
                  w.destructive ? "border-danger/30 bg-danger/10 text-danger" : w.mode === "read_only" ? "border-accent/25 bg-accent/10 text-accent" : "border-line-strong bg-surface-3 text-fg-muted",
                )}
              >
                {w.mode === "read_only" ? <Eye /> : w.destructive ? <ShieldAlert /> : <Wrench />}
              </span>
              <span className="flex-1 truncate text-[13px] font-medium text-fg">{w.title}</span>
              <Play className="size-3.5 text-fg-subtle opacity-0 transition-opacity group-hover:opacity-100" />
            </div>
            <p className="line-clamp-2 text-[12px] leading-relaxed text-fg-subtle">{w.summary}</p>
            {!w.available && <p className="text-[11.5px] font-medium text-warn">{w.unavailable_reason}</p>}
          </button>
        ))}
    </div>
  );
}

/** Standard table styling. */
export function Table({ head, children, className }: { head: ReactNode[]; children: ReactNode; className?: string }) {
  return (
    <div className={cn("overflow-x-auto", className)}>
      <table className="w-full border-collapse text-[12.5px]">
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
