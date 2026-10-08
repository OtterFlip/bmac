import { Fragment, type ReactNode } from "react";
import { Database, Disc3, HardDrive, Layers, Lock, Plus, Replace, Trash2 } from "lucide-react";
import { useStore } from "@/state/store";
import { Card, CardHeader } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Menu } from "@/components/ui/menu";
import { EmptyState, Meter, Skeleton, StatusPill, type Health } from "@/components/status";
import { AttentionBanner, PageBody, PageHeader } from "@/components/layout/Page";
import { SourceRefresh, Table, WorkflowTiles, useSource } from "./common";
import { bytes, percent } from "@/lib/format";
import type { DiskHost, DiskStatus, VdevRow, VdevStatus } from "@/protocol/state";

const vdevHealth: Record<VdevStatus, Health> = {
  online: "ok",
  degraded: "warn",
  faulted: "danger",
  evacuating: "run",
  resilvering: "run",
};

const diskHealth: Record<DiskStatus, Health> = {
  available: "ok",
  awaiting_finalization: "warn",
  evacuating: "run",
  pending_replacement: "warn",
  pending_addition: "warn",
  in_use: "info",
  no_serial: "offline",
};

function memberHealth(state: string | null): Health {
  if (state === "ONLINE") return "ok";
  if (state === "DEGRADED" || state === "OFFLINE") return "warn";
  return state ? "danger" : "unknown";
}

function Section({ title, children }: { title: string; children: ReactNode }) {
  return (
    <>
      <h2 className="mb-2.5 text-[12px] font-semibold uppercase tracking-[0.08em] text-fg-subtle">{title}</h2>
      <div className="mb-6 space-y-4">{children}</div>
    </>
  );
}

function Bytes({ value }: { value: number | null }) {
  return <span title={value !== null ? `${value.toLocaleString()} bytes` : undefined}>{bytes(value)}</span>;
}

function Encryption({ value }: { value: VdevRow["encryption"] }) {
  if (value === "luks") return <Badge tone="accent"><Lock />LUKS</Badge>;
  if (value === "mixed") return <Badge tone="warn">mixed</Badge>;
  if (value === "none") return <Badge>none</Badge>;
  return <span className="text-fg-subtle">—</span>;
}

function DisksLoading({ status, error }: { status: string; error: string | null }) {
  if (status === "loading") {
    return (
      <Card className="space-y-3 p-4">
        <Skeleton className="w-1/4" />
        <Skeleton className="w-full" />
        <Skeleton className="w-2/3" />
      </Card>
    );
  }
  return <Card><EmptyState icon={<Disc3 />} title="Disk state not loaded" body={error ?? "Refresh to read every host's disks."} /></Card>;
}

function HostUnavailable({ host }: { host: DiskHost }) {
  return host.online ? (
    <EmptyState title="Disks could not be read" body={host.error ?? "Refresh to try again."} />
  ) : (
    <EmptyState title="Host is offline" body="Its disks can't be read until it's back." />
  );
}

function VdevCard({ host }: { host: DiskHost }) {
  const progress = host.pools.flatMap((p) =>
    [
      host.vdevs.some((v) => v.pool === p.name && v.status === "evacuating") && p.remove ? `${p.name} removal: ${p.remove.split("\n").join(" ")}` : null,
      host.vdevs.some((v) => v.pool === p.name && v.status === "resilvering") && p.scan ? `${p.name} ${p.scan.split("\n").join(" ")}` : null,
    ].filter((line): line is string => line !== null),
  );
  return (
    <Card>
      <CardHeader
        icon={<Layers />}
        title={<span className="font-mono">{host.node}</span>}
        subtitle={host.readable ? `${host.vdevs.length} vdev${host.vdevs.length === 1 ? "" : "s"} in ${host.pools.length} pool${host.pools.length === 1 ? "" : "s"}` : host.online ? "unreadable" : "offline"}
      />
      {!host.readable && <HostUnavailable host={host} />}
      {progress.map((line) => (
        <div key={line} className="selectable border-b border-line/70 px-4 py-2 text-[12px] text-fg-muted">{line}</div>
      ))}
      {host.readable && host.vdevs.length === 0 && <div className="px-4 py-3 text-[12.5px] text-fg-subtle">No pool is imported.</div>}
      {host.vdevs.length > 0 && (
        <Table head={["Vdev", "Status", "Encryption", "Member disk", "Serial", "Size", "Member state"]}>
          {host.vdevs.map((v) => (
            <Fragment key={`${v.pool}/${v.name}`}>
              {v.members.map((m, i) => (
                <tr key={`${m.path}-${i}`}>
                  {i === 0 && (
                    <>
                      <td rowSpan={v.members.length} className="align-top">
                        <div className="flex items-center gap-1.5">
                          <span className="font-mono text-fg">{v.name}</span>
                          {v.holds_esp && <Badge tone="info">boot</Badge>}
                        </div>
                        <div className="mt-0.5 text-[11px] text-fg-subtle">
                          {v.pool} · {v.type}
                          {v.size ? <> · {bytes(v.allocated)} of {bytes(v.size)}</> : null}
                        </div>
                        {v.size ? <Meter fraction={(v.allocated ?? 0) / v.size} className="mt-1 w-[140px]" /> : null}
                      </td>
                      <td rowSpan={v.members.length} className="align-top">
                        <StatusPill health={vdevHealth[v.status]}>{v.status}</StatusPill>
                      </td>
                      <td rowSpan={v.members.length} className="align-top">
                        <Encryption value={v.encryption} />
                      </td>
                    </>
                  )}
                  <td>
                    {m.missing ? <span className="text-danger">missing</span> : <span className="font-mono text-fg">{m.disk}</span>}
                    <div className="text-[11px] text-fg-subtle">
                      {m.missing ? <span className="font-mono">{m.path}</span> : [m.mapper, m.model].filter(Boolean).join(" · ") || null}
                    </div>
                  </td>
                  <td className="selectable font-mono text-[12px] text-fg-muted">{m.serial ?? "—"}</td>
                  <td className="whitespace-nowrap tabular-nums text-fg-muted"><Bytes value={m.size} /></td>
                  <td>
                    <StatusPill health={memberHealth(m.state)}>{m.state?.toLowerCase() ?? "unknown"}</StatusPill>
                  </td>
                </tr>
              ))}
            </Fragment>
          ))}
        </Table>
      )}
    </Card>
  );
}

