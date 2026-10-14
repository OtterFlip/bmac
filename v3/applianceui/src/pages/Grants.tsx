// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

import { useState } from "react";
import { KeyRound, LifeBuoy } from "lucide-react";
import { Badge, Button, Card, Dialog, EmptyState, Input, Label, type ClusterView, type IssuedGrant } from "@bmac/ui";
import { Done, PageBody, PageHeader, Problem, short, useAction, useApi, useLoad, when } from "../parts";

export function GrantsPage() {
  const api = useApi();
  const { data, error, reload } = useLoad((a) => a.grants());
  const clusters = useLoad((a) => a.status()).data?.clusters ?? [];
  const action = useAction();
  const [recovering, setRecovering] = useState(false);
  return (
    <>
      <PageHeader
        title="Grants"
        subtitle="What a cluster's hosts may read from here. Grant a restore from the PITs page; grant a recovery here, for a cluster rebuilt on new hardware."
        actions={
          <Button size="sm" disabled={clusters.length === 0} onClick={() => setRecovering(true)}>
            <LifeBuoy className="size-4" /> Grant a recovery
          </Button>
        }
      />
      <PageBody>
        <Problem>{error ?? action.error}</Problem>
        <Done>{action.done}</Done>
        {data && data.length === 0 && <EmptyState icon={<KeyRound />} title="No grants" body="A restore grant lets one cluster read the PITs it names, for a few hours." />}
        {data && data.length > 0 && (
          <Card>
            <table className="w-full text-[12.5px]">
              <thead className="text-left text-fg-subtle">
                <tr className="border-b border-line">
                  <th className="px-3 py-2 font-medium">Grant</th>
                  <th className="px-3 py-2 font-medium">Kind</th>
                  <th className="px-3 py-2 font-medium">Cluster</th>
                  <th className="px-3 py-2 font-medium">For</th>
                  <th className="px-3 py-2 font-medium">Until</th>
                  <th className="px-3 py-2 font-medium">Last used</th>
                  <th className="px-3 py-2 font-medium">State</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {data.map((g) => (
                  <tr key={g.grant_id} className="border-b border-line/50">
                    <td className="px-3 py-1.5 font-mono">{g.grant_id}</td>
                    <td className="px-3 py-1.5">{g.kind}</td>
                    <td className="px-3 py-1.5 font-mono">{short(g.cluster_uuid)}</td>
                    <td className="px-3 py-1.5 font-mono">{g.kind === "recovery" ? `host ${g.host_fingerprint ?? "?"}` : g.pits.map(short).join(", ")}</td>
                    <td className="px-3 py-1.5">{when(g.expires_at_ms)}</td>
                    <td className="px-3 py-1.5">{when(g.last_used_ms)}</td>
                    <td className="px-3 py-1.5">
                      <Badge tone={g.state === "live" ? "ok" : "neutral"}>{g.state}</Badge>
                    </td>
                    <td className="px-3 py-1.5 text-right">
                      {g.state === "live" && (
                        <Button size="sm" variant="danger-outline" disabled={action.busy} onClick={() => void action.run(async () => (await api.revokeGrant(g.grant_id), await reload(), `Grant ${g.grant_id} is revoked.`))}>
                          Revoke
                        </Button>
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </Card>
        )}
        {recovering && <RecoveryDialog clusters={clusters} done={() => void reload()} close={() => setRecovering(false)} />}
      </PageBody>
    </>
  );
}

function RecoveryDialog({ clusters, done, close }: { clusters: ClusterView[]; done: () => void; close: () => void }) {
  const api = useApi();
  const [cluster, setCluster] = useState(clusters[0]?.cluster_uuid ?? "");
  const [fingerprint, setFingerprint] = useState("");
  const [hours, setHours] = useState("24");
  const [issued, setIssued] = useState<IssuedGrant | null>(null);
  const action = useAction();
  const name = clusters.find((c) => c.cluster_uuid === cluster)?.cluster_name ?? cluster;
  const issue = () =>
    action.run(async () => {
      setIssued(await api.issueRecoveryGrant({ cluster_uuid: cluster, hours: Number(hours), host_fingerprint: fingerprint.trim() }));
      done();
    });
  return (
    <Dialog
      open
      onOpenChange={(open) => !open && close()}
      title="Recovery grant"
      icon={<LifeBuoy />}
      description={
        issued
          ? "Paste this token into the new cluster's Recover from appliance job. It isn't shown again."
          : `Lets one host of a cluster rebuilt on new hardware read everything of ${name} here, and take over its backups. Only grant it when ${name} is lost.`
      }
      footer={
        issued ? (
          <Button variant="primary" onClick={close}>
            Done
          </Button>
        ) : (
          <>
            <Button variant="ghost" onClick={close}>
              Cancel
            </Button>
            <Button variant="primary" disabled={action.busy || !cluster || fingerprint.trim() === "" || !(Number(hours) >= 1)} onClick={() => void issue()}>
              Issue grant
            </Button>
          </>
        )
      }
    >
      <Problem>{action.error}</Problem>
      {issued ? (
        <div className="space-y-2">
          <p className="selectable break-all rounded-lg border border-line bg-sunken px-3 py-2 font-mono text-[12px] text-fg">{issued.token}</p>
          <Button size="sm" onClick={() => void navigator.clipboard?.writeText(issued.token)}>
            Copy
          </Button>
          <p className="text-[12px] text-fg-muted">
            Grant {issued.grant.grant_id} lets the host {issued.grant.host_fingerprint} in until {new Date(issued.grant.expires_at_ms).toLocaleString()}.
          </p>
        </div>
      ) : (
        <div className="space-y-3">
          <div>
            <Label htmlFor="recovery-cluster">Lost cluster</Label>
            <select id="recovery-cluster" className="h-8 w-full rounded-lg border border-line bg-sunken px-2 text-[13px]" value={cluster} onChange={(e) => setCluster(e.target.value)}>
              {clusters.map((c) => (
                <option key={c.cluster_uuid} value={c.cluster_uuid}>
                  {c.cluster_name} ({short(c.cluster_uuid)})
                </option>
              ))}
            </select>
          </div>
          <div>
            <Label htmlFor="recovery-host">New host's certificate fingerprint (SHA-256)</Label>
            <Input id="recovery-host" value={fingerprint} onChange={(e) => setFingerprint(e.target.value)} placeholder="AB:CD:..." />
            <p className="mt-1 text-[12px] text-fg-muted">The recover job shows it when it starts. Compare it with the host's own certificate before you issue the grant.</p>
          </div>
          <div>
            <Label htmlFor="recovery-hours">Hours it lasts (1 to 168)</Label>
            <Input id="recovery-hours" value={hours} onChange={(e) => setHours(e.target.value)} inputMode="numeric" />
          </div>
        </div>
      )}
    </Dialog>
  );
}
