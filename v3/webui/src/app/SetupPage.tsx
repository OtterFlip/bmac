// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// Setup mode (DEPLOYMENT.md, "Host setup job"): until the host's setup job
// finishes, the Dashboard shows only that job, its live output, and a way
// to start it.

import { useEffect, useState, type ReactNode } from "react";
import { Loader2, LogOut, Play, RotateCcw, Server } from "lucide-react";
import { Button, isTerminal, recordStatus, type JobRecord, type JoinProgress } from "@bmac/ui";
import { UnderTheHood } from "../components/console/UnderTheHood";
import { useStore } from "../state/store";

/** The newest setup job this tab knows about. */
function useSetupJob(): JobRecord | undefined {
  return useStore((s) => {
    const operation = s.setup?.operation;
    let newest: JobRecord | undefined;
    for (const record of Object.values(s.jobs)) {
      if (record.summary.operation !== operation) continue;
      if (!newest || record.summary.created_at_ms > newest.summary.created_at_ms) newest = record;
    }
    return newest;
  });
}

/** How often a joining host's page reads its progress again. */
const JOIN_REFRESH_MS = 10_000;

function JoinStatus({ join, cluster }: { join: JoinProgress; cluster: string | null | undefined }) {
  const where = join.host ?? "a member";
  if (join.state === "refused") {
    return (
      <p role="alert" className="text-[12.5px] leading-relaxed text-danger">
        The cluster refused this host's join request{join.message ? `: ${join.message}` : "."} Issue a new bundle from a member's Hosts page and build this host's ISO again.
      </p>
    );
  }
  if (join.state === "requesting") {
    return (
      <p role="status" className="text-[12.5px] leading-relaxed text-fg-muted">
        Asking the cluster{cluster ? ` ${cluster}` : ""} to let this host join…{join.message ? ` The last attempt failed: ${join.message}` : ""}
      </p>
    );
  }
  return (
    <div role="status" className="space-y-2 text-[12.5px] leading-relaxed text-fg-muted">
      <p className="text-fg">{join.message ?? `Waiting for an administrator to approve this host on ${where}.`}</p>
      <p>
        On {where}'s Dashboard, open Hosts and approve the request under Adding hosts. Its setup runs from there, and this page shows only this summary until it has finished.
        {join.dashboard_url && (
          <>
            {" "}
            <a className="font-mono text-accent underline" href={join.dashboard_url} target="_blank" rel="noopener noreferrer">
              {join.dashboard_url}
            </a>
          </>
        )}
      </p>
    </div>
  );
}

export function SetupPage({ panel }: { panel: ReactNode }) {
  const setup = useStore((s) => s.setup);
  const session = useStore((s) => s.session);
  const logout = useStore((s) => s.logout);
  const startSetup = useStore((s) => s.startSetup);
  const showInPanel = useStore((s) => s.showInPanel);
  const panelOpen = useStore((s) => s.panelOpen);
  const refreshSetup = useStore((s) => s.refreshSetup);
  const record = useSetupJob();
  const [starting, setStarting] = useState(false);
  const joining = setup?.join ?? null;
  useEffect(() => {
    if (!joining) return;
    const timer = setInterval(() => void refreshSetup(), JOIN_REFRESH_MS);
    return () => clearInterval(timer);
  }, [joining, refreshSetup]);
  if (!setup) return null;

  const status = record ? recordStatus(record) : null;
  const canStart = !joining && (!record || status === "failed" || status === "cancelled");
  const host = setup.host ?? location.hostname;
  const start = async () => {
    setStarting(true);
    try {
      await startSetup();
    } finally {
      setStarting(false);
    }
  };

  return (
    <div className="flex h-full flex-col">
      <div className="flex min-h-0 flex-1">
        <main className="grid-bg flex min-w-0 flex-1 flex-col overflow-y-auto px-8 py-8">
          <div className="mb-8 flex items-center gap-3">
            <img src="/bmac.svg" alt="" className="size-10 rounded-[11px]" />
            <div className="min-w-0 flex-1">
              <h1 className="text-[18px] font-semibold tracking-[-0.015em] text-fg">Set up {host}</h1>
              <p className="text-[12.5px] text-fg-subtle">{location.hostname}</p>
            </div>
            <span className="truncate text-[12.5px] text-fg-muted">{session?.user}</span>
            <Button variant="ghost" onClick={() => void logout()} aria-label="Log out">
              <LogOut /> Log out
            </Button>
          </div>

          <div className="max-w-[620px] space-y-4 rounded-xl border border-line bg-surface p-5 shadow-panel">
            <p className="text-[13px] leading-relaxed text-fg">
              {setup.new_cluster
                ? `This is the first host: setup creates the cluster${setup.cluster_name ? ` ${setup.cluster_name}` : ""} on it.`
                : `This host joins the cluster${setup.cluster_name ? ` ${setup.cluster_name}` : ""}.`}{" "}
              Setup checks the disks against the ones the ISO was built for, builds the storage, sets up the private network, and brings up the cluster and its routing.
            </p>
            <p className="text-[12.5px] leading-relaxed text-fg-muted">
              Until it finishes, this Dashboard offers only the setup job. It keeps running if you close this page or your browser; log in again from any browser to follow it and answer its questions. Some steps restart the host, and the page reconnects when it is back.
            </p>
            {joining && <JoinStatus join={joining} cluster={setup.cluster_name} />}
            <div className="flex flex-wrap items-center gap-2">
              {canStart && (
                <Button variant="primary" size="lg" disabled={starting} onClick={() => void start()}>
                  {starting ? <Loader2 className="animate-spin" /> : record ? <RotateCcw /> : <Play />} {record ? "Start setup again" : "Start setup"}
                </Button>
              )}
              {record && !panelOpen && (
                <Button variant="secondary" onClick={() => showInPanel(record.summary.id)}>
                  <Server /> Show the setup job
                </Button>
              )}
              {record && !isTerminal(status!) && <span className="text-[12.5px] text-fg-subtle">{record.pending_request ? "The setup job is waiting for you." : "Setup is running."}</span>}
            </div>
          </div>
        </main>
        {panel}
      </div>
      <UnderTheHood />
    </div>
  );
}
