// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// Cluster settings and host facts, edited with revision checks (admins
// only), every host's certificate and SSH host key fingerprints, and
// rotating the certificate of the host serving this page.

import { useCallback, useEffect, useMemo, useState } from "react";
import { AlertTriangle, Copy, FileCog, KeyRound, RefreshCw, RotateCcw, Save, Server, ShieldCheck } from "lucide-react";
import {
  Badge,
  Button,
  Card,
  CardHeader,
  Dialog,
  Input,
  Label,
  RefreshControl,
  Select,
  ago,
  cn,
  dateTimeMs,
  useNow,
  type CertificateInfo,
  type CertificateView,
  type HostIdentity,
  type SettingKey,
  type SettingsRecord,
  type SettingsView,
} from "@bmac/ui";
import { PageBody, PageHeader } from "../components/layout/Page";
import { ApiFailure, errorMessage } from "../lib/backend";
import { api } from "../state/store";
import { toast } from "../state/toast";
import { SectionTitle, Table } from "./common";
import { LogReplication } from "./LogReplication";
import { NotificationSettings } from "./NotificationSettings";

const KIND_HELP: Record<string, string> = {
  positive_integer: "A whole number above 0",
  host_count: "1 to 32",
  prefix_length: "1 to 32",
  ipv4: "An IPv4 address, such as 203.0.113.10",
  private_address: "An address on the private subnet",
  private_interface: "An address on the private subnet, with the subnet's prefix",
  private_subnet: "An IPv4 network from /16 to /24, such as 10.213.0.0/24",
  network: "An IPv4 network, such as 10.213.0.240/28",
  internal_domain: "A private domain ending in .internal",
  host: "mox1 to mox32",
  interface: "A network interface name, up to 15 characters",
  sha256: "64 hexadecimal digits",
  iso_url: "An https:// address ending in .iso",
  ssh_public_key: "One SSH public key line",
  mac: "A MAC address, such as aa:bb:cc:dd:ee:ff",
  mac_or_auto: "A MAC address, or auto",
  block_sizes: "Some of 16K, 32K, 64K, and 128K, comma-separated; all four when unset",
};

type Values = Record<string, string>;

/** Host slots run from mox1 to mox32. */
const HOST_SLOTS = 32;

const NOT_SET = "__not_set__";

function copy(text: string, what: string) {
  void navigator.clipboard.writeText(text).then(
    () => toast.success(`Copied the ${what}`),
    () => toast.error("Could not copy to the clipboard"),
  );
}

function Field({ setting, value, error, readOnly, locked, onChange }: { setting: SettingKey; value: string; error?: string; readOnly: boolean; locked: boolean; onChange: (v: string) => void }) {
  const id = `setting-${setting.name}`;
  const kindHelp = setting.kind === "choice" ? undefined : KIND_HELP[setting.kind];
  const help = locked ? "Part of the cluster's address plan, fixed once the cluster exists" : kindHelp;
  return (
    <div>
      <Label htmlFor={id} required={setting.required} className="font-mono text-[12px]">
        {setting.name}
      </Label>
      {setting.kind === "choice" ? (
        <Select
          id={id}
          value={value || NOT_SET}
          disabled={readOnly}
          invalid={!!error}
          describedBy={error ? `${id}-error` : undefined}
          onValueChange={(v) => onChange(v === NOT_SET ? "" : v)}
          options={[...(setting.required ? [] : [{ value: NOT_SET, label: "Not set" }]), ...setting.choices.map((c) => ({ value: c, label: c }))]}
        />
      ) : (
        <Input
          id={id}
          mono
          value={value}
          readOnly={readOnly}
          spellCheck={false}
          autoComplete="off"
          aria-invalid={error ? true : undefined}
          aria-describedby={error ? `${id}-error` : help ? `${id}-help` : undefined}
          placeholder={readOnly ? "not set" : undefined}
          onChange={(e) => onChange(e.target.value)}
        />
      )}
      {error ? (
        <p id={`${id}-error`} className="mt-1 text-[11.5px] font-medium text-danger">
          {error}
        </p>
      ) : (
        help && (
          <p id={`${id}-help`} className="mt-1 text-[11.5px] text-fg-subtle">
            {help}
          </p>
        )
      )}
    </div>
  );
}

