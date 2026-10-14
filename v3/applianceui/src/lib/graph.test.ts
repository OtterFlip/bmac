// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

import { describe, expect, it } from "vitest";
import type { PitView } from "@bmac/ui";
import { byVm, layout } from "./graph";

function pit(id: string, taken_s: number, branch: string, parent: string | null, vm = "vm1"): PitView {
  return {
    cluster_uuid: "c1",
    cluster_name: "prod",
    vm_uuid: vm,
    branch_uuid: branch,
    pit_uuid: id,
    parent,
    name: `bmac-pit-${id}`,
    taken_s,
    published_at_ms: 0,
    bytes: 1,
    volumes: 1,
    received_from: parent,
    held: false,
    suspect: false,
    granted: false,
  };
}

describe("the PIT graph", () => {
  it("runs time down the page, the oldest at the top left, and branches to the right", () => {
    // main: a -> b -> d; branch x forks from b with c, then e.
    const graph = layout([pit("d", 40, "main", "b"), pit("c", 30, "x", "b"), pit("a", 10, "main", null), pit("e", 50, "x", "c"), pit("b", 20, "main", "a")]);
    expect(graph.nodes.map((n) => [n.pit.pit_uuid, n.row, n.column])).toEqual([
      ["a", 0, 0],
      ["b", 1, 0],
      ["c", 2, 1],
      ["d", 3, 0],
      ["e", 4, 1],
    ]);
    expect(graph.columns).toBe(2);
    expect(graph.edges.map((e) => `${e.from.pit.pit_uuid}>${e.to.pit.pit_uuid}`).sort()).toEqual(["a>b", "b>c", "b>d", "c>e"]);
  });

  it("draws no edge to a parent that was pruned", () => {
    const graph = layout([pit("b", 20, "main", "gone"), pit("c", 30, "main", "b")]);
    expect(graph.edges.map((e) => e.to.pit.pit_uuid)).toEqual(["c"]);
    expect(graph.nodes[0].row).toBe(0);
  });

  it("groups PITs by VM", () => {
    const groups = byVm([pit("a", 1, "m", null, "vm2"), pit("b", 1, "m", null, "vm1"), pit("c", 2, "m", "b", "vm1")]);
    expect(groups.map((g) => [g.vm, g.pits.length])).toEqual([
      ["vm1", 2],
      ["vm2", 1],
    ]);
  });
});
