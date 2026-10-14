// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// The appliance's own retention. A change that keeps less in any tier waits
// out the delay (BACKUP.md, "Appliance retention"), so a stolen session
// can't quietly empty the appliance; the floor is set only at the console.

import { useEffect, useState } from "react";
import { Archive, Scissors } from "lucide-react";
import { Button, Card, CardBody, CardHeader, Dialog, Input, KV, Label, bytes, type RetentionCounts } from "@bmac/ui";
import { Done, PageBody, PageHeader, Problem, Warning, short, useAction, useApi, useLoad, when, whenSeconds } from "../parts";

const TIERS = ["daily", "weekly", "monthly"] as const;

export function RetentionPage() {
  const api = useApi();
  const { data, error, reload } = useLoad((a) => a.retention());
  const [form, setForm] = useState<Record<(typeof TIERS)[number], string>>({ daily: "", weekly: "", monthly: "" });
  const [confirming, setConfirming] = useState(false);
  const action = useAction();
  useEffect(() => {
    if (data) setForm({ daily: String(data.policy.daily), weekly: String(data.policy.weekly), monthly: String(data.policy.monthly) });
  }, [data]);
  const policy: RetentionCounts = { daily: Number(form.daily), weekly: Number(form.weekly), monthly: Number(form.monthly) };
  const valid = TIERS.every((t) => /^\d+$/.test(form[t]));
  const save = () =>
    action.run(async () => {
      const change = await api.setRetention(policy);
      await reload();
      return change.applied ? "The new policy is in effect." : `It keeps less, so it takes effect ${when(change.effective_at_ms)}. Until then the current policy stays.`;
    });
  return (
    <>
      <PageHeader title="Retention" subtitle="How many daily, weekly, and monthly PITs this appliance keeps of each VM" />
      <PageBody>
        <Problem>{error ?? action.error}</Problem>
        <Done>{action.done}</Done>
        {data?.pending && (
          <Warning>
            Pending: {data.pending.policy.daily} daily, {data.pending.policy.weekly} weekly, {data.pending.policy.monthly} monthly, from {when(data.pending.effective_at_ms)} (asked for by {data.pending.by}).{" "}
            <Button size="sm" variant="link" onClick={() => void action.run(async () => (await api.cancelRetention(), await reload(), "The pending change is canceled."))}>
              Cancel it
            </Button>
          </Warning>
        )}
        {data && (
          <div className="grid gap-5 lg:grid-cols-2">
            <Card>
              <CardHeader title="Policy" icon={<Archive />} subtitle={`A change that keeps less waits ${data.delay_hours} hours`} />
              <CardBody className="space-y-3">
                <div className="grid grid-cols-3 gap-3">
                  {TIERS.map((t) => (
                    <div key={t}>
                      <Label htmlFor={`keep-${t}`}>{t}</Label>
                      <Input id={`keep-${t}`} value={form[t]} inputMode="numeric" onChange={(e) => setForm({ ...form, [t]: e.target.value })} />
                      <p className="mt-1 text-[11.5px] text-fg-subtle">at least {data.floor[t]}</p>
                    </div>
                  ))}
                </div>
                <Button variant="primary" disabled={!valid || action.busy} onClick={() => void save()}>
                  Save
                </Button>
              </CardBody>
            </Card>
            <Card>
              <CardHeader
                title="A prune now"
                icon={<Scissors />}
                actions={
                  <Button size="sm" variant="danger-outline" disabled={!data.prune.length} onClick={() => setConfirming(true)}>
                    Prune now
                  </Button>
                }
              />
              <CardBody>
                <KV
                  items={[
                    ["Would delete", `${data.prune.length} PITs`],
                    ["Would free about", bytes(data.reclaim_bytes)],
                    ["Held by you", `${data.holds.length} PITs`],
                  ]}
                />
                <p className="mt-2 text-[12px] text-fg-subtle">Prunes also run every hour. The newest PIT of each VM, each branch's head and root, held PITs, and PITs a live restore grant names always stay.</p>
              </CardBody>
            </Card>
          </div>
        )}
        {data && (
          <Card>
            <CardHeader title="What each VM can be restored to after a prune" />
            <table className="w-full text-[12.5px]">
              <thead className="text-left text-fg-subtle">
                <tr className="border-b border-line">
                  <th className="px-3 py-2 font-medium">VM</th>
                  <th className="px-3 py-2 font-medium">PITs kept</th>
                  <th className="px-3 py-2 font-medium">Oldest</th>
                  <th className="px-3 py-2 font-medium">Newest</th>
                </tr>
              </thead>
              <tbody>
                {data.coverage.map((c) => (
                  <tr key={`${c.cluster_uuid}/${c.vm_uuid}`} className="border-b border-line/50">
                    <td className="px-3 py-1.5 font-mono">{short(c.vm_uuid)}</td>
                    <td className="px-3 py-1.5">{c.pits}</td>
                    <td className="px-3 py-1.5">{whenSeconds(c.oldest_s)}</td>
                    <td className="px-3 py-1.5">{whenSeconds(c.newest_s)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </Card>
        )}
      </PageBody>
      {confirming && data && (
        <Dialog
          open
          onOpenChange={(open) => !open && setConfirming(false)}
          title={`Delete ${data.prune.length} PITs now?`}
          tone="danger"
          description="They can't be restored afterwards. The policy would prune them within the hour anyway."
          footer={
            <>
              <Button variant="ghost" onClick={() => setConfirming(false)}>
                Cancel
              </Button>
              <Button
                variant="danger"
                disabled={action.busy}
                onClick={() =>
                  void action
                    .run(async () => {
                      const result = await api.prune();
                      await reload();
                      return `Pruned ${result.pruned} PITs, about ${bytes(result.reclaim_bytes)}.${result.skipped.length ? ` ${result.skipped.length} stay after a failure: ${result.skipped.join("; ")}` : ""}`;
                    })
                    .then(() => setConfirming(false))
                }
              >
                Prune
              </Button>
            </>
          }
        />
      )}
    </>
  );
}
