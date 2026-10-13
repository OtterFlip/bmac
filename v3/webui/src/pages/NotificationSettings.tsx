// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// Email notifications through Postmark: the server token (entered, never
// shown or kept here), sender, recipients, routes, throttle and digest,
// the email templates on the Postmark server, and a test email. Admins
// change them; everyone else sees them read-only.

import { useCallback, useEffect, useState } from "react";
import { AlertTriangle, KeyRound, Mail, Plus, RotateCcw, Save, Send, Trash2 } from "lucide-react";
import {
  Badge,
  Button,
  Card,
  CardHeader,
  Input,
  Label,
  Select,
  Switch,
  Textarea,
  ago,
  useNow,
  type NotifySettings,
  type NotifyView,
  type Route,
  type TemplatesView,
  type TestEmailResult,
} from "@bmac/ui";
import { ApiFailure, errorMessage } from "../lib/backend";
import { api } from "../state/store";
import { toast } from "../state/toast";
import { Table } from "./common";

const NO_DIGEST = "__none__";

function addresses(text: string): string[] {
  return text
    .split(/[\s,;]+/)
    .map((a) => a.trim())
    .filter(Boolean);
}

interface Draft {
  sender: string;
  recipients: string;
  throttle: string;
  digest: string;
  dashboard: string;
  disabled: string;
  routes: { categories: string; min_severity: Route["min_severity"]; recipients: string; recovery: boolean }[];
}

function toDraft(s: NotifySettings): Draft {
  return {
    sender: s.sender ?? "",
    recipients: s.recipients.join(", "),
    throttle: String(s.throttle_minutes),
    digest: s.digest_hour === null ? NO_DIGEST : String(s.digest_hour),
    dashboard: s.dashboard_url ?? "",
    disabled: s.disabled_conditions.join(", "),
    routes: s.routes.map((r) => ({ categories: r.categories.join(", "), min_severity: r.min_severity, recipients: r.recipients.join(", "), recovery: r.recovery })),
  };
}

function fromDraft(d: Draft): NotifySettings {
  return {
    sender: d.sender.trim() || null,
    recipients: addresses(d.recipients),
    routes: d.routes.map((r) => ({ categories: addresses(r.categories), min_severity: r.min_severity, recipients: addresses(r.recipients), recovery: r.recovery })),
    throttle_minutes: Math.max(0, Math.floor(Number(d.throttle) || 0)),
    digest_hour: d.digest === NO_DIGEST ? null : Number(d.digest),
    dashboard_url: d.dashboard.trim() || null,
    disabled_conditions: addresses(d.disabled),
  };
}

