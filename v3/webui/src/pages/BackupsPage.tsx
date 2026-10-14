// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// Backups (BACKUP.md): the appliances and their enrollment, each VM's
// policy and PIT graph (time runs down the page, the oldest PIT at the top
// left, each branch to the right of the line it came from), the space PIT
// snapshots hold on the hosts, transfers, restores, and the backup jobs.

import { useCallback, useEffect, useMemo, useState } from "react";
import { Archive, GitBranch, KeyRound, MapPin, Pencil, Play, Plus, RefreshCw, Server, Trash2, UserCheck } from "lucide-react";
import {
  Badge,
  Button,
  Card,
  CardHeader,
  Checkbox,
  Dialog,
  EmptyState,
  Input,
  Label,
  PIT_ROW,
  PitGraphLines,
  Select,
  StatusPill,
  Switch,
  ago,
  bytes,
  dateTimeMs,
  pitLayout,
  plural,
  useNow,
  type ApplianceList,
  type BackupApplianceView,
  type BackupVmView,
  type BackupsView,
  type EnrollRequestView,
  type Health,
  type PitNodeView,
} from "@bmac/ui";
import { PageBody, PageHeader } from "../components/layout/Page";
import { errorMessage } from "../lib/backend";
import { api, useStore } from "../state/store";
import { toast } from "../state/toast";
import { OperationTiles, SectionTitle, Table } from "./common";

const STATE_HEALTH: Record<string, Health> = {
  ACTIVE: "ok",
  SEEDING: "run",
  REPAIRING: "run",
  PENDING: "info",
  LATE: "warn",
  PAUSED: "warn",
  DELINQUENT: "danger",
  RESEED_REQUIRED: "danger",
  RETIRED: "offline",
};

const MODE_LABEL: Record<string, string> = {
  new_vm: "new VM",
  test: "restore test",
  into_existing: "into the VM",
  rollback: "rollback",
  promotion: "promotion",
  recovery: "recovery",
};

const FREEZE_OPTIONS = [
  { value: "if_available", label: "Freeze when the guest agent answers" },
  { value: "required", label: "Require a freeze; fail without one" },
  { value: "never", label: "Never freeze (crash-consistent)" },
];

/** Saves `content` as a file named `name` in the browser's downloads. */
function download(name: string, content: string) {
  const url = URL.createObjectURL(new Blob([content], { type: "application/json" }));
  const link = document.createElement("a");
  link.href = url;
  link.download = name;
  link.click();
  URL.revokeObjectURL(url);
}

function short(uuid: string) {
  return uuid.slice(0, 8);
}

function rate(bytesMoved: number, ms: number) {
  return ms > 0 ? `${bytes((bytesMoved / ms) * 1000)}/s` : "—";
}

function Problem({ children }: { children: string | null }) {
  if (!children) return null;
  return (
    <p role="alert" className="selectable mt-3 text-[12.5px] text-danger">
      {children}
    </p>
  );
}