/** One record's editor: required keys, then the optional ones that are set, with the rest behind a toggle. */
function SettingsForm({
  label,
  keys,
  record,
  canEdit,
  save,
  reload,
}: {
  label: string;
  keys: SettingKey[];
  record: SettingsRecord | null;
  canEdit: boolean;
  save: (values: Values, expected: number | null) => Promise<void>;
  reload: () => Promise<void>;
}) {
  const now = useNow(30000);
  const [draft, setDraft] = useState<Values>(record?.values ?? {});
  const [fieldErrors, setFieldErrors] = useState<Record<string, string>>({});
  const [error, setError] = useState<string | null>(null);
  const [conflict, setConflict] = useState(false);
  const [saving, setSaving] = useState(false);
  const [showAll, setShowAll] = useState(false);

  useEffect(() => {
    setDraft(record?.values ?? {});
    setFieldErrors({});
    setError(null);
    setConflict(false);
  }, [record]);

  const saved = record?.values ?? {};
  const dirty = keys.some((k) => (draft[k.name] ?? "").trim() !== (saved[k.name] ?? ""));
  const required = keys.filter((k) => k.required);
  const optional = keys.filter((k) => !k.required);
  const unset = optional.filter((k) => !saved[k.name] && !draft[k.name] && !fieldErrors[k.name]);
  const shown = showAll ? optional : optional.filter((k) => !unset.includes(k));

  const submit = async () => {
    setSaving(true);
    setError(null);
    setFieldErrors({});
    const values = Object.fromEntries(
      keys.flatMap((k) => {
        const v = (draft[k.name] ?? "").trim();
        return v ? [[k.name, v]] : [];
      }),
    );
    try {
      await save(values, record?.revision ?? null);
    } catch (e) {
      if (e instanceof ApiFailure && e.code === "conflict") setConflict(true);
      else if (e instanceof ApiFailure && e.code === "invalid" && Object.keys(e.fieldErrors).length) {
        setFieldErrors(e.fieldErrors);
        setError("Fix the settings marked below, then save again.");
      } else setError(errorMessage(e));
    } finally {
      setSaving(false);
    }
  };

  const field = (k: SettingKey) => (
    <Field
      key={k.name}
      setting={k}
      value={draft[k.name] ?? ""}
      error={fieldErrors[k.name]}
      readOnly={!canEdit || (k.locked && record !== null)}
      locked={k.locked && record !== null}
      onChange={(v) => setDraft((d) => ({ ...d, [k.name]: v }))}
    />
  );

  return (
    <form
      aria-label={label}
      onSubmit={(e) => {
        e.preventDefault();
        if (canEdit && dirty && !saving) void submit();
      }}
    >
      <div className="flex flex-wrap items-center gap-2 border-b border-line/70 px-4 py-2.5 text-[12px] text-fg-subtle">
        {record ? (
          <span title={new Date(record.updated_at_ms).toLocaleString()}>
            Revision {record.revision}, saved by <span className="font-mono">{record.updated_by}</span> {ago(record.updated_at_ms, now)}
          </span>
        ) : (
          <span>Not saved yet</span>
        )}
        {!canEdit && <Badge className="ml-auto">read only · changing settings needs the admin role</Badge>}
      </div>
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
      <div className="grid gap-x-5 gap-y-4 p-4 md:grid-cols-2 xl:grid-cols-3">
        {required.map(field)}
        {shown.map(field)}
      </div>
      {(unset.length > 0 || canEdit) && (
        <div className="flex flex-wrap items-center gap-2 border-t border-line/70 px-4 py-3">
          {unset.length > 0 && (
            <Button size="sm" variant="ghost" onClick={() => setShowAll(!showAll)} aria-expanded={showAll}>
              {showAll ? "Hide unset optional settings" : `Show ${unset.length} unset optional settings`}
            </Button>
          )}
          {canEdit && (
            <div className="ml-auto flex items-center gap-2">
              <Button size="sm" variant="ghost" disabled={!dirty || saving} onClick={() => (setDraft(saved), setFieldErrors({}), setError(null))}>
                Discard changes
              </Button>
              <Button size="sm" variant="primary" type="submit" disabled={!dirty || saving || conflict}>
                <Save /> {saving ? "Saving…" : "Save"}
              </Button>
            </div>
          )}
        </div>
      )}
    </form>
  );
}

