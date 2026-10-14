// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// The PIT graph of one VM: time runs top to bottom, one row per PIT, the
// oldest at the top left. Each branch is a column; a branch takes the next
// column to the right when its first PIT appears, so branches extend to the
// right of the line they came from.

import type { PitView } from "@bmac/ui";

export interface GraphNode {
  pit: PitView;
  row: number;
  column: number;
}

export interface GraphEdge {
  from: GraphNode;
  to: GraphNode;
}

export interface Graph {
  nodes: GraphNode[];
  edges: GraphEdge[];
  columns: number;
}

export function layout(pits: PitView[]): Graph {
  const ordered = [...pits].sort((a, b) => a.taken_s - b.taken_s || a.pit_uuid.localeCompare(b.pit_uuid));
  const columnOf = new Map<string, number>();
  const byPit = new Map<string, GraphNode>();
  const nodes = ordered.map((pit, row) => {
    let column = columnOf.get(pit.branch_uuid);
    if (column === undefined) {
      column = columnOf.size;
      columnOf.set(pit.branch_uuid, column);
    }
    const node = { pit, row, column };
    byPit.set(pit.pit_uuid, node);
    return node;
  });
  const edges: GraphEdge[] = [];
  for (const node of nodes) {
    const parent = node.pit.parent ? byPit.get(node.pit.parent) : undefined;
    if (parent) edges.push({ from: parent, to: node });
  }
  return { nodes, edges, columns: columnOf.size };
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
