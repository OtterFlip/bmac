import { Database, HardDrive, Plus, Replace, Trash2 } from "lucide-react";
import { useStore } from "@/state/store";
import { Card, CardHeader } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { Menu } from "@/components/ui/menu";
import { EmptyState, Meter, Skeleton, StatusPill } from "@/components/status";
import { AttentionBanner, PageBody, PageHeader } from "@/components/layout/Page";
import { SourceRefresh, Table, WorkflowTiles, useSource } from "./common";
import { bytes, percent } from "@/lib/format";

export function StoragePage() {
  const { data, status, error, nextSteps } = useSource("list_storage");
  const launchWorkflow = useStore((s) => s.launchWorkflow);

  return (
    <>
      <PageHeader title="Storage" subtitle="ZFS pools and Proxmox storage on every host" actions={<SourceRefresh ids={["list_storage"]} />} />
      <PageBody>
        {data && <AttentionBanner problems={data.problems} steps={nextSteps} />}
        {!data && status === "loading" && (
          <div className="mb-4 grid gap-4 xl:grid-cols-2">
            {[0, 1].map((i) => (
              <Card key={i} className="space-y-3 p-4">
                <Skeleton className="w-1/3" />
                <Skeleton className="w-full" />
                <Skeleton className="w-2/3" />
              </Card>
            ))}
          </div>
        )}
        {!data && status !== "loading" && <Card className="mb-4"><EmptyState icon={<Database />} title="Storage state not loaded" body={error ?? "Refresh to read the cluster."} /></Card>}
        <div className="mb-4 grid gap-4 xl:grid-cols-2">
          {data?.hosts.map((host) => (
            <Card key={host.node}>
              <CardHeader
                icon={<HardDrive />}
                title={<span className="font-mono">{host.node}</span>}
                subtitle={host.online ? `${host.pools.length} pool${host.pools.length === 1 ? "" : "s"}` : "offline"}
                actions={
                  <Menu
                    items={[
                      { label: "Inventory disks", icon: <HardDrive />, onSelect: () => launchWorkflow("inventory_disks", { host: host.node }) },
                      { label: "Add a new disk vdev…", icon: <Plus />, onSelect: () => launchWorkflow("add_new_disk_vdev", { host: host.node }) },
                      { label: "Replace a failed disk…", icon: <Replace />, onSelect: () => launchWorkflow("add_replacement_disk", { host: host.node }) },
                      "separator",
                      { label: "Decommission disks…", icon: <Trash2 />, danger: true, onSelect: () => launchWorkflow("decommission_disks", { host: host.node }) },
                    ]}
                  />
                }
              />
              {!host.online && <EmptyState title="Host is offline" body="Its storage can't be read until it's back." />}
              {host.pools.map((p) => (
                <div key={p.name} className="border-b border-line/70 px-4 py-3">
                  <div className="mb-1.5 flex items-center gap-2">
                    <span className="font-mono text-[13px] font-medium text-fg">{p.name}</span>
                    <StatusPill health={p.health === "ONLINE" ? "ok" : p.health === "DEGRADED" ? "warn" : "danger"}>{p.health?.toLowerCase() ?? "unknown"}</StatusPill>
                    <span className="ml-auto text-[12px] text-fg-muted tabular-nums">
                      {bytes(p.alloc)} of {bytes(p.size)} · <span className="text-fg">{percent(p.free_fraction)} free</span>
                    </span>
                  </div>
                  <Meter fraction={p.size ? p.alloc / p.size : null} className="h-2" />
                  {p.frag !== null && <div className="mt-1 text-[11px] text-fg-subtle">fragmentation {p.frag}%</div>}
                </div>
              ))}
              {host.storages.length > 0 && (
                <Table head={["Storage", "Type", "Used", "Content"]}>
                  {host.storages.map((s) => (
                    <tr key={s.storage}>
                      <td>
                        <span className="font-mono text-fg">{s.storage}</span>
                        {!s.active && <Badge tone={s.enabled ? "danger" : "neutral"} className="ml-1.5">{s.enabled ? "inactive" : "disabled"}</Badge>}
                      </td>
                      <td className="text-fg-muted">{s.type ?? "—"}</td>
                      <td className="w-[180px]">
                        {s.total ? (
                          <>
                            <div className="mb-1 text-[11px] text-fg-muted tabular-nums">{bytes(s.used)} / {bytes(s.total)}</div>
                            <Meter fraction={(s.used ?? 0) / s.total} />
                          </>
                        ) : (
                          <span className="text-fg-subtle">—</span>
                        )}
                      </td>
                      <td className="text-[11.5px] text-fg-subtle">{s.content?.split(",").join(", ") ?? "—"}</td>
                    </tr>
                  ))}
                </Table>
              )}
            </Card>
          ))}
        </div>
        <h2 className="mb-2.5 text-[12px] font-semibold uppercase tracking-[0.08em] text-fg-subtle">Disk operations</h2>
        <WorkflowTiles ids={["inventory_disks", "add_new_disk_vdev", "add_replacement_disk", "decommission_disks", "simulate_disk_failure_and_replacement"]} />
      </PageBody>
    </>
  );
}
