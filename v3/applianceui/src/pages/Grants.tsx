// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

import { KeyRound } from "lucide-react";
import { Badge, Button, Card, EmptyState } from "@bmac/ui";
import { Done, PageBody, PageHeader, Problem, short, useAction, useApi, useLoad, when } from "../parts";

export function GrantsPage() {
  const api = useApi();
  const { data, error, reload } = useLoad((a) => a.grants());
  const action = useAction();
  return (
    <>
      <PageHeader title="Restore grants" subtitle="What a cluster's hosts may read from here for a restore. Grant new ones from the PITs page." />
      <PageBody>
        <Problem>{error ?? action.error}</Problem>
        <Done>{action.done}</Done>
        {data && data.length === 0 && <EmptyState icon={<KeyRound />} title="No restore grants" body="A grant lets one cluster read the PITs it names, for a few hours." />}
        {data && data.length > 0 && (
          <Card>
            <table className="w-full text-[12.5px]">
              <thead className="text-left text-fg-subtle">
                <tr className="border-b border-line">
                  <th className="px-3 py-2 font-medium">Grant</th>
                  <th className="px-3 py-2 font-medium">Cluster</th>
                  <th className="px-3 py-2 font-medium">PITs</th>
                  <th className="px-3 py-2 font-medium">Until</th>
                  <th className="px-3 py-2 font-medium">Last used</th>
                  <th className="px-3 py-2 font-medium">State</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {data.map((g) => (
                  <tr key={g.grant_id} className="border-b border-line/50">
                    <td className="px-3 py-1.5 font-mono">{g.grant_id}</td>
                    <td className="px-3 py-1.5 font-mono">{short(g.cluster_uuid)}</td>
                    <td className="px-3 py-1.5 font-mono">{g.pits.map(short).join(", ")}</td>
                    <td className="px-3 py-1.5">{when(g.expires_at_ms)}</td>
                    <td className="px-3 py-1.5">{when(g.last_used_ms)}</td>
                    <td className="px-3 py-1.5">
                      <Badge tone={g.state === "live" ? "ok" : "neutral"}>{g.state}</Badge>
                    </td>
                    <td className="px-3 py-1.5 text-right">
                      {g.state === "live" && (
                        <Button size="sm" variant="danger-outline" disabled={action.busy} onClick={() => void action.run(async () => (await api.revokeGrant(g.grant_id), await reload(), `Grant ${g.grant_id} is revoked.`))}>
                          Revoke
                        </Button>
                      )}
                    </td>
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
