// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// The PITs on this appliance, one graph per VM: time runs down the page,
// the oldest PIT at the top left, and each branch to the right of the line
// it came from. Selected PITs of one cluster can be granted to a restore.

import { useMemo, useState } from "react";
import { GitBranch, KeyRound, Lock, Unlock } from "lucide-react";
import { Badge, Button, Card, CardHeader, Checkbox, Dialog, EmptyState, Input, Label, PitGraphLines, bytes, cn, type IssuedGrant, type PitView } from "@bmac/ui";
import { Done, PageBody, PageHeader, Problem, short, useAction, useApi, useLoad, whenSeconds } from "../parts";
import { byVm, layout } from "../lib/graph";

export function PitGraph({ pits, selected, toggle }: { pits: PitView[]; selected: Set<string>; toggle: (pit: PitView) => void }) {
  const graph = useMemo(() => layout(pits), [pits]);
  return (
    <div className="flex">
      <PitGraphLines
        graph={graph}
        selected={(pit) => selected.has(pit.pit_uuid)}
        pick={toggle}
        nodeClass={(n) => (n.pit.suspect ? "fill-danger stroke-danger" : undefined)}
        title={(pit) => pit.name}
      />
      <ul className="min-w-0 flex-1">
        {graph.nodes.map((n) => (
          <li key={n.pit.pit_uuid} className={cn("flex h-[32px] items-center gap-2 border-b border-line/50 px-2 text-[12.5px]", selected.has(n.pit.pit_uuid) && "bg-accent/8")}>
            <Checkbox checked={selected.has(n.pit.pit_uuid)} onCheckedChange={() => toggle(n.pit)} aria-label={`select ${n.pit.name}`} />
            <span className="w-[170px] shrink-0 text-fg">{whenSeconds(n.pit.taken_s, n.pit.time_zone)}</span>
            <span className="truncate font-mono text-[11.5px] text-fg-subtle">{n.pit.name}</span>
            <span className="text-fg-subtle">{bytes(n.pit.bytes)}</span>
            <span className="ml-auto flex shrink-0 gap-1">
              {n.pit.suspect && <Badge tone="danger">suspect</Badge>}
              {n.pit.held && <Badge tone="info">held</Badge>}
              {n.pit.granted && <Badge tone="accent">granted</Badge>}
              {n.pit.received_from === null && n.row > 0 && <Badge>full</Badge>}
            </span>
          </li>
        ))}
      </ul>
    </div>
  );
}

function GrantDialog({ pits, done, close }: { pits: PitView[]; done: () => void; close: () => void }) {
  const api = useApi();
  const [hours, setHours] = useState("24");
  const [issued, setIssued] = useState<IssuedGrant | null>(null);
  const action = useAction();
  const cluster = pits[0]?.cluster_uuid ?? "";
  const issue = () =>
    action.run(async () => {
      const grant = await api.issueGrant({ cluster_uuid: cluster, hours: Number(hours), pits: pits.map((p) => p.pit_uuid) });
      setIssued(grant);
      done();
    });
  return (
    <Dialog
      open
      onOpenChange={(open) => !open && close()}
      title="Restore grant"
      icon={<KeyRound />}
      description={issued ? "Paste this token into the Dashboard's restore wizard. It isn't shown again." : `Lets cluster ${pits[0]?.cluster_name} read ${pits.length} selected PITs for a restore.`}
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
            <Button variant="primary" disabled={action.busy || !(Number(hours) >= 1)} onClick={() => void issue()}>
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
          <p className="text-[12px] text-fg-muted">Grant {issued.grant.grant_id} lasts until {new Date(issued.grant.expires_at_ms).toLocaleString()}.</p>
        </div>
      ) : (
        <div>
          <Label htmlFor="grant-hours">Hours it lasts (1 to 168)</Label>
          <Input id="grant-hours" value={hours} onChange={(e) => setHours(e.target.value)} inputMode="numeric" />
        </div>
      )}
    </Dialog>
  );
}

export function PitsPage() {
  const api = useApi();
  const { data, error, reload } = useLoad((a) => a.pits());
  const [selected, setSelected] = useState<Map<string, PitView>>(new Map());
  const [granting, setGranting] = useState(false);
  const action = useAction();
  const groups = useMemo(() => byVm(data?.pits ?? []), [data]);
  const chosen = [...selected.values()];
  const oneCluster = new Set(chosen.map((p) => p.cluster_uuid)).size === 1;
  const toggle = (pit: PitView) =>
    setSelected((prev) => {
      const next = new Map(prev);
      if (next.has(pit.pit_uuid)) next.delete(pit.pit_uuid);
      else next.set(pit.pit_uuid, pit);
      return next;
    });
  const hold = (on: boolean) =>
    action.run(async () => {
      for (const pit of chosen) await api.hold(pit.pit_uuid, on);
      await reload();
      return on ? `${chosen.length} PITs are held` : `${chosen.length} PITs are released`;
    });
  return (
    <>
      <PageHeader
        title="PITs"
        subtitle="Each VM's points in time, oldest at the top; branches extend to the right"
        actions={
          <>
            <Button disabled={!chosen.length || action.busy} onClick={() => void hold(true)}>
              <Lock /> Hold
            </Button>
            <Button disabled={!chosen.length || action.busy} onClick={() => void hold(false)}>
              <Unlock /> Release
            </Button>
            <Button variant="primary" disabled={!oneCluster} onClick={() => setGranting(true)}>
              <KeyRound /> Grant to a restore
            </Button>
          </>
        }
      />
      <PageBody>
        <Problem>{error ?? action.error}</Problem>
        <Done>{action.done}</Done>
        {chosen.length > 0 && !oneCluster && <Problem>A restore grant is for one cluster; select PITs of one cluster only.</Problem>}
        {data && groups.length === 0 && <EmptyState icon={<GitBranch />} title="No PITs yet" body="PITs appear here once a cluster offers them and the pull is verified." />}
        {groups.map((g) => (
          <Card key={g.key}>
            <CardHeader title={`VM ${short(g.vm)}`} subtitle={`${g.clusterName} · ${g.pits.length} PITs · times in ${g.pits[0]?.time_zone ?? "UTC"}`} icon={<GitBranch />} />
            <div className="p-2">
              <PitGraph pits={g.pits} selected={new Set(selected.keys())} toggle={toggle} />
            </div>
          </Card>
        ))}
        {data && data.receiving.length > 0 && (
          <Card>
            <CardHeader title="Receiving" />
            <ul className="p-3 text-[12.5px]">
              {data.receiving.map((r) => (
                <li key={r.pit_uuid}>
                  PIT <span className="font-mono">{short(r.pit_uuid)}</span> of VM <span className="font-mono">{short(r.vm_uuid)}</span>: {bytes(r.bytes)} since {new Date(r.started_at_ms).toLocaleString()}
                </li>
              ))}
            </ul>
          </Card>
        )}
        {data && data.quarantined.length > 0 && (
          <Card>
            <CardHeader title="Quarantined" subtitle="Receives that failed their checks; kept a week for a look" />
            <ul className="p-3 text-[12.5px]">
              {data.quarantined.map((q) => (
                <li key={q.pit_uuid}>
                  PIT <span className="font-mono">{short(q.pit_uuid)}</span>: <span className="selectable">{q.reason}</span>
                </li>
              ))}
            </ul>
          </Card>
        )}
      </PageBody>
      {granting && (
        <GrantDialog
          pits={chosen}
          done={() => {
            setSelected(new Map());
            void reload();
          }}
          close={() => setGranting(false)}
        />
      )}
    </>
  );
}
