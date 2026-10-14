// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// The lines and dots of a PIT graph (`lib/pitgraph.ts`); the caller draws
// each row's text beside it, one row of `PIT_ROW` pixels per PIT.

import type { Graph, GraphNode, GraphPit } from "../lib/pitgraph";
import { cn } from "../lib/utils";

export const PIT_ROW = 32;
const COLUMN = 22;
const PAD = 14;

const BRANCH_COLORS = ["stroke-accent fill-accent", "stroke-info fill-info", "stroke-ok fill-ok", "stroke-warn fill-warn", "stroke-run fill-run"];

export function branchColor(column: number) {
  return BRANCH_COLORS[column % BRANCH_COLORS.length];
}

export function PitGraphLines<P extends GraphPit>({
  graph,
  selected,
  pick,
  nodeClass,
  title,
}: {
  graph: Graph<P>;
  selected?: (pit: P) => boolean;
  pick?: (pit: P) => void;
  /** Overrides the branch color, such as for a suspect PIT. */
  nodeClass?: (node: GraphNode<P>) => string | undefined;
  title: (pit: P) => string;
}) {
  const width = PAD * 2 + Math.max(0, graph.columns - 1) * COLUMN;
  const height = graph.nodes.length * PIT_ROW;
  const x = (column: number) => PAD + column * COLUMN;
  const y = (row: number) => row * PIT_ROW + PIT_ROW / 2;
  return (
    <svg width={width} height={height} className="shrink-0" role="img" aria-label="PIT graph, oldest at the top">
      {graph.edges.map((e) => (
        <path
          key={`${e.from.pit.pit_uuid}-${e.to.pit.pit_uuid}`}
          d={
            e.from.column === e.to.column
              ? `M${x(e.from.column)},${y(e.from.row)} L${x(e.to.column)},${y(e.to.row)}`
              : `M${x(e.from.column)},${y(e.from.row)} C${x(e.from.column)},${y(e.to.row)} ${x(e.to.column)},${y(e.from.row)} ${x(e.to.column)},${y(e.to.row)}`
          }
          className={cn("fill-none", branchColor(e.to.column).split(" ")[0])}
          strokeWidth={2}
        />
      ))}
      {graph.nodes.map((n) => {
        const on = selected?.(n.pit) ?? false;
        return (
          <circle
            key={n.pit.pit_uuid}
            cx={x(n.column)}
            cy={y(n.row)}
            r={on ? 7 : 5}
            className={cn(nodeClass?.(n) ?? branchColor(n.column), pick && "cursor-pointer")}
            strokeWidth={on ? 3 : 1}
            onClick={pick ? () => pick(n.pit) : undefined}
          >
            <title>{title(n.pit)}</title>
          </circle>
        );
      })}
    </svg>
  );
}
