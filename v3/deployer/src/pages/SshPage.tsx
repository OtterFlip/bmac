// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// Workstation SSH set up after a host's handoff: a host this workstation
// didn't install, a guest through its host, and the hello app, which is
// deployed over that guest SSH.

import { useCallback, useEffect, useMemo, useState } from "react";
import { Play, Trash2 } from "lucide-react";
import { Button, Card, CardBody, CardHeader, Dialog, EmptyState, StatusPill, type SshEntry, type TaskKind } from "@bmac/ui";
import { PageBody, PageHeader } from "../components/Page";
import { errorText } from "../lib/backend";
import { useStore } from "../state/store";
import { toast } from "../state/toast";
import { Runs } from "./WorkflowPage";

interface Action {
  kind: TaskKind;
  title: string;
  start: string;
  description: string;
  steps: string[];
}

const actions: Action[] = [
  {
    kind: "host_ssh",
    title: "A host",
    start: "Set up SSH to a host",
    description: "For a host installed from another workstation. Hosts installed from this one were offered SSH at handoff.",
    steps: [
      "The Deployer finds the host on your tailnet and reads its SSH host key.",
      "You compare that key's fingerprint with the one in the Dashboard's Settings, under Certificates and SSH host keys. Nothing is written unless they match.",
      "It pins the key in ~/.ssh/config and ~/.ssh/known_hosts and checks that ssh works.",
      "It offers the cluster's other hosts, using the keys the cluster records for them, and reports entries left for hosts that have left the cluster.",
    ],
  },
  {
    kind: "guest_ssh",
    title: "A guest",
    start: "Set up SSH to a guest",
    description: "Reach a production or staging guest through one of the cluster's hosts. Needs SSH to at least one host first.",
    steps: [
      "You choose a running guest from the cluster's list.",
      "The cluster reads the guest's SSH host key from inside the guest, through the guest agent.",
      "The Deployer pins that key, with the host running the guest as the jump host, and checks that ssh works.",
    ],
  },
  {
    kind: "hello",
    title: "The hello app",
    start: "Deploy the hello app",
    description: "A static page on a production guest, to check its domains end to end. Needs SSH to the guest first.",
    steps: [
      "You choose an active production guest, and paste the Cloudflare Origin certificate and key for its domains. The key is held only in memory.",
      "The Deployer checks that the certificate matches the key, is current, and covers every production and staging name.",
      "After you confirm, it sends the files over SSH and installs the NGINX site. Nothing in Proxmox changes.",
    ],
  },
];

export function SshPage() {
  return (
    <>
      <PageHeader title="Workstation SSH" subtitle="SSH entries on this workstation for hosts and guests." />
      <PageBody className="space-y-5">
        <Entries />
        {actions.map((action) => (
          <ActionCard key={action.kind} action={action} />
        ))}
      </PageBody>
    </>
  );
}

function ActionCard({ action }: { action: Action }) {
  const startTask = useStore((s) => s.startTask);
  const [starting, setStarting] = useState(false);
  const start = async () => {
    setStarting(true);
    await startTask(action.kind);
    setStarting(false);
  };
  return (
    <section aria-label={action.title} className="space-y-3">
      <Card>
        <CardHeader
          title={action.title}
          actions={
            <Button variant="primary" disabled={starting} onClick={() => void start()}>
              <Play />
              {action.start}
            </Button>
          }
        />
        <CardBody className="space-y-2">
          <p className="text-[13px] text-fg-muted">{action.description}</p>
          <ol className="list-decimal space-y-1.5 pl-5 text-[13px] text-fg-muted">
            {action.steps.map((step) => (
              <li key={step}>{step}</li>
            ))}
          </ol>
        </CardBody>
      </Card>
      <Runs kind={action.kind} />
    </section>
  );
}

function Entries() {
  const backend = useStore((s) => s.backend);
  const tasks = useStore((s) => s.tasks);
  const [entries, setEntries] = useState<SshEntry[] | null>(null);
  const [removing, setRemoving] = useState<SshEntry | null>(null);
  // Re-read whenever an SSH task ends, since it may have written entries.
  const finished = useMemo(
    () => Object.values(tasks).filter((r) => (r.summary.operation === "host_ssh" || r.summary.operation === "guest_ssh") && ["COMPLETED", "FAILED", "CANCELLED"].includes(r.summary.state)).length,
    [tasks],
  );
  const load = useCallback(() => {
    if (!backend) return;
    backend.sshEntries().then(setEntries, (error) => toast.error("Couldn't read ~/.ssh/config", errorText(error)));
  }, [backend]);
  useEffect(load, [load, finished]);
  const remove = async (alias: string) => {
    setRemoving(null);
    try {
      setEntries(await backend!.removeSshEntry(alias));
    } catch (error) {
      toast.error("Couldn't remove the entry", errorText(error));
    }
  };
  return (
    <Card>
      <CardHeader title="Managed entries" />
      <CardBody className={entries?.length ? "divide-y divide-line p-0" : undefined}>
        {entries === null ? null : entries.length === 0 ? (
          <EmptyState title="No entries" body="Hosts and guests set up from this Deployer appear here." />
        ) : (
          entries.map((entry) => (
            <div key={entry.alias} className="flex items-center gap-3 px-4 py-3">
              <div className="min-w-0 flex-1">
                <div className="flex items-center gap-2">
                  <span className="font-mono text-[13px] font-medium text-fg">ssh {entry.names[0] ?? entry.alias}</span>
                  <span className="text-[12px] text-fg-subtle">{entry.kind === "host" ? "Host" : "Guest"}</span>
                  {entry.problem && <StatusPill health="warn">Needs repair</StatusPill>}
                </div>
                <div className="selectable truncate font-mono text-[12px] text-fg-subtle">
                  {entry.names.join(" ")} → {entry.address}
                  {entry.jump ? ` through ${entry.jump}` : ""}
                </div>
                {entry.problem && <p className="text-[12px] text-fg-muted">{entry.problem}</p>}
              </div>
              <Button variant="danger-outline" size="sm" aria-label={`Remove ${entry.alias}`} onClick={() => setRemoving(entry)}>
                <Trash2 />
                Remove
              </Button>
            </div>
          ))
        )}
      </CardBody>
      <Dialog
        open={removing !== null}
        onOpenChange={(open) => !open && setRemoving(null)}
        tone="danger"
        icon={<Trash2 />}
        title="Remove this SSH entry?"
        description="Its block leaves ~/.ssh/config and its pinned key leaves ~/.ssh/known_hosts. Both files are backed up first. Nothing on the cluster changes."
        footer={
          <>
            <Button onClick={() => setRemoving(null)}>Keep</Button>
            <Button variant="danger" onClick={() => void remove(removing!.alias)}>
              Remove
            </Button>
          </>
        }
      >
        {removing && <p className="selectable font-mono text-[12px] text-fg-muted">{removing.names.join(" ")}</p>}
      </Dialog>
    </Card>
  );
}
