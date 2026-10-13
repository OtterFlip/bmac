// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

import { Camera, ExternalLink, FlaskConical, Link2, Plus, Trash2 } from "lucide-react";
import { Badge, Card, CardHeader, EmptyState, Menu, StatusPill, ago, dateTimeMs, duration, useNow, type StagingSnapshot, type StagingVm } from "@bmac/ui";
import { PageBody, PageHeader } from "../components/layout/Page";
import { useStatus } from "../state/useStatus";
import { ConditionsBanner, OperationButton, OperationTiles, SectionTitle, SkeletonRows, StatusMissing, StatusRefresh, Table, useOperationItems, vmHealth } from "./common";

export function snapshotTaken(snapshot: StagingSnapshot | null, now = Date.now()): string {
  if (!snapshot?.created_at_ms) return "unknown time";
  return `${dateTimeMs(snapshot.created_at_ms)} · ${ago(snapshot.created_at_ms, now)}`;
}

function SnapshotCell({ snapshot }: { snapshot: StagingSnapshot | null }) {
  const now = useNow(30000);
  if (!snapshot) return <span className="text-fg-subtle">—</span>;
  return (
    <div className="min-w-0">
      <div className="flex items-center gap-1 text-[12px] text-fg-muted" title={snapshot.name}>
        <Camera className="size-3.5 shrink-0 text-fg-subtle" />
        <span className="max-w-[220px] truncate font-mono">{snapshot.name}</span>
      </div>
      <div className="text-[11px] text-fg-subtle tabular-nums">taken {snapshotTaken(snapshot, now)}</div>
      {!snapshot.verified && (
        <Badge tone="warn" className="mt-0.5">
          not verified
        </Badge>
      )}
      {snapshot.shared_with.length > 0 && (
        <Badge tone="info" className="mt-0.5" title="The snapshot is kept until the last staging VM using it is removed.">
          <Link2 className="size-3" /> shared with {snapshot.shared_with.join(", ")}
        </Badge>
      )}
    </div>
  );
}

function VmMenu({ vm }: { vm: StagingVm }) {
  const remove = useOperationItems([{ id: "staging.remove", values: { resource: vm.name }, label: "Remove staging VM…", icon: <Trash2 /> }]);
  return remove.length ? <Menu items={remove} label={`${vm.name} actions`} /> : null;
}

export function StagingPage() {
  const vms = useStatus("vms");
  const v = vms.data;
  const conditions = v?.conditions.filter((c) => c.id === "staging.cleanup" || v.staging.some((s) => s.name === c.resource)) ?? [];

  return (
    <>
      <PageHeader
        title="Staging"
        subtitle="Sanitized, disposable copies of production VMs"
        actions={
          <>
            <StatusRefresh kinds={["vms"]} />
            <OperationButton id="staging.add" variant="primary" size="md" icon={<Plus />} label="New staging VM" />
          </>
        }
      />
      <PageBody>
        <ConditionsBanner conditions={conditions} />
        <Card className="mb-4">
          <CardHeader icon={<FlaskConical />} title="Staging VMs" subtitle={v ? `${v.staging.length} registered` : undefined} />
          {v || vms.loading ? (
            <Table label="Staging VMs" head={["VM", "Status", "Copy of", "Snapshot", "Runs on", "Address", "Size", ""]}>
              {!v && <SkeletonRows cols={8} />}
              {v?.staging.map((vm) => (
                <tr key={vm.name}>
                  <td>
                    <div className="font-mono font-medium text-fg">{vm.name}</div>
                    <div className="text-[11px] text-fg-subtle">VM {vm.vmid}</div>
                  </td>
                  <td>
                    <StatusPill health={vmHealth(vm.live_status)}>{vm.live_status}</StatusPill>
                    {vm.registry_state !== "active" && (
                      <Badge tone={vm.registry_state === "failed" ? "danger" : "warn"} className="ml-1">
                        {vm.registry_state.replace(/_/g, " ")}
                      </Badge>
                    )}
                  </td>
                  <td className="font-mono text-fg-muted">{vm.source ?? "—"}</td>
                  <td>
                    <SnapshotCell snapshot={vm.snapshot} />
                  </td>
                  <td className="font-mono text-fg-muted">{vm.node ?? "—"}</td>
                  <td>
                    {vm.url ? (
                      <a href={vm.url} target="_blank" rel="noopener noreferrer" className="flex max-w-[240px] items-center gap-1 text-fg hover:text-accent">
                        <span className="truncate">{vm.url.replace(/^https?:\/\//, "").replace(/\/$/, "")}</span>
                        <ExternalLink className="size-3 shrink-0" />
                      </a>
                    ) : (
                      <span className="font-mono text-fg-subtle">{vm.ip || "—"}</span>
                    )}
                  </td>
                  <td className="whitespace-nowrap text-[12px] text-fg-muted tabular-nums">
                    {vm.cores} vCPU · {Math.round(vm.memory_mb / 1024)} GiB
                    <div className="text-[11px] text-fg-subtle">{vm.uptime_seconds ? `up ${duration(vm.uptime_seconds)}` : ""}</div>
                  </td>
                  <td>
                    <VmMenu vm={vm} />
                  </td>
                </tr>
              ))}
            </Table>
          ) : (
            <StatusMissing entry={vms} what="VM status" icon={<FlaskConical />} />
          )}
          {v && v.staging.length === 0 && (
            <EmptyState
              icon={<FlaskConical />}
              title="No staging VMs"
              body="A staging VM is a sanitized clone of a production VM. It's the safe place to try upgrades and data changes."
              action={<OperationButton id="staging.add" variant="secondary" icon={<Plus />} label="New staging VM" />}
            />
          )}
        </Card>
        <SectionTitle>Staging operations</SectionTitle>
        <OperationTiles ids={["staging.add", "staging.remove"]} />
      </PageBody>
    </>
  );
}
