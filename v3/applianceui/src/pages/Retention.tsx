// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// The appliance's own retention. A change that keeps less in any tier waits
// out the delay (BACKUP.md, "Appliance retention"), so a stolen session
// can't quietly empty the appliance; the floor is set only at the console.

import { useEffect, useState } from "react";
import { Archive, Scissors } from "lucide-react";
import { Button, Card, CardBody, CardHeader, Checkbox, Dialog, Input, KV, Label, bytes, type RetentionCounts } from "@bmac/ui";
import { Done, PageBody, PageHeader, Problem, Warning, short, useAction, useApi, useLoad, when, whenSeconds } from "../parts";

const TIERS = ["daily", "weekly", "monthly"] as const;

/** Such as "7 daily, 4 weekly, 12 monthly of 30 days, then yearly". */
export function describeRetention(p: RetentionCounts): string {
  return `${p.daily} daily, ${p.weekly} weekly, ${p.monthly} monthly of ${p.month_days} days, ${p.yearly ? "then yearly" : "then every monthly"}`;
}

export function RetentionPage() {
  const api = useApi();
  const { data, error, reload } = useLoad((a) => a.retention());
  const [form, setForm] = useState<Record<(typeof TIERS)[number] | "month_days", string>>({ daily: "", weekly: "", monthly: "", month_days: "" });
  const [yearly, setYearly] = useState(true);
  const [confirming, setConfirming] = useState(false);
  const action = useAction();
  useEffect(() => {
    if (!data) return;
    setForm({ daily: String(data.policy.daily), weekly: String(data.policy.weekly), monthly: String(data.policy.monthly), month_days: String(data.policy.month_days) });
    setYearly(data.policy.yearly);
  }, [data]);
  const policy: RetentionCounts = { daily: Number(form.daily), weekly: Number(form.weekly), monthly: Number(form.monthly), month_days: Number(form.month_days), yearly };
  const valid = TIERS.every((t) => /^\d+$/.test(form[t])) && /^\d+$/.test(form.month_days) && policy.month_days >= 28 && policy.month_days <= 31;
  const save = () =>
    action.run(async () => {
      const change = await api.setRetention(policy);
      await reload();
      return change.applied ? "The new policy is in effect." : `It keeps less, so it takes effect ${when(change.effective_at_ms)}. Until then the current policy stays.`;
    });
  return (
    <>
      <PageHeader title="Retention" subtitle="Which PITs this appliance keeps of each VM: all of today's, then the newest of each day, week, month, and optionally year, on its cluster's calendar" />
      <PageBody>
        <Problem>{error ?? action.error}</Problem>
        <Done>{action.done}</Done>
        {data?.pending && (
          <Warning>
            Pending: {describeRetention(data.pending.policy)}, from {when(data.pending.effective_at_ms)} (asked for by {data.pending.by}).{" "}
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
                <div className="grid grid-cols-3 gap-3">
                  <div>
                    <Label htmlFor="keep-month-days">days in a month</Label>
                    <Input id="keep-month-days" value={form.month_days} inputMode="numeric" onChange={(e) => setForm({ ...form, month_days: e.target.value })} />
                    <p className="mt-1 text-[11.5px] text-fg-subtle">28 to 31</p>
                  </div>
                  <label className="col-span-2 flex items-center gap-2 self-center text-[12.5px]">
                    <Checkbox checked={yearly} onCheckedChange={(on) => setYearly(on === true)} aria-label="yearly" />
                    Keep one PIT a year for months older than the monthly ones; off keeps every monthly PIT
                  </label>
                </div>
                <p className="text-[12px] text-fg-subtle">Every PIT of today stays. Weeks, months, and years count from each VM's first PIT; days are its cluster's calendar days.</p>
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
                  <th className="px-3 py-2 font-medium">Cluster's zone</th>
                </tr>
              </thead>
              <tbody>
                {data.coverage.map((c) => (
                  <tr key={`${c.cluster_uuid}/${c.vm_uuid}`} className="border-b border-line/50">
                    <td className="px-3 py-1.5 font-mono">{short(c.vm_uuid)}</td>
                    <td className="px-3 py-1.5">{c.pits}</td>
                    <td className="px-3 py-1.5">{whenSeconds(c.oldest_s, c.time_zone)}</td>
                    <td className="px-3 py-1.5">{whenSeconds(c.newest_s, c.time_zone)}</td>
                    <td className="px-3 py-1.5 text-fg-subtle">{c.time_zone}</td>
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
