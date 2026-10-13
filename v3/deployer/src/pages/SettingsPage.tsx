// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

import { useEffect, useState } from "react";
import { Trash2 } from "lucide-react";
import { Button, Card, CardBody, CardHeader, Dialog, EmptyState, Input, Label, Mono, StatusPill, Switch, ago, type IsoView, type LinkWait } from "@bmac/ui";
import { PageBody, PageHeader } from "../components/Page";
import { useStore } from "../state/store";

const isoKind = { discovery: "Discovery", host: "Proxmox host", appliance: "Backup appliance" } as const;

export function SettingsPage() {
  const view = useStore((s) => s.settings);
  const save = useStore((s) => s.saveSettings);
  const [linkWait, setLinkWait] = useState<{ attempts: string; interval: string }>({ attempts: "", interval: "" });
  const [deleting, setDeleting] = useState<IsoView | null>(null);
  const deleteIso = useStore((s) => s.deleteIso);

  useEffect(() => {
    if (view) setLinkWait({ attempts: String(view.settings.link_wait.attempts), interval: String(view.settings.link_wait.interval_seconds) });
  }, [view]);

  if (!view) return <PageHeader title="Settings" />;

  const parsed = parseLinkWait(linkWait.attempts, linkWait.interval);
  const changed = parsed.ok && (parsed.value.attempts !== view.settings.link_wait.attempts || parsed.value.interval_seconds !== view.settings.link_wait.interval_seconds);

  return (
    <>
      <PageHeader title="Settings" subtitle={<Mono>{view.path}</Mono>} />
      <PageBody className="space-y-5">
        <Card>
          <CardHeader title="Network link wait" subtitle="How long discovery and a new host wait for a network port to get a link before giving up on it." />
          <CardBody className="space-y-3">
            <div className="grid max-w-md grid-cols-2 gap-3">
              <div>
                <Label htmlFor="attempts">Checks</Label>
                <Input id="attempts" inputMode="numeric" value={linkWait.attempts} aria-invalid={!parsed.ok && parsed.field === "attempts"} onChange={(e) => setLinkWait({ ...linkWait, attempts: e.target.value })} />
              </div>
              <div>
                <Label htmlFor="interval">Seconds apart</Label>
                <Input id="interval" inputMode="numeric" suffix="s" value={linkWait.interval} aria-invalid={!parsed.ok && parsed.field === "interval"} onChange={(e) => setLinkWait({ ...linkWait, interval: e.target.value })} />
              </div>
            </div>
            {!parsed.ok && <p className="text-[12.5px] text-danger">{parsed.message}</p>}
            <Button variant="primary" disabled={!changed} onClick={() => parsed.ok && void save({ ...view.settings, link_wait: parsed.value })}>
              Save
            </Button>
          </CardBody>
        </Card>

        <DiscoverySource />

        <Card>
          <CardHeader title="Notifications" />
          <CardBody>
            <div className="flex items-center justify-between gap-4">
              <div>
                <div id="notify-label" className="text-[13px] font-medium text-fg">
                  Desktop notifications
                </div>
                <p id="notify-help" className="text-[12.5px] text-fg-muted">
                  When a task needs you or finishes while this window is in the background.
                </p>
              </div>
              <Switch checked={view.settings.notifications} labelledBy="notify-label" describedBy="notify-help" onCheckedChange={(on) => void save({ ...view.settings, notifications: on })} />
            </div>
          </CardBody>
        </Card>

        <Card>
          <CardHeader title="Built ISOs" />
          <p className="border-b border-line px-4 py-2.5 text-[12.5px] text-fg-muted">
            Each ISO holds keys made for one machine. The Deployer shreds it by itself once that machine is online on the tailnet; one listed here was left behind by a session that stopped earlier. Delete it unless the machine still needs it, and wipe any USB stick it was written to.
          </p>
          <CardBody className={view.isos.length ? "divide-y divide-line p-0" : undefined}>
            {view.isos.length === 0 ? (
              <EmptyState title="No ISOs" body="ISOs you build appear here until you delete them." />
            ) : (
              view.isos.map((entry) => (
                <div key={entry.iso.path} className="flex items-center gap-3 px-4 py-3">
                  <div className="min-w-0 flex-1">
                    <div className="flex items-center gap-2">
                      <span className="text-[13px] font-medium text-fg">
                        {isoKind[entry.iso.kind]}: {entry.iso.target}
                      </span>
                      {!entry.present && <StatusPill health="offline">File gone</StatusPill>}
                    </div>
                    <div className="selectable truncate font-mono text-[12px] text-fg-subtle">{entry.iso.path}</div>
                    <div className="text-[12px] text-fg-subtle">Built {ago(entry.iso.built_ms)}</div>
                  </div>
                  <Button variant="danger-outline" size="sm" onClick={() => setDeleting(entry)}>
                    <Trash2 />
                    {entry.present ? "Delete" : "Forget"}
                  </Button>
                </div>
              ))
            )}
          </CardBody>
        </Card>
      </PageBody>

      <Dialog
        open={deleting !== null}
        onOpenChange={(open) => !open && setDeleting(null)}
        tone="danger"
        icon={<Trash2 />}
        title={deleting?.present ? "Delete this ISO?" : "Forget this ISO?"}
        description={deleting?.present ? "The file is overwritten and removed from this workstation. Build a new one if the machine still needs it." : "The file is already gone; this removes it from the list."}
        footer={
          <>
            <Button onClick={() => setDeleting(null)}>Keep</Button>
            <Button
              variant="danger"
              onClick={() => {
                const path = deleting!.iso.path;
                setDeleting(null);
                void deleteIso(path);
              }}
            >
              {deleting?.present ? "Delete" : "Forget"}
            </Button>
          </>
        }
      >
        {deleting && <p className="selectable break-all font-mono text-[12px] text-fg-muted">{deleting.iso.path}</p>}
      </Dialog>
    </>
  );
}

