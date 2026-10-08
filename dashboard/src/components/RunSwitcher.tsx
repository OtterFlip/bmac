import type { ReactNode } from "react";
import * as DropdownMenu from "@radix-ui/react-dropdown-menu";
import { Check, ChevronDown } from "lucide-react";
import { useShallow } from "zustand/react/shallow";
import { RunStatusIcon, useNow } from "@/components/status";
import { Tooltip } from "@/components/ui/tooltip";
import { selectActiveRuns, useStore } from "@/state/store";
import { elapsed } from "@/lib/format";
import { cn } from "@/lib/utils";
import type { RunRecord } from "@/protocol/types";

/** The runs still going, plus the one currently shown if it has finished. */
export function useSwitchableRuns(currentId: string | null): RunRecord[] {
  return useStore(
    useShallow((s) => {
      const runs = selectActiveRuns(s);
      const current = currentId ? s.records[currentId] : undefined;
      if (!current || runs.includes(current)) return runs;
      return [...runs, current].sort((a, b) => a.started_at.localeCompare(b.started_at));
    }),
  );
}

/**
 * Wraps `children` (the name of the run on show) in a dropdown of the other
 * runs going at the same time. With nothing else to switch to it renders
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
  onSelect: (runId: string) => void;
  children: ReactNode;
  tooltip: string;
  side?: "top" | "bottom";
  align?: "start" | "end";
  className?: string;
}) {
  const runs = useSwitchableRuns(currentId);
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
              key={r.run_id}
              onSelect={() => onSelect(r.run_id)}
              className="flex cursor-default items-center gap-2.5 rounded-md px-2 py-1.5 outline-none select-none data-[highlighted]:bg-accent/10"
            >
              <RunStatusIcon status={r.status} />
              <span className="min-w-0 flex-1">
                <span className="block truncate text-[12.5px] text-fg">
                  {r.workflow_title}
                  {r.target && <span className="ml-1.5 font-mono text-[11.5px] text-accent">{r.target}</span>}
                </span>
                <span className="block truncate font-mono text-[11.5px] text-fg-subtle">{r.command_line}</span>
              </span>
              <span className="shrink-0 text-[11.5px] tabular-nums text-fg-subtle">{elapsed(r.started_at, r.ended_at, now)}</span>
              <Check className={cn("size-3.5 shrink-0 text-accent", r.run_id !== currentId && "invisible")} />
            </DropdownMenu.Item>
          ))}
        </DropdownMenu.Content>
      </DropdownMenu.Portal>
    </DropdownMenu.Root>
  );
}
