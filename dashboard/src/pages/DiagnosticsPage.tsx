import { Gauge, Timer } from "lucide-react";
import { useShallow } from "zustand/react/shallow";
import { useStore } from "@/state/store";
import { Card, CardHeader } from "@/components/ui/card";
import { RunStatusIcon, useNow } from "@/components/status";
import { PageBody, PageHeader } from "@/components/layout/Page";
import { WorkflowTiles } from "./common";
import { ago, elapsed } from "@/lib/format";

const FAST = ["list_hosts", "list_guests", "list_replication", "list_storage", "list_disks"];
const FULL = ["show_cluster_health", "show_cluster_state", "show_proxmox_host_state", "show_prod_vm_state", "show_qdevice_state"];

export function DiagnosticsPage() {
  const focusRun = useStore((s) => s.focusRun);
  const now = useNow(10000);
  const runs = useStore(
    useShallow((s) =>
      Object.values(s.records)
        .filter((r) => s.workflows[r.workflow_id]?.category === "diagnostics")
        .sort((a, b) => b.started_at.localeCompare(a.started_at))
        .slice(0, 10),
    ),
  );
  return (
    <>
      <PageHeader title="Diagnostics" subtitle="Read-only checks. None of these change the cluster." />
      <PageBody>
        <h2 className="mb-1 flex items-center gap-2 text-[12px] font-semibold uppercase tracking-[0.08em] text-fg-subtle">
          <Gauge className="size-3.5" /> Quick state
        </h2>
        <p className="mb-2.5 text-[12.5px] text-fg-subtle">Each one reads a single slice of the cluster in a few seconds. The dashboard pages use these to refresh.</p>
        <WorkflowTiles ids={FAST} className="mb-6" />
        <h2 className="mb-1 flex items-center gap-2 text-[12px] font-semibold uppercase tracking-[0.08em] text-fg-subtle">
          <Timer className="size-3.5" /> Full reports
        </h2>
        <p className="mb-2.5 text-[12.5px] text-fg-subtle">Thorough checks that can take a minute or more. Their full output streams to the console below.</p>
        <WorkflowTiles ids={FULL} className="mb-6" />
        {runs.length > 0 && (
          <Card>
            <CardHeader title="Recent diagnostic runs" />
            <div className="divide-y divide-line/70">
              {runs.map((run) => (
                <button key={run.run_id} onClick={() => focusRun(run.run_id)} className="flex w-full items-center gap-3 px-4 py-2.5 text-left hover:bg-surface-2/60">
                  <RunStatusIcon status={run.status} />
                  <span className="flex-1 truncate text-[12.5px] text-fg">
                    {run.workflow_title}
                    {run.target && <span className="text-fg-muted"> · {run.target}</span>}
                  </span>
                  <span className="text-[11.5px] text-fg-subtle tabular-nums">{elapsed(run.started_at, run.ended_at, now)}</span>
                  <span className="w-[72px] text-right text-[11.5px] text-fg-subtle">{ago(Date.parse(run.started_at), now)}</span>
                </button>
              ))}
            </div>
          </Card>
        )}
      </PageBody>
    </>
  );
}
