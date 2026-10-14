// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// The PIT graph of one VM: time runs top to bottom, one row per PIT, the
// oldest at the top left. Each branch is a column; a branch takes the next
// column to the right when its first PIT appears, so branches extend to the
// right of the line they came from.

/** What the layout needs of a PIT. */
export interface GraphPit {
  pit_uuid: string;
  branch_uuid: string;
  parent: string | null;
  taken_s: number;
}

export interface GraphNode<P extends GraphPit = GraphPit> {
  pit: P;
  row: number;
  column: number;
}

export interface GraphEdge<P extends GraphPit = GraphPit> {
  from: GraphNode<P>;
  to: GraphNode<P>;
}

export interface Graph<P extends GraphPit = GraphPit> {
  nodes: GraphNode<P>[];
  edges: GraphEdge<P>[];
  columns: number;
}

export function pitLayout<P extends GraphPit>(pits: P[]): Graph<P> {
  const ordered = [...pits].sort((a, b) => a.taken_s - b.taken_s || a.pit_uuid.localeCompare(b.pit_uuid));
  const columnOf = new Map<string, number>();
  const byPit = new Map<string, GraphNode<P>>();
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
  const edges: GraphEdge<P>[] = [];
  for (const node of nodes) {
    const parent = node.pit.parent ? byPit.get(node.pit.parent) : undefined;
    if (parent) edges.push({ from: parent, to: node });
  }
  return { nodes, edges, columns: columnOf.size };
}
