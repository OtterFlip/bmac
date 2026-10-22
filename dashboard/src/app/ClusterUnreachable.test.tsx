import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import { useStore } from "@/state/store";
import { SOURCE_IDS, type SourceId } from "@/protocol/state";
import { ClusterUnreachable } from "./App";

type Status = "ok" | "error" | "idle";

const entry = (status: Status) => ({
  data: null,
  updatedAt: null,
  status,
  error: status === "error" ? "No reachable mox jump host was found" : null,
  runId: null,
  nextSteps: [],
});

function setSources(statuses: Partial<Record<SourceId, Status>>) {
  useStore.setState({ sources: Object.fromEntries(SOURCE_IDS.map((id) => [id, entry(statuses[id] ?? "idle")])) as never });
}

describe("ClusterUnreachable", () => {
  it("says so only while every cluster read has failed", () => {
    setSources({ list_hosts: "error" });
    const { rerender } = render(<ClusterUnreachable />);
    expect(screen.getByRole("alert")).toHaveTextContent(/Can't reach the cluster.*No reachable mox jump host/);

    // The QDevice host answering directly says nothing about the cluster.
    setSources({ list_hosts: "error", check_qdevice_access: "ok" });
    rerender(<ClusterUnreachable />);
    expect(screen.getByRole("alert")).toBeInTheDocument();

    setSources({ list_hosts: "error", list_guests: "ok" });
    rerender(<ClusterUnreachable />);
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();

    setSources({});
    rerender(<ClusterUnreachable />);
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });
});
