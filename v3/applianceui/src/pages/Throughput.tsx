// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

import { TrendingUp } from "lucide-react";
import { Card, CardBody, CardHeader, EmptyState, bytes, type TransferRecord } from "@bmac/ui";
import { PageBody, PageHeader, Problem, short, useLoad, when } from "../parts";

const MIB = 1024 * 1024;

export function rate(t: TransferRecord): number {
  return t.duration_ms > 0 ? t.bytes / MIB / (t.duration_ms / 1000) : 0;
}

function Chart({ records }: { records: TransferRecord[] }) {
  const width = 720;
  const height = 160;
  const top = Math.max(1, ...records.map(rate));
  const bar = width / Math.max(records.length, 1);
  return (
    <svg viewBox={`0 0 ${width} ${height + 18}`} className="h-[190px] w-full" role="img" aria-label="MiB/s of each pull, oldest at the left">
      <line x1={0} y1={height} x2={width} y2={height} className="stroke-line" />
      {records.map((r, i) => {
        const h = (rate(r) / top) * (height - 8);
        return (
          <rect key={`${r.pit_uuid}-${r.at_ms}`} x={i * bar + 1} y={height - h} width={Math.max(bar - 2, 1)} height={h} className={r.from_peer ? "fill-info" : "fill-accent"}>
            <title>{`${rate(r).toFixed(1)} MiB/s, ${bytes(r.bytes)}, ${when(r.at_ms)}`}</title>
          </rect>
        );
      })}
      <text x={4} y={12} className="fill-fg-subtle text-[11px]">
        {top.toFixed(1)} MiB/s
      </text>
    </svg>
  );
}

export function ThroughputPage() {
  const { data, error } = useLoad((api) => api.throughput(), 10_000);
  const recent = data?.recent ?? [];
  return (
    <>
      <PageHeader title="Throughput" subtitle="Every verified pull: its size and speed. Repairs from a peer show in blue." />
      <PageBody>
        <Problem>{error}</Problem>
        {data && data.live.length > 0 && (
          <Card>
            <CardHeader title="Pulling now" />
            <CardBody className="space-y-1 text-[13px]">
              {data.live.map((t) => (
                <div key={t.pit_uuid}>
                  PIT <span className="font-mono">{short(t.pit_uuid)}</span> of VM <span className="font-mono">{short(t.vm_uuid)}</span>: {bytes(t.bytes)} so far
                </div>
              ))}
            </CardBody>
          </Card>
        )}
        {data && recent.length === 0 && <EmptyState icon={<TrendingUp />} title="No pulls yet" />}
        {recent.length > 0 && (
          <>
            <Card>
              <CardHeader title="Speed of each pull" icon={<TrendingUp />} />
              <CardBody>
                <Chart records={recent} />
              </CardBody>
            </Card>
            <Card>
              <table className="w-full text-[12.5px]">
                <thead className="text-left text-fg-subtle">
                  <tr className="border-b border-line">
                    <th className="px-3 py-2 font-medium">Finished</th>
                    <th className="px-3 py-2 font-medium">PIT</th>
                    <th className="px-3 py-2 font-medium">Size</th>
                    <th className="px-3 py-2 font-medium">Took</th>
                    <th className="px-3 py-2 font-medium">Speed</th>
                  </tr>
                </thead>
                <tbody>
                  {[...recent].reverse().map((r) => (
                    <tr key={`${r.pit_uuid}-${r.at_ms}`} className="border-b border-line/50">
                      <td className="px-3 py-1.5">{when(r.at_ms)}</td>
                      <td className="px-3 py-1.5 font-mono">{short(r.pit_uuid)}</td>
                      <td className="px-3 py-1.5">{bytes(r.bytes)}</td>
                      <td className="px-3 py-1.5">{(r.duration_ms / 1000).toFixed(1)} s</td>
                      <td className="px-3 py-1.5">
                        {rate(r).toFixed(1)} MiB/s{r.from_peer ? ` (from peer ${short(r.from_peer)})` : ""}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </Card>
          </>
        )}
      </PageBody>
    </>
  );
}