function DiscoverySource() {
  const view = useStore((s) => s.settings)!;
  const save = useStore((s) => s.saveSettings);
  const chosen = view.settings.discovery_iso;
  const [url, setUrl] = useState(chosen?.url ?? "");
  const [sha256, setSha256] = useState(chosen?.sha256 ?? "");
  useEffect(() => {
    setUrl(chosen?.url ?? "");
    setSha256(chosen?.sha256 ?? "");
  }, [chosen?.url, chosen?.sha256]);
  const problem = checkSource(url, sha256);
  const current = chosen ?? view.default_discovery_iso;
  return (
    <Card>
      <CardHeader
        title="Discovery ISO source"
        subtitle="Discovery ISOs are built from Alpine Extended, which runs entirely from RAM. Choose another Alpine ISO here; the Deployer checks the download against its SHA-256."
      />
      <CardBody className="space-y-3">
        <div className="text-[12.5px] text-fg-muted">
          {chosen ? "Using your choice:" : "Using the pinned default:"}
          <div className="selectable break-all font-mono text-[12px] text-fg">{current.url}</div>
          <div className="selectable break-all font-mono text-[12px] text-fg-subtle">SHA-256 {current.sha256}</div>
        </div>
        <div className="grid max-w-2xl gap-3">
          <div>
            <Label htmlFor="discovery-url">ISO URL</Label>
            <Input id="discovery-url" placeholder={view.default_discovery_iso.url} value={url} onChange={(e) => setUrl(e.target.value)} />
          </div>
          <div>
            <Label htmlFor="discovery-sha">SHA-256</Label>
            <Input id="discovery-sha" className="font-mono" placeholder={view.default_discovery_iso.sha256} value={sha256} onChange={(e) => setSha256(e.target.value)} />
          </div>
        </div>
        {(url || sha256) && problem && <p className="text-[12.5px] text-danger">{problem}</p>}
        <div className="flex gap-2">
          <Button variant="primary" disabled={problem !== null} onClick={() => void save({ ...view.settings, discovery_iso: { url: url.trim(), sha256: sha256.trim().toLowerCase() } })}>
            Use this ISO
          </Button>
          <Button disabled={!chosen} onClick={() => void save({ ...view.settings, discovery_iso: undefined })}>
            Use the default
          </Button>
        </div>
      </CardBody>
    </Card>
  );
}

/** The same checks the Rust side makes when it saves. */
export function checkSource(url: string, sha256: string): string | null {
  let parsed: URL;
  try {
    parsed = new URL(url.trim());
  } catch {
    return "Enter the ISO's https URL.";
  }
  if (parsed.protocol !== "https:" || parsed.username || parsed.password || /\s/.test(url.trim())) return "The URL must be a plain https URL.";
  if (!/^[0-9a-fA-F]{64}$/.test(sha256.trim())) return "The SHA-256 must be 64 hex digits.";
  return null;
}

type Parsed = { ok: true; value: LinkWait } | { ok: false; field: "attempts" | "interval"; message: string };

/** The same limits the Rust side enforces when it saves. */
export function parseLinkWait(attempts: string, interval: string): Parsed {
  const a = Number(attempts);
  if (!/^\d+$/.test(attempts.trim()) || a < 1 || a > 100) return { ok: false, field: "attempts", message: "Checks must be a whole number from 1 to 100." };
  const i = Number(interval);
  if (!/^\d+$/.test(interval.trim()) || i < 1 || i > 60) return { ok: false, field: "interval", message: "Seconds apart must be a whole number from 1 to 60." };
  return { ok: true, value: { attempts: a, interval_seconds: i } };
}
