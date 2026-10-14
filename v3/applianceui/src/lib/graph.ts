// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// The appliance's PITs grouped for the graph, one per VM; the layout is
// shared with the Dashboard (`@bmac/ui` `pitLayout`).

import { pitLayout, type Graph, type PitView } from "@bmac/ui";

export function layout(pits: PitView[]): Graph<PitView> {
  return pitLayout(pits);
}

/** The PITs of each VM, by cluster and VM, in a stable order. */
export function byVm(pits: PitView[]): { key: string; cluster: string; clusterName: string; vm: string; pits: PitView[] }[] {
  const groups = new Map<string, { key: string; cluster: string; clusterName: string; vm: string; pits: PitView[] }>();
  for (const pit of pits) {
    const key = `${pit.cluster_uuid}/${pit.vm_uuid}`;
    let group = groups.get(key);
    if (!group) {
      group = { key, cluster: pit.cluster_uuid, clusterName: pit.cluster_name, vm: pit.vm_uuid, pits: [] };
      groups.set(key, group);
    }
    group.pits.push(pit);
  }
  return [...groups.values()].sort((a, b) => a.key.localeCompare(b.key));
}
