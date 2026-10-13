// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// Open and recently resolved alerts, acknowledging them, silences, and the
// state of email delivery. Operators and admins acknowledge and silence;
// everyone can look.

import { useCallback, useEffect, useState } from "react";
import { BellOff, BellRing, Check, History, MailWarning, Plus, Trash2, XCircle } from "lucide-react";
import {
  Badge,
  Button,
  Card,
  CardHeader,
  Dialog,
  EmptyState,
  Input,
  Label,
  NextSteps,
  RefreshControl,
  Select,
  ago,
  dateTimeMs,
  useNow,
  type Alert,
  type NotifyView,
  type SaveSilence,
} from "@bmac/ui";
import { PageBody, PageHeader } from "../components/layout/Page";
import { errorMessage } from "../lib/backend";
import { api, useStore } from "../state/store";
import { toast } from "../state/toast";
import { SectionTitle, Table } from "./common";

const POLL_MS = 30_000;
const HOUR_MS = 3_600_000;
const ANY = "__any__";

const DURATIONS = [
  { value: String(HOUR_MS), label: "1 hour" },
  { value: String(4 * HOUR_MS), label: "4 hours" },
  { value: String(24 * HOUR_MS), label: "1 day" },
  { value: String(7 * 24 * HOUR_MS), label: "7 days" },
  { value: String(30 * 24 * HOUR_MS), label: "30 days" },
];

function SeverityBadge({ severity }: { severity: Alert["severity"] }) {
  return <Badge tone={severity === "critical" ? "danger" : severity === "warning" ? "warn" : "info"}>{severity}</Badge>;
}

function matches(silence: NotifyView["silences"][number], alert: Alert) {
  const condition = silence.condition;
  return (
    (!condition || (condition.endsWith(".") ? alert.condition.startsWith(condition) : alert.condition === condition)) &&
    (!silence.resource || silence.resource === alert.resource) &&
    (!silence.host || silence.host === alert.host)
  );
}

function SilenceDialog({ draft, onClose, onSaved }: { draft: SaveSilence | null; onClose: () => void; onSaved: () => void }) {
  const [condition, setCondition] = useState("");
  const [resource, setResource] = useState("");
  const [host, setHost] = useState(ANY);
  const [duration, setDuration] = useState(DURATIONS[1].value);
  const [reason, setReason] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    if (!draft) return;
    setCondition(draft.condition ?? "");
    setResource(draft.resource ?? "");
    setHost(draft.host ?? ANY);
    setReason("");
    setError(null);
  }, [draft]);

  const save = async () => {
    setSaving(true);
    setError(null);
    try {
      await api().saveSilence({
        condition: condition.trim() || null,
        resource: resource.trim() || null,
        host: host === ANY ? null : (host as SaveSilence["host"]),
        until_ms: Date.now() + Number(duration),
        reason: reason.trim(),
      });
      toast.success("Silence saved", "Matching alerts still show here; they are not emailed until it ends.");
      onSaved();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setSaving(false);
    }
  };

  const everything = !condition.trim() && !resource.trim() && host === ANY;
  return (
    <Dialog
      open={draft !== null}
      onOpenChange={(open) => !open && onClose()}
      icon={<BellOff />}
      title="Silence notifications"
      description="Alerts that match still open and show here, but no email is sent for them until the silence ends."
      footer={
        <>
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button variant="primary" disabled={saving || !reason.trim()} onClick={() => void save()}>
            {saving ? "Saving…" : "Silence"}
          </Button>
        </>
      }
    >
      <div className="grid gap-4 sm:grid-cols-2">
        <div className="sm:col-span-2">
          <Label htmlFor="silence-condition">Condition</Label>
          <Input id="silence-condition" mono value={condition} spellCheck={false} autoComplete="off" placeholder="any" aria-describedby="silence-condition-help" onChange={(e) => setCondition(e.target.value)} />
          <p id="silence-condition-help" className="mt-1 text-[11.5px] text-fg-subtle">
            Such as <span className="font-mono">zfs.pool.capacity</span>; end with a dot (<span className="font-mono">zfs.</span>) for every condition that starts that way.
          </p>
        </div>
        <div>
          <Label htmlFor="silence-resource">Resource</Label>
          <Input id="silence-resource" mono value={resource} spellCheck={false} autoComplete="off" placeholder="any" onChange={(e) => setResource(e.target.value)} />
        </div>
        <div>
          <Label htmlFor="silence-host">Host</Label>
          <Select
            id="silence-host"
            value={host}
            onValueChange={setHost}
            options={[{ value: ANY, label: "Any host" }, ...Array.from({ length: 10 }, (_, i) => ({ value: `mox${i + 1}`, label: `mox${i + 1}` }))]}
          />
        </div>
        <div>
          <Label htmlFor="silence-duration">For</Label>
          <Select id="silence-duration" value={duration} onValueChange={setDuration} options={DURATIONS} />
        </div>
        <div>
          <Label htmlFor="silence-reason" required>
            Reason
          </Label>
          <Input id="silence-reason" value={reason} maxLength={200} placeholder="Replacing a disk on mox2" onChange={(e) => setReason(e.target.value)} />
        </div>
      </div>
      {everything && (
        <p role="note" className="mt-3 text-[12.5px] font-medium text-warn">
          With no condition, resource, or host, this silences every alert in the cluster.
        </p>
      )}
      {error && (
        <p role="alert" className="mt-3 text-[12.5px] font-medium text-danger">
          {error}
        </p>
      )}
    </Dialog>
  );
}

