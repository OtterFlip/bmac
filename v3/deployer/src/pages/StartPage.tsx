// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

import { Copy, ExternalLink, RefreshCw } from "lucide-react";
import { Button, Card, CardBody, CardHeader, Skeleton, StatusPill, type Check, type Health } from "@bmac/ui";
import { PageBody, PageHeader } from "../components/Page";
import { useStore } from "../state/store";
import { toast } from "../state/toast";

const health: Record<Check["status"], Health> = { ok: "ok", warning: "warn", error: "danger" };
const statusLabel: Record<Check["status"], string> = { ok: "Ready", warning: "Warning", error: "Missing" };

export function StartPage() {
  const checks = useStore((s) => s.checks);
  const checking = useStore((s) => s.checking);
  const recheck = useStore((s) => s.recheck);
  const info = useStore((s) => s.info);
  const errors = checks?.filter((c) => c.status === "error").length ?? 0;
  const warnings = checks?.filter((c) => c.status === "warning").length ?? 0;

  return (
    <>
      <PageHeader
        title={info?.first_launch ? "Welcome to the BMAC Deployer" : "Start"}
        subtitle="Installs Proxmox hosts and backup appliances, then hands each one to its Dashboard."
        actions={
          <Button onClick={() => void recheck()} disabled={checking}>
            <RefreshCw className={checking ? "animate-spin" : undefined} />
            Check again
          </Button>
        }
      />
      <PageBody className="space-y-5">
        <Card>
          <CardHeader
            title="This workstation"
            subtitle={
              checks === null
                ? "Checking the commands the Deployer uses…"
                : errors
                  ? `${errors} required ${errors === 1 ? "item is" : "items are"} missing. Install ${errors === 1 ? "it" : "them"} before deploying.`
                  : warnings
                    ? `Ready, with ${warnings} ${warnings === 1 ? "warning" : "warnings"}.`
                    : "Everything the Deployer needs is here."
            }
          />
          <CardBody className="divide-y divide-line p-0">
            {checks === null
              ? [0, 1, 2].map((i) => (
                  <div key={i} className="px-4 py-3">
                    <Skeleton className="h-4 w-1/2" />
                  </div>
                ))
              : checks.map((check) => <CheckRow key={check.id} check={check} />)}
          </CardBody>
        </Card>
        <Card>
          <CardHeader title="How a deployment goes" />
          <CardBody>
            <ol className="list-decimal space-y-1.5 pl-5 text-[13px] text-fg-muted">
              <li>Boot the machine from a discovery ISO to read its disks and network ports.</li>
              <li>Build its Proxmox ISO from what discovery found and the choices you make.</li>
              <li>Install it yourself: mount the ISO in the iDRAC's Virtual Console or another remote console, or boot it from USB. The Deployer gives the steps and waits for the host to join your tailnet.</li>
              <li>Check the host's certificate fingerprint, then open its Dashboard to finish setup.</li>
            </ol>
          </CardBody>
        </Card>
      </PageBody>
    </>
  );
}

function CheckRow({ check }: { check: Check }) {
  const backend = useStore((s) => s.backend);
  const copy = (command: string) =>
    navigator.clipboard.writeText(command).then(
      () => toast.success("Copied", command),
      () => toast.error("Couldn't copy the command"),
    );
  return (
    <div className="flex items-start gap-3 px-4 py-3">
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2">
          <span className="text-[13px] font-medium text-fg">{check.label}</span>
          <StatusPill health={health[check.status]}>{statusLabel[check.status]}</StatusPill>
        </div>
        {check.purpose && <p className="mt-0.5 text-[12.5px] text-fg-muted">{check.purpose}</p>}
        <p className="selectable mt-0.5 font-mono text-[12px] text-fg-subtle">{check.detail}</p>
        {check.fix && <p className="mt-1.5 text-[12.5px] text-fg">{check.fix}</p>}
        {check.command && (
          <div className="mt-1.5 flex items-center gap-2">
            <code className="selectable rounded border border-line bg-sunken px-2 py-1 font-mono text-[12px] text-fg">{check.command}</code>
            <Button size="icon-sm" variant="ghost" aria-label={`Copy ${check.command}`} onClick={() => void copy(check.command!)}>
              <Copy />
            </Button>
          </div>
        )}
        {check.link && (
          <Button variant="link" className="mt-1.5" onClick={() => void backend?.openExternal(check.link!.url)}>
            {check.link.label}
            <ExternalLink />
          </Button>
        )}
      </div>
    </div>
  );
}
