// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// The cluster at a glance: quorum, hosts, VMs, replication, pools, and
// everything that needs attention.

import type { ReactNode } from "react";
import { ArrowRight, Boxes, Database, FlaskConical, GitBranch, History, Scale, Server, Users } from "lucide-react";
import { useShallow } from "zustand/react/shallow";
import { Button, Card, CardHeader, EmptyState, Meter, RunStatusIcon, Skeleton, StatusPill, ago, bytes, cn, elapsedMs, isTerminal, percent, plural, recordStatus, useNow, type Health } from "@bmac/ui";
import { PageBody, PageHeader } from "../components/layout/Page";
import { useStore, type Page } from "../state/store";
import { useStatus } from "../state/useStatus";
import { ConditionsBanner, StatusRefresh, vmHealth } from "./common";

function Stat({ icon, label, value, detail, health, onClick, loading }: { icon: ReactNode; label: string; value: ReactNode; detail?: ReactNode; health: Health; onClick?: () => void; loading?: boolean }) {
  const ring: Record<Health, [string, string]> = {
    ok: ["from-ok/14", "text-ok"],
    warn: ["from-warn/14", "text-warn"],
    danger: ["from-danger/16", "text-danger"],
    info: ["from-info/14", "text-info"],
    run: ["from-run/14", "text-run"],
    offline: ["from-fg-subtle/10", "text-fg-subtle"],
    unknown: ["from-fg-subtle/10", "text-fg-subtle"],
  };
  return (
    <button onClick={onClick} className="group relative overflow-hidden rounded-xl border border-line bg-surface p-4 text-left transition-colors hover:border-line-strong">
      <div className={cn("pointer-events-none absolute inset-0 bg-gradient-to-br to-transparent to-60% opacity-70", ring[health][0])} />
      <div className="relative">
        <div className="flex items-center gap-2 text-[12px] font-medium text-fg-muted">
          <span className={cn("[&_svg]:size-4", ring[health][1])}>{icon}</span>
          {label}
          <ArrowRight className="ml-auto size-3.5 text-fg-subtle opacity-0 transition-opacity group-hover:opacity-100" />
        </div>
        {loading ? <Skeleton className="mt-3 h-7 w-24" /> : <div className="mt-2 text-[24px] font-semibold tracking-[-0.02em] text-fg tabular-nums">{value}</div>}
        <div className="mt-0.5 h-[18px] truncate text-[12px] text-fg-subtle">{loading ? "" : detail}</div>
      </div>
    </button>
  );
}

