import type { ReactNode } from "react";
import { ArrowRight, Boxes, Database, FlaskConical, GitBranch, History, Scale, Server, Users } from "lucide-react";
import { useShallow } from "zustand/react/shallow";
import { useStore, type Page } from "@/state/store";
import { Card, CardHeader } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { EmptyState, Meter, RunStatusIcon, Skeleton, StatusPill, useNow, type Health } from "@/components/status";
import { AttentionBanner, PageBody, PageHeader } from "@/components/layout/Page";
import { ExternalButton, SourceRefresh, proxmoxUrl, useSource } from "./common";
import { ago, agoEpoch, bytes, elapsed, percent, plural } from "@/lib/format";
import { cn } from "@/lib/utils";
import type { NextStep } from "@/protocol/types";

function Stat({ icon, label, value, detail, health, onClick, loading }: { icon: ReactNode; label: string; value: ReactNode; detail?: ReactNode; health: Health; onClick?: () => void; loading?: boolean }) {
  const ring: Record<Health, string> = {
    ok: "from-ok/14 text-ok",
    warn: "from-warn/14 text-warn",
    danger: "from-danger/16 text-danger",
    info: "from-info/14 text-info",
    run: "from-run/14 text-run",
    offline: "from-fg-subtle/10 text-fg-subtle",
    unknown: "from-fg-subtle/10 text-fg-subtle",
  };
  return (
    <button onClick={onClick} className="group relative overflow-hidden rounded-xl border border-line bg-surface p-4 text-left transition-colors hover:border-line-strong">
      <div className={cn("pointer-events-none absolute inset-0 bg-gradient-to-br to-transparent to-60% opacity-70", ring[health].split(" ")[0])} />
      <div className="relative">
        <div className="flex items-center gap-2 text-[12px] font-medium text-fg-muted">
          <span className={cn("[&_svg]:size-4", ring[health].split(" ")[1])}>{icon}</span>
          {label}
          <ArrowRight className="ml-auto size-3.5 text-fg-subtle opacity-0 transition-opacity group-hover:opacity-100" />
        </div>
        {loading ? <Skeleton className="mt-3 h-7 w-24" /> : <div className="mt-2 text-[24px] font-semibold tracking-[-0.02em] text-fg tabular-nums">{value}</div>}
        <div className="mt-0.5 h-[18px] truncate text-[12px] text-fg-subtle">{loading ? "" : detail}</div>
      </div>
    </button>
  );
}

const vmHealth = (status: string): Health => (status === "running" ? "ok" : status === "stopped" ? "offline" : status === "missing" ? "danger" : "unknown");