function TokenRow({ view, onChange }: { view: NotifyView; onChange: (v: NotifyView) => void }) {
  const [token, setToken] = useState("");
  const [busy, setBusy] = useState(false);
  const store = async (value: string) => {
    setBusy(true);
    try {
      onChange(await api().setPostmarkToken(value));
      setToken("");
      toast.success(value ? "Postmark token stored" : "Postmark token removed");
    } catch (e) {
      toast.error("Could not store the token", errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="border-b border-line/70 px-4 py-3">
      <div className="mb-2 flex flex-wrap items-center gap-2 text-[12.5px]">
        <KeyRound className="size-3.5 text-fg-subtle" />
        <span className="font-medium text-fg">Postmark server token</span>
        {view.token_set ? <Badge tone="ok">stored on the cluster</Badge> : <Badge tone="warn">not set</Badge>}
      </div>
      {view.can_edit && (
        <div className="flex flex-wrap items-center gap-2">
          <div className="w-[360px] max-w-full">
            <Input
              id="postmark-token"
              type="password"
              mono
              value={token}
              autoComplete="off"
              spellCheck={false}
              aria-label="Postmark server token"
              placeholder={view.token_set ? "Paste a new token to replace it" : "Paste the server's API token"}
              onChange={(e) => setToken(e.target.value)}
            />
          </div>
          <Button size="sm" variant="primary" disabled={busy || !token.trim()} onClick={() => void store(token.trim())}>
            <Save /> Store token
          </Button>
          {view.token_set && (
            <Button size="sm" variant="ghost" disabled={busy} onClick={() => void store("")}>
              <Trash2 /> Remove
            </Button>
          )}
        </div>
      )}
      <p className="mt-2 max-w-[760px] text-[11.5px] leading-relaxed text-fg-subtle">
        The token of one Postmark server, from its API Tokens tab. It is kept in the cluster's private configuration, readable only by root, and is never shown again here or sent back to any browser.
      </p>
    </div>
  );
}

function RoutesEditor({ draft, setDraft, readOnly }: { draft: Draft; setDraft: (d: Draft) => void; readOnly: boolean }) {
  const update = (i: number, change: Partial<Draft["routes"][number]>) => setDraft({ ...draft, routes: draft.routes.map((r, j) => (j === i ? { ...r, ...change } : r)) });
  return (
    <div className="sm:col-span-2 xl:col-span-3">
      <div className="mb-1.5 text-[12.5px] font-medium text-fg">Routes</div>
      {draft.routes.length === 0 && <p className="text-[12px] text-fg-subtle">No routes: warnings and criticals of every kind go to the recipients above, with a message when each resolves.</p>}
      <div className="space-y-2">
        {draft.routes.map((r, i) => (
          <div key={i} className="grid items-end gap-2 rounded-lg border border-line bg-surface-2/50 p-2.5 md:grid-cols-[1fr_140px_1fr_auto_auto]">
            <div>
              <Label htmlFor={`route-${i}-categories`} className="text-[11.5px]">
                Categories
              </Label>
              <Input id={`route-${i}-categories`} mono value={r.categories} readOnly={readOnly} placeholder="every category" onChange={(e) => update(i, { categories: e.target.value })} />
            </div>
            <div>
              <Label htmlFor={`route-${i}-severity`} className="text-[11.5px]">
                At least
              </Label>
              <Select
                id={`route-${i}-severity`}
                value={r.min_severity}
                disabled={readOnly}
                onValueChange={(v) => update(i, { min_severity: v as Route["min_severity"] })}
                options={[
                  { value: "warning", label: "warning" },
                  { value: "critical", label: "critical" },
                ]}
              />
            </div>
            <div>
              <Label htmlFor={`route-${i}-recipients`} className="text-[11.5px]">
                Recipients
              </Label>
              <Input id={`route-${i}-recipients`} value={r.recipients} readOnly={readOnly} placeholder="the default recipients" onChange={(e) => update(i, { recipients: e.target.value })} />
            </div>
            <label className="flex h-8 items-center gap-2 text-[12px] text-fg-muted">
              <Switch checked={r.recovery} disabled={readOnly} onCheckedChange={(v) => update(i, { recovery: v })} /> Resolved too
            </label>
            {!readOnly && (
              <Button size="sm" variant="ghost" aria-label={`Remove route ${i + 1}`} onClick={() => setDraft({ ...draft, routes: draft.routes.filter((_, j) => j !== i) })}>
                <Trash2 />
              </Button>
            )}
          </div>
        ))}
      </div>
      {!readOnly && (
        <Button size="sm" variant="ghost" className="-ml-2 mt-1.5" onClick={() => setDraft({ ...draft, routes: [...draft.routes, { categories: "", min_severity: "critical", recipients: "", recovery: true }] })}>
          <Plus /> Add a route
        </Button>
      )}
      <p className="mt-1 text-[11.5px] text-fg-subtle">
        A category is the first part of a condition, such as <span className="font-mono">zfs</span>, <span className="font-mono">disk</span>, or <span className="font-mono">corosync</span>. An alert goes to every route it matches.
      </p>
    </div>
  );
}

function SettingsForm({ view, onChange, reload }: { view: NotifyView; onChange: (v: NotifyView) => void; reload: () => Promise<void> }) {
  const [draft, setDraft] = useState(() => toDraft(view.settings));
  const [error, setError] = useState<string | null>(null);
  const [conflict, setConflict] = useState(false);
  const [saving, setSaving] = useState(false);

  const saved = `${view.settings_revision}:${JSON.stringify(view.settings)}`;
  useEffect(() => {
    setDraft(toDraft(view.settings));
    setError(null);
    setConflict(false);
  }, [saved]);

  const readOnly = !view.can_edit;
  const dirty = JSON.stringify(fromDraft(draft)) !== JSON.stringify(view.settings);
  const save = async () => {
    setSaving(true);
    setError(null);
    try {
      onChange(await api().saveNotifySettings({ settings: fromDraft(draft), expected_revision: view.settings_revision }));
      toast.success("Notification settings saved");
    } catch (e) {
      if (e instanceof ApiFailure && e.code === "conflict") setConflict(true);
      else setError(errorMessage(e));
    } finally {
      setSaving(false);
    }
  };

  return (
    <form
      aria-label="Notification settings"
      onSubmit={(e) => {
        e.preventDefault();
        if (!readOnly && dirty && !saving) void save();
      }}
    >
      {conflict && (
        <div role="alert" className="flex flex-wrap items-center gap-3 border-b border-warn/25 bg-warn/[0.07] px-4 py-3 text-[12.5px] text-fg">
          <AlertTriangle className="size-4 shrink-0 text-warn" />
          <span className="min-w-0 flex-1">Someone saved these settings after you opened them. Reload to see their version; your unsaved changes here will be lost.</span>
          <Button size="sm" variant="primary" onClick={() => void reload()}>
            <RotateCcw /> Reload
          </Button>
        </div>
      )}
      {error && (
        <div role="alert" className="border-b border-danger/25 bg-danger/[0.06] px-4 py-2.5 text-[12.5px] font-medium text-danger">
          {error}
        </div>
      )}
      <div className="grid gap-x-5 gap-y-4 p-4 sm:grid-cols-2 xl:grid-cols-3">
        <div>
          <Label htmlFor="notify-sender">Sender</Label>
          <Input id="notify-sender" value={draft.sender} readOnly={readOnly} autoComplete="off" placeholder="bmac@example.com" aria-describedby="notify-sender-help" onChange={(e) => setDraft({ ...draft, sender: e.target.value })} />
          <p id="notify-sender-help" className="mt-1 text-[11.5px] text-fg-subtle">
            A confirmed Sender Signature, or an address on a verified domain, of your Postmark account.
          </p>
        </div>
        <div className="xl:col-span-2">
          <Label htmlFor="notify-recipients">Recipients</Label>
          <Textarea id="notify-recipients" rows={2} value={draft.recipients} readOnly={readOnly} placeholder="ops@example.com, oncall@example.com" onChange={(e) => setDraft({ ...draft, recipients: e.target.value })} />
        </div>
        <div>
          <Label htmlFor="notify-throttle">Repeat at most every (minutes)</Label>
          <Input id="notify-throttle" inputMode="numeric" value={draft.throttle} readOnly={readOnly} aria-describedby="notify-throttle-help" onChange={(e) => setDraft({ ...draft, throttle: e.target.value })} />
          <p id="notify-throttle-help" className="mt-1 text-[11.5px] text-fg-subtle">
            An alert that clears and comes back within this time is not emailed again, unless it got worse.
          </p>
        </div>
        <div>
          <Label htmlFor="notify-digest">Daily summary</Label>
          <Select
            id="notify-digest"
            value={draft.digest}
            disabled={readOnly}
            onValueChange={(v) => setDraft({ ...draft, digest: v })}
            options={[{ value: NO_DIGEST, label: "None" }, ...Array.from({ length: 24 }, (_, h) => ({ value: String(h), label: `At ${String(h).padStart(2, "0")}:00, cluster time` }))]}
          />
        </div>
        <div>
          <Label htmlFor="notify-dashboard">Dashboard address in emails</Label>
          <Input id="notify-dashboard" mono value={draft.dashboard} readOnly={readOnly} spellCheck={false} autoComplete="off" placeholder="https://mox1.your-tailnet.ts.net:8443/" onChange={(e) => setDraft({ ...draft, dashboard: e.target.value })} />
        </div>
        <div className="xl:col-span-2">
          <Label htmlFor="notify-disabled">Switched-off alerts</Label>
          <Input id="notify-disabled" mono value={draft.disabled} readOnly={readOnly} spellCheck={false} autoComplete="off" aria-describedby="notify-disabled-help" placeholder="backup.skipped, disk." onChange={(e) => setDraft({ ...draft, disabled: e.target.value })} />
          <p id="notify-disabled-help" className="mt-1 text-[11.5px] text-fg-subtle">
            Conditions that open no alert and send no email, by their name on the Alerts page, or a whole kind ending in a dot. They still show on their resource&apos;s page.
          </p>
        </div>
        <RoutesEditor draft={draft} setDraft={setDraft} readOnly={readOnly} />
      </div>
      {!readOnly && (
        <div className="flex items-center justify-end gap-2 border-t border-line/70 px-4 py-3">
          <Button size="sm" variant="ghost" disabled={!dirty || saving} onClick={() => setDraft(toDraft(view.settings))}>
            Discard changes
          </Button>
          <Button size="sm" variant="primary" type="submit" disabled={!dirty || saving || conflict}>
            <Save /> {saving ? "Saving…" : "Save"}
          </Button>
        </div>
      )}
    </form>
  );
}

function Templates({ onCreated }: { onCreated: () => void }) {
  const [view, setView] = useState<TemplatesView | null>(null);
  const [busy, setBusy] = useState(false);
  const run = async (what: string, call: () => Promise<TemplatesView>) => {
    setBusy(true);
    try {
      setView(await call());
      return true;
    } catch (e) {
      toast.error(`Could not ${what}`, errorMessage(e));
      return false;
    } finally {
      setBusy(false);
    }
  };
  const missing = view?.templates.filter((t) => t.presence === "missing").length ?? 0;
  const differs = view?.templates.filter((t) => t.presence === "differs").length ?? 0;
  return (
    <div className="border-t border-line/70 px-4 py-3">
      <div className="flex flex-wrap items-center gap-2">
        <span className="text-[12.5px] font-medium text-fg">Email templates</span>
        <Button size="sm" variant="ghost" disabled={busy} onClick={() => void run("check the templates", () => api().emailTemplates())}>
          Check the Postmark server
        </Button>
        {missing > 0 && (
          <Button
            size="sm"
            variant="primary"
            disabled={busy}
            onClick={() =>
              void run("create the templates", () => api().createEmailTemplates()).then((ok) => {
                if (ok) {
                  toast.success("Templates created");
                  onCreated();
                }
              })
            }
          >
            <Plus /> Create {missing} missing
          </Button>
        )}
      </div>
      {view && (
        <>
          <p className="mt-2 text-[12px] text-fg-muted">
            Server <span className="font-mono">{view.server_name}</span>, delivery {view.delivery_type}.{" "}
            {view.delivery_type.toLowerCase() === "sandbox" && <span className="text-warn">A sandbox server accepts email and delivers none.</span>}
          </p>
          <Table label="Email templates" head={["Alias", "On the server"]} className="mt-1">
            {view.templates.map((t) => (
              <tr key={t.alias}>
                <td className="font-mono text-[12px]">{t.alias}</td>
                <td>
                  <Badge tone={t.presence === "present" ? "ok" : t.presence === "missing" ? "warn" : "danger"}>{t.presence}</Badge>
                </td>
              </tr>
            ))}
          </Table>
          {differs > 0 && (
            <p className="mt-2 text-[12px] text-danger">
              A template on the server differs from the one this release sends with. Someone edited it in Postmark; BMAC never overwrites it. Restore it there, or delete it so BMAC can create it again.
            </p>
          )}
        </>
      )}
    </div>
  );
}

function TestEmail({ view }: { view: NotifyView }) {
  const [to, setTo] = useState("");
  const [busy, setBusy] = useState(false);
  const [results, setResults] = useState<TestEmailResult[] | null>(null);
  const send = async () => {
    setBusy(true);
    try {
      setResults(await api().sendTestEmail(to.trim() || null));
    } catch (e) {
      toast.error("Could not send the test email", errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="border-t border-line/70 px-4 py-3">
      <div className="flex flex-wrap items-center gap-2">
        <span className="text-[12.5px] font-medium text-fg">Test email</span>
        <div className="w-[280px] max-w-full">
          <Input value={to} aria-label="Send the test email to" autoComplete="off" placeholder={view.settings.recipients.length ? "the recipients above" : "an address"} onChange={(e) => setTo(e.target.value)} />
        </div>
        <Button size="sm" disabled={busy || !view.token_set} onClick={() => void send()}>
          <Send /> Send
        </Button>
      </div>
      {results && (
        <ul aria-label="Test email results" className="mt-2 space-y-0.5 text-[12px]">
          {results.map((r) => (
            <li key={r.recipient} className={r.error ? "text-danger" : "text-ok"}>
              <span className="font-mono">{r.recipient}</span>: {r.error ?? "accepted by Postmark"}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

export function NotificationSettings() {
  const now = useNow(30000);
  const [view, setView] = useState<NotifyView | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      setView(await api().alerts());
      setError(null);
    } catch (e) {
      setError(errorMessage(e));
    }
  }, []);

  useEffect(() => void load(), [load]);

  return (
    <Card className="mb-5" role="region" aria-label="Email notifications">
      <CardHeader icon={<Mail />} title="Email notifications" subtitle="Alerts are emailed through Postmark, with templates BMAC creates on your Postmark server" />
      {error && (
        <p role="alert" className="px-4 py-3 text-[12.5px] text-danger">
          Notification settings aren't available: {error}
        </p>
      )}
      {view && (
        <>
          <div className="flex flex-wrap items-center gap-2 border-b border-line/70 px-4 py-2.5 text-[12px] text-fg-subtle">
            <span>
              {view.settings_revision === null ? "Not saved yet" : `Revision ${view.settings_revision}`}
              {view.outbox.last_sent_ms ? ` · last email ${ago(view.outbox.last_sent_ms, now)}` : ""}
              {view.outbox.pending ? ` · ${view.outbox.pending} waiting to send` : ""}
            </span>
            {!view.can_edit && <Badge className="ml-auto">read only · changing notifications needs the admin role</Badge>}
          </div>
          <p className="max-w-[820px] px-4 pt-3 text-[12.5px] leading-relaxed text-fg-muted">
            Create a server in your Postmark account, confirm a sender address or domain, then store the server's token here and create the templates. The BMAC README explains each step, under "Email alerts with Postmark".
          </p>
          <TokenRow view={view} onChange={setView} />
          <SettingsForm view={view} onChange={setView} reload={load} />
          {view.can_edit && view.token_set && <Templates onCreated={() => void load()} />}
          {view.can_edit && <TestEmail view={view} />}
        </>
      )}
    </Card>
  );
}
