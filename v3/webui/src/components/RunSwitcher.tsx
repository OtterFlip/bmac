// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

import type { ReactNode } from "react";
import * as DropdownMenu from "@radix-ui/react-dropdown-menu";
import { Check, ChevronDown } from "lucide-react";
import { useShallow } from "zustand/react/shallow";
import { RunStatusIcon, Tooltip, cn, elapsedMs, recordStatus, useNow, type JobRecord } from "@bmac/ui";
import { selectActiveJobs, useStore } from "../state/store";

/** The jobs still going, plus the one currently shown if it has finished. */
export function useSwitchableJobs(currentId: string | null): JobRecord[] {
  return useStore(
    useShallow((s) => {
      const jobs = selectActiveJobs(s);
      const current = currentId ? s.jobs[currentId] : undefined;
      if (!current || jobs.includes(current)) return jobs;
      return [...jobs, current].sort((a, b) => a.summary.created_at_ms - b.summary.created_at_ms);
    }),
  );
}

/**
 * Wraps `children` (the name of the job on show) in a dropdown of the other
 * jobs going at the same time. With nothing else to switch to it renders
 * `children` unchanged.
 */
export function RunSwitcher({
  currentId,
  onSelect,
  children,
  tooltip,
  side = "bottom",
  align = "start",
  className,
}: {
  currentId: string | null;
  onSelect: (jobId: string) => void;
  children: ReactNode;
  tooltip: string;
  side?: "top" | "bottom";
  align?: "start" | "end";
  className?: string;
}) {
  const runs = useSwitchableJobs(currentId);
  const now = useNow(1000);
  if (runs.length < 2) return <>{children}</>;
  return (
    <DropdownMenu.Root>
      <Tooltip content={tooltip}>
        <DropdownMenu.Trigger asChild>
          <button
            className={cn(
              "flex min-w-0 items-center gap-1 rounded-md px-1 -mx-1 text-left outline-none hover:bg-surface-3 focus-visible:ring-1 focus-visible:ring-accent data-[state=open]:bg-surface-3",
              className,
            )}
            aria-label={`${tooltip} (${runs.length} running)`}
          >
            {children}
            <ChevronDown className="size-3.5 shrink-0 text-fg-subtle" />
          </button>
        </DropdownMenu.Trigger>
      </Tooltip>
      <DropdownMenu.Portal>
        <DropdownMenu.Content
          side={side}
          align={align}
          sideOffset={6}
          className="z-[70] w-[min(560px,90vw)] rounded-lg border border-line-strong bg-surface-2 p-1 shadow-pop animate-fade-in"
        >
          <div className="px-2 pb-1 pt-1.5 text-[11px] font-semibold uppercase tracking-wider text-fg-subtle">Running now</div>
          {runs.map((r) => (
            <DropdownMenu.Item
              key={r.summary.id}
              onSelect={() => onSelect(r.summary.id)}
              className="flex cursor-default items-center gap-2.5 rounded-md px-2 py-1.5 outline-none select-none data-[highlighted]:bg-accent/10"
            >
              <RunStatusIcon status={recordStatus(r)} />
              <span className="min-w-0 flex-1">
                <span className="block truncate text-[12.5px] text-fg">
                  {r.summary.title}
                  <span className="ml-1.5 font-mono text-[11.5px] text-accent">{r.summary.host}</span>
                </span>
                <span className="block truncate text-[11.5px] text-fg-subtle">started by {r.summary.user}</span>
              </span>
              <span className="shrink-0 text-[11.5px] tabular-nums text-fg-subtle">{elapsedMs(r.started_at_ms ?? r.summary.created_at_ms, r.ended_at_ms, now)}</span>
              <Check className={cn("size-3.5 shrink-0 text-accent", r.summary.id !== currentId && "invisible")} />
            </DropdownMenu.Item>
          ))}
        </DropdownMenu.Content>
      </DropdownMenu.Portal>
    </DropdownMenu.Root>
  );
}