function OpenAlerts({ view, reload, silence }: { view: NotifyView; reload: () => Promise<void>; silence: (draft: SaveSilence) => void }) {
  const now = useNow(15000);
  const [busy, setBusy] = useState<string | null>(null);
  const acknowledge = async (alert: Alert) => {
    setBusy(alert.id);
    try {
      await api().acknowledgeAlert(alert.id);
      await reload();
    } catch (e) {
      toast.error("Could not acknowledge the alert", errorMessage(e));
    } finally {
      setBusy(null);
    }
  };
  const open = view.alerts.filter((a) => a.state !== "resolved");
  const resolved = view.alerts.filter((a) => a.state === "resolved");
  if (!open.length && !resolved.length)
    return (
      <Card className="mb-5">
        <EmptyState icon={<Check />} title="No alerts" body="Nothing has stayed at warning or critical long enough to raise an alert." />
      </Card>
    );
  return (
    <Card className="mb-5">
      <ul aria-label="Alerts" className="divide-y divide-line/70">
        {[...open, ...resolved].map((alert) => {
          const silenced = view.silences.some((s) => s.until_ms > now && matches(s, alert));
          return (
            <li key={alert.id} className="px-4 py-3">
              <div className="flex flex-wrap items-start gap-3">
                <SeverityBadge severity={alert.severity} />
                <div className="min-w-0 flex-1">
                  <div className="selectable text-[13px] leading-snug text-fg">{alert.message}</div>
                  <div className="mt-1 flex flex-wrap gap-x-3 gap-y-1 text-[11.5px] text-fg-subtle">
                    <span className="font-mono">{alert.condition}</span>
                    <span className="font-mono">{alert.resource}</span>
                    <span title={dateTimeMs(alert.opened_ms)}>opened {ago(alert.opened_ms, now)}</span>
                    {alert.state === "acknowledged" && alert.acknowledged_by && (
                      <span>
                        acknowledged by <span className="font-mono">{alert.acknowledged_by}</span>
                        {alert.acknowledged_ms ? ` ${ago(alert.acknowledged_ms, now)}` : ""}
                      </span>
                    )}
                    {alert.state === "resolved" && alert.resolved_ms && <span className="text-ok">resolved {ago(alert.resolved_ms, now)}</span>}
                    {silenced && <span className="text-warn">silenced</span>}
                  </div>
                </div>
                {view.can_acknowledge && alert.state !== "resolved" && (
                  <div className="flex shrink-0 gap-2">
                    {alert.state === "open" && (
                      <Button size="sm" disabled={busy === alert.id} onClick={() => void acknowledge(alert)}>
                        <Check /> Acknowledge
                      </Button>
                    )}
                    <Button size="sm" variant="ghost" onClick={() => silence({ condition: alert.condition, resource: alert.resource, host: alert.host, until_ms: 0, reason: "" })}>
                      <BellOff /> Silence
                    </Button>
                  </div>
                )}
              </div>
              {alert.next_step && alert.state !== "resolved" && (
                <div className="mt-2.5">
                  <NextSteps steps={[alert.next_step]} compact />
                </div>
              )}
            </li>
          );
        })}
      </ul>
    </Card>
  );
}

