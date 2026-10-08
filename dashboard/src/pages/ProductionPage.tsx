import { Boxes, Eye, GitBranch, HardDriveDownload, KeyRound, Plus, Trash2 } from "lucide-react";
import { useStore } from "@/state/store";
import { Card, CardHeader } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Menu } from "@/components/ui/menu";
import { Tooltip } from "@/components/ui/tooltip";
import { EmptyState, StatusPill, useNow, type Health } from "@/components/status";
import { AttentionBanner, PageBody, PageHeader } from "@/components/layout/Page";
import { ExternalButton, SkeletonRows, SourceRefresh, Table, WorkflowTiles, useSource } from "./common";
import { agoEpoch, duration, untilEpoch } from "@/lib/format";

export const vmHealth = (status: string): Health =>
  status === "running" ? "ok" : status === "stopped" ? "offline" : status === "missing" || status === "unknown" ? "danger" : "warn";

export function ProductionPage() {
  const guests = useSource("list_guests");
  const repl = useSource("list_replication");
  const launchWorkflow = useStore((s) => s.launchWorkflow);
  const now = useNow(15000);
  const g = guests.data;
  const r = repl.data;

  return (
    <>
      <PageHeader
        title="Production"
        subtitle="Highly available VMs, their placement, and replication"
        actions={
          <>
            <SourceRefresh ids={["list_guests", "list_replication"]} />
            <Button variant="primary" onClick={() => launchWorkflow("add_prod_vm")}>
              <Plus /> Add production VM
            </Button>
          </>
        }
      />
      <PageBody>
        <AttentionBanner problems={[...(g?.problems.filter((p) => !p.startsWith("staging")) ?? []), ...(r?.problems ?? [])]} steps={[...guests.nextSteps, ...repl.nextSteps]} />
        <Card className="mb-4">
          <CardHeader icon={<Boxes />} title="Production VMs" subtitle={g ? `${g.production.length} registered` : undefined} />
          <Table head={["VM", "Status", "Runs on", "HA", "Replication", "Serves", "Size", ""]}>
            {!g && guests.status === "loading" && <SkeletonRows cols={8} />}
            {g?.production.map((vm) => (
              <tr key={vm.name}>
                <td>
                  <div className="font-mono font-medium text-fg">{vm.name}</div>
                  <div className="whitespace-nowrap text-[11px] text-fg-subtle">VM {vm.vmid ?? "—"}{vm.purpose && ` · ${vm.purpose}`}</div>
                </td>
                <td>
                  <StatusPill health={vmHealth(vm.live_status)}>{vm.live_status}</StatusPill>
                  {vm.registry_state && vm.registry_state !== "active" && <Badge tone="warn" className="ml-1">{vm.registry_state}</Badge>}
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
                      <span><StatusPill health={vm.ha.state === "started" ? "ok" : vm.ha.state === "error" || vm.ha.state === "fence" ? "danger" : "warn"}>{vm.ha.state ?? "unknown"}</StatusPill></span>
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
                        <div className="mt-0.5 text-[11px] text-fg-subtle">synced {agoEpoch(vm.replication.oldest_last_sync, now)}</div>
                      </div>
                    </Tooltip>
                  )}
                </td>
                <td>
                  {vm.domain ? (
                    <div className="flex items-center gap-1">
                      <span className="max-w-[180px] truncate text-fg">{vm.domain}</span>
                      <ExternalButton url={`https://${vm.domain}/`} />
                    </div>
                  ) : (
                    <span className="text-fg-subtle">—</span>
                  )}
                  {vm.routes > 0 && <div className="text-[11px] text-fg-subtle">{vm.routes} route{vm.routes === 1 ? "" : "s"}{vm.routes_enabled ? "" : " (disabled)"}</div>}
                </td>
                <td className="whitespace-nowrap text-[12px] text-fg-muted tabular-nums">
                  {vm.cores ?? "?"} vCPU · {vm.memory_mb ? `${Math.round(vm.memory_mb / 1024)} GiB` : "?"} · {vm.disk_bytes ? Number((vm.disk_bytes / 2 ** 30).toFixed(3)) : vm.disk_gib ?? "?"} GiB
                  <div className="text-[11px] text-fg-subtle">{vm.uptime_seconds ? `up ${duration(vm.uptime_seconds)}` : ""}</div>
                </td>
                <td>
                  <Menu
                    items={[
                      { label: "Show VM state", icon: <Eye />, onSelect: () => launchWorkflow("show_prod_vm_state", { resource: vm.name }) },
                      { label: "Replication jobs", icon: <GitBranch />, onSelect: () => launchWorkflow("list_replication", { guest: vm.name }) },
                      { label: "Extend disk…", icon: <HardDriveDownload />, onSelect: () => launchWorkflow("extend_prod_vm_disk") },
                      { label: "Set up jump SSH…", icon: <KeyRound />, onSelect: () => launchWorkflow("setup_jump_ssh_access", { resource: vm.name }) },
                      "separator",
                      { label: "Remove VM…", icon: <Trash2 />, danger: true, onSelect: () => launchWorkflow("remove_prod_vm", { resource: vm.name }) },
                    ]}
                  />
                </td>
              </tr>
            ))}
          </Table>
          {g && g.production.length === 0 && <EmptyState icon={<Boxes />} title="No production VMs yet" body="Add one to create an HA guest with replication to the other hosts." />}
          {!g && guests.status !== "loading" && <EmptyState icon={<Boxes />} title="Guest state not loaded" body={guests.error ?? "Refresh to read the cluster."} />}
          {g && g.unregistered.length > 0 && (
            <div className="border-t border-line px-4 py-3 text-[12px] text-fg-muted">
              <span className="font-medium text-warn">Not in the BMAC registry: </span>
              {g.unregistered.map((u) => `${u.name ?? "unnamed"} (VM ${u.vmid}${u.node ? ` on ${u.node}` : ""})`).join(", ")}
            </div>
          )}
        </Card>

        <Card className="mb-4">
          <CardHeader icon={<GitBranch />} title="Replication jobs" subtitle={r ? `${r.jobs.length} jobs` : undefined} />
          <Table head={["Job", "Guest", "Route", "Schedule", "Last sync", "Next sync", "Status"]}>
            {!r && repl.status === "loading" && <SkeletonRows cols={7} />}
            {r?.jobs.map((j) => (
              <tr key={j.id}>
                <td className="whitespace-nowrap font-mono text-fg">{j.id}</td>
                <td className="font-mono text-fg-muted">{j.guest_name ?? j.guest ?? "—"}</td>
                <td className="font-mono text-fg-muted whitespace-nowrap">{j.source ?? "?"} → {j.target ?? "?"}</td>
                <td className="font-mono text-fg-subtle">{j.schedule ?? "—"}</td>
                <td className="text-fg-muted tabular-nums whitespace-nowrap">{agoEpoch(j.last_sync, now)}{j.duration ? <span className="text-fg-subtle"> · {j.duration.toFixed(1)}s</span> : null}</td>
                <td className="text-fg-muted tabular-nums whitespace-nowrap">{untilEpoch(j.next_sync, now)}</td>
                <td>
                  {j.disabled ? (
                    <StatusPill health="offline">disabled</StatusPill>
                  ) : j.fail_count > 0 || j.error ? (
                    <Tooltip content={j.error ?? `${j.fail_count} failures`}>
                      <span><StatusPill health="danger">failing{j.fail_count ? ` ×${j.fail_count}` : ""}</StatusPill></span>
                    </Tooltip>
                  ) : (
                    <StatusPill health="ok">ok</StatusPill>
                  )}
                </td>
              </tr>
            ))}
          </Table>
          {r && r.jobs.length === 0 && <EmptyState icon={<GitBranch />} title="No replication jobs" />}
          {!r && repl.status !== "loading" && <EmptyState icon={<GitBranch />} title="Replication state not loaded" body={repl.error ?? "Refresh to read the cluster."} />}
        </Card>

        <h2 className="mb-2.5 text-[12px] font-semibold uppercase tracking-[0.08em] text-fg-subtle">Production operations</h2>
        <WorkflowTiles ids={["add_prod_vm", "change_prod_vm_owner", "change_prod_vm_placement", "extend_prod_vm_disk", "setup_jump_ssh_access", "deploy_hello_app_to_prod", "show_prod_vm_state", "remove_prod_vm"]} />
      </PageBody>
    </>
  );
}
