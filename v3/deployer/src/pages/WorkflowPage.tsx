// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// A page that starts one workflow. The workflow asks for everything it
// needs in the task panel, so the page only explains it and lists its runs.

import { useMemo, useState } from "react";
import { Play } from "lucide-react";
import { Button, Card, CardBody, CardHeader, RunStatusIcon, recordStatus, type JobRecord, type TaskKind } from "@bmac/ui";
import { PageBody, PageHeader } from "../components/Page";
import { useStore } from "../state/store";

export interface Workflow {
  kind: TaskKind;
  title: string;
  subtitle: string;
  start: string;
  needs: string[];
  steps: string[];
}

export const discovery: Workflow = {
  kind: "discovery",
  title: "Discovery",
  subtitle: "Boot a machine from a discovery ISO to read its disks and network ports.",
  start: "Start discovery",
  needs: [
    "The machine's public IPv4 address, prefix, gateway, and DNS server.",
    "A Tailscale auth key for discovery. A reusable, ephemeral key is best; it is written only into the ISO.",
    "A way to boot the ISO: the iDRAC's Virtual Console, another remote console, or a USB stick.",
  ],
  steps: [
    "The Deployer builds a discovery ISO for this machine.",
    "You boot the machine from it. The Deployer waits for it on your tailnet.",
    "It reads the disks and network ports, and offers to clear old Proxmox pool labels.",
    "It turns the machine off and saves the inventory for the host's ISO.",
  ],
};

export const host: Workflow = {
  kind: "host",
  title: "Proxmox hosts",
  subtitle: "Build a host's ISO, boot it, and hand the host to its Dashboard.",
  start: "Install a host",
  needs: [
    "For the first host: the cluster's settings file. For another host: the join bundle from the Dashboard.",
    "The machine's inventory, from a discovery session now or a saved inventory file.",
    "The root password, and a Tailscale auth key for the host.",
  ],
  steps: [
    "You choose the host's settings and the disks for its pool.",
    "The Deployer builds the host's ISO, warns which disks it erases, and gives the boot steps.",
    "It waits for the host on your tailnet and checks the host's certificate.",
    "It offers to add the host to this workstation's SSH config, then gives you the Dashboard link. Host setup continues there.",
  ],
};

export function WorkflowPage({ workflow }: { workflow: Workflow }) {
  const startTask = useStore((s) => s.startTask);
  const [starting, setStarting] = useState(false);
  const start = async () => {
    setStarting(true);
    await startTask(workflow.kind);
    setStarting(false);
  };
  return (
    <>
      <PageHeader
        title={workflow.title}
        subtitle={workflow.subtitle}
        actions={
          <Button variant="primary" disabled={starting} onClick={() => void start()}>
            <Play />
            {workflow.start}
          </Button>
        }
      />
      <PageBody className="space-y-5">
        <Card>
          <CardHeader title="You'll need" />
          <CardBody>
            <ul className="list-disc space-y-1.5 pl-5 text-[13px] text-fg-muted">
              {workflow.needs.map((need) => (
                <li key={need}>{need}</li>
              ))}
            </ul>
          </CardBody>
        </Card>
        <Card>
          <CardHeader title="What happens" />
          <CardBody>
            <ol className="list-decimal space-y-1.5 pl-5 text-[13px] text-fg-muted">
              {workflow.steps.map((step) => (
                <li key={step}>{step}</li>
              ))}
            </ol>
          </CardBody>
        </Card>
        <Runs kind={workflow.kind} />
      </PageBody>
    </>
  );
}

// This session's runs of one kind of task.
export function Runs({ kind }: { kind: TaskKind }) {
  const tasks = useStore((s) => s.tasks);
  const order = useStore((s) => s.order);
  const runs = useMemo(() => order.map((id) => tasks[id]).filter((r): r is JobRecord => r?.summary.operation === kind), [order, tasks, kind]);
  const focus = useStore((s) => s.focus);
  if (runs.length === 0) return null;
  return (
    <Card>
      <CardHeader title="This session" />
      <CardBody className="divide-y divide-line p-0">
        {runs.map((record) => (
          <button
            key={record.summary.id}
            onClick={() => focus(record.summary.id)}
            className="flex w-full items-center gap-2 px-4 py-2.5 text-left text-[13px] text-fg hover:bg-surface-2"
          >
            <RunStatusIcon status={recordStatus(record)} />
            <span className="flex-1 truncate">{record.summary.title}</span>
            {record.summary.message && <span className="truncate text-[12px] text-fg-muted">{record.summary.message}</span>}
          </button>
        ))}
      </CardBody>
    </Card>
  );
}
