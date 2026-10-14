// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// Scrubs and the I/O policy, the watchdog and its email, and the
// administrator's password. The Postmark token is write-only here.

import { useEffect, useState } from "react";
import { HardDrive, KeyRound, Mail, Radio } from "lucide-react";
import { Button, Card, CardBody, CardHeader, Input, Label, Select, Textarea, type PlatformView } from "@bmac/ui";
import { Done, PageBody, PageHeader, Problem, useAction, useApi, useLoad } from "../parts";

const IO_POLICIES = [
  { value: "pulls_first", label: "Pulls first", help: "A pull pauses a running scrub, and no scrub starts during a pull" },
  { value: "scrub_first", label: "Scrub first", help: "No pull starts while a scrub runs" },
  { value: "together", label: "Together", help: "Both run at once" },
];

interface Form {
  scrub_every_days: string;
  io_policy: string;
  down_after_minutes: string;
  urls: string;
  from: string;
  to: string;
}

function toForm(p: PlatformView): Form {
  return {
    scrub_every_days: String(p.scrub_every_days),
    io_policy: p.io_policy,
    down_after_minutes: String(p.down_after_minutes),
    urls: p.urls.join("\n"),
    from: p.from,
    to: p.to.join(", "),
  };
}

const lines = (text: string, split: RegExp) =>
  text
    .split(split)
    .map((s) => s.trim())
    .filter(Boolean);

function PasswordCard() {
  const api = useApi();
  const [current, setCurrent] = useState("");
  const [next, setNext] = useState("");
  const [again, setAgain] = useState("");
  const action = useAction();
  const change = () =>
    action.run(async () => {
      try {
        await api.changePassword(current, next);
        return "The password is changed; every other session has ended.";
      } finally {
        setCurrent("");
        setNext("");
        setAgain("");
      }
    });
  return (
    <Card>
      <CardHeader title="Administrator password" icon={<KeyRound />} subtitle="At least 12 characters. Changing it logs out every other session." />
      <CardBody className="space-y-3">
        <Problem>{action.error}</Problem>
        <Done>{action.done}</Done>
        <div className="grid gap-3 lg:grid-cols-3">
          <div>
            <Label htmlFor="pw-current">Current</Label>
            <Input id="pw-current" type="password" value={current} onChange={(e) => setCurrent(e.target.value)} autoComplete="current-password" />
          </div>
          <div>
            <Label htmlFor="pw-new">New</Label>
            <Input id="pw-new" type="password" value={next} onChange={(e) => setNext(e.target.value)} autoComplete="new-password" />
          </div>
          <div>
            <Label htmlFor="pw-again">New, again</Label>
            <Input id="pw-again" type="password" value={again} onChange={(e) => setAgain(e.target.value)} autoComplete="new-password" />
          </div>
        </div>
        {again && next !== again && <p className="text-[12px] text-danger">The new passwords differ.</p>}
        <Button variant="primary" disabled={action.busy || !current || next.length < 12 || next !== again} onClick={() => void change()}>
          Change password
        </Button>
      </CardBody>
    </Card>
  );
}

