// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

import type { ReactNode } from "react";
import { Activity, Archive, Bell, Boxes, Database, FlaskConical, Gauge, History, LogOut, Scale, ScrollText, Server, Settings, Stethoscope } from "lucide-react";
import { useShallow } from "zustand/react/shallow";
import { Tooltip, cn, isWaiting, recordStatus, type Condition } from "@bmac/ui";
import { selectActiveJobs, useStore, type Page } from "../../state/store";
import { useStatus } from "../../state/useStatus";

interface NavItem {
  id: Page;
  label: string;
  icon: ReactNode;
}

export const NAV: NavItem[] = [
  { id: "overview", label: "Overview", icon: <Gauge /> },
  { id: "hosts", label: "Hosts", icon: <Server /> },
  { id: "storage", label: "Storage", icon: <Database /> },
  { id: "production", label: "Production VMs", icon: <Boxes /> },
  { id: "staging", label: "Staging VMs", icon: <FlaskConical /> },
  { id: "qdevice", label: "QDevice", icon: <Scale /> },
  { id: "backups", label: "Backups", icon: <Archive /> },
  { id: "alerts", label: "Alerts", icon: <Bell /> },
  { id: "activity", label: "Activity", icon: <History /> },
  { id: "logs", label: "Logs", icon: <ScrollText /> },
  { id: "diagnostics", label: "Diagnostics", icon: <Stethoscope /> },
];

/** The page that shows a condition, by its id and resource. */
function conditionPage(c: Condition): Page {
  if (c.id.startsWith("zfs.")) return "storage";
  if (c.id.startsWith("qdevice.")) return "qdevice";
  if (c.id.startsWith("vm.") || c.id.startsWith("replication.")) return c.resource.startsWith("stage") ? "staging" : "production";
  return "hosts";
}

function NavButton({ item, children }: { item: NavItem; children?: ReactNode }) {
  const page = useStore((s) => s.page);
  const setPage = useStore((s) => s.setPage);
  const on = page === item.id;
  return (
    <button
      onClick={() => setPage(item.id)}
      aria-current={on ? "page" : undefined}
      className={cn(
        "group relative flex h-8 w-full items-center gap-2.5 rounded-md px-2.5 text-[13px] transition-colors [&_svg]:size-4",
        on ? "bg-surface-3 text-fg shadow-[inset_0_1px_0_rgb(255_255_255/0.04)]" : "text-fg-muted hover:bg-surface-2 hover:text-fg",
      )}
    >
      {on && <span className="absolute left-0 top-1.5 bottom-1.5 w-[2px] rounded-full bg-accent" />}
      <span className={cn(on ? "text-accent" : "text-fg-subtle group-hover:text-fg-muted")}>{item.icon}</span>
      <span className="flex-1 text-left">{item.label}</span>
      {children}
    </button>
  );
}

export function Sidebar() {
  const session = useStore((s) => s.session);
  const logout = useStore((s) => s.logout);
  const stream = useStore((s) => s.stream);
  const active = useStore(useShallow(selectActiveJobs));
  const waiting = active.filter((r) => isWaiting(recordStatus(r))).length;
  const conditions = useStatus("cluster").data?.conditions ?? [];
  const attention: Partial<Record<Page, number>> = {};
  for (const c of conditions) {
    const page = conditionPage(c);
    attention[page] = (attention[page] ?? 0) + 1;
  }

  return (
    <aside className="flex w-[216px] shrink-0 flex-col border-r border-line bg-sunken">
      <div className="flex h-[60px] items-center gap-2.5 px-4">
        <img src="/bmac.svg" alt="" className="size-8 shrink-0 rounded-[9px]" />
        <div className="min-w-0">
          <div className="text-[14px] font-semibold tracking-[-0.01em] text-fg">BMAC</div>
          <div className="truncate text-[11.5px] text-fg-subtle">{location.hostname}</div>
        </div>
      </div>
      <nav className="flex-1 space-y-0.5 overflow-y-auto px-2 pt-2">
        {NAV.map((item) => (
          <NavButton key={item.id} item={item}>
            {item.id === "activity" && active.length > 0 && (
              <Tooltip content={`${active.length} running${waiting ? `, ${waiting} waiting for you` : ""}`} side="right">
                <span className={cn("flex h-[18px] min-w-[18px] items-center justify-center gap-1 rounded-full px-1.5 text-[10.5px] font-semibold", waiting ? "bg-info/20 text-info animate-pulse-soft" : "bg-run/18 text-run")}>
                  <Activity className="!size-2.5" />
                  {active.length}
                </span>
              </Tooltip>
            )}
            {!!attention[item.id] && (
              <Tooltip content={`${attention[item.id]} item${attention[item.id] === 1 ? "" : "s"} need attention`} side="right">
                <span className="flex h-[18px] min-w-[18px] items-center justify-center rounded-full bg-warn/18 px-1.5 text-[10.5px] font-semibold text-warn">
                  !{attention[item.id]! > 1 ? attention[item.id] : ""}
                </span>
              </Tooltip>
            )}
          </NavButton>
        ))}
      </nav>
      <div className="space-y-0.5 px-2 pb-2">
        <NavButton item={{ id: "settings", label: "Settings", icon: <Settings /> }} />
        <div className="mt-2 rounded-lg border border-line bg-surface/60 px-2.5 py-2 text-[11px] leading-relaxed text-fg-subtle">
          <div className="flex items-center gap-1.5">
            <span className="min-w-0 flex-1 truncate text-fg-muted" title={session?.user}>
              {session?.user}
            </span>
            <Tooltip content="Log out" side="right">
              <button onClick={() => void logout()} className="rounded p-0.5 text-fg-subtle hover:bg-surface-3 hover:text-fg" aria-label="Log out">
                <LogOut className="size-3.5" />
              </button>
            </Tooltip>
          </div>
          <div className="truncate">{session?.role}</div>
          {stream !== "open" && <div className="mt-1 font-medium text-warn">{stream === "closed" ? "Live updates stopped" : "Reconnecting live updates…"}</div>}
        </div>
      </div>
    </aside>
  );
}
