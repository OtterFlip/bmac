import { describe, expect, it } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { App } from "@/app/App";
import { setBackend } from "@/lib/api";
import { createMockBackend } from "@/lib/mock/backend";
import { useStore } from "@/state/store";

// Everything is checked off and the dashboard page has read the mock's hosts.
describe("settings first-host hint", () => {
  it("is not shown once hosts are detected", async () => {
    setBackend(createMockBackend());
    const user = userEvent.setup();
    render(<App />);
    await waitFor(() => expect(useStore.getState().sources.list_hosts.data?.hosts.length).toBeGreaterThan(0), { timeout: 5000 });
    await user.click(screen.getByRole("button", { name: /^Settings/ }));
    expect(await screen.findByText(/Cluster-wide settings every workflow reads/)).toBeInTheDocument();
    expect(await screen.findByText("jq", undefined, { timeout: 3000 })).toBeInTheDocument();
    expect(screen.queryByText(/deploy your first host/)).not.toBeInTheDocument();
    expect(screen.queryByText(/prepare your cluster's QDevice/)).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Settings" })).toBeInTheDocument();
  }, 15000);
});
