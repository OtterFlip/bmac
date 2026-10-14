// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// What this appliance keeps to rebuild each cluster it backs up: the
// recovery manifests (BMAC's configuration, registry, and backup records)
// and each host's LUKS header backups. And the restore tests the cluster
// reported: a separate signal from publication and scrub health.

import { FileArchive, FlaskConical, LifeBuoy } from "lucide-react";
import { Badge, Card, CardHeader, EmptyState, type ClusterRecoveryView } from "@bmac/ui";
import { PageBody, PageHeader, Problem, short, useLoad, when } from "../parts";

function Tests({ cluster }: { cluster: ClusterRecoveryView }) {
  if (cluster.restore_tests.length === 0) return <p className="px-4 py-3 text-[12.5px] text-fg-subtle">No restore test has read from this appliance yet.</p>;
  return (
    <ul className="divide-y divide-line/60 text-[12.5px]" aria-label={`${cluster.cluster_name} restore tests`}>
      {cluster.restore_tests.map((t) => (
        <li key={t.restore_uuid} className="flex flex-wrap items-center gap-2 px-4 py-2">
          <span className="w-[170px] shrink-0">{when(t.started_at_ms)}</span>
          <span className="text-fg-muted">
            VM <span className="font-mono">{short(t.vm_uuid)}</span>, PIT <span className="font-mono">{short(t.pit_uuid)}</span> on {t.host}
          </span>
          <span className="ml-auto flex gap-1">
            {t.error ? (
              <Badge tone="danger" title={t.error}>
                failed
              </Badge>
            ) : (
              <>
                <Badge tone={t.booted ? "ok" : "danger"}>{t.booted ? "booted" : "didn't boot"}</Badge>
                <Badge tone={t.agent ? "ok" : "warn"}>{t.agent ? `agent after ${t.agent_after_s ?? "?"} s` : "no agent"}</Badge>
              </>
            )}
          </span>
          {t.error && <div className="selectable w-full text-[12px] text-danger">{t.error}</div>}
        </li>
      ))}
    </ul>
  );
}

function ClusterCard({ cluster }: { cluster: ClusterRecoveryView }) {
  const newest = cluster.manifests[0];
  const hosts = [...new Set(cluster.headers.map((h) => h.host))];
  return (
    <Card>
      <CardHeader
        icon={<LifeBuoy />}
        title={cluster.cluster_name}
        subtitle={newest ? `Recovery manifest from ${when(newest.generated_at_ms)} · ${cluster.manifests.length} kept` : "No recovery manifest yet"}
      />
      <div className="border-b border-line/70 px-4 py-3 text-[12.5px]">
        <div className="mb-1 flex items-center gap-1.5 font-medium text-fg">
          <FileArchive className="size-4" /> LUKS header backups
        </div>
        {cluster.headers.length === 0 ? (
          <p className="text-fg-subtle">None kept yet; they are pulled hourly from each host.</p>
        ) : (
          <ul aria-label={`${cluster.cluster_name} LUKS header backups`}>
            {hosts.map((host) => (
              <li key={host} className="py-0.5">
                <span className="font-mono">{host}</span>:{" "}
                {cluster.headers
                  .filter((h) => h.host === host)
                  .map((h) => `${h.file} (${h.versions} kept, newest ${when(h.newest_at_ms)})`)
                  .join(", ")}
              </li>
            ))}
          </ul>
        )}
      </div>
      <div className="flex items-center gap-1.5 px-4 pt-3 text-[12.5px] font-medium text-fg">
        <FlaskConical className="size-4" /> Restore tests
      </div>
      <Tests cluster={cluster} />
    </Card>
  );
}

export function RecoveryPage() {
  const { data, error } = useLoad((a) => a.recovery(), 60_000);
  return (
    <>
      <PageHeader title="Recovery" subtitle="What this appliance keeps to rebuild a lost cluster, and the restore tests that read from it" />
      <PageBody>
        <Problem>{error}</Problem>
        {data && data.clusters.length === 0 && <EmptyState icon={<LifeBuoy />} title="No cluster yet" body="Enroll this appliance with a cluster first." />}
        {data?.clusters.map((c) => (
          <ClusterCard key={c.cluster_uuid} cluster={c} />
        ))}
      </PageBody>
    </>
  );
}
