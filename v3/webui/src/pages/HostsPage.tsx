// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

import { Eye, Server, ShieldAlert, Trash2 } from "lucide-react";
import { Badge, Card, CardHeader, KV, Menu, Meter, StatusPill, Tooltip, bytes, duration, percent } from "@bmac/ui";
import { PageBody, PageHeader } from "../components/layout/Page";
import { useStatus } from "../state/useStatus";
import { JoinBundles } from "./JoinBundles";
import { ConditionsBanner, Freshness, OperationTiles, SectionTitle, SkeletonRows, StatusMissing, StatusRefresh, Table, useOperationItems } from "./common";
import type { HostRow } from "@bmac/ui";

const SLOT_STATE_HELP: Record<string, string> = {
  member: "The cluster registry records this host slot as a joined cluster member.",
  joining: "The cluster registry has reserved this host slot for a host that is still being added; it becomes a member once the join finishes.",
};

function HostMenu({ host }: { host: HostRow }) {
  const items = useOperationItems([{ id: "diagnostics.host_state", values: { host: host.name }, label: "Inspect this host", icon: <Eye /> }]);
  const remove = useOperationItems([{ id: "hosts.remove", values: { host: host.name }, label: "Remove host…", icon: <Trash2 /> }]);
  const all = [...items, ...(items.length && remove.length ? (["separator"] as const) : []), ...remove];
  return all.length ? <Menu items={all} label={`${host.name} actions`} /> : null;
}

export function HostsPage() {
  const hosts = useStatus("hosts");
  const data = hosts.data;

  return (
    <>
      <PageHeader title="Hosts" subtitle="Proxmox hosts in the cluster and their quorum" actions={<StatusRefresh kinds={["hosts"]} />} />
      <PageBody>
        <ConditionsBanner conditions={data?.conditions ?? []} />
        <div className="mb-4 grid gap-4 xl:grid-cols-[minmax(0,1fr)_320px]">
          <Card>
            <CardHeader icon={<Server />} title="Hosts" subtitle={data ? `${data.cluster.online_count} of ${data.cluster.host_count} online` : undefined} />
            {data || hosts.loading ? (
              <Table label="Hosts" head={["Host", "Status", "CPU", "Memory", "Root disk", "Uptime", "Role", ""]}>
                {!data && <SkeletonRows cols={8} />}
                {data?.hosts.map((h) => {
                  const o = h.observation;
                  return (
                    <tr key={h.name}>
                      <td>
                        <div className="font-mono font-medium text-fg">{h.name}</div>
                        <div className="font-mono text-[11px] text-fg-subtle">{h.address ?? "—"}</div>
                      </td>
                      <td>
                        <div className="flex flex-col items-start gap-1">
                          <StatusPill health={h.online ? (h.freshness.reachable ? "ok" : "warn") : "danger"}>{h.online ? (h.freshness.reachable ? "online" : "unreachable") : "offline"}</StatusPill>
                          <span className="text-[11px]">
                            <Freshness observedAt={h.freshness.observed_at_ms} reachable={h.freshness.reachable} online={h.online} />
                          </span>
                        </div>
                      </td>
                      <td className="w-[120px]">
                        <div className="mb-1 whitespace-nowrap text-[11.5px] text-fg-muted tabular-nums">
                          {percent(o?.cpu_fraction)} of {o?.cores ?? "?"} cores
                        </div>
                        <Meter fraction={o?.cpu_fraction} />
                      </td>
                      <td className="w-[140px]">
                        <div className="mb-1 whitespace-nowrap text-[11.5px] text-fg-muted tabular-nums">
                          {bytes(o?.memory_used, 0)} / {bytes(o?.memory_total, 0)}
                        </div>
                        <Meter fraction={o?.memory_total ? (o.memory_used ?? 0) / o.memory_total : null} />
                      </td>
                      <td className="w-[140px]">
                        <div className="mb-1 whitespace-nowrap text-[11.5px] text-fg-muted tabular-nums">
                          {bytes(o?.root_used, 0)} / {bytes(o?.root_total, 0)}
                        </div>
                        <Meter fraction={o?.root_total ? (o.root_used ?? 0) / o.root_total : null} />
                      </td>
                      <td className="whitespace-nowrap text-fg-muted tabular-nums">{h.online ? duration(o?.uptime_seconds) : "—"}</td>
                      <td>
                        <div className="flex flex-wrap gap-1">
                          {h.is_duty && (
                            <Tooltip content="The duty host: the lowest-numbered online host of the quorate partition. It runs schedules and cluster-wide checks.">
                              <span>
                                <Badge tone="accent">duty</Badge>
                              </span>
                            </Tooltip>
                          )}
                          {h.is_serving && (
                            <Tooltip content="You are connected to this host; this page shows the cluster as it sees it.">
                              <span>
                                <Badge>this host</Badge>
                              </span>
                            </Tooltip>
                          )}
                          {h.slot_state && (
                            <Tooltip content={SLOT_STATE_HELP[h.slot_state] ?? `Registry host slot state: ${h.slot_state}.`}>
                              <span>
                                <Badge tone={h.slot_state === "member" ? "neutral" : "warn"}>{h.slot_state}</Badge>
                              </span>
                            </Tooltip>
                          )}
                        </div>
                        {o && <div className="mt-1 text-[11px] text-fg-subtle">bmac {o.bmac_version}</div>}
                      </td>
                      <td>
                        <div className="flex items-center justify-end">
                          <HostMenu host={h} />
                        </div>
                      </td>
                    </tr>
                  );
                })}
              </Table>
            ) : (
              <StatusMissing entry={hosts} what="Host status" icon={<Server />} />
            )}
          </Card>

          <Card>
            <CardHeader icon={<ShieldAlert />} title="Cluster" subtitle={data?.cluster.name ?? undefined} />
            <div className="p-4">
              {data ? (
                <KV
                  items={[
                    [
                      "Quorum",
                      <StatusPill key="q" health={data.cluster.quorate ? "ok" : data.cluster.quorate === false ? "danger" : "unknown"}>
                        {data.cluster.quorate ? "quorate" : data.cluster.quorate === false ? "not quorate" : "unknown"}
                      </StatusPill>,
                    ],
                    ["Votes", data.cluster.total_votes !== null ? `${data.cluster.total_votes} of ${data.cluster.expected_votes} (needs ${data.cluster.quorum})` : "—"],
                    ["Duty host", <span key="d" className="font-mono">{data.cluster.duty_host ?? "—"}</span>],
                    ["QDevice", data.qdevice.registered ? (data.qdevice.voting === false ? "registered, not voting" : "registered") : "not registered"],
                    ["Answered by", <span key="s" className="font-mono">{data.served_by ?? "—"}</span>],
                  ]}
                />
              ) : (
                <p className="text-[12.5px] text-fg-subtle">{hosts.loading ? "Reading…" : "No data."}</p>
              )}
            </div>
          </Card>
        </div>

        <JoinBundles members={data?.hosts.map((h) => h.name) ?? []} />

        <SectionTitle>Host operations</SectionTitle>
        <OperationTiles ids={["diagnostics.host_state", "diagnostics.cluster_state", "hosts.remove"]} />
      </PageBody>
    </>
  );
}
