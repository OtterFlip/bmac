// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

import { Fragment } from "react";
import { Database, HardDrive, Layers, Lock } from "lucide-react";
import { Badge, Card, CardHeader, EmptyState, Meter, StatusPill, bytes, percent, plural, type DiskStatus, type Encryption, type Health, type HostStorage, type VdevStatus } from "@bmac/ui";
import { PageBody, PageHeader } from "../components/layout/Page";
import { useStatus } from "../state/useStatus";
import { ConditionsBanner, Freshness, OperationTiles, SectionTitle, StatusMissing, StatusRefresh, Table } from "./common";

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

function poolHealth(health: string | null): Health {
  return health === "ONLINE" ? "ok" : health === "DEGRADED" ? "warn" : health ? "danger" : "unknown";
}

function Bytes({ value }: { value: number | null }) {
  return <span title={value !== null ? `${value.toLocaleString()} bytes` : undefined}>{bytes(value)}</span>;
}

function EncryptionBadge({ value }: { value: Encryption }) {
  if (value === "luks")
    return (
      <Badge tone="accent">
        <Lock />
        LUKS
      </Badge>
    );
  if (value === "mixed") return <Badge tone="warn">mixed</Badge>;
  if (value === "none") return <Badge>none</Badge>;
  return <span className="text-fg-subtle">—</span>;
}

function Unread({ host }: { host: HostStorage }) {
  if (!host.online) return <EmptyState title="Host is offline" body="Its storage can't be read until it's back." />;
  if (!host.observation) return <EmptyState title="Not observed yet" body="This host's storage collector hasn't reported. It runs every minute." />;
  return null;
}