function HostFacts({ view, reload, update }: { view: SettingsView; reload: () => Promise<void>; update: (change: (v: SettingsView | null) => SettingsView | null) => void }) {
  const max = Math.min(HOST_SLOTS, Math.max(1, Number(view.cluster?.values.MAX_MOX_HOSTS) || 10));
  const slots = useMemo(() => {
    const known = new Set([...view.hosts.map((h) => h.host), ...view.identities.map((i) => i.host)]);
    return Array.from({ length: HOST_SLOTS }, (_, i) => `mox${i + 1}`).filter((h, i) => i < max || known.has(h));
  }, [view, max]);
  const [host, setHost] = useState(() => view.hosts[0]?.host ?? "mox1");
  const facts = view.hosts.find((h) => h.host === host) ?? null;
  const record = useMemo(() => (facts ? { revision: facts.revision, values: facts.values, updated_by: facts.updated_by, updated_at_ms: facts.updated_at_ms } : null), [facts]);

  return (
    <Card className="mb-5">
      <CardHeader
        icon={<Server />}
        title="Host facts"
        subtitle="Each host's moxN.conf: its public address, network cards, guest limits, and NVMe mirrors"
        actions={
          <div className="w-[220px]">
            <Select
              id="facts-host"
              labelledBy="facts-host-label"
              value={host}
              onValueChange={setHost}
              options={slots.map((h) => {
                const f = view.hosts.find((x) => x.host === h);
                return { value: h, label: f ? `${h} · revision ${f.revision}` : `${h} · no facts yet` };
              })}
            />
            <span id="facts-host-label" className="sr-only">
              Host
            </span>
          </div>
        }
      />
      <SettingsForm
        key={host}
        label={`${host}'s facts`}
        keys={view.host_keys}
        record={record}
        canEdit={view.can_edit}
        reload={reload}
        save={async (values, expected) => {
          const saved = await api().saveHostFacts(host, { values, expected_revision: expected });
          update((v) => v && { ...v, hosts: [...v.hosts.filter((h) => h.host !== host), saved].sort((a, b) => a.host.localeCompare(b.host, undefined, { numeric: true })) });
          toast.success(`${host}'s facts saved`, `Revision ${saved.revision}`);
        }}
      />
    </Card>
  );
}

function Identities({ identities }: { identities: HostIdentity[] }) {
  const now = useNow(30000);
  return (
    <Card className="mb-5">
      <CardHeader icon={<KeyRound />} title="Certificates and SSH host keys" subtitle="What each host published about its own keys" />
      <p className="max-w-[760px] px-4 pt-3 text-[12.5px] leading-relaxed text-fg-muted">
        Before trusting a host, compare its Dashboard certificate fingerprint with the one your browser shows for that host (click the lock or site icon in the address bar, then view the certificate). Compare a host's SSH key fingerprint with what{" "}
        <span className="font-mono text-[12px]">ssh-keyscan</span> or the Deployer shows. If either differs, stop: something is between you and the host.
      </p>
      {identities.length ? (
        <Table label="Host keys" head={["Host", "Dashboard certificate (SHA-256)", "SSH host key", "Published"]} className="mt-2">
          {identities.map((i) => (
            <tr key={i.host}>
              <td className="font-mono font-medium text-fg">{i.host}</td>
              <td className="selectable max-w-[300px] font-mono text-[11.5px] break-all text-fg-muted">{i.https_fingerprint ?? "not published"}</td>
              <td className="max-w-[340px]">
                <div className="selectable font-mono text-[11.5px] break-all text-fg-muted">{i.ssh_host_key_fingerprint ?? "not published"}</div>
                {i.ssh_host_key && (
                  <Button size="sm" variant="ghost" className="mt-1 -ml-2" aria-label={`Copy ${i.host}'s known_hosts line`} onClick={() => copy(`${i.host} ${i.ssh_host_key}`, "known_hosts line")}>
                    <Copy /> known_hosts line
                  </Button>
                )}
              </td>
              <td className="whitespace-nowrap text-fg-subtle" title={new Date(i.updated_at_ms).toLocaleString()}>
                {ago(i.updated_at_ms, now)}
              </td>
            </tr>
          ))}
        </Table>
      ) : (
        <p className="px-4 py-4 text-[12.5px] text-fg-subtle">No host has published its keys yet. Each host's bmacd publishes them when it starts.</p>
      )}
    </Card>
  );
}

const DAY_MS = 86_400_000;
/** Below this, the expiry date is shown as a warning. */
const EXPIRY_WARNING_DAYS = 90;