export function SettingsPage() {
  const api = useApi();
  const { data, error, reload } = useLoad((a) => a.settings());
  const [form, setForm] = useState<Form | null>(null);
  const [token, setToken] = useState("");
  const action = useAction();
  useEffect(() => {
    if (data) setForm(toForm(data));
  }, [data]);
  const save = () =>
    action.run(async () => {
      if (!form) return;
      const saved = await api.saveSettings({
        scrub_every_days: Number(form.scrub_every_days),
        io_policy: form.io_policy,
        down_after_minutes: Number(form.down_after_minutes),
        urls: lines(form.urls, /\n/),
        from: form.from.trim(),
        to: lines(form.to, /[,\s]+/),
      });
      setForm(toForm(saved));
      return "Saved.";
    });
  const set = (key: keyof Form) => (value: string) => setForm((f) => (f ? { ...f, [key]: value } : f));
  return (
    <>
      <PageHeader title="Settings" subtitle="This appliance's scrubs, watchdog, and email" />
      <PageBody>
        <Problem>{error ?? action.error}</Problem>
        <Done>{action.done}</Done>
        {form && data && (
          <>
            <Card>
              <CardHeader
                title="Scrubs and disk use"
                icon={<HardDrive />}
                actions={
                  <Button size="sm" disabled={action.busy} onClick={() => void action.run(async () => (await api.startScrub(), "A scrub starts within a few seconds, when no pull holds it back."))}>
                    Scrub now
                  </Button>
                }
              />
              <CardBody className="grid gap-3 lg:grid-cols-2">
                <div>
                  <Label htmlFor="scrub-days">Scrub every (days; 0 is off)</Label>
                  <Input id="scrub-days" value={form.scrub_every_days} onChange={(e) => set("scrub_every_days")(e.target.value)} inputMode="numeric" />
                </div>
                <div>
                  <Label htmlFor="io-policy">When a scrub and a pull meet</Label>
                  <Select id="io-policy" value={form.io_policy} onValueChange={set("io_policy")} options={IO_POLICIES} />
                </div>
              </CardBody>
            </Card>
            <Card>
              <CardHeader title="Watchdog" icon={<Radio />} subtitle="From outside the cluster: whether any host answers, and whether these URLs do" />
              <CardBody className="grid gap-3 lg:grid-cols-2">
                <div>
                  <Label htmlFor="down-minutes">Email after this many minutes without an answer</Label>
                  <Input id="down-minutes" value={form.down_after_minutes} onChange={(e) => set("down_after_minutes")(e.target.value)} inputMode="numeric" />
                </div>
                <div>
                  <Label htmlFor="urls">Public URLs to check, one per line</Label>
                  <Textarea id="urls" rows={4} value={form.urls} onChange={(e) => set("urls")(e.target.value)} spellCheck={false} />
                </div>
              </CardBody>
            </Card>
            <Card>
              <CardHeader title="Email" icon={<Mail />} subtitle="Sent through Postmark, with the same templates as the cluster's" />
              <CardBody className="space-y-3">
                <div className="grid gap-3 lg:grid-cols-2">
                  <div>
                    <Label htmlFor="from">From (a confirmed Postmark sender)</Label>
                    <Input id="from" value={form.from} onChange={(e) => set("from")(e.target.value)} spellCheck={false} />
                  </div>
                  <div>
                    <Label htmlFor="to">To (comma-separated)</Label>
                    <Input id="to" value={form.to} onChange={(e) => set("to")(e.target.value)} spellCheck={false} />
                  </div>
                </div>
                <div className="flex flex-wrap items-end gap-2">
                  <div className="min-w-[280px] flex-1">
                    <Label htmlFor="token">Postmark server token ({data.token_set ? "set; type a new one to replace it" : "not set"})</Label>
                    <Input id="token" type="password" value={token} onChange={(e) => setToken(e.target.value)} autoComplete="off" />
                  </div>
                  <Button disabled={!token || action.busy} onClick={() => void action.run(async () => (await api.setToken(token), setToken(""), await reload(), "The token is saved."))}>
                    Save token
                  </Button>
                  {data.token_set && (
                    <Button variant="danger-outline" disabled={action.busy} onClick={() => void action.run(async () => (await api.setToken(null), await reload(), "The token is removed."))}>
                      Remove token
                    </Button>
                  )}
                  <Button disabled={!data.token_set || action.busy} onClick={() => void action.run(async () => (await api.testEmail()).message)}>
                    Send a test email
                  </Button>
                </div>
              </CardBody>
            </Card>
            <Button variant="primary" disabled={action.busy} onClick={() => void save()}>
              Save settings
            </Button>
            <PasswordCard />
          </>
        )}
      </PageBody>
    </>
  );
}
