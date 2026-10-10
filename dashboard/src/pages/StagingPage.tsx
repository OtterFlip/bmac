import { Camera, ExternalLink, FlaskConical, KeyRound, Link2, Plus, Trash2 } from "lucide-react";
import type { StagingSnapshot } from "@/protocol/state";
import { useStore } from "@/state/store";
import { Card, CardHeader } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Menu } from "@/components/ui/menu";
import { EmptyState, StatusPill } from "@/components/status";
import { AttentionBanner, PageBody, PageHeader } from "@/components/layout/Page";
import { ExternalButton, SkeletonRows, SourceRefresh, Table, WorkflowTiles, openExternal, useSource } from "./common";
import { agoEpoch, dateTime, duration } from "@/lib/format";
import { vmHealth } from "./ProductionPage";

export function snapshotTaken(snapshot: StagingSnapshot | null | undefined): string {
  if (!snapshot?.created_at) return "unknown time";
  return `${dateTime(new Date(snapshot.created_at * 1000).toISOString())} · ${agoEpoch(snapshot.created_at)}`;
}

function SnapshotCell({ snapshot }: { snapshot: StagingSnapshot | null }) {
  if (!snapshot) return <span className="text-fg-subtle">—</span>;
  const shared = snapshot.shared_with.length > 0;
  return (
    <div className="min-w-0">
      <div className="flex items-center gap-1 text-[12px] text-fg-muted" title={snapshot.name}>
        <Camera className="size-3.5 shrink-0 text-fg-subtle" />
        <span className="max-w-[220px] truncate font-mono">{snapshot.name}</span>
      </div>
      <div className="text-[11px] text-fg-subtle tabular-nums">taken {snapshotTaken(snapshot)}</div>
      {shared && (
        <Badge tone="info" className="mt-0.5" title="The snapshot is kept until the last staging VM using it is removed.">
          <Link2 className="size-3" /> shared with {snapshot.shared_with.join(", ")}
        </Badge>
      )}
    </div>
  );
}

export function StagingPage() {
  const guests = useSource("list_guests");
  const launchWorkflow = useStore((s) => s.launchWorkflow);
  const g = guests.data;

  return (
    <>
      <PageHeader
        title="Staging"
        subtitle="Sanitized, disposable copies of production VMs"
        actions={
          <>
            <SourceRefresh ids={["list_guests"]} />
            <Button variant="primary" onClick={() => launchWorkflow("add_staging_vm")}>
              <Plus /> New staging VM
            </Button>
          </>
        }
      />
      <PageBody>
        <AttentionBanner problems={g?.problems.filter((p) => p.startsWith("staging")) ?? []} />
        <Card className="mb-4">
          <CardHeader icon={<FlaskConical />} title="Staging VMs" subtitle={g ? `${g.staging.length} registered` : undefined} />
          <Table head={["VM", "Status", "Copy of", "Snapshot", "Runs on", "Address", "Size", ""]}>
            {!g && guests.status === "loading" && <SkeletonRows cols={8} />}
            {g?.staging.map((vm) => (
              <tr key={vm.name}>
                <td>
                  <div className="font-mono font-medium text-fg">{vm.name}</div>
                  <div className="text-[11px] text-fg-subtle">VM {vm.vmid ?? "—"}</div>
                </td>
                <td>
                  <StatusPill health={vmHealth(vm.live_status)}>{vm.live_status}</StatusPill>
                  {vm.registry_state && vm.registry_state !== "active" && <Badge tone={vm.registry_state === "failed" ? "danger" : "warn"} className="ml-1">{vm.registry_state.replace(/_/g, " ")}</Badge>}
                </td>
                <td className="font-mono text-fg-muted">{vm.source ?? "—"}</td>
                <td><SnapshotCell snapshot={vm.snapshot ?? null} /></td>
                <td className="font-mono text-fg-muted">{vm.node ?? "—"}</td>
                <td>
                  {vm.url ? (
                    <div className="flex items-center gap-1">
                      <span className="max-w-[240px] truncate text-fg">{vm.url.replace(/^https?:\/\//, "").replace(/\/$/, "")}</span>
                      <ExternalButton url={vm.url} />
                    </div>
                  ) : (
                    <span className="font-mono text-fg-subtle">{vm.ip ?? "—"}</span>
                  )}
                </td>
                <td className="whitespace-nowrap text-[12px] text-fg-muted tabular-nums">
                  {vm.cores ?? "?"} vCPU · {vm.memory_mb ? `${Math.round(vm.memory_mb / 1024)} GiB` : "?"}
                  <div className="text-[11px] text-fg-subtle">{vm.uptime_seconds ? `up ${duration(vm.uptime_seconds)}` : ""}</div>
                </td>
                <td>
                  <Menu
                    items={[
                      ...(vm.url ? [{ label: "Open site", icon: <ExternalLink />, onSelect: () => openExternal(vm.url!) }] : []),
                      { label: "Set up jump SSH…", icon: <KeyRound />, onSelect: () => launchWorkflow("setup_jump_ssh_access", { resource: vm.name }) },
                      "separator" as const,
                      { label: "Remove staging VM…", icon: <Trash2 />, danger: true, onSelect: () => launchWorkflow("remove_staging_vm", { resource: vm.name }) },
                    ]}
                  />
                </td>
              </tr>
            ))}
          </Table>
          {g && g.staging.length === 0 && (
            <EmptyState
              icon={<FlaskConical />}
              title="No staging VMs"
              body="A staging VM is a sanitized clone of a production VM. It's the safe place to try upgrades and data changes."
              action={<Button onClick={() => launchWorkflow("add_staging_vm")}><Plus /> New staging VM</Button>}
            />
          )}
          {!g && guests.status !== "loading" && <EmptyState icon={<FlaskConical />} title="Guest state not loaded" body={guests.error ?? "Refresh to read the cluster."} />}
        </Card>
        <h2 className="mb-2.5 text-[12px] font-semibold uppercase tracking-[0.08em] text-fg-subtle">Staging operations</h2>
        <WorkflowTiles ids={["add_staging_vm", "setup_jump_ssh_access", "remove_staging_vm"]} />
      </PageBody>
    </>
  );
}
