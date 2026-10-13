// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// Adding a host (DEPLOYMENT.md, "Joining a host"): issue a join bundle for a
// free slot, hand it to the Deployer, and approve the request the new host
// makes with it once it has booted. Approving starts `hosts.join` here.

import { useCallback, useEffect, useState } from "react";
import { KeyRound, Plus, Trash2, UserCheck } from "lucide-react";
import { Badge, Button, Card, CardHeader, Dialog, Input, Label, Select, dateTimeMs, untilEpoch, useNow, type HostSlot, type JoinList, type JoinView } from "@bmac/ui";
import { errorMessage } from "../lib/backend";
import { api, useStore } from "../state/store";
import { toast } from "../state/toast";
import { Table } from "./common";

/** Host slots run from mox1 to mox32. */
const HOST_SLOTS = 32;
const DEFAULT_TTL_HOURS = 168;

/** Saves `content` as a file named `name` in the browser's downloads. */
function download(name: string, content: string) {
  const url = URL.createObjectURL(new Blob([content], { type: "application/json" }));
  const link = document.createElement("a");
  link.href = url;
  link.download = name;
  link.click();
  URL.revokeObjectURL(url);
}

function IssueDialog({ open, onOpenChange, taken, onIssued }: { open: boolean; onOpenChange: (open: boolean) => void; taken: Set<string>; onIssued: () => void }) {
  const free = Array.from({ length: HOST_SLOTS }, (_, i) => `mox${i + 1}`).filter((h) => !taken.has(h));
  const [host, setHost] = useState<string | undefined>(undefined);
  const [prod, setProd] = useState("4");
  const [staging, setStaging] = useState("8");
  const [idrac, setIdrac] = useState("");
  const [ttl, setTtl] = useState(String(DEFAULT_TTL_HOURS));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const chosen = host ?? free[0];

  const issue = async () => {
    if (!chosen) return;
    setBusy(true);
    setError(null);
    try {
      const issued = await api().issueJoin({
        host: chosen as HostSlot,
        max_prod_vms: Number(prod),
        max_staging_vms: Number(staging),
        idrac_ip: idrac.trim() || undefined,
        ttl_hours: Number(ttl),
      });
      download(issued.file_name, issued.content);
      toast.success(`Downloaded ${issued.file_name}`);
      onIssued();
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
      title="Add a host"
      description="Issues a join bundle for a free host slot and downloads it. Open it in the Deployer to build the new host's ISO. It holds a single-use token, so it is shown only this once; keep it as private as a password until the host has joined."
      footer={
        <>
          <Button variant="ghost" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button variant="primary" disabled={busy || !chosen} onClick={() => void issue()}>
            Issue and download
          </Button>
        </>
      }
    >
      <div className="grid gap-3 sm:grid-cols-2">
        <div>
          <Label htmlFor="join-host" required>
            Host slot
          </Label>
          <Select id="join-host" value={chosen} onValueChange={setHost} options={free.map((h) => ({ value: h, label: h }))} />
        </div>
        <div>
          <Label htmlFor="join-ttl" required>
            Valid for
          </Label>
          <Input id="join-ttl" type="number" min={1} max={DEFAULT_TTL_HOURS} value={ttl} onChange={(e) => setTtl(e.target.value)} suffix="hours" />
        </div>
        <div>
          <Label htmlFor="join-prod" required>
            Production VMs on this host
          </Label>
          <Input id="join-prod" type="number" min={1} value={prod} onChange={(e) => setProd(e.target.value)} />
        </div>
        <div>
          <Label htmlFor="join-staging" required>
            Staging VMs on this host
          </Label>
          <Input id="join-staging" type="number" min={1} value={staging} onChange={(e) => setStaging(e.target.value)} />
        </div>
        <div className="sm:col-span-2">
          <Label htmlFor="join-idrac">iDRAC address</Label>
          <Input id="join-idrac" mono placeholder="optional" value={idrac} onChange={(e) => setIdrac(e.target.value)} />
        </div>
      </div>
      {error && (
        <p role="alert" className="mt-3 text-[12.5px] text-danger">
          {error}
        </p>
      )}
    </Dialog>
  );
}

function RequestCell({ join }: { join: JoinView }) {
  const r = join.request;
  if (!r) return <span className="text-fg-subtle">{join.used_at_ms ? "token used, no request yet" : "waiting for the host"}</span>;
  return (
    <div className="space-y-0.5 text-[11.5px]">
      <div>
        from <span className="font-mono">{r.address}</span> via <span className="font-mono">{r.received_by}</span>
      </div>
      <div className="selectable font-mono break-all text-fg">SSH {r.ssh_host_key_fingerprint ?? "—"}</div>
      <div className="selectable font-mono break-all text-fg">HTTPS {r.https_fingerprint ?? "—"}</div>
    </div>
  );
}

export function JoinBundles({ members }: { members: string[] }) {
  const [list, setList] = useState<JoinList | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [issuing, setIssuing] = useState(false);
  const [revoking, setRevoking] = useState<string | null>(null);
  const join = useStore((s) => s.operations["hosts.join"]);
  const startOperation = useStore((s) => s.startOperation);
  const now = useNow(30_000);

  const load = useCallback(() => {
    api()
      .joins()
      .then(
        (next) => {
          setList(next);
          setError(null);
        },
        (e) => setError(errorMessage(e)),
      );
  }, []);
  useEffect(load, [load]);

  const revoke = async (host: string) => {
    try {
      await api().revokeJoin(host);
      toast.success(`Revoked ${host}'s bundle`);
    } catch (e) {
      toast.error(errorMessage(e));
    }
    setRevoking(null);
    load();
  };

  const taken = new Set([...members, ...(list?.joins.map((j) => j.host) ?? [])]);
  return (
    <Card className="mb-4">
      <CardHeader
        icon={<KeyRound />}
        title="Adding hosts"
        subtitle="Join bundles this cluster issued, and the requests new hosts made with them"
        actions={
          list?.can_manage && (
            <Button size="sm" onClick={() => setIssuing(true)}>
              <Plus /> Add a host
            </Button>
          )
        }
      />
      {error ? (
        <p role="alert" className="px-4 py-3 text-[12.5px] text-danger">
          Join bundles aren't available: {error}
        </p>
      ) : list && list.joins.length === 0 ? (
        <p className="px-4 py-3 text-[12.5px] text-fg-subtle">No outstanding bundles.</p>
      ) : (
        <Table label="Join bundles" head={["Host", "Issued", "Expires", "Request", ""]}>
          {list?.joins.map((j) => (
            <tr key={j.host}>
              <td className="font-mono font-medium text-fg">{j.host}</td>
              <td className="text-[11.5px]">
                {dateTimeMs(j.issued_at_ms)} by {j.issued_by}
              </td>
              <td className="text-[11.5px]">{j.expires_at_ms <= now ? <Badge tone="warn">expired</Badge> : untilEpoch(j.expires_at_ms / 1000, now)}</td>
              <td>
                <RequestCell join={j} />
              </td>
              <td>
                {list.can_manage && (
                  <div className="flex items-center justify-end gap-2">
                    {j.request && join && (
                      <Button size="sm" variant="primary" disabled={!join.allowed} onClick={() => startOperation("hosts.join", { host: j.host })}>
                        <UserCheck /> Approve…
                      </Button>
                    )}
                    <Button size="sm" variant="ghost" aria-label={`Revoke ${j.host}'s bundle`} onClick={() => setRevoking(j.host)}>
                      <Trash2 />
                    </Button>
                  </div>
                )}
              </td>
            </tr>
          ))}
        </Table>
      )}
      <IssueDialog open={issuing} onOpenChange={setIssuing} taken={taken} onIssued={load} />
      <Dialog
        open={revoking !== null}
        onOpenChange={(open) => !open && setRevoking(null)}
        tone="danger"
        icon={<Trash2 />}
        title={`Revoke ${revoking}'s bundle?`}
        description="Its token stops working, any request made with it is dropped, and the slot is free again. A host built from it can't join; build its ISO again from a new bundle."
        footer={
          <>
            <Button variant="ghost" onClick={() => setRevoking(null)}>
              Keep it
            </Button>
            <Button variant="danger" onClick={() => revoking && void revoke(revoking)}>
              Revoke
            </Button>
          </>
        }
      />
    </Card>
  );
}
