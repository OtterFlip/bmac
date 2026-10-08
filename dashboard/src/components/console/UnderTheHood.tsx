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
import { Button } from "@/components/ui/button";
import { Tooltip } from "@/components/ui/tooltip";
import { RunStatusIcon, runStatusMeta, useNow } from "@/components/status";
import { RunSwitcher } from "@/components/RunSwitcher";
import { Terminal, type TerminalHandle } from "./Terminal";
import { selectActiveRuns, useStore } from "@/state/store";
import { logs } from "@/state/logs";
import { toast } from "@/state/toast";
import { api, errorMessage } from "@/lib/api";
import { elapsed, dateTime } from "@/lib/format";
import { cn } from "@/lib/utils";
import { isTerminal, type RunRecord } from "@/protocol/types";
import { useShallow } from "zustand/react/shallow";

const MIN_HEIGHT = 160;

/**
 * The always-visible strip at the bottom of the window. It shows the exact
 * command the dashboard is running right now, and expands into the script's
 * live output, so the operator can always see what happens under the hood.
 */
export function UnderTheHood() {
  const { open, setOpen, consoleRunId, record, showInConsole, showInPanel, focusRun } = useStore(
    useShallow((s) => ({
      open: s.consoleOpen,
      setOpen: s.setConsoleOpen,
      consoleRunId: s.consoleRunId,
      record: s.consoleRunId ? s.records[s.consoleRunId] : undefined,
      showInConsole: s.showInConsole,
      showInPanel: s.showInPanel,
      focusRun: s.focusRun,
    })),
  );
  const active = useStore(useShallow(selectActiveRuns));
  const recent = useStore(
    useShallow((s) =>
      Object.values(s.records)
        .sort((a, b) => b.started_at.localeCompare(a.started_at))
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
    if (consoleRunId) void useStore.getState().loadRun(consoleRunId);
  }, [consoleRunId]);

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
      await navigator.clipboard.writeText(`$ ${record.command_line}\n${logs.toText(record.run_id, protocol)}`);
      setCopied(true);
      setTimeout(() => setCopied(false), 1400);
    } catch (error) {
      toast.error("Could not copy", errorMessage(error));
    }
  };

  const save = async () => {
    if (!record) return;
    try {
      const path = await api().exportRunLog(record.run_id);
      toast.success("Log saved", path);
    } catch (error) {
      toast.error("Could not save the log", errorMessage(error));
    }
  };

  const pick = (runId: string) => {
    showInConsole(runId);
    showInPanel(runId);
    setFollow(true);
  };

  const running = record && !isTerminal(record.status);
  const header = record ? `${record.command_line}` : undefined;
  const others = active.filter((r) => r.run_id !== consoleRunId).length;

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
              <RunStatusIcon status={record.status} />
              <RunSwitcher currentId={consoleRunId} onSelect={pick} tooltip="Show another running script" side="top" className="max-w-[60%]">
                <span className="flex min-w-0 items-center gap-2" onClick={others > 0 ? undefined : () => setOpen(!open)}>
                  <span className="shrink-0 font-mono text-[12px] text-fg-subtle">$</span>
                  <span className="min-w-0 truncate font-mono text-[12px] text-fg">{record.command_line}</span>
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
              Nothing has run yet. Every script the dashboard calls appears here with its live output.
            </button>
          )}
        </div>
        {record && (
          <span className="flex shrink-0 items-center gap-2 text-[11.5px] tabular-nums text-fg-subtle">
            <span className={cn(running ? "text-run" : "")}>{runStatusMeta[record.status].label}</span>
            {record.pid && <span>pid {record.pid}</span>}
            <span className="flex items-center gap-1">
              <Clock4 className="size-3" />
              {elapsed(record.started_at, record.ended_at, now)}
            </span>
          </span>
        )}
        {others > 0 && (
          <span className="shrink-0 rounded bg-run/15 px-1.5 py-0.5 text-[11px] font-medium text-run">+{others} running</span>
        )}
        <Menu.Root>
          <Tooltip content="Show another run">
            <Menu.Trigger asChild>
              <Button size="icon" variant="ghost" aria-label="Choose run">
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
              <div className="px-2 pb-1 pt-1.5 text-[11px] font-semibold uppercase tracking-wider text-fg-subtle">Recent runs</div>
              {recent.length === 0 && <div className="px-2 py-3 text-[12.5px] text-fg-subtle">No runs yet.</div>}
              {recent.map((r: RunRecord) => (
                <Menu.Item
                  key={r.run_id}
                  onSelect={() => {
                    showInConsole(r.run_id, !isTerminal(r.status) ? false : true);
                    setOpen(true);
                    setFollow(true);
                  }}
                  className="flex cursor-default items-center gap-2 rounded-md px-2 py-1.5 text-[12.5px] outline-none data-[highlighted]:bg-accent/10"
                >
                  <RunStatusIcon status={r.status} />
                  <span className="min-w-0 flex-1 truncate font-mono text-[12px]">{r.command_line}</span>
                  <span className="shrink-0 text-[11.5px] text-fg-subtle">{dateTime(r.started_at)}</span>
                  {r.run_id === consoleRunId && <Check className="size-3.5 text-accent" />}
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
                <span className="font-medium text-fg">{record.workflow_title}</span>
                {record.target && <span className="font-mono text-[11.5px] text-fg-subtle">{record.target}</span>}
                <span className="text-fg-subtle">· {record.log_lines} lines</span>
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
            <Tooltip content="Save the full log to your Downloads folder">
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
              <Tooltip content="Open this run's workflow panel">
                <Button size="sm" variant="ghost" onClick={() => focusRun(record.run_id)}>
                  <PanelRightOpen /> Details
                </Button>
              </Tooltip>
            )}
          </div>
          <div className="min-h-0 flex-1 bg-[#07090c]">
            {record ? (
              <Terminal
                key={record.run_id}
                ref={terminal}
                runId={record.run_id}
                follow={follow}
                timestamps={timestamps}
                protocol={protocol}
                header={header}
                onUserScroll={onUserScroll}
              />
            ) : (
              <div className="flex h-full items-center justify-center text-[12.5px] text-fg-subtle">
                <ExternalLink className="mr-2 size-3.5" /> Run a workflow or refresh a page to see its output here.
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