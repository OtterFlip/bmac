import { useState } from "react";
import { Download, FlaskConical, History, Search, SquareTerminal, Trash2 } from "lucide-react";
import { useStore } from "@/state/store";
import { Card } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Dialog } from "@/components/ui/dialog";
import { Input } from "@/components/ui/inputs";
import { Menu } from "@/components/ui/menu";
import { EmptyState, RefreshControl, RunStatusBadge, useNow } from "@/components/status";
import { PageBody, PageHeader } from "@/components/layout/Page";
import { Table } from "./common";
import { api, errorMessage } from "@/lib/api";
import { toast } from "@/state/toast";
import { ago, dateTime, elapsed } from "@/lib/format";
import { cn } from "@/lib/utils";
import { isTerminal, type RunRecord, type RunStatus } from "@/protocol/types";

type Filter = "all" | "active" | "failed" | "changes";

const FILTERS: { id: Filter; label: string; match: (r: RunRecord) => boolean }[] = [
  { id: "all", label: "All", match: () => true },
  { id: "active", label: "Active", match: (r) => !isTerminal(r.status) },
  { id: "failed", label: "Problems", match: (r) => (["failed", "interrupted"] as RunStatus[]).includes(r.status) },
  { id: "changes", label: "Changes", match: (r) => r.mode === "mutating" && !r.dry_run },
];

export async function exportLog(runId: string) {
  try {
    const path = await api().exportRunLog(runId);
    toast.success("Log saved", path);
    return path;
  } catch (e) {
    toast.error("Could not save the log", errorMessage(e));
    return null;
  }
}

export function OperationsPage() {
  const records = useStore((s) => s.records);
  const focusRun = useStore((s) => s.focusRun);
  const focusedRunId = useStore((s) => s.focusedRunId);
  const showInConsole = useStore((s) => s.showInConsole);
  const setConsoleOpen = useStore((s) => s.setConsoleOpen);
  const [query, setQuery] = useState("");
  const [filter, setFilter] = useState<Filter>("all");
  const [loading, setLoading] = useState(false);
  const [updatedAt, setUpdatedAt] = useState<number | null>(() => Date.now());
  const [error, setError] = useState<string | null>(null);
  const [deleting, setDeleting] = useState<RunRecord | null>(null);
  const now = useNow(10000);

  const refresh = async () => {
    setLoading(true);
    try {
      const [history, active] = await Promise.all([api().getRunHistory(300), api().getActiveRuns()]);
      useStore.setState((s) => {
        const next = { ...s.records };
        for (const r of [...history, ...active]) if (!next[r.run_id] || isTerminal(r.status) || !isTerminal(next[r.run_id].status)) next[r.run_id] = r;
        return { records: next };
      });
      setUpdatedAt(Date.now());
      setError(null);
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setLoading(false);
    }
  };

  const q = query.trim().toLowerCase();
  const match = FILTERS.find((f) => f.id === filter)!.match;
  const rows = Object.values(records)
    .filter(match)
    .filter((r) => !q || [r.workflow_title, r.target ?? "", r.command_line, r.status, r.message ?? ""].some((s) => s.toLowerCase().includes(q)))
    .sort((a, b) => b.started_at.localeCompare(a.started_at));

  return (
    <>
      <PageHeader title="Operations" subtitle="Every script the dashboard has run, with its full output" actions={<RefreshControl updatedAt={updatedAt} loading={loading} error={error} onRefresh={() => void refresh()} />} />
      <PageBody>
        <div className="mb-3 flex items-center gap-3">
          <div className="flex rounded-lg border border-line bg-sunken p-0.5">
            {FILTERS.map((f) => {
              const count = Object.values(records).filter(f.match).length;
              return (
                <button
                  key={f.id}
                  onClick={() => setFilter(f.id)}
                  className={cn("rounded-md px-3 py-1 text-[12.5px] transition-colors", filter === f.id ? "bg-surface-3 text-fg shadow-sm" : "text-fg-muted hover:text-fg")}
                >
                  {f.label}
                  <span className="ml-1.5 text-[11px] text-fg-subtle tabular-nums">{count}</span>
                </button>
              );
            })}
          </div>
          <div className="relative ml-auto w-[280px]">
            <Search className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-fg-subtle" />
            <Input value={query} onChange={(e) => setQuery(e.target.value)} placeholder="Filter by workflow, target, command…" className="pl-8" />
          </div>
        </div>
        <Card>
          <Table head={["Status", "Operation", "Command", "Started", "Duration", ""]}>
            {rows.map((r) => (
              <tr key={r.run_id} onClick={() => focusRun(r.run_id)} className={cn("cursor-default", focusedRunId === r.run_id && "!bg-accent/[0.06]")}>
                <td className="w-[130px]"><RunStatusBadge status={r.status} /></td>
                <td>
                  <div className="flex items-center gap-1.5 text-fg">
                    {r.workflow_title}
                    {r.dry_run && <span className="flex items-center gap-0.5 text-[11px] text-info"><FlaskConical className="size-3" />dry run</span>}
                  </div>
                  <div className="max-w-[320px] truncate text-[11.5px] text-fg-subtle">{r.target ?? r.message ?? r.current_activity ?? ""}</div>
                </td>
                <td className="max-w-[340px]"><code className="block truncate font-mono text-[11.5px] text-fg-muted">{r.command_line}</code></td>
                <td className="whitespace-nowrap text-fg-muted" title={dateTime(r.started_at)}>{ago(Date.parse(r.started_at), now)}</td>
                <td className="whitespace-nowrap text-fg-muted tabular-nums">{elapsed(r.started_at, r.ended_at, now)}</td>
                <td className="w-[40px]">
                  <Menu
                    items={[
                      { label: "Show output", icon: <SquareTerminal />, onSelect: () => (showInConsole(r.run_id), setConsoleOpen(true)) },
                      { label: "Save log to Downloads", icon: <Download />, onSelect: () => void exportLog(r.run_id) },
                      "separator",
                      { label: "Delete from history", icon: <Trash2 />, danger: true, disabled: !isTerminal(r.status), onSelect: () => setDeleting(r) },
                    ]}
                  />
                </td>
              </tr>
            ))}
          </Table>
          {rows.length === 0 && <EmptyState icon={<History />} title={q || filter !== "all" ? "No matching operations" : "Nothing has run yet"} body="Runs appear here as soon as they start, and stay after the dashboard restarts." />}
        </Card>
      </PageBody>
      <Dialog
        open={!!deleting}
        onOpenChange={(o) => !o && setDeleting(null)}
        tone="danger"
        icon={<Trash2 />}
        title="Delete this run from history?"
        description="This removes the dashboard's saved record and output. It doesn't undo anything the script did."
        footer={
          <>
            <Button variant="ghost" onClick={() => setDeleting(null)}>Keep</Button>
            <Button
              variant="danger"
              onClick={async () => {
                const r = deleting!;
                setDeleting(null);
                try {
                  await api().deleteRun(r.run_id);
                  useStore.setState((s) => {
                    const next = { ...s.records };
                    delete next[r.run_id];
                    return { records: next, focusedRunId: s.focusedRunId === r.run_id ? null : s.focusedRunId, panelOpen: s.focusedRunId === r.run_id ? false : s.panelOpen };
                  });
                } catch (e) {
                  toast.error("Could not delete the run", errorMessage(e));
                }
              }}
            >
              Delete
            </Button>
          </>
        }
      >
        {deleting && <code className="block truncate rounded-md bg-surface-2 px-2.5 py-1.5 font-mono text-[12px] text-fg-muted">{deleting.command_line}</code>}
      </Dialog>
    </>
  );
}
