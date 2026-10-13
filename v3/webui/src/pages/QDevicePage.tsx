// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

import { Plus, Scale, Trash2 } from "lucide-react";
import { Card, CardHeader, KV, StatusPill } from "@bmac/ui";
import { PageBody, PageHeader } from "../components/layout/Page";
import { useStatus } from "../state/useStatus";
import { ConditionsBanner, OperationButton, OperationTiles, SectionTitle, StatusMissing, StatusRefresh } from "./common";

export const QDEVICE_SETUP_URL = "https://github.com/OtterFlip/bmac/blob/main/docs/QDEVICE_MANUAL_SETUP.md";

type Verdict = { health: "ok" | "warn" | "danger" | "unknown"; title: string; body: string; action?: "add" | "remove" };

export function QDevicePage() {
  const hosts = useStatus("hosts");
  const data = hosts.data;
  const qd = data?.qdevice;
  const count = data?.cluster.host_count ?? 0;

  let verdict: Verdict = { health: "unknown", title: "Unknown", body: "The cluster's QDevice state hasn't been read yet." };
  if (qd && data) {
    if (qd.registered && qd.voting === false)
      verdict = {
        health: "danger",
        title: "Registered but not voting",
        body: "The QDevice is configured in corosync but isn't contributing a vote. Check the qnetd service on the QDevice host, then inspect the QDevice.",
      };
    else if (qd.registered && qd.needed === false)
      verdict = { health: "warn", title: "Registered, but not needed", body: `The cluster has ${count} hosts, an odd number, so a QDevice adds no resilience. BMAC removes it when the host count becomes odd.`, action: "remove" };
    else if (qd.registered) verdict = { health: "ok", title: "Providing the tie-breaking vote", body: `With ${count} hosts, the QDevice lets the cluster keep quorum when half the hosts are down.` };
    else if (qd.needed) verdict = { health: "warn", title: "Needed but not registered", body: `The cluster has ${count} hosts. Without a QDevice, losing half of them loses quorum.`, action: "add" };
    else if (qd.needed === false) verdict = { health: "ok", title: "Not needed", body: `The cluster has ${count} hosts, an odd number, so it doesn't need a tie-breaker.` };
  }
  const conditions = data?.conditions.filter((c) => c.id.startsWith("qdevice.")) ?? [];

  return (
    <>
      <PageHeader title="QDevice" subtitle="The external tie-breaking vote for even-sized clusters" actions={<StatusRefresh kinds={["hosts"]} />} />
      <PageBody>
        <ConditionsBanner conditions={conditions} />
        <div className="mb-4 grid gap-4 xl:grid-cols-[minmax(0,1fr)_340px]">
          <Card>
            <CardHeader icon={<Scale />} title="Status" />
            {data ? (
              <div className="p-5">
                <StatusPill health={verdict.health}>{verdict.title}</StatusPill>
                <p className="mt-3 max-w-[560px] text-[13px] leading-relaxed text-fg-muted">{verdict.body}</p>
                {verdict.action === "add" && <OperationButton id="qdevice.add" variant="primary" size="md" icon={<Plus />} label="Add QDevice" className="mt-4 inline-block" />}
                {verdict.action === "remove" && <OperationButton id="qdevice.remove" variant="danger-outline" size="md" icon={<Trash2 />} label="Remove QDevice" className="mt-4 inline-block" />}
              </div>
            ) : (
              <StatusMissing entry={hosts} what="QDevice status" icon={<Scale />} />
            )}
          </Card>
          <Card>
            <CardHeader title="Details" />
            <div className="p-4">
              <KV
                items={[
                  ["Registered", qd?.registered === null || qd?.registered === undefined ? "—" : qd.registered ? "yes" : "no"],
                  ["Address", <span key="a" className="font-mono">{qd?.address ?? "—"}</span>],
                  ["Voting", qd?.voting === null || qd?.voting === undefined ? "—" : qd.voting ? "yes" : "no"],
                  ["Host count", data ? `${count} (${count % 2 === 0 ? "even" : "odd"})` : "—"],
                  ["Votes", data && data.cluster.total_votes !== null ? `${data.cluster.total_votes} of ${data.cluster.expected_votes}` : "—"],
                ]}
              />
            </div>
          </Card>
        </div>
        <SectionTitle>QDevice operations</SectionTitle>
        <OperationTiles
          links={[{ title: "QDevice Setup Prereq", summary: "Prepare the QDevice machine and reach it over Tailscale before adding it.", url: QDEVICE_SETUP_URL }]}
          ids={["qdevice.add", "qdevice.remove", "diagnostics.qdevice_state"]}
        />
      </PageBody>
    </>
  );
}