function EnrollDialog({ open, onOpenChange }: { open: boolean; onOpenChange: (open: boolean) => void }) {
  const [ttl, setTtl] = useState("168");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const issue = async () => {
    setBusy(true);
    setError(null);
    try {
      const issued = await api().issueEnroll({ ttl_hours: Number(ttl) });
      download(issued.file_name, issued.content);
      toast.success(`Downloaded ${issued.file_name}`);
      onOpenChange(false);
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Dialog
      open={open}
      onOpenChange={onOpenChange}
      icon={<KeyRound />}
      title="Enroll a backup appliance"
      description="Issues an enrollment bundle and downloads it. Give it to bmac-backupd enroll on the appliance. It holds a single-use token, so it is shown only this once; keep it as private as a password until the appliance has enrolled."
      footer={
        <>
          <Button variant="ghost" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button variant="primary" disabled={busy || !(Number(ttl) >= 1)} onClick={() => void issue()}>
            Issue and download
          </Button>
        </>
      }
    >
      <Label htmlFor="enroll-ttl" required>
        Valid for
      </Label>
      <Input id="enroll-ttl" type="number" min={1} max={168} value={ttl} onChange={(e) => setTtl(e.target.value)} suffix="hours" />
      <Problem>{error}</Problem>
    </Dialog>
  );
}

/** Approval is for the certificate the administrator read on the appliance's console. */
function ApproveDialog({ request, close, done }: { request: EnrollRequestView; close: () => void; done: () => void }) {
  const [typed, setTyped] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const approve = async () => {
    setBusy(true);
    setError(null);
    try {
      await api().approveAppliance(request.request_id, typed.trim());
      toast.success(`Approved ${request.name ?? "the appliance"}`);
      done();
      close();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Dialog
      open
      onOpenChange={(open) => !open && close()}
      icon={<UserCheck />}
      tone="accent"
      title={`Approve ${request.name ?? "this appliance"}?`}
      description="The cluster will trust exactly this certificate. Read the fingerprint bmac-backupd init printed on the appliance's console and enter it here; it must match the request's."
      footer={
        <>
          <Button variant="ghost" onClick={close}>
            Cancel
          </Button>
          <Button variant="primary" disabled={busy || !typed.trim()} onClick={() => void approve()}>
            Approve
          </Button>
        </>
      }
    >
      <div className="mb-3 text-[12px] text-fg-muted">
        Request from <span className="font-mono">{request.from}</span> via <span className="font-mono">{request.received_by}</span>
        <div className="selectable mt-1 break-all font-mono text-fg">{request.fingerprint ?? "no certificate"}</div>
      </div>
      <Label htmlFor="approve-fingerprint" required>
        Fingerprint on the appliance's console
      </Label>
      <Input id="approve-fingerprint" mono value={typed} onChange={(e) => setTyped(e.target.value)} />
      <Problem>{error}</Problem>
    </Dialog>
  );
}

function AddressDialog({ appliance, close, done }: { appliance: BackupApplianceView["appliance"]; close: () => void; done: () => void }) {
  const [address, setAddress] = useState(appliance.address ?? "");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const save = async () => {
    setBusy(true);
    setError(null);
    try {
      await api().setApplianceAddress(appliance.appliance_uuid, address.trim() || null);
      toast.success(`Saved ${appliance.name}'s address`);
      done();
      close();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Dialog
      open
      onOpenChange={(open) => !open && close()}
      icon={<MapPin />}
      title={`${appliance.name}'s address`}
      description="Where restore jobs reach the appliance's port 8445: an IP address or name, with :port if it isn't 8445. Without one, nothing can restore from it."
      footer={
        <>
          <Button variant="ghost" onClick={close}>
            Cancel
          </Button>
          <Button variant="primary" disabled={busy} onClick={() => void save()}>
            Save
          </Button>
        </>
      }
    >
      <Label htmlFor="appliance-address">Address</Label>
      <Input id="appliance-address" mono value={address} placeholder="10.40.0.20 or backup.example.net" onChange={(e) => setAddress(e.target.value)} />
      <Problem>{error}</Problem>
    </Dialog>
  );
}

function Appliances({ view, list, reload }: { view: BackupsView; list: ApplianceList | null; reload: () => void }) {
  const now = useNow(15_000);
  const [enrolling, setEnrolling] = useState(false);
  const [approving, setApproving] = useState<EnrollRequestView | null>(null);
  const [addressing, setAddressing] = useState<BackupApplianceView["appliance"] | null>(null);
  const [revoking, setRevoking] = useState<BackupApplianceView["appliance"] | null>(null);
  const manage = view.can_manage;
  const act = async (what: () => Promise<void>, done: string) => {
    try {
      await what();
      toast.success(done);
    } catch (e) {
      toast.error(errorMessage(e));
    }
    reload();
  };
  const requests = list?.requests ?? [];
  return (
    <Card className="mb-4">
      <CardHeader
        icon={<Server />}
        title="Appliances"
        subtitle="Each pulls PITs from the cluster over its own connection; the cluster trusts it only by the certificate approved here"
        actions={
          manage && (
            <Button size="sm" onClick={() => setEnrolling(true)}>
              <Plus /> Enroll an appliance
            </Button>
          )
        }
      />
      {view.appliances.length === 0 ? (
        <EmptyState icon={<Archive />} title="No appliances yet" body="Enroll a backup appliance to start keeping PITs off the cluster." />
      ) : (
        <Table label="Backup appliances" head={["Appliance", "State", "Address", "Last check-in", "Pulling", ""]}>
          {view.appliances.map(({ appliance: a, status, live }) => (
            <tr key={a.appliance_uuid}>
              <td>
                <div className="font-medium text-fg">{a.name}</div>
                <div className="selectable max-w-[260px] truncate font-mono text-[10.5px] text-fg-subtle" title={a.fingerprint}>
                  {a.fingerprint}
                </div>
              </td>
              <td>
                <div className="flex flex-wrap items-center gap-1">
                  {a.enrollment !== "approved" ? (
                    <Badge tone={a.enrollment === "revoked" ? "danger" : "info"}>{a.enrollment}</Badge>
                  ) : (
                    <StatusPill health={STATE_HEALTH[status?.state ?? ""] ?? "unknown"}>{status?.state.toLowerCase().replace("_", " ") ?? "unknown"}</StatusPill>
                  )}
                  {(status?.locked ?? a.locked) && <Badge tone="warn">locked</Badge>}
                  {(status?.suspect_pits ?? 0) > 0 && <Badge tone="danger">{plural(status!.suspect_pits, "suspect PIT")}</Badge>}
                  {(status?.scrub_errors ?? 0) > 0 && <Badge tone="danger">scrub errors</Badge>}
                </div>
              </td>
              <td className="font-mono text-[12px]">{a.address ?? <span className="text-warn">none</span>}</td>
              <td className="text-[12px] text-fg-muted">{a.last_checkin_ms ? ago(a.last_checkin_ms, now) : "never"}</td>
              <td className="text-[12px]">
                {live.length === 0 ? (
                  <span className="text-fg-subtle">—</span>
                ) : (
                  live.map((l) => (
                    <div key={l.pit_uuid}>
                      {l.vm}: {bytes(l.bytes)} <span className="text-fg-subtle">· moved {ago(l.progress_s * 1000, now)}</span>
                    </div>
                  ))
                )}
              </td>
              <td>
                {manage && (
                  <div className="flex justify-end gap-1.5">
                    <Button size="sm" variant="ghost" aria-label={`Set ${a.name}'s address`} onClick={() => setAddressing(a)}>
                      <MapPin />
                    </Button>
                    {a.enrollment !== "revoked" && (
                      <Button size="sm" variant="ghost" aria-label={`Revoke ${a.name}`} onClick={() => setRevoking(a)}>
                        <Trash2 />
                      </Button>
                    )}
                  </div>
                )}
              </td>
            </tr>
          ))}
        </Table>
      )}
      {requests.length > 0 && (
        <div className="border-t border-line">
          <SectionTitle className="px-4 pt-3">Waiting for approval</SectionTitle>
          <Table label="Enrollment requests" head={["Appliance", "From", "Certificate", "Asked", ""]}>
            {requests.map((r) => (
              <tr key={r.request_id}>
                <td className="font-medium text-fg">{r.name ?? "unnamed"}</td>
                <td className="font-mono text-[12px]">
                  {r.from}
                  {r.suggested_address && <div className="text-[11px] text-fg-subtle">suggests {r.suggested_address}</div>}
                </td>
                <td className="selectable max-w-[300px] break-all font-mono text-[10.5px]">{r.fingerprint ?? "—"}</td>
                <td className="text-[12px] text-fg-muted">{ago(r.created_at_ms, now)}</td>
                <td>
                  {manage && (
                    <div className="flex justify-end gap-1.5">
                      <Button size="sm" variant="primary" disabled={!r.fingerprint} onClick={() => setApproving(r)}>
                        <UserCheck /> Approve…
                      </Button>
                      <Button size="sm" variant="ghost" onClick={() => void act(() => api().rejectAppliance(r.request_id), "Rejected the request")}>
                        Reject
                      </Button>
                    </div>
                  )}
                </td>
              </tr>
            ))}
          </Table>
        </div>
      )}
      <EnrollDialog open={enrolling} onOpenChange={setEnrolling} />
      {approving && <ApproveDialog request={approving} close={() => setApproving(null)} done={reload} />}
      {addressing && <AddressDialog appliance={addressing} close={() => setAddressing(null)} done={reload} />}
      <Dialog
        open={revoking !== null}
        onOpenChange={(open) => !open && setRevoking(null)}
        tone="danger"
        icon={<Trash2 />}
        title={`Revoke ${revoking?.name}?`}
        description="The cluster stops trusting its certificate: it can't check in, claim, or pull, and no restore reads from it. Its PITs stay on the appliance. Enrolling it again needs a new bundle."
        footer={
          <>
            <Button variant="ghost" onClick={() => setRevoking(null)}>
              Keep it
            </Button>
            <Button
              variant="danger"
              onClick={() => {
                const a = revoking;
                setRevoking(null);
                if (a) void act(() => api().revokeAppliance(a.appliance_uuid), `Revoked ${a.name}`);
              }}
            >
              Revoke
            </Button>
          </>
        }
      />
    </Card>
  );
}

type Schedule = { kind: "daily"; at: string } | { kind: "interval"; minutes: number } | { kind: "keep" };

function parseSchedule(json: string | undefined): Schedule {
  if (!json) return { kind: "daily", at: "03:00" };
  try {
    const every = JSON.parse(json) as { kind?: string; at?: string; minutes?: number };
    if (every.kind === "daily" && every.at) return { kind: "daily", at: every.at };
    if (every.kind === "interval" && every.minutes) return { kind: "interval", minutes: every.minutes };
  } catch {
    // An unreadable schedule is kept as it is.
  }
  return { kind: "keep" };
}

function PolicyDialog({ vm, appliances, close, done }: { vm: BackupVmView; appliances: BackupApplianceView[]; close: () => void; done: () => void }) {
  const policy = vm.policy;
  const initial = parseSchedule(policy?.every_json);
  const [enabled, setEnabled] = useState(policy?.enabled ?? true);
  const [kind, setKind] = useState<Schedule["kind"]>(initial.kind);
  const [at, setAt] = useState(initial.kind === "daily" ? initial.at : "03:00");
  const [minutes, setMinutes] = useState(String(initial.kind === "interval" ? initial.minutes : 60));
  const [chosen, setChosen] = useState<Set<string>>(new Set(policy?.appliances ?? []));
  const [newest, setNewest] = useState(String(policy?.local_newest ?? 2));
  const [days, setDays] = useState(String(policy?.local_days ?? 0));
  const [weeks, setWeeks] = useState(String(policy?.local_weeks ?? 0));
  const [freeze, setFreeze] = useState(policy?.freeze ?? "if_available");
  const [multiPool, setMultiPool] = useState(policy?.accept_multi_pool ?? false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const active = appliances.filter((a) => a.appliance.enrollment === "approved" && !a.appliance.retired);
  const every = kind === "daily" ? { kind, at } : kind === "interval" ? { kind, minutes: Number(minutes) } : JSON.parse(policy?.every_json || "null");
  const save = async () => {
    setBusy(true);
    setError(null);
    try {
      await api().saveBackupPolicy({
        vm: vm.vm,
        every,
        appliances: [...chosen],
        local_retention: { newest: Number(newest), days: Number(days), weeks: Number(weeks) },
        freeze,
        accept_multi_pool: multiPool,
        enabled,
        revision: policy?.revision ?? null,
      });
      toast.success(`Saved ${vm.vm}'s backup policy`);
      done();
      close();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  const remove = async () => {
    if (!policy) return;
    setBusy(true);
    try {
      await api().deleteBackupPolicy(vm.vm_uuid, policy.revision);
      toast.success(`Deleted ${vm.vm}'s backup policy`);
      done();
      close();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  const scheduleOptions = [
    { value: "daily", label: "Daily at a time" },
    { value: "interval", label: "Every N minutes" },
    ...(initial.kind === "keep" ? [{ value: "keep", label: `As it is: ${policy?.every}` }] : []),
  ];
  return (
    <Dialog
      open
      onOpenChange={(open) => !open && close()}
      icon={<Pencil />}
      title={`${vm.vm}'s backup policy`}
      description="Lowering local retention takes effect after the settings' delay, so a mistake can be undone before snapshots go."
      footer={
        <>
          {policy && (
            <Button variant="danger" className="mr-auto" disabled={busy} onClick={() => void remove()}>
              Delete policy
            </Button>
          )}
          <Button variant="ghost" onClick={close}>
            Cancel
          </Button>
          <Button variant="primary" disabled={busy} onClick={() => void save()}>
            Save
          </Button>
        </>
      }
    >
      <div className="grid gap-3 sm:grid-cols-2">
        <div className="flex items-center gap-2 sm:col-span-2">
          <Switch id="policy-enabled" checked={enabled} onCheckedChange={setEnabled} />
          <Label htmlFor="policy-enabled">Back up on schedule</Label>
        </div>
        <div>
          <Label htmlFor="policy-schedule">Schedule</Label>
          <Select id="policy-schedule" value={kind} onValueChange={(v) => setKind(v as Schedule["kind"])} options={scheduleOptions} />
        </div>
        <div>
          {kind === "daily" && (
            <>
              <Label htmlFor="policy-at">At (cluster time)</Label>
              <Input id="policy-at" mono value={at} onChange={(e) => setAt(e.target.value)} placeholder="03:00" />
            </>
          )}
          {kind === "interval" && (
            <>
              <Label htmlFor="policy-minutes">Every</Label>
              <Input id="policy-minutes" type="number" min={5} value={minutes} onChange={(e) => setMinutes(e.target.value)} suffix="minutes" />
            </>
          )}
        </div>
        <fieldset className="sm:col-span-2">
          <legend className="mb-1 text-[12px] font-medium text-fg">Appliances</legend>
          {active.length === 0 && <p className="text-[12px] text-fg-subtle">No approved appliance yet.</p>}
          {active.map(({ appliance: a }) => (
            <label key={a.appliance_uuid} className="flex items-center gap-2 py-0.5 text-[12.5px]">
              <Checkbox
                checked={chosen.has(a.appliance_uuid)}
                aria-label={`back up to ${a.name}`}
                onCheckedChange={(on) =>
                  setChosen((prev) => {
                    const next = new Set(prev);
                    if (on) next.add(a.appliance_uuid);
                    else next.delete(a.appliance_uuid);
                    return next;
                  })
                }
              />
              {a.name}
            </label>
          ))}
        </fieldset>
        <div className="grid grid-cols-3 gap-2 sm:col-span-2">
          <div>
            <Label htmlFor="policy-newest">Keep newest</Label>
            <Input id="policy-newest" type="number" min={0} value={newest} onChange={(e) => setNewest(e.target.value)} />
          </div>
          <div>
            <Label htmlFor="policy-days">Daily for</Label>
            <Input id="policy-days" type="number" min={0} value={days} onChange={(e) => setDays(e.target.value)} suffix="days" />
          </div>
          <div>
            <Label htmlFor="policy-weeks">Weekly for</Label>
            <Input id="policy-weeks" type="number" min={0} value={weeks} onChange={(e) => setWeeks(e.target.value)} suffix="weeks" />
          </div>
        </div>
        <div className="sm:col-span-2">
          <Label htmlFor="policy-freeze">Guest freeze</Label>
          <Select id="policy-freeze" value={freeze} onValueChange={setFreeze} options={FREEZE_OPTIONS} />
        </div>
        <label className="flex items-center gap-2 text-[12.5px] sm:col-span-2">
          <Checkbox checked={multiPool} onCheckedChange={setMultiPool} aria-label="accept disks on several pools" />
          Accept disks on several pools (their snapshots aren't atomic together)
        </label>
      </div>
      <Problem>{error}</Problem>
    </Dialog>
  );
}

function OfferBadges({ pit, names }: { pit: PitNodeView; names: Map<string, string> }) {
  return (
    <>
      {pit.offers.map((o) => {
        const name = names.get(o.appliance_uuid) ?? short(o.appliance_uuid);
        const tone = o.state === "published" ? "ok" : o.state === "failed" ? "danger" : o.state === "claimed" ? "run" : "neutral";
        return (
          <Badge key={o.appliance_uuid} tone={tone} title={`${name}: ${o.state}`}>
            {name}
            {o.state === "claimed" && o.bytes !== null ? ` ${bytes(o.bytes)}` : o.state === "published" ? "" : ` ${o.state}`}
          </Badge>
        );
      })}
    </>
  );
}

function VmCard({ vm, view, reload }: { vm: BackupVmView; view: BackupsView; reload: () => void }) {
  const graph = useMemo(() => pitLayout(vm.pits), [vm.pits]);
  const names = useMemo(() => new Map(view.appliances.map((a) => [a.appliance.appliance_uuid, a.appliance.name])), [view.appliances]);
  const capture = useStore((s) => s.operations["backup.capture"]);
  const startOperation = useStore((s) => s.startOperation);
  const [editing, setEditing] = useState(false);
  const policy = vm.policy;
  const status = vm.status;
  const branches = graph.columns;
  return (
    <Card className="mb-4">
      <CardHeader
        icon={<GitBranch />}
        title={
          <span>
            {vm.vm}
            {vm.vmid !== null && <span className="ml-1.5 font-mono text-[12px] text-fg-subtle">VM {vm.vmid}</span>}
          </span>
        }
        subtitle={
          policy
            ? `${policy.enabled ? policy.every : "paused"} · to ${policy.appliances.map((a) => names.get(a) ?? short(a)).join(", ") || "no appliance"} · keeps ${policy.local_newest} newest${policy.local_days ? `, ${policy.local_days} days` : ""}${policy.local_weeks ? `, ${policy.local_weeks} weeks` : ""} on the cluster`
            : "No backup policy"
        }
        actions={
          <div className="flex gap-1.5">
            {policy && capture && (
              <Button size="sm" disabled={!capture.allowed} onClick={() => startOperation("backup.capture", { vm: vm.vm })}>
                <Play /> Back up now
              </Button>
            )}
            {view.can_manage && (
              <Button size="sm" variant={policy ? "ghost" : "primary"} onClick={() => setEditing(true)}>
                <Pencil /> {policy ? "Policy" : "Add a policy"}
              </Button>
            )}
          </div>
        }
      />
      {status && (status.overdue.length > 0 || status.unclaimed.length > 0 || status.release_blocked.length > 0 || status.last_attempt?.outcome === "failed") && (
        <div className="space-y-0.5 border-b border-line/70 px-4 py-2 text-[12px]">
          {status.last_attempt?.outcome === "failed" && <div className="selectable text-danger">The last backup failed: {status.last_attempt.message}</div>}
          {status.overdue.length > 0 && <div className="text-warn">Overdue on {status.overdue.join(", ")}</div>}
          {status.unclaimed.length > 0 && <div className="text-warn">Not taken by {status.unclaimed.join(", ")}</div>}
          {status.release_blocked.map((b) => (
            <div key={b} className="selectable text-warn">
              Snapshot kept: {b}
            </div>
          ))}
        </div>
      )}
      {vm.pits.length === 0 ? (
        <p className="px-4 py-3 text-[12.5px] text-fg-subtle">No PITs yet.</p>
      ) : (
        <div className="p-2">
          <div className="mb-1 px-2 text-[11.5px] text-fg-subtle">
            {plural(vm.pits.length, "PIT")} · {plural(branches, "branch", "branches")} · oldest at the top
          </div>
          <div className="flex">
            <PitGraphLines graph={graph} title={(p) => p.name} nodeClass={(n) => (n.pit.presence === "bookmark" ? "fill-surface stroke-fg-subtle" : undefined)} />
            <ul className="min-w-0 flex-1" aria-label={`${vm.vm} PITs`}>
              {graph.nodes.map((n) => (
                <li key={n.pit.pit_uuid} className="flex items-center gap-2 border-b border-line/50 px-2 text-[12.5px]" style={{ height: PIT_ROW }}>
                  <span className="w-[150px] shrink-0 text-fg">{dateTimeMs(n.pit.taken_s * 1000, view.time_zone)}</span>
                  <span className="hidden truncate font-mono text-[11px] text-fg-subtle lg:inline">{n.pit.name}</span>
                  <span className="ml-auto flex shrink-0 items-center gap-1">
                    {n.pit.release_blocked && (
                      <Badge tone="warn" title={n.pit.release_blocked}>
                        kept
                      </Badge>
                    )}
                    <Badge tone={n.pit.presence === "snapshot" ? "info" : "neutral"} title={n.pit.presence === "snapshot" ? "Its snapshots are on the hosts" : "Only bookmarks remain on the hosts"}>
                      {n.pit.presence}
                    </Badge>
                    {n.pit.consistency !== "filesystem" && <Badge tone={n.pit.consistency === "crash" ? "warn" : "ok"}>{n.pit.consistency}</Badge>}
                    <OfferBadges pit={n.pit} names={names} />
                  </span>
                </li>
              ))}
            </ul>
          </div>
        </div>
      )}
      {vm.local.length > 0 && (
        <div className="border-t border-line/70 px-4 py-2 text-[12px] text-fg-muted">
          On the hosts:{" "}
          {vm.local.map((l, i) => (
            <span key={l.host}>
              {i > 0 && " · "}
              <span className="font-mono">{l.host}</span> {plural(l.snapshots, "snapshot")}, {bytes(l.bytes)}
            </span>
          ))}
        </div>
      )}
      {editing && <PolicyDialog vm={vm} appliances={view.appliances} close={() => setEditing(false)} done={reload} />}
    </Card>
  );
}

function Restores({ view }: { view: BackupsView }) {
  if (view.restores.length === 0) return <p className="mb-6 text-[12.5px] text-fg-subtle">No restores yet.</p>;
  return (
    <Card className="mb-6">
      <Table label="Restores" head={["Started", "VM", "Kind", "On", "Disks", "Outcome"]}>
        {view.restores.map((r) => (
          <tr key={r.restore_uuid}>
            <td className="text-[12px]">{dateTimeMs(r.started_at_ms, view.time_zone)}</td>
            <td>{r.vm ?? <span className="font-mono">{short(r.vm_uuid)}</span>}</td>
            <td>{MODE_LABEL[r.mode] ?? r.mode}</td>
            <td className="text-[12px]">
              <span className="font-mono">{r.host}</span>
              {r.vmid !== null && <span className="text-fg-subtle"> · VM {r.vmid}</span>}
            </td>
            <td className="text-[11.5px] text-fg-muted">
              {Object.entries(r.methods)
                .map(([slot, m]) => `${slot}: ${m === 1 ? "clone" : m === 2 ? "incremental" : "full"}`)
                .join(", ") || "—"}
            </td>
            <td className="text-[12px]">
              {r.error ? (
                <span className="selectable text-danger">{r.error}</span>
              ) : r.finished_at_ms === null ? (
                <StatusPill health="run">running</StatusPill>
              ) : r.test ? (
                <span className={r.test.booted ? "text-ok" : "text-danger"}>
                  {r.test.booted ? "booted" : "didn't boot"}
                  {r.test.agent ? `, agent after ${r.test.agent_after_s ?? "?"} s` : ", no agent"}
                </span>
              ) : r.ended_at_ms !== null ? (
                <span className="text-fg-subtle">cleaned up</span>
              ) : (
                <span className="text-ok">done</span>
              )}
            </td>
          </tr>
        ))}
      </Table>
    </Card>
  );
}

function Transfers({ view }: { view: BackupsView }) {
  const vms = new Map(view.vms.map((v) => [v.vm_uuid, v.vm]));
  if (view.transfers.length === 0) return <p className="mb-6 text-[12.5px] text-fg-subtle">No transfers yet.</p>;
  return (
    <Card className="mb-6">
      <Table label="Transfers" head={["Finished", "From", "To", "VM", "Sent", "Rate", ""]}>
        {view.transfers.slice(0, 50).map((t, i) => (
          <tr key={`${t.at_ms}-${t.volume_uuid}-${i}`}>
            <td className="text-[12px]">{dateTimeMs(t.at_ms, view.time_zone)}</td>
            <td className="font-mono text-[12px]">{t.host}</td>
            <td>{t.appliance}</td>
            <td>{vms.get(t.vm_uuid) ?? <span className="font-mono">{short(t.vm_uuid)}</span>}</td>
            <td className="tabular-nums">{bytes(t.bytes)}</td>
            <td className="tabular-nums text-fg-muted">{rate(t.bytes, t.duration_ms)}</td>
            <td className="selectable text-[12px] text-danger">{t.error ?? ""}</td>
          </tr>
        ))}
      </Table>
    </Card>
  );
}

export function BackupsPage() {
  const [view, setView] = useState<BackupsView | null>(null);
  const [list, setList] = useState<ApplianceList | null>(null);
  const [error, setError] = useState<string | null>(null);
  const load = useCallback(() => {
    api()
      .backups()
      .then(
        (next) => {
          setView(next);
          setError(null);
        },
        (e) => setError(errorMessage(e)),
      );
    api()
      .appliances()
      .then(setList, () => setList(null));
  }, []);
  useEffect(() => {
    load();
    const timer = setInterval(load, 30_000);
    return () => clearInterval(timer);
  }, [load]);
  return (
    <>
      <PageHeader
        title="Backups"
        subtitle={`PITs on the backup appliances, the snapshots kept on the hosts, and restores${view?.time_zone ? `; times in ${view.time_zone}` : ""}`}
        actions={
          <Button size="sm" variant="ghost" onClick={load} aria-label="Refresh backups">
            <RefreshCw />
          </Button>
        }
      />
      <PageBody>
        {error && (
          <Card className="mb-4">
            <EmptyState icon={<Archive />} title="Backups aren't available" body={error} />
          </Card>
        )}
        {view && view.errors.length > 0 && (
          <div role="alert" className="selectable mb-4 rounded-lg border border-warn/40 bg-warn/8 px-4 py-2 text-[12.5px] text-warn">
            {view.errors.map((e) => (
              <div key={e}>{e}</div>
            ))}
          </div>
        )}
        {view && (
          <>
            <Appliances view={view} list={list} reload={load} />
            <SectionTitle>VMs</SectionTitle>
            {view.vms.length === 0 && <p className="mb-4 text-[12.5px] text-fg-subtle">No production VMs.</p>}
            {view.vms.map((vm) => (
              <VmCard key={vm.vm_uuid} vm={vm} view={view} reload={load} />
            ))}
            <SectionTitle>Restores</SectionTitle>
            <Restores view={view} />
            <SectionTitle>Transfers</SectionTitle>
            <Transfers view={view} />
          </>
        )}
        <SectionTitle>Backup operations</SectionTitle>
        <p className="mb-2.5 text-[12.5px] text-fg-subtle">
          A restore needs a grant from the appliance's administrator. Recover a lost cluster runs on a freshly deployed one, from an appliance that holds its recovery manifest.
        </p>
        <OperationTiles
          ids={["backup.capture", "backup.restore", "backup.restore_test", "backup.restore_into", "backup.rollback", "backup.promote", "backup.cleanup", "backup.recover"]}
        />
      </PageBody>
    </>
  );
}
