// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

import { useEffect, useMemo, type ReactNode } from "react";
import { Archive, KeyRound, Rocket, ScanSearch, Server, Settings } from "lucide-react";
import { Button, Dialog, JobActionsProvider, RunStatusIcon, TooltipProvider, cn, recordStatus, type JobActions } from "@bmac/ui";
import type { Backend } from "../lib/backend";
import { TaskPanel } from "../components/TaskPanel";
import { Toaster } from "../components/Toaster";
import { PlaceholderPage } from "../pages/PlaceholderPage";
import { SettingsPage } from "../pages/SettingsPage";
import { SshPage } from "../pages/SshPage";
import { StartPage } from "../pages/StartPage";
import { WorkflowPage, discovery, host } from "../pages/WorkflowPage";
import { useStore, type Page } from "../state/store";
import { toast } from "../state/toast";

const pages: { id: Page; label: string; icon: ReactNode; subtitle: string }[] = [
  { id: "start", label: "Start", icon: <Rocket />, subtitle: "" },
  { id: "discovery", label: "Discovery", icon: <ScanSearch />, subtitle: "Boot a machine from a discovery ISO to read its disks and network ports." },
  { id: "hosts", label: "Proxmox hosts", icon: <Server />, subtitle: "Build a host's ISO, boot it, and hand the host to its Dashboard." },
  { id: "appliances", label: "Backup appliances", icon: <Archive />, subtitle: "Build and install a backup appliance." },
  { id: "ssh", label: "Workstation SSH", icon: <KeyRound />, subtitle: "SSH entries on this workstation for hosts and guests." },
  { id: "settings", label: "Settings", icon: <Settings />, subtitle: "" },
];

export function App({ backend }: { backend: Backend }) {
  const setBackend = useStore((s) => s.setBackend);
  useEffect(() => setBackend(backend), [backend, setBackend]);

  const answer = useStore((s) => s.answer);
  const cancel = useStore((s) => s.cancel);
  const actions = useMemo<JobActions>(
    () => ({
      operations: {},
      answer,
      cancel: (id) => cancel(id),
      // Deployer tasks end with the window; there is nothing to resume.
      resume: async () => false,
      reconcile: async () => false,
      openLaunch: () => {},
      notify: (kind, title, body) => (kind === "error" ? toast.error(title, body) : toast.info(title, body)),
    }),
    [answer, cancel],
  );

  return (
    <TooltipProvider>
      <JobActionsProvider value={actions}>
        <div className="flex h-screen overflow-hidden bg-bg text-fg">
          <Sidebar />
          <main className="flex min-w-0 flex-1 flex-col">
            <Current />
          </main>
          <FocusedTask />
        </div>
        <CloseDialog />
        <Toaster />
      </JobActionsProvider>
    </TooltipProvider>
  );
}

function Current() {
  const page = useStore((s) => s.page);
  if (page === "start") return <StartPage />;
  if (page === "settings") return <SettingsPage />;
  if (page === "discovery") return <WorkflowPage workflow={discovery} />;
  if (page === "hosts") return <WorkflowPage workflow={host} />;
  if (page === "ssh") return <SshPage />;
  const meta = pages.find((p) => p.id === page)!;
  return <PlaceholderPage title={meta.label} subtitle={meta.subtitle} icon={meta.icon} />;
}

function FocusedTask() {
  const record = useStore((s) => (s.focused ? s.tasks[s.focused] : undefined));
  const focus = useStore((s) => s.focus);
  return record ? <TaskPanel record={record} onClose={() => focus(null)} /> : null;
}

function Sidebar() {
  const page = useStore((s) => s.page);
  const setPage = useStore((s) => s.setPage);
  const tasks = useStore((s) => s.tasks);
  const order = useStore((s) => s.order);
  const focused = useStore((s) => s.focused);
  const focus = useStore((s) => s.focus);
  const info = useStore((s) => s.info);
  return (
    <nav aria-label="Deployer" className="flex w-[220px] shrink-0 flex-col border-r border-line bg-surface">
      <div className="flex items-center gap-2 px-4 py-4">
        <img src="/bmac.svg" alt="" className="size-6" />
        <div className="leading-tight">
          <div className="text-[13.5px] font-semibold text-fg">BMAC Deployer</div>
          {info && <div className="text-[11.5px] text-fg-subtle">{info.version}</div>}
        </div>
      </div>
      <ul className="space-y-0.5 px-2">
        {pages.map((p) => (
          <li key={p.id}>
            <button
              aria-current={page === p.id ? "page" : undefined}
              onClick={() => setPage(p.id)}
              className={cn(
                "flex w-full items-center gap-2.5 rounded-md px-2.5 py-1.5 text-left text-[13px] [&_svg]:size-4 [&_svg]:shrink-0",
                page === p.id ? "bg-surface-3 font-medium text-fg" : "text-fg-muted hover:bg-surface-2 hover:text-fg",
              )}
            >
              {p.icon}
              {p.label}
            </button>
          </li>
        ))}
      </ul>
      {order.length > 0 && (
        <div className="mt-5 min-h-0 flex-1 overflow-y-auto px-2">
          <h2 className="px-2.5 pb-1 text-[11px] font-semibold uppercase tracking-[0.08em] text-fg-subtle">Tasks</h2>
          <ul className="space-y-0.5">
            {order.map((id) => {
              const record = tasks[id];
              if (!record) return null;
              return (
                <li key={id}>
                  <button
                    aria-current={focused === id ? "true" : undefined}
                    onClick={() => focus(focused === id ? null : id)}
                    className={cn(
                      "flex w-full items-center gap-2 rounded-md px-2.5 py-1.5 text-left text-[12.5px]",
                      focused === id ? "bg-surface-3 text-fg" : "text-fg-muted hover:bg-surface-2 hover:text-fg",
                    )}
                  >
                    <RunStatusIcon status={recordStatus(record)} />
                    <span className="truncate">{record.summary.title}</span>
                  </button>
                </li>
              );
            })}
          </ul>
        </div>
      )}
    </nav>
  );
}

function CloseDialog() {
  const running = useStore((s) => s.closeRequest);
  const dismiss = useStore((s) => s.dismissClose);
  const quit = useStore((s) => s.quit);
  return (
    <Dialog
      open={running !== null}
      onOpenChange={(open) => !open && dismiss()}
      tone="warn"
      title={running === 1 ? "A task is still running" : `${running ?? 0} tasks are still running`}
      description="Closing cancels them. Each stops at its next safe point and cleans up what it made on this workstation; a machine that is mid-install may need its ISO booted again."
      footer={
        <>
          <Button onClick={dismiss}>Keep working</Button>
          <Button variant="danger" onClick={() => void quit(true)}>
            Cancel tasks and close
          </Button>
        </>
      }
    />
  );
}
