// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

import { Gauge, Timer } from "lucide-react";
import { useShallow } from "zustand/react/shallow";
import { Card, CardHeader, RunStatusIcon, ago, elapsedMs, isTerminal, recordStatus, useNow } from "@bmac/ui";
import { PageBody, PageHeader } from "../components/layout/Page";
import { useStore } from "../state/store";
import { OperationTiles, SectionTitle } from "./common";

const REPORTS = ["diagnostics.cluster_health", "diagnostics.cluster_state", "diagnostics.host_state", "diagnostics.prod_vm_state", "diagnostics.qdevice_state"];

export function DiagnosticsPage() {
  const focusJob = useStore((s) => s.focusJob);
  const now = useNow(10000);
  const runs = useStore(
    useShallow((s) =>
      Object.values(s.jobs)
        .filter((r) => s.operations[r.summary.operation]?.category === "diagnostics")
        .sort((a, b) => b.summary.created_at_ms - a.summary.created_at_ms)
        .slice(0, 10),
    ),
  );
  return (
    <>
      <PageHeader title="Diagnostics" subtitle="Read-only checks. None of these change the cluster." />
      <PageBody>
        <SectionTitle className="mb-1 flex items-center gap-2">
          <Gauge className="size-3.5" /> Live status
        </SectionTitle>
        <p className="mb-6 max-w-[720px] text-[12.5px] leading-relaxed text-fg-subtle">
          Every host reports its own state continuously, and the Overview, Hosts, Production, Staging, Storage, and QDevice pages show it as it changes. There's nothing to run for
          that.
        </p>
        <SectionTitle className="mb-1 flex items-center gap-2">
          <Timer className="size-3.5" /> Full reports
        </SectionTitle>
        <p className="mb-2.5 text-[12.5px] text-fg-subtle">Thorough checks that can take a minute or more. Their full output streams to the console below.</p>
        <OperationTiles ids={REPORTS} className="mb-6" />
        {runs.length > 0 && (
          <Card>
            <CardHeader title="Recent diagnostic runs" />
            <div className="divide-y divide-line/70">
              {runs.map((r) => {
                const status = recordStatus(r);
                const end = r.ended_at_ms ?? (isTerminal(status) ? r.summary.updated_at_ms : null);
                return (
                  <button key={r.summary.id} onClick={() => focusJob(r.summary.id)} className="flex w-full items-center gap-3 px-4 py-2.5 text-left hover:bg-surface-2/60">
                    <RunStatusIcon status={status} />
                    <span className="flex-1 truncate text-[12.5px] text-fg">
                      {r.summary.title}
                      <span className="text-fg-muted"> · {r.summary.host}</span>
                    </span>
                    <span className="text-[11.5px] text-fg-subtle tabular-nums">{elapsedMs(r.started_at_ms ?? r.summary.created_at_ms, end, now)}</span>
                    <span className="w-[72px] text-right text-[11.5px] text-fg-subtle">{ago(r.summary.created_at_ms, now)}</span>
                  </button>
                );
              })}
            </div>
          </Card>
        )}
      </PageBody>
    </>
  );
}
