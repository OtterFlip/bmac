// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

import { ArrowRightLeft, Boxes, Eye, GitBranch, HardDriveDownload, MapPin, Plus, Trash2 } from "lucide-react";
import { Badge, Card, CardHeader, EmptyState, Menu, StatusPill, Tooltip, ago, duration, useNow, type ProductionVm } from "@bmac/ui";
import { PageBody, PageHeader } from "../components/layout/Page";
import { useStatus } from "../state/useStatus";
import { ConditionsBanner, OperationButton, OperationTiles, SectionTitle, SkeletonRows, StatusMissing, StatusRefresh, Table, useOperationItems, vmHealth } from "./common";

function until(ms: number | null, now: number): string {
  if (!ms) return "—";
  const s = Math.round((ms - now) / 1000);
  return s <= 0 ? "due" : `in ${duration(s)}`;
}

function VmMenu({ vm }: { vm: ProductionVm }) {
  const resource = { resource: vm.name };
  const items = useOperationItems([
    { id: "diagnostics.prod_vm_state", values: resource, label: "Inspect this VM", icon: <Eye /> },
    { id: "prod.change_owner", values: resource, icon: <ArrowRightLeft /> },
    { id: "prod.change_placement", values: resource, icon: <MapPin /> },
    { id: "prod.extend_disk", values: resource, icon: <HardDriveDownload /> },
  ]);
  const remove = useOperationItems([{ id: "prod.remove", values: resource, label: "Remove VM…", icon: <Trash2 /> }]);
  const all = [...items, ...(items.length && remove.length ? (["separator"] as const) : []), ...remove];
  return all.length ? <Menu items={all} label={`${vm.name} actions`} /> : null;
}