function DiskCard({ host }: { host: DiskHost }) {
  const launchWorkflow = useStore((s) => s.launchWorkflow);
  const has = (status: DiskStatus) => host.disks.some((d) => d.status === status);
  const available = host.disks.filter((d) => d.status === "available").length;
  return (
    <Card>
      <CardHeader
        icon={<HardDrive />}
        title={<span className="font-mono">{host.node}</span>}
        subtitle={
          host.readable
            ? `${host.disks.length} disk${host.disks.length === 1 ? "" : "s"} outside pools · ${available} available`
            : host.online ? "unreadable" : "offline"
        }
        actions={
          <>
            {has("awaiting_finalization") && (
              <Button size="sm" onClick={() => launchWorkflow("inventory_disks", { host: host.node })}>Finalize retirement…</Button>
            )}
            {has("pending_replacement") && (
              <Button size="sm" onClick={() => launchWorkflow("add_replacement_disk", { host: host.node })}>Finish replacement…</Button>
            )}
            {has("pending_addition") && (
              <Button size="sm" onClick={() => launchWorkflow("add_new_disk_vdev", { host: host.node })}>Resume new mirror…</Button>
            )}
          </>
        }
      />
      {!host.readable && <HostUnavailable host={host} />}
      {host.readable && host.disks.length === 0 && <div className="px-4 py-3 text-[12.5px] text-fg-subtle">Every disk on this host is in a pool.</div>}
      {host.disks.length > 0 && (
        <Table head={["Disk", "Serial", "Size", "Status", "Details"]}>
          {host.disks.map((d) => (
            <tr key={d.disk}>
              <td>
                <span className="font-mono text-fg">{d.disk}</span>
                <div className="text-[11px] text-fg-subtle">{[d.model, d.tran].filter(Boolean).join(" · ") || "—"}</div>
              </td>
              <td className="selectable font-mono text-[12px] text-fg-muted">{d.serial ?? "—"}</td>
              <td className="whitespace-nowrap tabular-nums text-fg-muted"><Bytes value={d.size} /></td>
              <td>
                <StatusPill health={diskHealth[d.status]}>{d.label}</StatusPill>
              </td>
              <td className="text-[12px] leading-snug text-fg-muted">{d.detail}</td>
            </tr>
          ))}
        </Table>
      )}
    </Card>
  );
}

export function StoragePage() {
  const { data, status, error, nextSteps } = useSource("list_storage");
  const disks = useSource("list_disks");
  const launchWorkflow = useStore((s) => s.launchWorkflow);
  const problems = [...(data?.problems ?? []), ...(disks.data?.problems ?? [])];
  const steps = [...(nextSteps ?? []), ...(disks.nextSteps ?? [])];

  return (
    <>
      <PageHeader title="Storage" subtitle="ZFS pools, vdevs, disks, and Proxmox storage on every host" actions={<SourceRefresh ids={["list_storage", "list_disks"]} />} />
      <PageBody>
        {(data || disks.data) && <AttentionBanner problems={problems} steps={steps} />}
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
        <div className="mb-6 grid gap-4 xl:grid-cols-2">
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
        <Section title="Pool vdevs">
          {disks.data ? disks.data.hosts.map((host) => <VdevCard key={host.node} host={host} />) : <DisksLoading status={disks.status} error={disks.error} />}
        </Section>
        <Section title="Disks outside pools">
          {disks.data ? disks.data.hosts.map((host) => <DiskCard key={host.node} host={host} />) : <DisksLoading status={disks.status} error={disks.error} />}
        </Section>
        <h2 className="mb-2.5 text-[12px] font-semibold uppercase tracking-[0.08em] text-fg-subtle">Disk operations</h2>
        <WorkflowTiles ids={["inventory_disks", "add_new_disk_vdev", "add_replacement_disk", "decommission_disks", "simulate_disk_failure_and_replacement"]} />
      </PageBody>
    </>
  );
}