function Silences({ view, reload, create }: { view: NotifyView; reload: () => Promise<void>; create: () => void }) {
  const now = useNow(30000);
  const remove = async (id: string) => {
    try {
      await api().deleteSilence(id);
      await reload();
    } catch (e) {
      toast.error("Could not remove the silence", errorMessage(e));
    }
  };
  const active = view.silences.filter((s) => s.until_ms > now);
  return (
    <Card className="mb-5">
      <CardHeader
        icon={<BellOff />}
        title="Silences"
        subtitle="Alerts that match still show here, but are not emailed"
        actions={
          view.can_acknowledge && (
            <Button size="sm" onClick={create}>
              <Plus /> New silence
            </Button>
          )
        }
      />
      {active.length ? (
        <Table label="Silences" head={["Condition", "Resource", "Host", "Until", "Reason", "By", ""]}>
          {active.map((s) => (
            <tr key={s.id}>
              <td className="font-mono text-[12px]">{s.condition ?? "any"}</td>
              <td className="font-mono text-[12px]">{s.resource ?? "any"}</td>
              <td className="font-mono text-[12px]">{s.host ?? "any"}</td>
              <td className="whitespace-nowrap text-fg-muted">{dateTimeMs(s.until_ms)}</td>
              <td className="text-fg-muted">{s.reason}</td>
              <td className="font-mono text-[12px] text-fg-subtle">{s.created_by}</td>
              <td className="text-right">
                {view.can_acknowledge && (
                  <Button size="sm" variant="ghost" aria-label={`Remove the silence: ${s.reason}`} onClick={() => void remove(s.id)}>
                    <Trash2 />
                  </Button>
                )}
              </td>
            </tr>
          ))}
        </Table>
      ) : (
        <p className="px-4 py-4 text-[12.5px] text-fg-subtle">No silences. Add one before planned work, so the alerts it causes aren't emailed.</p>
      )}
    </Card>
  );
}

function HistoryCard({ history }: { history: Alert[] }) {
  if (!history.length) return null;
  return (
    <Card className="mb-5">
      <CardHeader icon={<History />} title="History" subtitle="The newest resolved alerts" />
      <Table label="Alert history" head={["Severity", "What", "Resource", "Opened", "Resolved"]}>
        {history.slice(0, 50).map((a) => (
          <tr key={`${a.id}-${a.generation}-${a.resolved_ms}`}>
            <td>
              <SeverityBadge severity={a.severity} />
            </td>
            <td className="selectable text-fg-muted">{a.message}</td>
            <td className="font-mono text-[12px]">{a.resource}</td>
            <td className="whitespace-nowrap text-fg-subtle">{dateTimeMs(a.opened_ms)}</td>
            <td className="whitespace-nowrap text-fg-subtle">{a.resolved_ms ? dateTimeMs(a.resolved_ms) : ""}</td>
          </tr>
        ))}
      </Table>
    </Card>
  );
}