export function DashboardPage() {
  const setPage = useStore((s) => s.setPage);
  const hosts = useSource("list_hosts");
  const guests = useSource("list_guests");
  const repl = useSource("list_replication");
  const storage = useSource("list_storage");
  const recent = useStore(useShallow((s) => Object.values(s.records).sort((a, b) => b.started_at.localeCompare(a.started_at)).slice(0, 7)));
  const focusRun = useStore((s) => s.focusRun);
  const now = useNow(10000);
  const go = (p: Page) => () => setPage(p);

  const h = hosts.data;
  const g = guests.data;
  const r = repl.data;
  const st = storage.data;
  const firstLoad = (e: { data: unknown; status: string }) => !e.data && e.status === "loading";

  const failingJobs = r?.jobs.filter((j) => j.fail_count > 0 || j.error).length ?? 0;
  const pools = st?.hosts.flatMap((x) => x.pools.map((p) => ({ ...p, node: x.node }))) ?? [];
  const tightest = pools.reduce<(typeof pools)[number] | null>((m, p) => (p.free_fraction !== null && (!m || (m.free_fraction ?? 1) > p.free_fraction) ? p : m), null);
  const prodRunning = g?.production.filter((p) => p.live_status === "running").length ?? 0;
  const problems = [...(h?.problems ?? []), ...(g?.problems ?? []), ...(r?.problems ?? []), ...(st?.problems ?? [])];
  const steps: NextStep[] = [];
  const seen = new Set<string>();
  for (const s of [...hosts.nextSteps, ...guests.nextSteps, ...repl.nextSteps, ...storage.nextSteps]) {
    const key = s.workflow ?? s.command ?? s.text;
    if (!seen.has(key)) (seen.add(key), steps.push(s));
  }

  const quorumHealth: Health = !h ? "unknown" : h.cluster.quorate === false ? "danger" : h.cluster.quorate ? "ok" : "unknown";
  const hostsHealth: Health = !h ? "unknown" : h.cluster.online_count < h.cluster.host_count ? "warn" : "ok";

  return (
    <>
      <PageHeader
        title="Overview"
        subtitle={h?.cluster.name ? <>Cluster <span className="font-medium text-fg">{h.cluster.name}</span>{h.probe && <> · read through {h.probe}</>}</> : "Cluster health at a glance"}
        actions={<SourceRefresh ids={["list_hosts", "list_guests", "list_replication", "list_storage"]} label="Refresh all" />}
      />
      <PageBody>
        <div className="grid grid-cols-2 gap-3 xl:grid-cols-4">
          <Stat
            icon={<Users />}
            label="Quorum"
            health={quorumHealth}
            loading={firstLoad(hosts)}
            value={!h ? "—" : h.cluster.quorate ? "Quorate" : h.cluster.quorate === false ? "Lost" : "Unknown"}
            detail={h && h.cluster.total_votes !== null ? `${h.cluster.total_votes} of ${h.cluster.expected_votes} votes · needs ${h.cluster.quorum}` : hosts.error ?? undefined}
            onClick={go("hosts")}
          />
          <Stat
            icon={<Server />}
            label="Hosts online"
            health={hostsHealth}
            loading={firstLoad(hosts)}
            value={h ? `${h.cluster.online_count}/${h.cluster.host_count}` : "—"}
            detail={h?.cluster.control_node ? `control node ${h.cluster.control_node}` : undefined}
            onClick={go("hosts")}
          />
          <Stat
            icon={<Boxes />}
            label="Production VMs"
            health={!g ? "unknown" : prodRunning < g.production.length ? "warn" : "ok"}
            loading={firstLoad(guests)}
            value={g ? `${prodRunning}/${g.production.length}` : "—"}
            detail={g ? `running · ${plural(g.staging.length, "staging VM")}` : guests.error ?? undefined}
            onClick={go("production")}
          />
          <Stat
            icon={<GitBranch />}
            label="Replication"
            health={!r ? "unknown" : failingJobs ? "danger" : "ok"}
            loading={firstLoad(repl)}
            value={r ? (failingJobs ? `${failingJobs} failing` : "Healthy") : "—"}
            detail={r ? `${plural(r.jobs.length, "job")}${tightest ? ` · tightest pool ${percent(tightest.free_fraction)} free` : ""}` : repl.error ?? undefined}
            onClick={go("production")}
          />
        </div>

        <div className="mt-5">
          <AttentionBanner problems={problems} steps={steps} />
        </div>

        <div className="grid gap-4 xl:grid-cols-2">
          <Card>
            <CardHeader icon={<Server />} title="Hosts" subtitle={h ? `${h.cluster.online_count} of ${plural(h.cluster.host_count, "host")} online` : "Proxmox nodes"} actions={<Button size="sm" variant="ghost" onClick={go("hosts")}>All hosts <ArrowRight /></Button>} />
            <div className="divide-y divide-line/70">
              {!h && firstLoad(hosts) && [0, 1, 2].map((i) => <div key={i} className="px-4 py-3"><Skeleton className="w-2/3" /></div>)}
              {!h && !firstLoad(hosts) && <EmptyState icon={<Server />} title="No host data yet" body={hosts.error ?? "Refresh to read the cluster."} />}
              {h?.hosts.map((host) => (
                <div key={host.name} className="grid grid-cols-[110px_1fr_1fr_auto] items-center gap-4 px-4 py-2.5">
                  <div className="flex items-center gap-2">
                    <span className={cn("size-2 rounded-full", host.online ? "bg-ok shadow-[0_0_8px] shadow-ok/60" : "bg-danger")} />
                    <span className="font-mono text-[12.5px] font-medium text-fg">{host.name}</span>
                  </div>
                  <div>
                    <div className="mb-1 flex justify-between text-[11px] text-fg-subtle"><span>CPU</span><span className="tabular-nums">{percent(host.cpu_fraction)}</span></div>
                    <Meter fraction={host.cpu_fraction} />
                  </div>
                  <div>
                    <div className="mb-1 flex justify-between text-[11px] text-fg-subtle"><span>Memory</span><span className="tabular-nums">{bytes(host.memory_used, 0)} / {bytes(host.memory_total, 0)}</span></div>
                    <Meter fraction={host.memory_total ? (host.memory_used ?? 0) / host.memory_total : null} />
                  </div>
                  <ExternalButton url={proxmoxUrl(host.name)} />
                </div>
              ))}
            </div>
          </Card>

          <Card>
            <CardHeader icon={<Boxes />} title="Production VMs" subtitle={g ? `${prodRunning} running` : "HA guests"} actions={<Button size="sm" variant="ghost" onClick={go("production")}>Manage <ArrowRight /></Button>} />
            <div className="divide-y divide-line/70">
              {!g && firstLoad(guests) && [0, 1, 2].map((i) => <div key={i} className="px-4 py-3"><Skeleton className="w-1/2" /></div>)}
              {!g && !firstLoad(guests) && <EmptyState icon={<Boxes />} title="No guest data yet" body={guests.error ?? "Refresh to read the cluster."} />}
              {g && g.production.length === 0 && <EmptyState icon={<Boxes />} title="No production VMs" body="Add one from the Production page." />}
              {g?.production.map((vm) => (
                <div key={vm.name} className="flex items-center gap-3 px-4 py-2.5">
                  <StatusPill health={vmHealth(vm.live_status)}>{vm.live_status}</StatusPill>
                  <span className="w-[64px] font-mono text-[12.5px] font-medium text-fg">{vm.name}</span>
                  <span className="min-w-0 flex-1 truncate text-[12.5px] text-fg-muted">{vm.domain ?? vm.purpose ?? "—"}</span>
                  <span className="text-[12px] text-fg-subtle">{vm.node ? `on ${vm.node}` : ""}</span>
                  {vm.replication.failing > 0 && <StatusPill health="danger">{vm.replication.failing} repl failing</StatusPill>}
                </div>
              ))}
            </div>
          </Card>

          <Card>
            <CardHeader icon={<FlaskConical />} title="Staging" subtitle={g ? plural(g.staging.length, "staging VM") : "Disposable copies"} actions={<Button size="sm" variant="ghost" onClick={go("staging")}>Manage <ArrowRight /></Button>} />
            <div className="divide-y divide-line/70">
              {g && g.staging.length === 0 && <EmptyState icon={<FlaskConical />} title="No staging VMs" body="Create a sanitized copy of a production VM to test changes." />}
              {g?.staging.map((vm) => (
                <div key={vm.name} className="flex items-center gap-3 px-4 py-2.5">
                  <StatusPill health={vmHealth(vm.live_status)}>{vm.live_status}</StatusPill>
                  <span className="font-mono text-[12.5px] font-medium text-fg">{vm.name}</span>
                  <span className="min-w-0 flex-1 truncate text-[12px] text-fg-subtle" title={vm.snapshot?.name}>
                    {[
                      vm.source && `copy of ${vm.source}`,
                      vm.snapshot?.created_at && `snapshot ${agoEpoch(vm.snapshot.created_at)}`,
                      vm.snapshot?.shared_with.length && `shared with ${vm.snapshot.shared_with.join(", ")}`,
                    ].filter(Boolean).join(" · ")}
                  </span>
                  {vm.url && <ExternalButton url={vm.url} />}
                </div>
              ))}
              {!g && <div className="px-4 py-3"><Skeleton className="w-1/2" /></div>}
            </div>
          </Card>

          <Card>
            <CardHeader icon={<History />} title="Recent operations" subtitle="Everything the dashboard ran" actions={<Button size="sm" variant="ghost" onClick={go("operations")}>History <ArrowRight /></Button>} />
            <div className="divide-y divide-line/70">
              {recent.length === 0 && <EmptyState icon={<History />} title="Nothing has run yet" />}
              {recent.map((run) => (
                <button key={run.run_id} onClick={() => focusRun(run.run_id)} className="flex w-full items-center gap-3 px-4 py-2.5 text-left hover:bg-surface-2/60">
                  <RunStatusIcon status={run.status} />
                  <span className="min-w-0 flex-1 truncate text-[12.5px] text-fg">
                    {run.workflow_title}
                    {run.target && <span className="text-fg-muted"> · {run.target}</span>}
                    {run.dry_run && <span className="ml-1.5 text-[11px] text-info">dry run</span>}
                  </span>
                  <span className="text-[11.5px] text-fg-subtle tabular-nums">{run.ended_at ? elapsed(run.started_at, run.ended_at) : "running"}</span>
                  <span className="w-[70px] text-right text-[11.5px] text-fg-subtle">{ago(Date.parse(run.started_at), now)}</span>
                </button>
              ))}
            </div>
          </Card>
        </div>

        {st && pools.length > 0 && (
          <Card className="mt-4">
            <CardHeader icon={<Database />} title="Storage pools" actions={<Button size="sm" variant="ghost" onClick={go("storage")}>Storage <ArrowRight /></Button>} />
            <div className="grid grid-cols-[repeat(auto-fill,minmax(220px,1fr))] gap-x-6 gap-y-3 p-4">
              {pools.map((p) => (
                <div key={`${p.node}/${p.name}`}>
                  <div className="mb-1 flex items-baseline justify-between text-[12px]">
                    <span className="font-mono text-fg">{p.node}/{p.name}</span>
                    <span className="text-fg-subtle tabular-nums">{bytes(p.free)} free</span>
                  </div>
                  <Meter fraction={p.size ? p.alloc / p.size : null} />
                </div>
              ))}
            </div>
          </Card>
        )}
        {h && (
          <button onClick={go("qdevice")} className="mt-4 flex w-full items-center gap-3 rounded-xl border border-line bg-surface px-4 py-3 text-left hover:border-line-strong">
            <Scale className="size-4 text-fg-subtle" />
            <span className="text-[13px] font-medium text-fg">QDevice</span>
            <span className="text-[12.5px] text-fg-muted">
              {h.qdevice.registered ? `registered${h.qdevice.address ? ` at ${h.qdevice.address}` : ""}` : "not registered"}
              {h.qdevice.needed !== null && ` · ${h.qdevice.needed ? "needed (even number of hosts)" : "not needed (odd number of hosts)"}`}
            </span>
            <ArrowRight className="ml-auto size-3.5 text-fg-subtle" />
          </button>
        )}
      </PageBody>
    </>
  );
}
