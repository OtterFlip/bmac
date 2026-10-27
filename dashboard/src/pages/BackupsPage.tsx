import { Archive, CalendarClock, History, Lock, LockOpen, MessageSquare, Play, RotateCcw, ShieldCheck } from "lucide-react";
import { useStore } from "@/state/store";
import { Card, CardHeader } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Menu } from "@/components/ui/menu";
import { Tooltip } from "@/components/ui/tooltip";
import { EmptyState, KV, StatusPill, useNow } from "@/components/status";
import { AttentionBanner, PageBody, PageHeader } from "@/components/layout/Page";
import { PBS_SETUP_URL, SkeletonRows, SourceRefresh, Table, WorkflowTiles, useSource } from "./common";
import { agoEpoch, bytes, localDateTime, untilEpoch } from "@/lib/format";
import type { BackupMetrics, BackupVm, RestorePoint } from "@/protocol/state";

const KEEP_ORDER = ["last", "hourly", "daily", "weekly", "monthly", "yearly"];

/** The snapshot part of a restore point's volume ID: vm/100/2026-10-27T14:50:01Z. */
export function snapshotOf(point: RestorePoint): string {
  return point.volid.replace(/^[^:]*:backup\//, "");
}

function shortKey(fingerprint: string | null): string {
  return fingerprint ? fingerprint.split(":").slice(0, 8).join(":") : "—";
}

function seconds(value: number | null): string {
  if (value === null) return "?";
  if (value < 60) return `${value}s`;
  const minutes = Math.floor(value / 60);
  return minutes < 60 ? `${minutes}m ${value % 60}s` : `${Math.floor(minutes / 60)}h ${minutes % 60}m`;
}

function rate(amount: number | null, time: number | null): string {
  return amount !== null && time ? `${bytes(amount / time)}/s` : "—";
}

/** How the backup was made: incremental or a full read, with the details from its task log. */
function BackupMode({ metrics }: { metrics: BackupMetrics | null }) {
  if (!metrics) {
    return (
      <Tooltip content="No task log was found for this backup: PVE has pruned it from its task history, or the host that made it is offline.">
        <span className="text-fg-subtle">no log</span>
      </Tooltip>
    );
  }
  const incremental = metrics.mode === "fast-incremental";
  const lines = [
    metrics.detail,
    metrics.dirty_bytes !== null ? `Changed since the previous backup: ${bytes(metrics.dirty_bytes)}` : null,
    `Read: ${bytes(metrics.read_bytes)} in ${seconds(metrics.transfer_seconds)} (${rate(metrics.read_bytes, metrics.transfer_seconds)})`,
    metrics.zero_bytes !== null ? `Zeros (stored once): ${bytes(metrics.zero_bytes)}` : null,
    `Reused from earlier backups: ${bytes(metrics.reused_bytes)}${metrics.reused_percent !== null ? ` (${metrics.reused_percent}%)` : ""}`,
    `New data stored on PBS: about ${bytes(metrics.new_bytes)}`,
    `Total time: ${seconds(metrics.duration_seconds)} on ${metrics.node}`,
  ].filter(Boolean);
  return (
    <Tooltip content={<div className="space-y-0.5">{lines.map((line) => <div key={line}>{line}</div>)}</div>}>
      <div>
        <Badge tone={incremental ? "ok" : "info"}>{incremental ? "incremental" : "full read"}</Badge>
        <div className="mt-0.5 whitespace-nowrap text-[11px] text-fg-subtle">
          {incremental ? `${bytes(metrics.dirty_bytes)} changed` : `${bytes(metrics.read_bytes)} read`} · {seconds(metrics.duration_seconds)}
        </div>
      </div>
    </Tooltip>
  );
}

function schedule(vm: BackupVm) {
  if (!vm.job) return <span className="text-fg-subtle">not backed up</span>;
  if (!vm.job.enabled) return <StatusPill health="offline">disabled</StatusPill>;
  return (
    <span className="whitespace-nowrap text-fg">
      {vm.job.every_hours ? `every ${vm.job.every_hours}h` : <span className="font-mono">{vm.job.schedule}</span>}
      {vm.job.bwlimit ? <span className="text-fg-subtle"> · {Math.round(vm.job.bwlimit / 1024)} MiB/s</span> : null}
    </span>
  );
}

export function BackupsPage() {
  const backups = useSource("list_backups");
  const launchWorkflow = useStore((s) => s.launchWorkflow);
  const now = useNow(15000);
  const b = backups.data;
  const storage = b?.storage;
  const registered = new Map((b?.vms ?? []).map((vm) => [vm.name, vm.vmid]));

  return (
    <>
      <PageHeader
        title="Backups"
        subtitle="Production VM backups on the cluster's Proxmox Backup Server"
        actions={
          <>
            <SourceRefresh ids={["list_backups"]} />
            {b && storage?.configured && (
              <Button variant="primary" onClick={() => launchWorkflow("backup_now")}>
                <Play /> Back up now
              </Button>
            )}
          </>
        }
      />
      <PageBody>
        <AttentionBanner problems={b?.problems ?? []} steps={backups.nextSteps} />

        {b && !storage?.configured && (
          <Card className="mb-4">
            <EmptyState
              icon={<Archive />}
              title="PBS is not set up"
              body="Setup PBS connects the cluster to its namespace on the PBS server as the storage bmac-pbs, with an encryption key for this cluster and your master public key."
              action={<Button variant="primary" onClick={() => launchWorkflow("setup_pbs")}>Setup PBS</Button>}
            />
          </Card>
        )}

        {b && storage?.configured && (
          <div className="mb-4 grid gap-4 xl:grid-cols-2">
            <Card>
              <CardHeader icon={<Archive />} title="Storage" subtitle={storage.id} />
              <div className="p-4">
                <KV
                  items={[
                    ["Status", <StatusPill key="s" health={storage.active ? "ok" : "danger"}>{storage.active ? "active" : "not active"}</StatusPill>],
                    ["Server", <span key="v" className="font-mono">{storage.server}:{storage.port}</span>],
                    ["Datastore / namespace", <span key="n" className="font-mono">{storage.datastore} / {storage.namespace}</span>],
                    ["Token", <span key="t" className="font-mono">{storage.username}</span>],
                    [
                      "Encryption key fingerprint",
                      <Tooltip key="k" content={storage.key_fingerprint ?? "no key"}>
                        <span className="font-mono">{shortKey(storage.key_fingerprint)}…</span>
                      </Tooltip>,
                    ],
                    ["Master public key", storage.master_pubkey ? "added to every backup" : <Badge key="m" tone="warn">missing</Badge>],
                  ]}
                />
              </div>
            </Card>
            <Card>
              <CardHeader icon={<History />} title="Retention" subtitle="Set by the PBS server's prune job, read-only here" />
              <div className="p-4 text-[13px]">
                {!b.retention.readable ? (
                  <p className="text-fg-muted">Not readable with this cluster's token{b.retention.error ? `: ${b.retention.error}` : "."}</p>
                ) : b.retention.jobs.length === 0 ? (
                  <p className="text-warn">The PBS server has no prune job for this namespace, so it keeps every backup.</p>
                ) : (
                  b.retention.jobs.map((job) => (
                    <div key={job.id} className="mb-2 last:mb-0">
                      <div className="flex flex-wrap gap-1.5">
                        {KEEP_ORDER.filter((k) => job.keep[k]).map((k) => (
                          <Badge key={k} tone="info">keep-{k} {job.keep[k]}</Badge>
                        ))}
                      </div>
                      <p className="mt-2 text-[12px] text-fg-subtle">
                        <span className="font-mono">{job.id}</span> runs at {job.schedule ?? "?"} (PBS server time). Protected restore points are always kept.
                      </p>
                    </div>
                  ))
                )}
              </div>
            </Card>
          </div>
        )}

        {b && storage?.configured && (
          <Card className="mb-4">
            <CardHeader icon={<CalendarClock />} title="Production VM backups" subtitle={b ? `${b.vms.length} production VMs` : undefined} />
            <Table head={["VM", "Schedule", "Next run", "Newest backup", "Restore points", ""]}>
              {b.vms.map((vm) => (
                <tr key={vm.name}>
                  <td>
                    <div className="font-mono font-medium text-fg">{vm.name}</div>
                    <div className="whitespace-nowrap text-[11px] text-fg-subtle">VM {vm.vmid ?? "—"}{vm.purpose && ` · ${vm.purpose}`}</div>
                  </td>
                  <td>{schedule(vm)}</td>
                  <td className="whitespace-nowrap text-fg-muted tabular-nums">{vm.job?.enabled ? untilEpoch(vm.job.next_run, now) : "—"}</td>
                  <td className="whitespace-nowrap text-fg-muted tabular-nums">{vm.latest_backup ? agoEpoch(vm.latest_backup, now) : <span className="text-fg-subtle">none</span>}</td>
                  <td className="tabular-nums text-fg-muted">{vm.restore_points}</td>
                  <td>
                    <Menu
                      items={[
                        { label: vm.job ? "Change schedule…" : "Schedule backups…", icon: <CalendarClock />, onSelect: () => launchWorkflow("manage_backup_schedule", { resource: vm.name }) },
                        { label: "Back up now…", icon: <Play />, onSelect: () => launchWorkflow("backup_now", { resource: vm.name }) },
                      ]}
                    />
                  </td>
                </tr>
              ))}
            </Table>
            {b.vms.length === 0 && <EmptyState icon={<CalendarClock />} title="No production VMs yet" />}
          </Card>
        )}

        {b && storage?.configured && (
          <Card className="mb-4">
            <CardHeader
              icon={<ShieldCheck />}
              title="Restore points"
              subtitle={`${b.restore_points.length} made by BMAC${b.unmanaged_backups ? ` · ${b.unmanaged_backups} other backup${b.unmanaged_backups === 1 ? "" : "s"} not listed` : ""}`}
            />
            <Table head={["Time", "VM", "Backup", "Data", "Protected", "Key", "Verified", ""]}>
              {b.restore_points.map((point) => (
                <tr key={point.volid}>
                  <td className="whitespace-nowrap">
                    <div className="flex items-center gap-1.5 tabular-nums text-fg">
                      {localDateTime(point.time)}
                      {point.comment && (
                        <Tooltip content={point.comment}>
                          <span aria-label={`Comment: ${point.comment}`} className="text-accent [&_svg]:size-3.5">
                            <MessageSquare />
                          </span>
                        </Tooltip>
                      )}
                    </div>
                    <div className="text-[11px] text-fg-subtle">{agoEpoch(point.time, now)}</div>
                  </td>
                  <td>
                    <div className="font-mono text-fg">{point.name ?? "?"}</div>
                    <div className="whitespace-nowrap text-[11px] text-fg-subtle">VM {point.vmid ?? "—"}{point.fqdn && ` · ${point.fqdn}`}</div>
                  </td>
                  <td><BackupMode metrics={point.metrics} /></td>
                  <td className="whitespace-nowrap tabular-nums">
                    <Tooltip content="The image's full size; PBS stores only the chunks it did not already have.">
                      <span className="text-fg-muted">{bytes(point.size)}</span>
                    </Tooltip>
                    {point.metrics && (
                      <div className="text-[11px] text-fg-subtle">
                        {point.metrics.reused_percent ?? "?"}% reused · ~{bytes(point.metrics.new_bytes)} new
                      </div>
                    )}
                  </td>
                  <td>{point.protected ? <Badge tone="accent"><Lock /> protected</Badge> : <span className="text-fg-subtle">—</span>}</td>
                  <td>
                    {point.key_matches ? (
                      <StatusPill health="ok">this cluster's</StatusPill>
                    ) : (
                      <Tooltip content={point.encrypted ?? "not encrypted"}>
                        <span><StatusPill health="danger">{point.encrypted ? "other key" : "unencrypted"}</StatusPill></span>
                      </Tooltip>
                    )}
                  </td>
                  <td className="text-fg-muted">{point.verified ?? <span className="text-fg-subtle">not yet</span>}</td>
                  <td>
                    <Menu
                      items={[
                        ...(point.name && point.key_matches && registered.get(point.name) === point.vmid
                          ? [
                              {
                                label: `Roll back ${point.name} to this point…`,
                                icon: <RotateCcw />,
                                danger: true,
                                onSelect: () => launchWorkflow("rollback_restore", { resource: point.name!, snapshot: snapshotOf(point) }),
                              },
                              "separator" as const,
                            ]
                          : []),
                        point.protected
                          ? { label: "Clear protection…", icon: <LockOpen />, onSelect: () => launchWorkflow("protect_restore_point", { snapshot: snapshotOf(point), action: "unprotect" }) }
                          : { label: "Protect…", icon: <Lock />, onSelect: () => launchWorkflow("protect_restore_point", { snapshot: snapshotOf(point), action: "protect" }) },
                      ]}
                    />
                  </td>
                </tr>
              ))}
            </Table>
            {b.restore_points.length === 0 && <EmptyState icon={<ShieldCheck />} title="No restore points yet" body="Schedule backups for a production VM, or back one up now." />}
          </Card>
        )}

        {!b && (
          <Card className="mb-4">
            {backups.status === "loading" ? (
              <Table head={["", "", ""]}>
                <SkeletonRows cols={3} />
              </Table>
            ) : (
              <EmptyState icon={<Archive />} title="Backup state not loaded" body={backups.error ?? "Refresh to read the cluster."} />
            )}
          </Card>
        )}

        <h2 className="mb-2.5 text-[12px] font-semibold uppercase tracking-[0.08em] text-fg-subtle">Backup operations</h2>
        <WorkflowTiles
          links={[{ title: "PBS Server Setup", summary: "How the PBS server, its namespaces, tokens, and prune jobs are set up.", url: PBS_SETUP_URL }]}
          ids={["setup_pbs", "manage_backup_schedule", "backup_now", "protect_restore_point", "rollback_restore", "list_backups"]}
        />
      </PageBody>
    </>
  );
}
