// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

import { useCallback, useEffect, useRef, useState } from "react";
import * as Menu from "@radix-ui/react-dropdown-menu";
import {
  ArrowDownToLine,
  Check,
  ChevronDown,
  ChevronUp,
  Clock4,
  Copy,
  Download,
  Eraser,
  ExternalLink,
  History,
  PanelRightOpen,
  SquareTerminal,
} from "lucide-react";
import { useShallow } from "zustand/react/shallow";
import { Button, RunStatusIcon, Terminal, Tooltip, cn, dateTimeMs, elapsedMs, isTerminal, logs, recordStatus, runStatusMeta, useNow, type JobRecord, type TerminalHandle } from "@bmac/ui";
import { selectActiveJobs, useStore } from "../../state/store";
import { toast } from "../../state/toast";
import { errorMessage } from "../../lib/backend";
import { RunSwitcher } from "../RunSwitcher";

const describe = (r: JobRecord) => `${r.summary.title} on ${r.summary.host}`;

const MIN_HEIGHT = 160;

/**
 * The always-visible strip at the bottom of the window. It shows the job
 * the console follows, and expands into the script's live output, so the
 * operator can always see what happens under the hood.
 */
export function UnderTheHood() {
  const { open, setOpen, consoleRunId, record, showInConsole, showInPanel, focusRun } = useStore(
    useShallow((s) => ({
      open: s.consoleOpen,
      setOpen: s.setConsoleOpen,
      consoleRunId: s.consoleJobId,
      record: s.consoleJobId ? s.jobs[s.consoleJobId] : undefined,
      showInConsole: s.showInConsole,
      showInPanel: s.showInPanel,
      focusRun: s.focusJob,
    })),
  );
  const active = useStore(useShallow(selectActiveJobs));
  const recent = useStore(
    useShallow((s) =>
      Object.values(s.jobs)
        .sort((a, b) => b.summary.created_at_ms - a.summary.created_at_ms)
        .slice(0, 12),
    ),
  );
  const [height, setHeight] = useState(320);
  const [follow, setFollow] = useState(true);
  const [timestamps, setTimestamps] = useState(false);
  const [protocol, setProtocol] = useState(true);
  const [copied, setCopied] = useState(false);
  const terminal = useRef<TerminalHandle>(null);
  const now = useNow(1000);

  useEffect(() => {
    if (consoleRunId && open) void useStore.getState().loadJob(consoleRunId);
  }, [consoleRunId, open]);

  const onDrag = useCallback((e: React.PointerEvent) => {
    const startY = e.clientY;
    const startH = height;
    const move = (ev: PointerEvent) => {
      const next = Math.min(window.innerHeight * 0.75, Math.max(MIN_HEIGHT, startH + (startY - ev.clientY)));
      setHeight(next);
    };
    const up = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
  }, [height]);

  const onUserScroll = useCallback(() => setFollow(false), []);

  const copy = async () => {
    if (!record) return;
    try {
      await navigator.clipboard.writeText(`${describe(record)}\n${logs.toText(record.summary.id, protocol)}`);
      setCopied(true);
      setTimeout(() => setCopied(false), 1400);
    } catch (error) {
      toast.error("Could not copy", errorMessage(error));
    }
  };

  const save = () => {
    if (!record) return;
    const blob = new Blob([`${describe(record)}\n${logs.toText(record.summary.id, true)}\n`], { type: "text/plain" });
    const url = URL.createObjectURL(blob);
    const link = document.createElement("a");
    link.href = url;
    link.download = `${record.summary.operation}-${record.summary.id}.log`;
    link.click();
    setTimeout(() => URL.revokeObjectURL(url), 1000);
  };

  const pick = (jobId: string) => {
    showInConsole(jobId);
    showInPanel(jobId);
    setFollow(true);
  };

  const status = record ? recordStatus(record) : null;
  const running = !!status && !isTerminal(status);
  const header = record ? describe(record) : undefined;
  const others = active.filter((r) => r.summary.id !== consoleRunId).length;

  return (
    <div className="relative z-30 shrink-0 border-t border-line bg-sunken">
      {open && (
        <div
          onPointerDown={onDrag}
          className="absolute -top-[3px] left-0 right-0 h-[6px] cursor-row-resize hover:bg-accent/30"
          aria-label="Resize console"
        />
      )}
      <div className="flex h-9 items-center gap-2 pl-3 pr-2">
        <div className="flex min-w-0 flex-1 items-center gap-2">
          <button
            onClick={() => setOpen(!open)}
            className="flex shrink-0 items-center gap-2 text-left"
            aria-expanded={open}
            aria-label={open ? "Collapse under-the-hood console" : "Expand under-the-hood console"}
          >
            <SquareTerminal className="size-4 shrink-0 text-accent" />
            <span className="shrink-0 text-[12px] font-semibold uppercase tracking-[0.06em] text-fg-muted">Under the hood</span>
          </button>
          <span className="mx-1 h-4 w-px shrink-0 bg-line-strong" />
          {record ? (
            <>
              <RunStatusIcon status={status!} />
              <RunSwitcher currentId={consoleRunId} onSelect={pick} tooltip="Show another running job" side="top" className="max-w-[60%]">
                <span className="flex min-w-0 items-center gap-2" onClick={others > 0 ? undefined : () => setOpen(!open)}>
                  <span className="min-w-0 truncate text-[12px] text-fg">{describe(record)}</span>
                </span>
              </RunSwitcher>
              <button onClick={() => setOpen(!open)} className="flex h-full min-w-0 flex-1 items-center self-stretch text-left" tabIndex={-1} aria-hidden>
                {running && record.last_log && !open && (
                  <span className="hidden min-w-0 truncate font-mono text-[11.5px] text-fg-subtle xl:inline">— {record.last_log}</span>
                )}
              </button>
            </>
          ) : (
            <button onClick={() => setOpen(!open)} className="min-w-0 flex-1 truncate text-left text-[12px] text-fg-subtle" tabIndex={-1}>
              No job yet. Every job you start or open appears here with its live output.
            </button>
          )}
        </div>
        {record && (
          <span className="flex shrink-0 items-center gap-2 text-[11.5px] tabular-nums text-fg-subtle">
            <span className={cn(running ? "text-run" : "")}>{runStatusMeta[status!].label}</span>
            <span className="flex items-center gap-1">
              <Clock4 className="size-3" />
              {elapsedMs(record.started_at_ms, record.ended_at_ms, now)}
            </span>
          </span>
        )}
        {others > 0 && (
          <span className="shrink-0 rounded bg-run/15 px-1.5 py-0.5 text-[11px] font-medium text-run">+{others} running</span>
        )}
        <Menu.Root>
          <Tooltip content="Show another job">
            <Menu.Trigger asChild>
              <Button size="icon" variant="ghost" aria-label="Choose job">
                <History />
              </Button>
            </Menu.Trigger>
          </Tooltip>
          <Menu.Portal>
            <Menu.Content
              align="end"
              side="top"
              sideOffset={6}
              className="z-[70] w-[520px] rounded-lg border border-line-strong bg-surface-2 p-1 shadow-pop animate-fade-in"
            >
              <div className="px-2 pb-1 pt-1.5 text-[11px] font-semibold uppercase tracking-wider text-fg-subtle">Recent jobs</div>
              {recent.length === 0 && <div className="px-2 py-3 text-[12.5px] text-fg-subtle">No jobs yet.</div>}
              {recent.map((r: JobRecord) => (
                <Menu.Item
                  key={r.summary.id}
                  onSelect={() => {
                    showInConsole(r.summary.id, isTerminal(recordStatus(r)));
                    setOpen(true);
                    setFollow(true);
                  }}
                  className="flex cursor-default items-center gap-2 rounded-md px-2 py-1.5 text-[12.5px] outline-none data-[highlighted]:bg-accent/10"
                >
                  <RunStatusIcon status={recordStatus(r)} />
                  <span className="min-w-0 flex-1 truncate text-[12px]">{describe(r)}</span>
                  <span className="shrink-0 text-[11.5px] text-fg-subtle">{dateTimeMs(r.started_at_ms)}</span>
                  {r.summary.id === consoleRunId && <Check className="size-3.5 text-accent" />}
                </Menu.Item>
              ))}
            </Menu.Content>
          </Menu.Portal>
        </Menu.Root>
        <Button size="icon" variant="ghost" onClick={() => setOpen(!open)} aria-label={open ? "Collapse" : "Expand"}>
          {open ? <ChevronDown /> : <ChevronUp />}
        </Button>
      </div>
      {open && (
        <div style={{ height }} className="flex flex-col border-t border-line">
          <div className="flex h-9 shrink-0 items-center gap-1 border-b border-line bg-canvas/60 px-2">
            {record && (
              <div className="mr-2 flex min-w-0 items-center gap-2 pl-1 text-[12px] text-fg-muted">
                <span className="font-medium text-fg">{record.summary.title}</span>
                <span className="font-mono text-[11.5px] text-fg-subtle">{record.summary.host}</span>
                <span className="text-fg-subtle">· {record.log_lines} {record.log_lines === 1 ? "line" : "lines"}</span>
              </div>
            )}
            <div className="flex-1" />
            <Toggle on={follow} onClick={() => setFollow(!follow)} icon={<ArrowDownToLine />} label="Follow output" />
            <Toggle on={timestamps} onClick={() => setTimestamps(!timestamps)} icon={<Clock4 />} label="Timestamps" />
            <Toggle on={protocol} onClick={() => setProtocol(!protocol)} icon={<span className="text-[13px] leading-none">◆</span>} label="Protocol events" />
            <span className="mx-1 h-4 w-px bg-line-strong" />
            <Tooltip content="Copy the command and its output">
              <Button size="sm" variant="ghost" onClick={copy} disabled={!record}>
                {copied ? <Check /> : <Copy />} Copy
              </Button>
            </Tooltip>
            <Tooltip content="Download the output as a text file">
              <Button size="sm" variant="ghost" onClick={save} disabled={!record}>
                <Download /> Save log
              </Button>
            </Tooltip>
            <Tooltip content="Clear this view (the stored log is kept)">
              <Button size="sm" variant="ghost" onClick={() => terminal.current?.clear()} disabled={!record}>
                <Eraser /> Clear view
              </Button>
            </Tooltip>
            {record && (
              <Tooltip content="Open this job's panel">
                <Button size="sm" variant="ghost" onClick={() => focusRun(record.summary.id)}>
                  <PanelRightOpen /> Details
                </Button>
              </Tooltip>
            )}
          </div>
          <div className="min-h-0 flex-1 bg-[#07090c]">
            {record ? (
              <Terminal
                key={record.summary.id}
                ref={terminal}
                runId={record.summary.id}
                follow={follow}
                timestamps={timestamps}
                protocol={protocol}
                header={header}
                onUserScroll={onUserScroll}
              />
            ) : (
              <div className="flex h-full items-center justify-center text-[12.5px] text-fg-subtle">
                <ExternalLink className="mr-2 size-3.5" /> Start or open a job to see its output here.
              </div>
            )}
          </div>
        </div>
      )}
    </div>
  );
}

function Toggle({ on, onClick, icon, label }: { on: boolean; onClick: () => void; icon: React.ReactNode; label: string }) {
  return (
    <Tooltip content={label}>
      <button
        onClick={onClick}
        aria-pressed={on}
        aria-label={label}
        className={cn(
          "flex h-7 items-center gap-1 rounded-md px-2 text-[12px] transition-colors [&_svg]:size-3.5",
          on ? "bg-accent/12 text-accent" : "text-fg-subtle hover:bg-surface-3 hover:text-fg",
        )}
      >
        {icon}
      </button>
    </Tooltip>
  );
}