import type { ReactNode } from "react";
import {
  Activity,
  Boxes,
  Database,
  FlaskConical,
  Gauge,
  History,
  Scale,
  Server,
  Settings,
  Stethoscope,
} from "lucide-react";
import { useShallow } from "zustand/react/shallow";
import { selectActiveRuns, useStore, type Page } from "@/state/store";
import { Tooltip } from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";
import { isWaiting } from "@/protocol/types";

interface NavItem {
  id: Page;
  label: string;
  icon: ReactNode;
}

const NAV: NavItem[] = [
  { id: "dashboard", label: "Dashboard", icon: <Gauge /> },
  { id: "hosts", label: "Hosts", icon: <Server /> },
  { id: "production", label: "Production", icon: <Boxes /> },
  { id: "staging", label: "Staging", icon: <FlaskConical /> },
  { id: "storage", label: "Storage", icon: <Database /> },
  { id: "qdevice", label: "QDevice", icon: <Scale /> },
  { id: "diagnostics", label: "Diagnostics", icon: <Stethoscope /> },
  { id: "operations", label: "Operations", icon: <History /> },
];

function useAttention() {
  return useStore(
    useShallow((s) => {
      const hosts = s.sources.list_hosts.data;
      const guests = s.sources.list_guests.data;
      const storage = s.sources.list_storage.data;
      const repl = s.sources.list_replication.data;
      const hostProblems = hosts ? hosts.hosts.filter((h) => !h.online || h.ssh_from_here === false).length + (hosts.cluster.quorate === false ? 1 : 0) : 0;
      const prodProblems = guests ? guests.problems.filter((p) => !p.startsWith("staging")).length : 0;
      const stageProblems = guests ? guests.staging.filter((g) => ["cleanup_pending", "failed"].includes(g.registry_state ?? "")).length : 0;
      const storageProblems = storage ? storage.problems.length : 0;
      const qd = hosts?.qdevice;
      const qdProblem = qd ? (qd.needed && !qd.registered) || (qd.registered && qd.needed === false) || (qd.registered && qd.voting === false) : false;
      return {
        hosts: hostProblems,
        production: prodProblems + (repl ? 0 : 0),
        staging: stageProblems,
        storage: storageProblems,
        qdevice: qdProblem ? 1 : 0,
      } as Partial<Record<Page, number>>;
    }),
  );
}

export function Sidebar() {
  const page = useStore((s) => s.page);
  const setPage = useStore((s) => s.setPage);
  const repo = useStore((s) => s.repo);
  const backendKind = useStore((s) => s.backendKind);
  const clusterName = useStore((s) => s.sources.list_hosts.data?.cluster.name ?? s.repo?.cluster_settings.find((c) => c.key === "PROXMOX_CLUSTER_NAME")?.value);
  const active = useStore(useShallow(selectActiveRuns));
  const waiting = active.filter((r) => isWaiting(r.status)).length;
  const attention = useAttention();

  return (
    <aside className="flex w-[216px] shrink-0 flex-col border-r border-line bg-sunken">
      <div className="flex h-[60px] items-center gap-2.5 px-4">
        <img src="/bmac.svg" alt="" className="size-8 rounded-[9px]" />
        <div className="min-w-0">
          <div className="text-[14px] font-semibold tracking-[-0.01em] text-fg">BMAC</div>
          <div className="truncate text-[11.5px] text-fg-subtle">{clusterName ? `cluster ${clusterName}` : "Control panel"}</div>
        </div>
      </div>
      <nav className="flex-1 space-y-0.5 px-2 pt-2">
        {NAV.map((item) => {
          const count = attention[item.id] ?? 0;
          const on = page === item.id;
          return (
            <button
              key={item.id}
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
              {item.id === "operations" && active.length > 0 && (
                <Tooltip content={`${active.length} running${waiting ? `, ${waiting} waiting for you` : ""}`} side="right">
                  <span className={cn("flex h-[18px] min-w-[18px] items-center justify-center gap-1 rounded-full px-1.5 text-[10.5px] font-semibold", waiting ? "bg-info/20 text-info animate-pulse-soft" : "bg-run/18 text-run")}>
                    <Activity className="!size-2.5" />
                    {active.length}
                  </span>
                </Tooltip>
              )}
              {count > 0 && (
                <Tooltip content={`${count} item${count === 1 ? "" : "s"} need attention`} side="right">
                  <span className="flex h-[18px] min-w-[18px] items-center justify-center rounded-full bg-warn/18 px-1.5 text-[10.5px] font-semibold text-warn">!{count > 1 ? count : ""}</span>
                </Tooltip>
              )}
            </button>
          );
        })}
      </nav>
      <div className="space-y-0.5 px-2 pb-2">
        <button
          onClick={() => setPage("settings")}
          className={cn(
            "flex h-8 w-full items-center gap-2.5 rounded-md px-2.5 text-[13px] [&_svg]:size-4",
            page === "settings" ? "bg-surface-3 text-fg" : "text-fg-muted hover:bg-surface-2 hover:text-fg",
          )}
        >
          <Settings className={page === "settings" ? "text-accent" : "text-fg-subtle"} />
          Settings
        </button>
        <div className="mt-2 rounded-lg border border-line bg-surface/60 px-2.5 py-2 text-[11px] leading-relaxed text-fg-subtle">
          <div className="truncate font-mono text-fg-muted" title={repo?.root}>
            {repo ? repo.root.split("/").slice(-2).join("/") : "No repository"}
          </div>
          <div className="truncate">
            {repo?.git_describe ?? repo?.git_commit ?? "—"}
            {repo?.git_branch ? ` · ${repo.git_branch}` : ""}
          </div>
          {backendKind === "mock" && <div className="mt-1 font-medium text-warn">Browser preview · simulated data</div>}
        </div>
      </div>
    </aside>
  );
}