export function ProductionPage() {
  const vms = useStatus("vms");
  const now = useNow(15000);
  const v = vms.data;
  const conditions = v?.conditions.filter((c) => c.id !== "staging.cleanup" && !v.staging.some((s) => s.name === c.resource)) ?? [];

  return (
    <>
      <PageHeader
        title="Production"
        subtitle="Highly available VMs, their placement, and replication"
        actions={
          <>
            <StatusRefresh kinds={["vms"]} />
            <OperationButton id="prod.add" variant="primary" size="md" icon={<Plus />} label="Add production VM" />
          </>
        }
      />
      <PageBody>
        <ConditionsBanner conditions={conditions} />
        {v && v.stale_hosts.length > 0 && (
          <p className="mb-4 rounded-lg border border-warn/30 bg-warn/8 px-3 py-2 text-[12.5px] text-warn">
            The guests on {v.stale_hosts.join(", ")} may be out of date: the host answering can't reach {v.stale_hosts.length === 1 ? "it" : "them"} right now.
          </p>
        )}
        <Card className="mb-4">
          <CardHeader icon={<Boxes />} title="Production VMs" subtitle={v ? `${v.production.length} registered` : undefined} />
          {v || vms.loading ? (
            <Table label="Production VMs" head={["VM", "Status", "Runs on", "HA", "Replication", "Serves", "Size", ""]}>
              {!v && <SkeletonRows cols={8} />}
              {v?.production.map((vm) => (
                <tr key={vm.name}>
                  <td>
                    <div className="font-mono font-medium text-fg">{vm.name}</div>
                    <div className="whitespace-nowrap text-[11px] text-fg-subtle">
                      VM {vm.vmid}
                      {vm.purpose && ` · ${vm.purpose}`}
                    </div>
                  </td>
                  <td>
                    <StatusPill health={vmHealth(vm.live_status)}>{vm.live_status}</StatusPill>
                    {vm.registry_state !== "active" && (
                      <Badge tone="warn" className="ml-1">
                        {vm.registry_state.replace(/_/g, " ")}
                      </Badge>
                    )}
                  </td>
                  <td>
                    <div className="font-mono text-fg">{vm.node ?? "—"}</div>
                    <div className="whitespace-nowrap text-[11px] text-fg-subtle">
                      {vm.owner_node && vm.owner_node !== vm.node ? `owner ${vm.owner_node} · ` : ""}
                      {vm.placement.length ? `placement ${vm.placement.join(",")}` : ""}
                    </div>
                  </td>
                  <td>
                    {vm.ha.configured ? (
                      <Tooltip content={`requested ${vm.ha.requested_state ?? "?"}`}>
                        <span>
                          <StatusPill health={vm.ha.state === "started" ? "ok" : vm.ha.state === "error" || vm.ha.state === "fence" ? "danger" : "warn"}>{vm.ha.state ?? "unknown"}</StatusPill>
                        </span>
                      </Tooltip>
                    ) : (
                      <StatusPill health="warn">not managed</StatusPill>
                    )}
                  </td>
                  <td>
                    {vm.replication.jobs === 0 ? (
                      <span className="text-fg-subtle">none</span>
                    ) : (
                      <Tooltip content={vm.replication.errors.join("\n") || `to ${vm.replication.targets.join(", ")}`}>
                        <div>
                          <StatusPill health={vm.replication.failing ? "danger" : "ok"}>
                            {vm.replication.failing ? `${vm.replication.failing}/${vm.replication.jobs} failing` : `${vm.replication.jobs} healthy`}
                          </StatusPill>
                          <div className="mt-0.5 text-[11px] text-fg-subtle">synced {ago(vm.replication.oldest_last_sync_ms, now)}</div>
                        </div>
                      </Tooltip>
                    )}
                  </td>
                  <td>
                    {vm.domain ? <span className="block max-w-[200px] truncate text-fg">{vm.domain}</span> : <span className="text-fg-subtle">—</span>}
                    {vm.aliases.length > 0 && <div className="max-w-[200px] truncate text-[11px] text-fg-subtle">also {vm.aliases.join(", ")}</div>}
                    {vm.domain && !vm.routes_enabled && <div className="text-[11px] text-warn">routes disabled</div>}
                  </td>
                  <td className="whitespace-nowrap text-[12px] text-fg-muted tabular-nums">
                    {vm.cores} vCPU · {Math.round(vm.memory_mb / 1024)} GiB · {Number((vm.disk_bytes / 2 ** 30).toFixed(3))} GiB
                    <div className="text-[11px] text-fg-subtle">{vm.uptime_seconds ? `up ${duration(vm.uptime_seconds)}` : ""}</div>
                  </td>
                  <td>
                    <VmMenu vm={vm} />
                  </td>
                </tr>
              ))}
            </Table>
          ) : (
            <StatusMissing entry={vms} what="VM status" icon={<Boxes />} />
          )}
          {v && v.production.length === 0 && <EmptyState icon={<Boxes />} title="No production VMs yet" body="Add one to create an HA guest with replication to the other hosts." />}
          {v && v.unregistered.length > 0 && (
            <div className="border-t border-line px-4 py-3 text-[12px] text-fg-muted">
              <span className="font-medium text-warn">Not in the BMAC registry: </span>
              {v.unregistered.map((u) => `${u.name ?? "unnamed"} (VM ${u.vmid} on ${u.node})`).join(", ")}
            </div>
          )}
        </Card>

        <Card className="mb-4">
          <CardHeader icon={<GitBranch />} title="Replication jobs" subtitle={v ? `${v.replication.length} jobs` : undefined} />
          {v || vms.loading ? (
            <Table label="Replication jobs" head={["Job", "Guest", "Route", "Schedule", "Last sync", "Next sync", "Status"]}>
              {!v && <SkeletonRows cols={7} />}
              {v?.replication.map((j) => (
                <tr key={`${j.source}/${j.id}`}>
                  <td className="whitespace-nowrap font-mono text-fg">{j.id}</td>
                  <td className="font-mono text-fg-muted">{j.guest_name ?? j.guest}</td>
                  <td className="whitespace-nowrap font-mono text-fg-muted">
                    {j.source} → {j.target}
                  </td>
                  <td className="font-mono text-fg-subtle">{j.schedule ?? "—"}</td>
                  <td className="whitespace-nowrap text-fg-muted tabular-nums">
                    {ago(j.last_sync_ms, now)}
                    {j.duration_seconds ? <span className="text-fg-subtle"> · {j.duration_seconds.toFixed(1)}s</span> : null}
                  </td>
                  <td className="whitespace-nowrap text-fg-muted tabular-nums">{until(j.next_sync_ms, now)}</td>
                  <td>
                    {j.disabled ? (
                      <StatusPill health="offline">disabled</StatusPill>
                    ) : j.fail_count > 0 || j.error ? (
                      <Tooltip content={j.error ?? `${j.fail_count} failures`}>
                        <span>
                          <StatusPill health="danger">failing{j.fail_count ? ` ×${j.fail_count}` : ""}</StatusPill>
                        </span>
                      </Tooltip>
                    ) : (
                      <StatusPill health="ok">ok</StatusPill>
                    )}
                  </td>
                </tr>
              ))}
            </Table>
          ) : (
            <StatusMissing entry={vms} what="Replication status" icon={<GitBranch />} />
          )}
          {v && v.replication.length === 0 && <EmptyState icon={<GitBranch />} title="No replication jobs" />}
        </Card>

        <SectionTitle>Production operations</SectionTitle>
        <OperationTiles ids={["prod.add", "prod.change_owner", "prod.change_placement", "prod.extend_disk", "diagnostics.prod_vm_state", "prod.remove"]} />
      </PageBody>
    </>
  );
}
