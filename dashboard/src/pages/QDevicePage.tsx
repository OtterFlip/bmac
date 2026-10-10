import { Plus, Scale, Trash2 } from "lucide-react";
import { useStore } from "@/state/store";
import { Card, CardHeader } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { EmptyState, KV, StatusPill } from "@/components/status";
import { PageBody, PageHeader } from "@/components/layout/Page";
import { QDEVICE_SETUP_URL, SourceRefresh, WorkflowTiles, useSource } from "./common";

export function QDevicePage() {
  const { data, status, error } = useSource("list_hosts");
  const launchWorkflow = useStore((s) => s.launchWorkflow);
  const qd = data?.qdevice;
  const even = data ? data.cluster.host_count % 2 === 0 : null;

  let verdict: { health: "ok" | "warn" | "danger" | "unknown"; title: string; body: string; action?: "add" | "remove" } = {
    health: "unknown",
    title: "Unknown",
    body: "Refresh to read the cluster.",
  };
  if (qd && data) {
    if (qd.registered && qd.voting === false) verdict = { health: "danger", title: "Registered but not voting", body: "The QDevice is configured in corosync but isn't contributing a vote. Check the qnetd service on the QDevice host, then run the QDevice state diagnostic." };
    else if (qd.registered && qd.needed === false) verdict = { health: "warn", title: "Registered, but not needed", body: `The cluster has ${data.cluster.host_count} hosts, an odd number, so a QDevice adds no resilience. BMAC removes it when the host count becomes odd.`, action: "remove" };
    else if (qd.registered) verdict = { health: "ok", title: "Providing the tie-breaking vote", body: `With ${data.cluster.host_count} hosts, the QDevice lets the cluster keep quorum when half the hosts are down.` };
    else if (qd.needed) verdict = { health: "warn", title: "Needed but not registered", body: `The cluster has ${data.cluster.host_count} hosts. Without a QDevice, losing half of them loses quorum.`, action: "add" };
    else verdict = { health: "ok", title: "Not needed", body: `The cluster has ${data.cluster.host_count} hosts, an odd number, so it doesn't need a tie-breaker.` };
  }

  return (
    <>
      <PageHeader title="QDevice" subtitle="The external tie-breaking vote for even-sized clusters" actions={<SourceRefresh ids={["list_hosts"]} />} />
      <PageBody>
        <div className="mb-4 grid gap-4 xl:grid-cols-[minmax(0,1fr)_340px]">
          <Card>
            <CardHeader icon={<Scale />} title="Status" />
            {!data && status !== "loading" ? (
              <EmptyState icon={<Scale />} title="Not loaded" body={error ?? "Refresh to read the cluster."} />
            ) : (
              <div className="p-5">
                <StatusPill health={status === "loading" && !data ? "unknown" : verdict.health}>{status === "loading" && !data ? "reading…" : verdict.title}</StatusPill>
                <p className="mt-3 max-w-[560px] text-[13px] leading-relaxed text-fg-muted">{verdict.body}</p>
                {verdict.action === "add" && (
                  <Button variant="primary" className="mt-4" onClick={() => launchWorkflow("add_qdevice")}>
                    <Plus /> Add QDevice
                  </Button>
                )}
                {verdict.action === "remove" && (
                  <Button variant="danger-outline" className="mt-4" onClick={() => launchWorkflow("remove_qdevice")}>
                    <Trash2 /> Remove QDevice
                  </Button>
                )}
              </div>
            )}
          </Card>
          <Card>
            <CardHeader title="Details" />
            <div className="p-4">
              <KV
                items={[
                  ["Configured host", <span key="h" className="font-mono">{qd?.configured_host ?? "—"}</span>],
                  ["Registered", qd ? (qd.registered ? "yes" : "no") : "—"],
                  ["Address", <span key="a" className="font-mono">{qd?.address ?? "—"}</span>],
                  ["Voting", qd?.voting === null || qd?.voting === undefined ? "—" : qd.voting ? "yes" : "no"],
                  ["Host count", data ? `${data.cluster.host_count} (${even ? "even" : "odd"})` : "—"],
                  ["Votes", data?.cluster.total_votes !== null && data ? `${data.cluster.total_votes} of ${data.cluster.expected_votes}` : "—"],
                ]}
              />
            </div>
          </Card>
        </div>
        <h2 className="mb-2.5 text-[12px] font-semibold uppercase tracking-[0.08em] text-fg-subtle">QDevice operations</h2>
        <WorkflowTiles
          links={[{ title: "QDevice Setup Prereq", summary: "Prepare the QDevice machine and reach it over Tailscale before adding it.", url: QDEVICE_SETUP_URL }]}
          ids={["add_qdevice", "remove_qdevice", "show_qdevice_state"]}
        />
      </PageBody>
    </>
  );
}
