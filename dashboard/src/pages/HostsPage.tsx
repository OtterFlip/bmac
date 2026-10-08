import { ExternalLink, Eye, HardDrive, Server, ShieldAlert, Trash2 } from "lucide-react";
import { useStore } from "@/state/store";
import { Card, CardHeader } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { Menu } from "@/components/ui/menu";
import { Tooltip } from "@/components/ui/tooltip";
import { EmptyState, KV, Meter, StatusPill } from "@/components/status";
import { AttentionBanner, PageBody, PageHeader } from "@/components/layout/Page";
import { ExternalButton, SkeletonRows, SourceRefresh, Table, WorkflowTiles, openExternal, proxmoxUrl, useSource } from "./common";
import { bytes, duration, percent } from "@/lib/format";

export function HostsPage() {
  const { data, status, error, nextSteps } = useSource("list_hosts");
  const launchWorkflow = useStore((s) => s.launchWorkflow);
  const loading = status === "loading" && !data;

  return (
    <>
      <PageHeader title="Hosts" subtitle="Proxmox nodes in the cluster and their quorum" actions={<SourceRefresh ids={["list_hosts"]} />} />
      <PageBody>
        {data && <AttentionBanner problems={data.problems} steps={nextSteps} />}
        <div className="mb-4 grid gap-4 xl:grid-cols-[minmax(0,1fr)_320px]">
          <Card>
            <CardHeader icon={<Server />} title="Nodes" subtitle={data ? `${data.cluster.online_count} of ${data.cluster.host_count} online` : undefined} />
            <Table head={["Host", "Status", "CPU", "Memory", "Root disk", "Uptime", "Role", ""]}>
              {loading && <SkeletonRows cols={8} />}
              {data?.hosts.map((h) => (
                <tr key={h.name}>
                  <td>
                    <div className="font-mono font-medium text-fg">{h.name}</div>
                    <div className="font-mono text-[11px] text-fg-subtle">{h.ip ?? "—"}</div>
                  </td>
                  <td>
                    <div className="flex flex-col items-start gap-1">
                      <StatusPill health={h.online ? "ok" : "danger"}>{h.online ? "online" : "offline"}</StatusPill>
                      {h.ssh_from_here === false && (
                        <Tooltip content="This computer could not reach the host over SSH.">
                          <span><StatusPill health="warn">no SSH</StatusPill></span>
                        </Tooltip>
                      )}
                    </div>
                  </td>
                  <td className="w-[120px]">
                    <div className="mb-1 whitespace-nowrap text-[11.5px] text-fg-muted tabular-nums">{percent(h.cpu_fraction)} of {h.cores ?? "?"} cores</div>
                    <Meter fraction={h.cpu_fraction} />
                  </td>
                  <td className="w-[140px]">
                    <div className="mb-1 whitespace-nowrap text-[11.5px] text-fg-muted tabular-nums">{bytes(h.memory_used, 0)} / {bytes(h.memory_total, 0)}</div>
                    <Meter fraction={h.memory_total ? (h.memory_used ?? 0) / h.memory_total : null} />
                  </td>
                  <td className="w-[140px]">
                    <div className="mb-1 whitespace-nowrap text-[11.5px] text-fg-muted tabular-nums">{bytes(h.disk_used, 0)} / {bytes(h.disk_total, 0)}</div>
                    <Meter fraction={h.disk_total ? (h.disk_used ?? 0) / h.disk_total : null} />
                  </td>
                  <td className="whitespace-nowrap text-fg-muted tabular-nums">{h.online ? duration(h.uptime_seconds) : "—"}</td>
                  <td>
                    <div className="flex flex-wrap gap-1">
                      {h.is_control && <Badge tone="accent">control</Badge>}
                      {h.is_probe && <Badge>read via</Badge>}
                      {h.slot_state && h.slot_state !== "active" && <Badge tone="warn">{h.slot_state}</Badge>}
                    </div>
                  </td>
                  <td>
                    <div className="flex items-center justify-end gap-0.5">
                      <ExternalButton url={proxmoxUrl(h.name)} />
                      <Menu
                        items={[
                          { label: "Show host state", icon: <Eye />, onSelect: () => launchWorkflow("show_proxmox_host_state", { host: h.name }) },
                          { label: "Inventory disks", icon: <HardDrive />, onSelect: () => launchWorkflow("inventory_disks", { host: h.name }) },
                          { label: "Open Proxmox web UI", icon: <ExternalLink />, onSelect: () => openExternal(proxmoxUrl(h.name)) },
                          "separator",
                          { label: "Remove host…", icon: <Trash2 />, danger: true, onSelect: () => launchWorkflow("remove_proxmox_host", { host: h.name }) },
                        ]}
                      />
                    </div>
                  </td>
                </tr>
              ))}
            </Table>
            {!data && !loading && <EmptyState icon={<Server />} title="Host state not loaded" body={error ?? "Refresh to read the cluster."} />}
          </Card>

          <Card>
            <CardHeader icon={<ShieldAlert />} title="Cluster" subtitle={data?.cluster.name ?? undefined} />
            <div className="p-4">
              {data ? (
                <KV
                  items={[
                    ["Quorum", <StatusPill key="q" health={data.cluster.quorate ? "ok" : data.cluster.quorate === false ? "danger" : "unknown"}>{data.cluster.quorate ? "quorate" : data.cluster.quorate === false ? "not quorate" : "unknown"}</StatusPill>],
                    ["Votes", data.cluster.total_votes !== null ? `${data.cluster.total_votes} of ${data.cluster.expected_votes} (needs ${data.cluster.quorum})` : "—"],
                    ["Control node", <span key="c" className="font-mono">{data.cluster.control_node ?? "—"}</span>],
                    ["QDevice", data.qdevice.registered ? "registered" : data.qdevice.configured_host ? `configured (${data.qdevice.configured_host}), not registered` : "not configured"],
                    ["Read through", <span key="p" className="font-mono">{data.probe ?? "—"}</span>],
                  ]}
                />
              ) : (
                <p className="text-[12.5px] text-fg-subtle">{loading ? "Reading…" : "No data."}</p>
              )}
            </div>
          </Card>
        </div>

        <h2 className="mb-2.5 text-[12px] font-semibold uppercase tracking-[0.08em] text-fg-subtle">Host operations</h2>
        <WorkflowTiles ids={["add_proxmox_host", "remove_proxmox_host", "update_cluster_runtime", "show_proxmox_host_state", "show_cluster_state"]} />
      </PageBody>
    </>
  );
}