function HostCard({ host }: { host: HostStorage }) {
  const o = host.observation;
  return (
    <Card>
      <CardHeader
        icon={<HardDrive />}
        title={<span className="font-mono">{host.host}</span>}
        subtitle={<Freshness observedAt={host.freshness.observed_at_ms} reachable={host.freshness.reachable} online={host.online} />}
      />
      <Unread host={host} />
      {o?.errors.map((e) => (
        <div key={e} className="selectable border-b border-line/70 px-4 py-2 text-[12px] text-danger">
          {e}
        </div>
      ))}
      {o?.pools.map((p) => (
        <div key={p.name} className="border-b border-line/70 px-4 py-3">
          <div className="mb-1.5 flex items-center gap-2">
            <span className="font-mono text-[13px] font-medium text-fg">{p.name}</span>
            <StatusPill health={poolHealth(p.health)}>{p.health?.toLowerCase() ?? "unknown"}</StatusPill>
            <span className="ml-auto text-[12px] text-fg-muted tabular-nums">
              {bytes(p.allocated)} of {bytes(p.size)} · <span className="text-fg">{percent(p.free_fraction)} free</span>
            </span>
          </div>
          <Meter fraction={p.size ? (p.allocated ?? 0) / p.size : null} className="h-2" />
          {p.fragmentation_percent !== null && <div className="mt-1 text-[11px] text-fg-subtle">fragmentation {p.fragmentation_percent}%</div>}
          {p.scan && <div className="selectable mt-1 text-[11px] text-fg-subtle">{p.scan.split("\n").join(" ")}</div>}
          {p.remove && <div className="selectable mt-1 text-[11px] text-run">{p.remove.split("\n").join(" ")}</div>}
          {p.errors && p.errors !== "No known data errors" && <div className="selectable mt-1 text-[11px] text-danger">{p.errors}</div>}
        </div>
      ))}
      {o && o.storages.length > 0 && (
        <Table label={`${host.host} Proxmox storages`} head={["Storage", "Type", "Used", "Content"]}>
          {o.storages.map((s) => (
            <tr key={s.storage}>
              <td>
                <span className="font-mono text-fg">{s.storage}</span>
                {!s.active && (
                  <Badge tone={s.enabled ? "danger" : "neutral"} className="ml-1.5">
                    {s.enabled ? "inactive" : "disabled"}
                  </Badge>
                )}
              </td>
              <td className="text-fg-muted">{s.type ?? "—"}</td>
              <td className="w-[180px]">
                {s.total ? (
                  <>
                    <div className="mb-1 text-[11px] text-fg-muted tabular-nums">
                      {bytes(s.used)} / {bytes(s.total)}
                    </div>
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
  );
}

function VdevCard({ host }: { host: HostStorage }) {
  const o = host.observation;
  return (
    <Card>
      <CardHeader
        icon={<Layers />}
        title={<span className="font-mono">{host.host}</span>}
        subtitle={o ? `${plural(o.vdevs.length, "vdev")} in ${plural(o.pools.length, "pool")}` : host.online ? "not observed yet" : "offline"}
      />
      <Unread host={host} />
      {o && o.vdevs.length === 0 && <div className="px-4 py-3 text-[12.5px] text-fg-subtle">No pool is imported.</div>}
      {o && o.vdevs.length > 0 && (
        <Table label={`${host.host} vdevs`} head={["Vdev", "Status", "Encryption", "Member disk", "Serial", "Size", "Member state"]}>
          {o.vdevs.map((v) => (
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
                          {v.size ? (
                            <>
                              {" "}
                              · {bytes(v.allocated)} of {bytes(v.size)}
                            </>
                          ) : null}
                        </div>
                        {v.size ? <Meter fraction={(v.allocated ?? 0) / v.size} className="mt-1 w-[140px]" /> : null}
                      </td>
                      <td rowSpan={v.members.length} className="align-top">
                        <StatusPill health={vdevHealth[v.status]}>{v.status}</StatusPill>
                      </td>
                      <td rowSpan={v.members.length} className="align-top">
                        <EncryptionBadge value={v.encryption} />
                      </td>
                    </>
                  )}
                  <td>
                    {m.missing ? <span className="text-danger">missing</span> : <span className="font-mono text-fg">{m.disk}</span>}
                    <div className="text-[11px] text-fg-subtle">{m.missing ? <span className="font-mono">{m.path}</span> : [m.mapper, m.model].filter(Boolean).join(" · ") || null}</div>
                  </td>
                  <td className="selectable font-mono text-[12px] text-fg-muted">{m.serial ?? "—"}</td>
                  <td className="whitespace-nowrap tabular-nums text-fg-muted">
                    <Bytes value={m.size} />
                  </td>
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

function DiskCard({ host }: { host: HostStorage }) {
  const o = host.observation;
  const available = o?.disks.filter((d) => d.status === "available").length ?? 0;
  return (
    <Card>
      <CardHeader
        icon={<HardDrive />}
        title={<span className="font-mono">{host.host}</span>}
        subtitle={o ? `${plural(o.disks.length, "disk")} outside pools · ${available} available` : host.online ? "not observed yet" : "offline"}
      />
      <Unread host={host} />
      {o && o.disks.length === 0 && <div className="px-4 py-3 text-[12.5px] text-fg-subtle">Every disk on this host is in a pool.</div>}
      {o && o.disks.length > 0 && (
        <Table label={`${host.host} disks outside pools`} head={["Disk", "Serial", "Size", "Status", "Details"]}>
          {o.disks.map((d) => (
            <tr key={d.disk}>
              <td>
                <span className="font-mono text-fg">{d.disk}</span>
                <div className="text-[11px] text-fg-subtle">{[d.model, d.tran].filter(Boolean).join(" · ") || "—"}</div>
              </td>
              <td className="selectable font-mono text-[12px] text-fg-muted">{d.serial ?? "—"}</td>
              <td className="whitespace-nowrap tabular-nums text-fg-muted">
                <Bytes value={d.size} />
              </td>
              <td>
                <StatusPill health={diskHealth[d.status]}>{d.label}</StatusPill>
              </td>
              <td className="text-[12px] leading-snug text-fg-muted">
                {d.detail}
                {d.removable && <div className="text-[11px] text-fg-subtle">Nothing holds it; it can be pulled.</div>}
              </td>
            </tr>
          ))}
        </Table>
      )}
    </Card>
  );
}

export function StoragePage() {
  const storage = useStatus("storage");
  const data = storage.data;

  return (
    <>
      <PageHeader title="Storage" subtitle="ZFS pools, vdevs, disks, and Proxmox storage on every host" actions={<StatusRefresh kinds={["storage"]} />} />
      <PageBody>
        <ConditionsBanner conditions={data?.conditions ?? []} />
        {!data && (
          <Card className="mb-4">
            <StatusMissing entry={storage} what="Storage status" icon={<Database />} />
          </Card>
        )}
        {data && (
          <>
            <div className="mb-6 grid gap-4 xl:grid-cols-2">
              {data.hosts.map((host) => (
                <HostCard key={host.host} host={host} />
              ))}
            </div>
            <SectionTitle>Pool vdevs</SectionTitle>
            <div className="mb-6 space-y-4">
              {data.hosts.map((host) => (
                <VdevCard key={host.host} host={host} />
              ))}
            </div>
            <SectionTitle>Disks outside pools</SectionTitle>
            <div className="mb-6 space-y-4">
              {data.hosts.map((host) => (
                <DiskCard key={host.host} host={host} />
              ))}
            </div>
          </>
        )}
        <SectionTitle>Storage operations</SectionTitle>
        <p className="mb-2.5 text-[12.5px] text-fg-subtle">
          Each runs on the host you choose. Hosts whose disks can't be pulled while running practice replacements with Simulate a disk failure; Storage inventory finishes removals and replacements and says which disks are safe to pull.
        </p>
        <OperationTiles
          ids={[
            "storage.add_vdev",
            "storage.attach_disk",
            "storage.replace_disk",
            "storage.remove_vdev",
            "storage.inventory",
            "storage.simulate_failure",
            "diagnostics.host_state",
          ]}
        />
      </PageBody>
    </>
  );
}
