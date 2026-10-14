// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// Trusted peers: other appliances that may read from this one, to repair
// a damaged PIT or seed a new appliance. Trust is by the fingerprint typed
// from the peer's own UI; the appliance connects and compares before
// saving anything.

import { useState } from "react";
import { Share2, ShieldAlert, ShieldCheck } from "lucide-react";
import { Badge, Button, Card, CardBody, CardHeader, Dialog, Input, Label, type PeerView, type TrustResult } from "@bmac/ui";
import { Done, PageBody, PageHeader, Problem, Warning, useAction, useApi, useLoad, when } from "../parts";

function UntrustDialog({ peer, close, done }: { peer: PeerView; close: () => void; done: () => void }) {
  const api = useApi();
  const [typed, setTyped] = useState("");
  const [password, setPassword] = useState("");
  const action = useAction();
  return (
    <Dialog
      open
      onOpenChange={(open) => !open && close()}
      tone="danger"
      icon={<ShieldAlert />}
      title="Stop trusting this peer?"
      description="It can no longer read from this appliance; its open connections close within seconds."
      footer={
        <>
          <Button variant="ghost" onClick={close}>
            Cancel
          </Button>
          <Button
            variant="danger"
            disabled={typed !== "UNTRUST" || !password || action.busy}
            onClick={() =>
              void action.run(async () => {
                await api.untrust(peer.appliance_uuid, password);
                setPassword("");
                done();
              })
            }
          >
            Untrust
          </Button>
        </>
      }
    >
      <div className="space-y-3">
        <Problem>{action.error}</Problem>
        <p className="text-[12.5px] text-fg-muted">
          Appliance <span className="font-mono text-fg">{peer.appliance_uuid}</span> at {peer.address}
        </p>
        <div>
          <Label htmlFor="untrust-typed">Type UNTRUST</Label>
          <Input id="untrust-typed" value={typed} onChange={(e) => setTyped(e.target.value)} mono />
        </div>
        <div>
          <Label htmlFor="untrust-password">Administrator password</Label>
          <Input id="untrust-password" type="password" value={password} onChange={(e) => setPassword(e.target.value)} autoComplete="current-password" />
        </div>
      </div>
    </Dialog>
  );
}

export function PeersPage() {
  const api = useApi();
  const { data, error, reload } = useLoad((a) => a.peers());
  const [form, setForm] = useState({ appliance_uuid: "", fingerprint: "", address: "" });
  const [result, setResult] = useState<TrustResult | null>(null);
  const [untrusting, setUntrusting] = useState<PeerView | null>(null);
  const action = useAction();
  const trust = () =>
    action.run(async () => {
      setResult(null);
      const answer = await api.trust({ ...form, appliance_uuid: form.appliance_uuid.trim(), address: form.address.trim() });
      setResult(answer);
      if (answer.trusted) {
        setForm({ appliance_uuid: "", fingerprint: "", address: "" });
        await reload();
      }
    });
  return (
    <>
      <PageHeader title="Peers" subtitle="Other appliances that may read from this one, to repair a damaged PIT or seed a new appliance" />
      <PageBody>
        <Problem>{error ?? action.error}</Problem>
        {data && (
          <Card>
            <CardHeader title="This appliance's fingerprint" icon={<ShieldCheck />} subtitle="Read it out to a peer's administrator, who types it into their appliance" />
            <CardBody>
              <p className="selectable break-all font-mono text-[12.5px] text-fg">{data.fingerprint}</p>
            </CardBody>
          </Card>
        )}
        <Card>
          <CardHeader title="Trust a peer" icon={<Share2 />} />
          <CardBody className="space-y-3">
            {data && <Warning>{data.reminder}</Warning>}
            {result && (result.trusted ? <Done>{result.message}</Done> : <Problem>{`${result.message}${result.presented ? ` It presented ${result.presented}.` : ""}`}</Problem>)}
            <div className="grid gap-3 lg:grid-cols-3">
              <div>
                <Label htmlFor="peer-uuid">Its appliance UUID</Label>
                <Input id="peer-uuid" value={form.appliance_uuid} onChange={(e) => setForm({ ...form, appliance_uuid: e.target.value })} mono spellCheck={false} />
              </div>
              <div>
                <Label htmlFor="peer-address">Its address (IP or DNS name)</Label>
                <Input id="peer-address" value={form.address} onChange={(e) => setForm({ ...form, address: e.target.value })} mono spellCheck={false} />
              </div>
              <div>
                <Label htmlFor="peer-fingerprint">Its fingerprint, from its own UI</Label>
                <Input id="peer-fingerprint" value={form.fingerprint} onChange={(e) => setForm({ ...form, fingerprint: e.target.value })} mono spellCheck={false} />
              </div>
            </div>
            <Button variant="primary" disabled={action.busy || !form.appliance_uuid || !form.address || !form.fingerprint} onClick={() => void trust()}>
              Connect and compare
            </Button>
          </CardBody>
        </Card>
        {data && data.peers.length > 0 && (
          <Card>
            <CardHeader title="Trusted peers" />
            <CardBody className="space-y-2">
              {data.peers.map((p) => (
                <div key={p.appliance_uuid} className="flex flex-wrap items-center gap-2 rounded-lg border border-line px-3 py-2 text-[12.5px]">
                  <span className="font-mono text-fg">{p.appliance_uuid}</span>
                  <span className="text-fg-muted">at {p.address}</span>
                  <Badge tone={p.trusts_us === false ? "warn" : p.trusts_us ? "ok" : "neutral"}>
                    {p.trusts_us === false ? "it refused this appliance" : p.trusts_us ? "trusts this appliance" : "trust back not known"}
                  </Badge>
                  <span className="text-fg-subtle">last read here {when(p.last_served_ms)}</span>
                  <Button size="sm" variant="danger-outline" className="ml-auto" onClick={() => setUntrusting(p)}>
                    Untrust
                  </Button>
                  <div className="w-full break-all font-mono text-[11px] text-fg-subtle">{p.fingerprint}</div>
                </div>
              ))}
            </CardBody>
          </Card>
        )}
        {data && data.refusals.length > 0 && (
          <Card>
            <CardHeader title="Refused connections" subtitle="Certificates no peer entry trusts; what they claim proves nothing" />
            <CardBody className="space-y-1 text-[12px]">
              {data.refusals.map((r) => (
                <div key={`${r.fingerprint}-${r.at_ms}`}>
                  {when(r.at_ms)} from {r.from}, claiming <span className="selectable">{r.claimed}</span>: <span className="font-mono">{r.fingerprint}</span>
                </div>
              ))}
            </CardBody>
          </Card>
        )}
      </PageBody>
      {untrusting && (
        <UntrustDialog
          peer={untrusting}
          close={() => setUntrusting(null)}
          done={() => {
            setUntrusting(null);
            void reload();
          }}
        />
      )}
    </>
  );
}