export function OverviewPage() {
  const setPage = useStore((s) => s.setPage);
  const cluster = useStatus("cluster");
  const hosts = useStatus("hosts");
  const vms = useStatus("vms");
  const storage = useStatus("storage");
  const recent = useStore(useShallow((s) => Object.values(s.jobs).sort((a, b) => b.summary.created_at_ms - a.summary.created_at_ms).slice(0, 7)));
  const focusJob = useStore((s) => s.focusJob);
  const now = useNow(10000);
  const go = (p: Page) => () => setPage(p);

  const c = cluster.data;
  const h = hosts.data;
  const v = vms.data;
  const st = storage.data;
  const summary = c?.cluster ?? h?.cluster;
  const firstLoad = (e: { data: unknown; error: string | null }) => !e.data && !e.error;

  const failing = v?.replication.filter((j) => j.fail_count > 0 || j.error).length ?? 0;
  const pools = st?.hosts.flatMap((x) => (x.observation?.pools ?? []).map((p) => ({ ...p, host: x.host }))) ?? [];
  const tightest = pools.reduce<(typeof pools)[number] | null>((m, p) => (p.free_fraction !== null && (!m || (m.free_fraction ?? 1) > p.free_fraction) ? p : m), null);
  const prodRunning = v?.production.filter((p) => p.live_status === "running").length ?? 0;
  const qdevice = c?.qdevice ?? h?.qdevice;

  const quorumHealth: Health = !summary ? "unknown" : summary.quorate === false ? "danger" : summary.quorate ? "ok" : "unknown";
  const hostsHealth: Health = !summary ? "unknown" : summary.online_count < summary.host_count ? "warn" : "ok";

  return (
    <>
      <PageHeader
        title="Overview"
        subtitle={
          summary?.name ? (
            <>
              Cluster <span className="font-medium text-fg">{summary.name}</span>
              {c?.served_by && <> · as {c.served_by} sees it</>}
            </>
          ) : (
            "Cluster health at a glance"
          )
        }
        actions={<StatusRefresh kinds={["cluster", "hosts", "vms", "storage"]} label="Refresh all" />}
      />
      <PageBody>
        <div className="grid grid-cols-2 gap-3 xl:grid-cols-4">
          <Stat
            icon={<Users />}
            label="Quorum"
            health={quorumHealth}
            loading={firstLoad(cluster)}
            value={!summary ? "—" : summary.quorate ? "Quorate" : summary.quorate === false ? "Lost" : "Unknown"}
            detail={summary && summary.total_votes !== null ? `${summary.total_votes} of ${summary.expected_votes} votes · needs ${summary.quorum}` : (cluster.error ?? undefined)}
            onClick={go("hosts")}
          />
          <Stat
            icon={<Server />}
            label="Hosts online"
            health={hostsHealth}
            loading={firstLoad(cluster)}
            value={summary ? `${summary.online_count}/${summary.host_count}` : "—"}
            detail={summary?.duty_host ? `duty host ${summary.duty_host}` : undefined}
            onClick={go("hosts")}
          />
          <Stat
            icon={<Boxes />}
            label="Production VMs"
            health={!v ? "unknown" : prodRunning < v.production.length ? "warn" : "ok"}
            loading={firstLoad(vms)}
            value={v ? `${prodRunning}/${v.production.length}` : "—"}
            detail={v ? `running · ${plural(v.staging.length, "staging VM")}` : (vms.error ?? undefined)}
            onClick={go("production")}
          />
          <Stat
            icon={<GitBranch />}
            label="Replication"
            health={!v ? "unknown" : failing ? "danger" : "ok"}
            loading={firstLoad(vms)}
            value={v ? (failing ? `${failing} failing` : "Healthy") : "—"}
            detail={v ? `${plural(v.replication.length, "job")}${tightest ? ` · tightest pool ${percent(tightest.free_fraction)} free` : ""}` : (vms.error ?? undefined)}
            onClick={go("production")}
          />
        </div>

        <div className="mt-5">
          <ConditionsBanner conditions={c?.conditions ?? []} />
        </div>

        <div className="grid gap-4 xl:grid-cols-2">
          <Card>
            <CardHeader
              icon={<Server />}
              title="Hosts"
              subtitle={h ? `${h.cluster.online_count} of ${plural(h.cluster.host_count, "host")} online` : "Proxmox hosts"}
              actions={
                <Button size="sm" variant="ghost" onClick={go("hosts")}>
                  All hosts <ArrowRight />
                </Button>
              }
            />
            <div className="divide-y divide-line/70">
              {!h && firstLoad(hosts) && [0, 1, 2].map((i) => <div key={i} className="px-4 py-3"><Skeleton className="w-2/3" /></div>)}
              {!h && hosts.error && <EmptyState icon={<Server />} title="Hosts aren't available" body={hosts.error} />}
              {h?.hosts.map((host) => {
                const o = host.observation;
                return (
                  <div key={host.name} className="grid grid-cols-[110px_minmax(0,1fr)_minmax(0,1fr)] items-center gap-4 px-4 py-2.5">
                    <div className="flex items-center gap-2">
                      <span className={cn("size-2 rounded-full", host.online ? "bg-ok shadow-[0_0_8px] shadow-ok/60" : "bg-danger")} aria-label={host.online ? "online" : "offline"} />
                      <span className="font-mono text-[12.5px] font-medium text-fg">{host.name}</span>
                    </div>
                    <div>
                      <div className="mb-1 flex justify-between gap-2 whitespace-nowrap text-[11px] text-fg-subtle"><span>CPU</span><span className="truncate tabular-nums">{percent(o?.cpu_fraction)}</span></div>
                      <Meter fraction={o?.cpu_fraction} />
                    </div>
                    <div>
                      <div className="mb-1 flex justify-between gap-2 whitespace-nowrap text-[11px] text-fg-subtle"><span>Memory</span><span className="truncate tabular-nums">{bytes(o?.memory_used, 0)} / {bytes(o?.memory_total, 0)}</span></div>
                      <Meter fraction={o?.memory_total ? (o.memory_used ?? 0) / o.memory_total : null} />
                    </div>
                  </div>
                );
              })}
            </div>
          </Card>

          <Card>
            <CardHeader
              icon={<Boxes />}
              title="Production VMs"
              subtitle={v ? `${prodRunning} running` : "HA guests"}
              actions={
                <Button size="sm" variant="ghost" onClick={go("production")}>
                  Manage <ArrowRight />
                </Button>
              }
            />
            <div className="divide-y divide-line/70">
              {!v && firstLoad(vms) && [0, 1, 2].map((i) => <div key={i} className="px-4 py-3"><Skeleton className="w-1/2" /></div>)}
              {!v && vms.error && <EmptyState icon={<Boxes />} title="VMs aren't available" body={vms.error} />}
              {v && v.production.length === 0 && <EmptyState icon={<Boxes />} title="No production VMs" body="Add one from the Production page." />}
              {v?.production.map((vm) => (
                <div key={vm.name} className="flex items-center gap-3 px-4 py-2.5">
                  <StatusPill health={vmHealth(vm.live_status)}>{vm.live_status}</StatusPill>
                  <span className="w-[64px] font-mono text-[12.5px] font-medium text-fg">{vm.name}</span>
                  <span className="min-w-0 flex-1 truncate text-[12.5px] text-fg-muted">{vm.domain ?? (vm.purpose || "—")}</span>
                  <span className="text-[12px] text-fg-subtle">{vm.node ? `on ${vm.node}` : ""}</span>
                  {vm.replication.failing > 0 && <StatusPill health="danger">{vm.replication.failing} repl failing</StatusPill>}
                </div>
              ))}
            </div>
          </Card>

          <Card>
            <CardHeader
              icon={<FlaskConical />}
              title="Staging"
              subtitle={v ? plural(v.staging.length, "staging VM") : "Disposable copies"}
              actions={
                <Button size="sm" variant="ghost" onClick={go("staging")}>
                  Manage <ArrowRight />
                </Button>
              }
            />
            <div className="divide-y divide-line/70">
              {!v && firstLoad(vms) && <div className="px-4 py-3"><Skeleton className="w-1/2" /></div>}
              {v && v.staging.length === 0 && <EmptyState icon={<FlaskConical />} title="No staging VMs" body="Create a sanitized copy of a production VM to test changes." />}
              {v?.staging.map((vm) => (
                <div key={vm.name} className="flex items-center gap-3 px-4 py-2.5">
                  <StatusPill health={vmHealth(vm.live_status)}>{vm.live_status}</StatusPill>
                  <span className="font-mono text-[12.5px] font-medium text-fg">{vm.name}</span>
                  <span className="min-w-0 flex-1 truncate text-[12px] text-fg-subtle" title={vm.snapshot?.name}>
                    {[
                      vm.source && `copy of ${vm.source}`,
                      vm.snapshot?.created_at_ms && `snapshot ${ago(vm.snapshot.created_at_ms, now)}`,
                      vm.snapshot?.shared_with.length && `shared with ${vm.snapshot.shared_with.join(", ")}`,
                    ]
                      .filter(Boolean)
                      .join(" · ")}
                  </span>
                </div>
              ))}
            </div>
          </Card>

          <Card>
            <CardHeader
              icon={<History />}
              title="Recent jobs"
              subtitle="Everything run on this cluster"
              actions={
                <Button size="sm" variant="ghost" onClick={go("activity")}>
                  Activity <ArrowRight />
                </Button>
              }
            />
            <div className="divide-y divide-line/70">
              {recent.length === 0 && <EmptyState icon={<History />} title="Nothing has run yet" />}
              {recent.map((r) => {
                const status = recordStatus(r);
                const end = r.ended_at_ms ?? (isTerminal(status) ? r.summary.updated_at_ms : null);
                return (
                  <button key={r.summary.id} onClick={() => focusJob(r.summary.id)} className="flex w-full items-center gap-3 px-4 py-2.5 text-left hover:bg-surface-2/60">
                    <RunStatusIcon status={status} />
                    <span className="min-w-0 flex-1 truncate text-[12.5px] text-fg">
                      {r.summary.title}
                      <span className="text-fg-muted"> · {r.summary.host}</span>
                      {r.summary.dry_run && <span className="ml-1.5 text-[11px] text-info">dry run</span>}
                    </span>
                    <span className="text-[11.5px] text-fg-subtle tabular-nums">{end ? elapsedMs(r.started_at_ms ?? r.summary.created_at_ms, end) : "running"}</span>
                    <span className="w-[70px] text-right text-[11.5px] text-fg-subtle">{ago(r.summary.created_at_ms, now)}</span>
                  </button>
                );
              })}
            </div>
          </Card>
        </div>

        {pools.length > 0 && (
          <Card className="mt-4">
            <CardHeader
              icon={<Database />}
              title="Storage pools"
              actions={
                <Button size="sm" variant="ghost" onClick={go("storage")}>
                  Storage <ArrowRight />
                </Button>
              }
            />
            <div className="grid grid-cols-[repeat(auto-fill,minmax(220px,1fr))] gap-x-6 gap-y-3 p-4">
              {pools.map((p) => (
                <div key={`${p.host}/${p.name}`}>
                  <div className="mb-1 flex items-baseline justify-between text-[12px]">
                    <span className="font-mono text-fg">
                      {p.host}/{p.name}
                    </span>
                    <span className="text-fg-subtle tabular-nums">{bytes(p.free)} free</span>
                  </div>
                  <Meter fraction={p.size ? (p.allocated ?? 0) / p.size : null} />
                </div>
              ))}
            </div>
          </Card>
        )}
        {qdevice && summary && (
          <button onClick={go("qdevice")} className="mt-4 flex w-full items-center gap-3 rounded-xl border border-line bg-surface px-4 py-3 text-left hover:border-line-strong">
            <Scale className="size-4 text-fg-subtle" />
            <span className="text-[13px] font-medium text-fg">QDevice</span>
            <span className="text-[12.5px] text-fg-muted">
              {qdevice.registered ? `registered${qdevice.address ? ` at ${qdevice.address}` : ""}` : "not registered"}
              {qdevice.needed !== null && ` · ${qdevice.needed ? "needed (even number of hosts)" : "not needed (odd number of hosts)"}`}
            </span>
            <ArrowRight className="ml-auto size-3.5 text-fg-subtle" />
          </button>
        )}
      </PageBody>
    </>
  );
}
