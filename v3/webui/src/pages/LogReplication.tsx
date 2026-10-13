// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// Settings, then Log replication: what the host serving this page holds of
// each host's records, how far each peer has copied them, and how much of
// the retention budget is used (MONITORING.md, "In the Dashboard").

import { useCallback, useEffect, useState } from "react";
import { DatabaseBackup, Network } from "lucide-react";
import { Badge, Card, CardHeader, ago, bytes, dateTimeMs, useNow, type ReplicationView } from "@bmac/ui";
import { errorMessage } from "../lib/backend";
import { api } from "../state/store";
import { Table } from "./common";

const LAG_MS = 5 * 60_000;

function minutes(ms: number | null) {
  if (ms === null) return "";
  return ms < 60_000 ? "under a minute" : `${Math.round(ms / 60_000)} min`;
}

export function LogReplication() {
  const now = useNow(30000);
  const [view, setView] = useState<ReplicationView | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      setView(await api().logReplication());
      setError(null);
    } catch (e) {
      setError(errorMessage(e));
    }
  }, []);

  useEffect(() => void load(), [load]);

  if (!view)
    return (
      <Card className="mb-5" role="region" aria-label="Log replication">
        <CardHeader icon={<DatabaseBackup />} title="Log storage" />
        {error ? (
          <p role="alert" className="px-4 pb-4 text-[12.5px] text-danger">
            Log replication isn't available: {error}
          </p>
        ) : (
          <div className="space-y-3 px-4 pb-4" aria-busy="true">
            <div className="skeleton h-4 w-1/3" />
          </div>
        )}
      </Card>
    );

  const { retention } = view;
  const share = retention.budget_bytes ? Math.min(100, Math.round((retention.used_bytes / retention.budget_bytes) * 100)) : 100;
  return (
    <div role="region" aria-label="Log replication">
      <Card className="mb-5">
        <CardHeader
          icon={<DatabaseBackup />}
          title={`Log storage${view.host ? ` on ${view.host}` : ""}`}
          subtitle={`Keeps ${retention.keep_days} days within ${bytes(retention.budget_bytes)}; ${bytes(retention.used_bytes)} used (${share}%)`}
        />
        {retention.audit_evicted_ms !== null && (
          <p role="note" className="mx-4 mb-3 text-[12.5px] font-medium text-warn">
            Log storage ran out of room and evicted audit events {ago(retention.audit_evicted_ms, now)}.
          </p>
        )}
        <Table label="Records held" head={["Host", "Records", "Bytes", "Oldest kept", "Last sequence", "Gaps"]}>
          {view.origins.map((o) => (
            <tr key={o.origin}>
              <td className="font-mono text-[12px]">{o.origin}</td>
              <td className="tabular-nums">{o.records.toLocaleString()}</td>
              <td className="tabular-nums">{bytes(o.bytes)}</td>
              <td className="whitespace-nowrap text-fg-subtle">{o.oldest_ms ? dateTimeMs(o.oldest_ms) : ""}</td>
              <td className="font-mono text-[12px] tabular-nums">{o.last_seq}</td>
              <td className={o.gaps ? "text-warn tabular-nums" : "text-fg-subtle tabular-nums"}>{o.gaps}</td>
            </tr>
          ))}
        </Table>
      </Card>
      <Card className="mb-5">
        <CardHeader icon={<Network />} title="Copies on other hosts" subtitle="From each peer's holdings, read every minute" />
        {view.peers.length ? (
          <Table label="Peer copies" head={["Peer", "Of host", "Last sequence", "Behind", "Gaps", "Checked"]}>
            {view.peers.flatMap((p) =>
              (p.origins.length ? p.origins : [null]).map((o, i) => (
                <tr key={`${p.peer}/${o?.origin ?? ""}`}>
                  <td className="font-mono text-[12px]">{i === 0 ? p.peer : ""}</td>
                  <td className="font-mono text-[12px]">{o?.origin ?? ""}</td>
                  <td className="font-mono text-[12px] tabular-nums">{o?.last_seq ?? ""}</td>
                  <td className="tabular-nums">
                    {o && o.behind_records > 0 ? (
                      <span className={o.behind_ms !== null && o.behind_ms > LAG_MS ? "text-warn" : undefined}>
                        {o.behind_records} records{o.behind_ms !== null ? `, ${minutes(o.behind_ms)}` : ""}
                      </span>
                    ) : o ? (
                      <span className="text-fg-subtle">up to date</span>
                    ) : (
                      ""
                    )}
                  </td>
                  <td className="tabular-nums">{o?.gaps ?? ""}</td>
                  <td className="whitespace-nowrap">
                    {i === 0 &&
                      (p.error ? (
                        <Badge tone="warn" title={p.error}>
                          unreachable
                        </Badge>
                      ) : (
                        <span className="text-fg-subtle">{p.checked_ms ? ago(p.checked_ms, now) : "not yet"}</span>
                      ))}
                  </td>
                </tr>
              )),
            )}
          </Table>
        ) : (
          <p className="px-4 pb-4 text-[12.5px] text-fg-subtle">No peer's holdings have been read yet. Records stay on this host until a peer copies them.</p>
        )}
      </Card>
    </div>
  );
}