function CertificateDetails({ info, now }: { info: CertificateInfo; now: number }) {
  const days = Math.floor((info.not_after_ms - now) / DAY_MS);
  return (
    <dl className="grid grid-cols-[max-content_1fr] gap-x-4 gap-y-1.5 text-[12.5px]">
      <dt className="text-fg-subtle">SHA-256</dt>
      <dd className="selectable font-mono text-[11.5px] break-all text-fg">{info.fingerprint}</dd>
      <dt className="text-fg-subtle">Names</dt>
      <dd className="font-mono text-[11.5px] text-fg-muted">{info.names.join(", ") || "none"}</dd>
      <dt className="text-fg-subtle">Valid until</dt>
      <dd className={cn("text-fg-muted", days < EXPIRY_WARNING_DAYS && "text-warn")}>
        {dateTimeMs(info.not_after_ms)} ({days < 0 ? "expired" : `${days} days left`})
      </dd>
    </dl>
  );
}

function Certificate() {
  const now = useNow(60000);
  const [view, setView] = useState<CertificateView | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [confirming, setConfirming] = useState(false);
  const [switched, setSwitched] = useState<CertificateInfo | null>(null);

  useEffect(() => {
    api()
      .certificate()
      .then(setView, (e) => setError(errorMessage(e)));
  }, []);

  const act = async (what: string, call: () => Promise<CertificateView>) => {
    setBusy(true);
    try {
      const next = await call();
      setView(next);
      return next;
    } catch (e) {
      toast.error(`Could not ${what}`, errorMessage(e));
      if (e instanceof ApiFailure && e.code === "conflict") setView(await api().certificate().catch(() => view));
      return null;
    } finally {
      setBusy(false);
    }
  };

  const doSwitch = async (pending: CertificateInfo) => {
    setConfirming(false);
    const next = await act("switch certificates", () => api().switchCertificate(pending.fingerprint));
    if (next) setSwitched(pending);
  };

  const pending = view?.pending ?? null;
  return (
    <Card className="mb-5">
      <CardHeader icon={<ShieldCheck />} title={view ? `${view.host}'s Dashboard certificate` : "This host's Dashboard certificate"} subtitle="The certificate this page is being served with. To rotate another host's, open that host's Dashboard." />
      <div className="space-y-4 px-4 py-3">
        {error && (
          <p role="alert" className="text-[12.5px] text-danger">
            Certificate details aren't available: {error}
          </p>
        )}
        {view && !switched && (view.current ? <CertificateDetails info={view.current} now={now} /> : <p className="text-[12.5px] text-fg-subtle">This host's certificate can't be read.</p>)}
        {switched ? (
          <section role="status" aria-label="Certificate switched" className="rounded-lg border border-warn/30 bg-warn/[0.06] px-4 py-3 text-[12.5px] leading-relaxed text-fg">
            <p className="font-semibold">{view?.host} now uses the new certificate. Its Dashboard restarts in a moment.</p>
            <p className="mt-1 text-fg-muted">
              Reload the page. Your browser will warn about the certificate again: open its certificate viewer and compare the SHA-256 fingerprint with the one below. Continue only if they are identical, then log in again.
            </p>
            <p className="selectable mt-2 font-mono text-[12px] break-all text-fg">{switched.fingerprint}</p>
            <div className="mt-3 flex gap-2">
              <Button size="sm" variant="ghost" aria-label="Copy the new fingerprint" onClick={() => copy(switched.fingerprint, "new fingerprint")}>
                <Copy /> Copy
              </Button>
              <Button size="sm" variant="primary" onClick={() => window.location.reload()}>
                <RefreshCw /> Reload the page
              </Button>
            </div>
          </section>
        ) : pending ? (
          <section aria-label="New certificate" className="rounded-lg border border-accent/30 bg-accent/[0.05] px-4 py-3">
            <h3 className="mb-2 text-[13px] font-semibold text-fg">New certificate, not in use yet</h3>
            <CertificateDetails info={pending} now={now} />
            <p className="mt-3 max-w-[760px] text-[12.5px] leading-relaxed text-fg-muted">
              You are seeing this fingerprint over the current certificate, which you already verified. Copy it or write it down: after the switch your browser warns about the certificate again, and you compare the fingerprint it shows with this one before logging in again.
            </p>
            {view?.can_rotate ? (
              <div className="mt-3 flex flex-wrap gap-2">
                <Button size="sm" variant="ghost" aria-label="Copy the new fingerprint" onClick={() => copy(pending.fingerprint, "new fingerprint")}>
                  <Copy /> Copy
                </Button>
                <Button size="sm" variant="primary" disabled={busy} onClick={() => setConfirming(true)}>
                  Switch to the new certificate
                </Button>
                <Button size="sm" variant="ghost" disabled={busy} onClick={() => void act("discard the new certificate", () => api().discardCertificate())}>
                  Discard
                </Button>
              </div>
            ) : (
              <p className="mt-2 text-[12.5px] text-fg-subtle">An administrator made this certificate; only an administrator can switch to it.</p>
            )}
          </section>
        ) : (
          view?.can_rotate &&
          view.current && (
            <div className="flex flex-wrap items-center gap-3">
              <Button size="sm" disabled={busy} onClick={() => void act("make a new certificate", () => api().rotateCertificate())}>
                <RotateCcw /> Make a new certificate
              </Button>
              <span className="text-[12px] text-fg-subtle">A new key and certificate with the same names. Nothing changes until you switch to it.</span>
            </div>
          )
        )}
      </div>
      <Dialog
        open={confirming && pending !== null}
        onOpenChange={setConfirming}
        tone="warn"
        icon={<ShieldCheck />}
        title="Switch to the new certificate?"
        description={`${view?.host}'s Dashboard restarts with the new certificate. Everyone using it sees a certificate warning and logs in again, after comparing the fingerprint.`}
        footer={
          <>
            <Button variant="ghost" onClick={() => setConfirming(false)}>
              Not yet
            </Button>
            <Button variant="primary" onClick={() => pending && void doSwitch(pending)}>
              Switch
            </Button>
          </>
        }
      >
        <p className="selectable font-mono text-[12px] break-all text-fg">{pending?.fingerprint}</p>
      </Dialog>
    </Card>
  );
}

