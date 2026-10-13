// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// Jobs on the cluster, newest first, and every operation this user can see.

import { useMemo } from "react";
import { History, Play } from "lucide-react";
import { useShallow } from "zustand/react/shallow";
import { Badge, Button, Card, CardHeader, EmptyState, RunStatusBadge, Tooltip, dateTimeMs, elapsedMs, isTerminal, recordStatus, useNow, type JobRecord, type OperationView } from "@bmac/ui";
import { PageBody, PageHeader } from "../components/layout/Page";
import { useStore } from "../state/store";

/** How long a job took, or has run so far; a finished job not yet opened ends at its last update. */
function took(r: JobRecord, now: number): string {
  const end = r.ended_at_ms ?? (isTerminal(recordStatus(r)) ? r.summary.updated_at_ms : undefined);
  return elapsedMs(r.started_at_ms ?? r.summary.created_at_ms, end, now);
}

export function ActivityPage() {
  const now = useNow(1000);
  const jobs = useStore(useShallow((s) => Object.values(s.jobs).sort((a, b) => b.summary.created_at_ms - a.summary.created_at_ms)));
  const operations = useStore(useShallow((s) => s.operationOrder.map((id) => s.operations[id])));
  const focusJob = useStore((s) => s.focusJob);
  const focusedJobId = useStore((s) => s.focusedJobId);
  const startOperation = useStore((s) => s.startOperation);
  const loadError = useStore((s) => s.loadError);

  const byCategory = useMemo(() => {
    const groups = new Map<string, OperationView[]>();
    for (const op of operations) groups.set(op.category, [...(groups.get(op.category) ?? []), op]);
    return [...groups.entries()];
  }, [operations]);

  return (
    <>
      <PageHeader title="Activity" subtitle="Jobs on this cluster, and the operations you can start." />
      <PageBody className="space-y-5">
        {loadError && <div className="rounded-lg border border-danger/40 bg-danger/8 px-3 py-2 text-[12.5px] text-danger">{loadError}</div>}
        <Card>
          <CardHeader title="Jobs" subtitle={`${jobs.length} held by bmacd`} icon={<History />} />
          {jobs.length === 0 ? (
            <EmptyState icon={<History />} title="No jobs yet" body="Jobs anyone starts on this cluster appear here." />
          ) : (
            <table className="w-full text-[12.5px]">
              <thead className="text-left text-[11px] uppercase tracking-[0.06em] text-fg-subtle">
                <tr className="border-b border-line">
                  <th className="px-4 py-2 font-semibold">Job</th>
                  <th className="px-2 py-2 font-semibold">Host</th>
                  <th className="px-2 py-2 font-semibold">Started by</th>
                  <th className="px-2 py-2 font-semibold">Started</th>
                  <th className="px-2 py-2 font-semibold">Took</th>
                  <th className="px-4 py-2 font-semibold">Status</th>
                </tr>
              </thead>
              <tbody>
                {jobs.map((r) => (
                  <tr
                    key={r.summary.id}
                    onClick={() => focusJob(r.summary.id)}
                    onKeyDown={(e) => {
                      if (e.key === "Enter" || e.key === " ") {
                        e.preventDefault();
                        focusJob(r.summary.id);
                      }
                    }}
                    tabIndex={0}
                    aria-selected={focusedJobId === r.summary.id}
                    className="cursor-pointer border-b border-line/60 outline-none last:border-0 hover:bg-surface-2 focus-visible:bg-surface-2 aria-selected:bg-accent/8"
                  >
                    <td className="px-4 py-2 text-fg">
                      {r.summary.title}
                      {r.summary.dry_run && (
                        <Badge tone="info" className="ml-2">
                          Dry run
                        </Badge>
                      )}
                    </td>
                    <td className="px-2 py-2 font-mono text-fg-muted">{r.summary.host}</td>
                    <td className="px-2 py-2 text-fg-muted">{r.summary.user}</td>
                    <td className="px-2 py-2 tabular-nums text-fg-muted">{dateTimeMs(r.summary.created_at_ms)}</td>
                    <td className="px-2 py-2 tabular-nums text-fg-muted">{took(r, now)}</td>
                    <td className="px-4 py-2">
                      <RunStatusBadge status={recordStatus(r)} />
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </Card>
        <Card>
          <CardHeader title="Operations" subtitle="Each one opens a dialog for its options before anything runs." icon={<Play />} />
          <div className="space-y-4 p-4">
            {byCategory.map(([category, ops]) => (
              <div key={category}>
                <div className="mb-1.5 text-[11px] font-semibold uppercase tracking-[0.08em] text-fg-subtle">{category}</div>
                <div className="flex flex-wrap gap-1.5">
                  {ops.map((op) => (
                    <Tooltip key={op.id} content={op.allowed ? op.summary : `Needs the ${op.min_role} role`}>
                      <span>
                        <Button size="sm" variant={op.destructive ? "danger" : "secondary"} disabled={!op.allowed} onClick={() => startOperation(op.id)}>
                          {op.title}
                        </Button>
                      </span>
                    </Tooltip>
                  ))}
                </div>
              </div>
            ))}
            {operations.length === 0 && <p className="text-[12.5px] text-fg-subtle">No operations are registered.</p>}
          </div>
        </Card>
      </PageBody>
    </>
  );
}