function Delivery({ view }: { view: NotifyView }) {
  const setPage = useStore((s) => s.setPage);
  const { outbox, settings, token_set } = view;
  const ready = token_set && !!settings.sender && settings.recipients.length > 0;
  if (ready && !outbox.problems.length && !outbox.held) return null;
  return (
    <section aria-label="Email delivery" className="mb-5 flex items-start gap-3 rounded-xl border border-warn/30 bg-gradient-to-br from-warn/[0.08] to-transparent px-4 py-3">
      <MailWarning className="mt-0.5 size-4 shrink-0 text-warn" />
      <div className="min-w-0 flex-1 text-[12.5px] leading-relaxed text-fg">
        {!ready ? (
          <p>
            <span className="font-semibold">Alerts are not emailed yet.</span>{" "}
            <span className="text-fg-muted">
              {view.can_edit ? "Set a Postmark server token, a sender, and recipients in Settings, under Notifications." : "An administrator sets up email in Settings, under Notifications."}
            </span>
          </p>
        ) : outbox.held ? (
          <p>
            <span className="font-semibold">{outbox.held} emails are waiting for their templates.</span>{" "}
            <span className="text-fg-muted">An administrator creates the missing templates in Settings, under Notifications.</span>
          </p>
        ) : null}
        {outbox.problems.length > 0 && (
          <ul className="mt-1 space-y-0.5 text-fg-muted">
            {outbox.problems.slice(0, 5).map((p) => (
              <li key={`${p.recipient}-${p.template}-${p.at_ms}`}>
                <span className="font-mono text-[12px]">{p.recipient}</span>: {p.error}
              </li>
            ))}
          </ul>
        )}
        {view.can_edit && (
          <Button size="sm" variant="ghost" className="-ml-2 mt-1" onClick={() => setPage("settings")}>
            Open Settings
          </Button>
        )}
      </div>
    </section>
  );
}

export function AlertsPage() {
  const [view, setView] = useState<NotifyView | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [updatedAt, setUpdatedAt] = useState<number | null>(null);
  const [silence, setSilence] = useState<SaveSilence | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      setView(await api().alerts());
      setError(null);
      setUpdatedAt(Date.now());
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
    const timer = setInterval(() => void load(), POLL_MS);
    return () => clearInterval(timer);
  }, [load]);

  const openCount = view?.alerts.filter((a) => a.state !== "resolved").length ?? 0;
  return (
    <>
      <PageHeader
        title="Alerts"
        subtitle={view ? (openCount ? `${openCount} open` : "Nothing open") : "Conditions that stayed at warning or critical"}
        actions={<RefreshControl updatedAt={updatedAt} loading={loading} error={error} onRefresh={() => void load()} label="Refresh" />}
      />
      <PageBody>
        {!view ? (
          loading ? (
            <div className="space-y-3" aria-busy="true">
              <div className="skeleton h-4 w-1/3" />
              <div className="skeleton h-4 w-full" />
            </div>
          ) : (
            <div role="alert" className="flex items-center gap-2 rounded-xl border border-danger/30 bg-danger/[0.06] px-4 py-3 text-[12.5px] text-danger">
              <XCircle className="size-4" /> Alerts aren't available: {error}
            </div>
          )
        ) : (
          <>
            {view.local && (
              <section aria-label="No quorum" className="mb-5 rounded-xl border border-warn/30 bg-warn/[0.06] px-4 py-3 text-[12.5px] leading-relaxed text-fg">
                <span className="font-semibold">This host has lost quorum.</span>{" "}
                <span className="text-fg-muted">It shows and emails its own alerts, and merges them back into the cluster's when quorum returns.</span>
              </section>
            )}
            <Delivery view={view} />
            <SectionTitle className="flex items-center gap-1.5">
              <BellRing className="size-3.5" /> Alerts
            </SectionTitle>
            <OpenAlerts view={view} reload={load} silence={setSilence} />
            <Silences view={view} reload={load} create={() => setSilence({ condition: null, resource: null, host: null, until_ms: 0, reason: "" })} />
            <HistoryCard history={view.history} />
          </>
        )}
      </PageBody>
      <SilenceDialog
        draft={silence}
        onClose={() => setSilence(null)}
        onSaved={() => {
          setSilence(null);
          void load();
        }}
      />
    </>
  );
}