export function SettingsPage() {
  const [view, setView] = useState<SettingsView | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [updatedAt, setUpdatedAt] = useState<number | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      setView(await api().settings());
      setError(null);
      setUpdatedAt(Date.now());
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => void load(), [load]);

  return (
    <>
      <PageHeader title="Settings" subtitle="Cluster settings, host facts, host keys, certificates, email notifications, and log replication" actions={<RefreshControl updatedAt={updatedAt} loading={loading} error={error} onRefresh={() => void load()} label="Reload" />} />
      <PageBody>
        {!view ? (
          loading ? (
            <div className="space-y-3" aria-busy="true">
              <div className="skeleton h-4 w-1/3" />
              <div className="skeleton h-4 w-full" />
              <div className="skeleton h-4 w-2/3" />
            </div>
          ) : (
            <div role="alert" className="rounded-xl border border-danger/30 bg-danger/[0.06] px-4 py-3 text-[12.5px] text-danger">
              Settings aren't available: {error}
            </div>
          )
        ) : (
          <>
            {!view.cluster && (
              <section aria-label="Needs attention" className={cn("mb-5 flex items-start gap-3 rounded-xl border border-warn/30 bg-gradient-to-br from-warn/[0.08] to-transparent px-4 py-3")}>
                <AlertTriangle className="mt-0.5 size-4 shrink-0 text-warn" />
                <div className="text-[12.5px] leading-relaxed text-fg">
                  <span className="font-semibold">
                    {view.can_edit ? "Fill in your cluster's values before running workflows." : "An administrator needs to fill in the cluster's values before workflows can run."}
                  </span>{" "}
                  <span className="text-fg-muted">Operations read these settings, and those that need them fail until they're saved.</span>
                </div>
              </section>
            )}
            <SectionTitle>Cluster</SectionTitle>
            <Card className="mb-5">
              <CardHeader icon={<FileCog />} title="Cluster settings" subtitle="The cluster.conf every operation reads: names, the private network and its address ranges, guest sizes, and install images" />
              <SettingsForm
                label="Cluster settings"
                keys={view.cluster_keys}
                record={view.cluster}
                canEdit={view.can_edit}
                reload={load}
                save={async (values, expected) => {
                  const saved = await api().saveClusterSettings({ values, expected_revision: expected });
                  setView((v) => (v ? { ...v, cluster: saved } : v));
                  toast.success("Cluster settings saved", `Revision ${saved.revision}`);
                }}
              />
            </Card>
            <SectionTitle>Hosts</SectionTitle>
            <HostFacts view={view} reload={load} update={setView} />
            <Identities identities={view.identities} />
            <Certificate />
            <SectionTitle>Notifications</SectionTitle>
            <NotificationSettings />
            <SectionTitle>Log replication</SectionTitle>
            <LogReplication />
          </>
        )}
      </PageBody>
    </>
  );
}
