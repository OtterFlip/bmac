// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

import { ScrollText } from "lucide-react";
import { Card, EmptyState } from "@bmac/ui";
import { PageBody, PageHeader, Problem, useLoad, when } from "../parts";

export function AuditPage() {
  const { data, error } = useLoad((api) => api.audit(500), 30_000);
  return (
    <>
      <PageHeader title="Audit log" subtitle="Everything done to this appliance, newest first" />
      <PageBody>
        <Problem>{error}</Problem>
        {data && data.entries.length === 0 && <EmptyState icon={<ScrollText />} title="Nothing yet" />}
        {data && data.entries.length > 0 && (
          <Card>
            <table className="w-full text-[12.5px]">
              <thead className="text-left text-fg-subtle">
                <tr className="border-b border-line">
                  <th className="px-3 py-2 font-medium">When</th>
                  <th className="px-3 py-2 font-medium">Event</th>
                  <th className="px-3 py-2 font-medium">By</th>
                  <th className="px-3 py-2 font-medium">Details</th>
                </tr>
              </thead>
              <tbody>
                {data.entries.map((e, i) => (
                  <tr key={`${e.at_ms}-${i}`} className="border-b border-line/50 align-top">
                    <td className="whitespace-nowrap px-3 py-1.5">{when(e.at_ms)}</td>
                    <td className="px-3 py-1.5 font-mono">{e.event}</td>
                    <td className="whitespace-nowrap px-3 py-1.5">{e.by}</td>
                    <td className="selectable break-all px-3 py-1.5 font-mono text-[11px] text-fg-subtle">{e.details === null ? "" : JSON.stringify(e.details)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </Card>
        )}
      </PageBody>
    </>
  );
}
