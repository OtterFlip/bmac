// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

import { Database, Radio, ShieldCheck, Waypoints } from "lucide-react";
import { Badge, Card, CardBody, CardHeader, KV, Meter, StatusPill, bytes, type ApplianceStatusView, type Health } from "@bmac/ui";
import { PageBody, PageHeader, Problem, Warning, short, useLoad, when } from "../parts";

function poolHealth(health: string): Health {
  if (health === "ONLINE") return "ok";
  if (health === "DEGRADED") return "warn";
  return "danger";
}

function ScrubLine({ status }: { status: ApplianceStatusView }) {
  const s = status.scrub;
  if (!s) return <span className="text-fg-subtle">scheduled scrubs are off</span>;
  const done = s.done_permille !== null ? ` ${(s.done_permille / 10).toFixed(1)}%` : "";
  return (
    <span>
      {s.state}
      {done}
      {s.finished_at_ms ? `, finished ${when(s.finished_at_ms)}` : ""}
      {s.next_due_ms ? `; next ${when(s.next_due_ms)}` : ""}
    </span>
  );
}

export function OverviewPage() {
  const { data, error } = useLoad((api) => api.status(), 10_000);
  return (
    <>
      <PageHeader title="Overview" subtitle={data ? `${data.hello.name} · ${data.pits} PITs published` : undefined} />
      <PageBody>
        <Problem>{error}</Problem>
        {data && (
          <>
            {data.scrub && data.scrub.errors > 0 && (
              <Warning>
                The last scrub found {data.scrub.errors} unrepairable errors. {data.scrub.suspect_pits.length} PITs are suspect; restore them from another copy, or take a new PIT.
              </Warning>
            )}
            {data.notify_error && <Warning>The watchdog's email is stopped: {data.notify_error}</Warning>}
            {data.pending_retention && (
              <Warning>
                A retention change that keeps less ({data.pending_retention.policy.daily} daily, {data.pending_retention.policy.weekly} weekly, {data.pending_retention.policy.monthly} monthly) takes effect {when(data.pending_retention.effective_at_ms)}.
              </Warning>
            )}
            <div className="grid gap-5 lg:grid-cols-2">
              <Card>
                <CardHeader title="Pool" icon={<Database />} actions={data.pool && <StatusPill health={poolHealth(data.pool.health)}>{data.pool.health}</StatusPill>} />
                <CardBody>
                  {data.pool ? (
                    <>
                      <Meter fraction={data.pool.size_bytes ? 1 - data.pool.free_bytes / data.pool.size_bytes : null} className="mb-3" />
                      <KV
                        items={[
                          ["Name", data.pool.name],
                          ["GUID", <span className="font-mono">{data.pool.guid}</span>],
                          ["Free", `${bytes(data.pool.free_bytes)} of ${bytes(data.pool.size_bytes)}`],
                          ["Scrub", <ScrubLine status={data} />],
                        ]}
                      />
                    </>
                  ) : (
                    <p className="text-[13px] text-fg-muted">The pool's state can't be read.</p>
                  )}
                </CardBody>
              </Card>
              <Card>
                <CardHeader title="This appliance" icon={<ShieldCheck />} />
                <CardBody>
                  <KV
                    items={[
                      ["Name", data.hello.name],
                      ["UUID", <span className="font-mono">{data.hello.appliance_uuid}</span>],
                      ["Version", data.hello.version],
                      ["Fingerprint", <span className="selectable break-all font-mono text-[11.5px]">{data.hello.fingerprint}</span>],
                    ]}
                  />
                </CardBody>
              </Card>
            </div>
            <Card>
              <CardHeader title="Clusters" icon={<Waypoints />} subtitle="Each enrolled cluster, as its last check-in left it" />
              <CardBody className="space-y-3">
                {data.clusters.length === 0 && <p className="text-[13px] text-fg-muted">No cluster is enrolled yet. Enroll with a bundle from the Dashboard: bmac-backupd enroll BUNDLE.</p>}
                {data.clusters.map((c) => (
                  <div key={c.cluster_uuid} className="rounded-lg border border-line px-3 py-2">
                    <div className="flex flex-wrap items-center gap-2">
                      <span className="text-[13px] font-semibold text-fg">{c.cluster_name}</span>
                      {c.state && <Badge tone={c.state === "ACTIVE" ? "ok" : "warn"}>{c.state}</Badge>}
                      {c.pulling && <Badge tone="run">{c.pulling}</Badge>}
                      {c.offers > 0 && <Badge tone="info">{c.offers} offered</Badge>}
                      <span className="ml-auto text-[12px] text-fg-subtle">last check-in {when(c.last_checkin_ms)}</span>
                    </div>
                    <div className="mt-1 text-[12px] text-fg-muted">hosts: {c.hosts.join(", ") || "none yet"}</div>
                    {c.last_error && <div className="mt-1 text-[12px] text-danger">{c.last_error}</div>}
                  </div>
                ))}
              </CardBody>
            </Card>
            <Card>
              <CardHeader title="Watchdog" icon={<Radio />} subtitle="Whether each cluster and checked URL answers from here" />
              <CardBody className="space-y-1.5">
                {data.watch.length === 0 && <p className="text-[13px] text-fg-muted">Nothing is watched yet.</p>}
                {data.watch.map((w) => (
                  <div key={`${w.kind}/${w.subject}`} className="flex flex-wrap items-center gap-2 text-[13px]">
                    <StatusPill health={w.down_since_ms ? "danger" : "ok"}>{w.down_since_ms ? "not answering" : "answering"}</StatusPill>
                    <span className="text-fg">{w.subject}</span>
                    {w.down_since_ms && <span className="text-fg-subtle">since {when(w.down_since_ms)}</span>}
                    {w.last_error && <span className="text-[12px] text-fg-subtle">({w.last_error})</span>}
                  </div>
                ))}
              </CardBody>
            </Card>
            {data.transfers.length > 0 && (
              <Card>
                <CardHeader title="Pulling now" />
                <CardBody className="space-y-1 text-[13px]">
                  {data.transfers.map((t) => (
                    <div key={t.pit_uuid}>
                      PIT <span className="font-mono">{short(t.pit_uuid)}</span> of VM <span className="font-mono">{short(t.vm_uuid)}</span>: {bytes(t.bytes)} so far
                    </div>
                  ))}
                </CardBody>
              </Card>
            )}
          </>
        )}
      </PageBody>
    </>
  );
}
