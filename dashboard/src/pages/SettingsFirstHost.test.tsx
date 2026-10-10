import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { App } from "@/app/App";
import { setBackend } from "@/lib/api";
import { createMockBackend } from "@/lib/mock/backend";

// Every config file and dependency is checked off, but no host can be read yet.
describe("settings first-host hint", () => {
  it("points to Add Proxmox host when no host is detected", async () => {
    const backend = createMockBackend();
    const listWorkflows = backend.listWorkflows;
    backend.listWorkflows = async () =>
      (await listWorkflows()).map((w) => (w.id === "list_hosts" ? { ...w, available: false, unavailable_reason: "No host is reachable." } : w));
    setBackend(backend);
    const user = userEvent.setup();
    render(<App />);
    await user.click(await screen.findByRole("button", { name: /^Settings/ }));
    expect(await screen.findByText(/deploy your first host using "Add Proxmox host" from the Hosts page/, undefined, { timeout: 3000 })).toBeInTheDocument();
    expect(screen.queryByText(/Fill in your cluster's values/)).not.toBeInTheDocument();
    expect(screen.queryByText(/Make sure all of the dependencies/)).not.toBeInTheDocument();
  }, 15000);
});
