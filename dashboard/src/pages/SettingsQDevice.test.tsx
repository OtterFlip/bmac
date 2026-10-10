import { describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { App } from "@/app/App";
import { setBackend } from "@/lib/api";
import { createMockBackend } from "@/lib/mock/backend";

const URL = "https://github.com/OtterFlip/bmac/blob/main/docs/QDEVICE_MANUAL_SETUP.md";

// Every config file and dependency is checked off, but neither a host nor the QDevice can be reached.
describe("settings QDevice prerequisite", () => {
  it("asks for the QDevice before the first host, linking to its setup guide", async () => {
    const backend = createMockBackend();
    const listWorkflows = backend.listWorkflows;
    backend.listWorkflows = async () =>
      (await listWorkflows()).map((w) =>
        w.id === "list_hosts" || w.id === "check_qdevice_access" ? { ...w, available: false, unavailable_reason: "Not reachable." } : w,
      );
    const openExternal = vi.fn(async () => {});
    backend.openExternal = openExternal;
    setBackend(backend);
    const user = userEvent.setup();
    render(<App />);
    await user.click(await screen.findByRole("button", { name: /^Settings/ }));
    expect(await screen.findByText(/you must prepare your cluster's QDevice/, undefined, { timeout: 3000 })).toBeInTheDocument();
    expect(screen.queryByText(/deploy your first host/)).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Settings!" })).toBeInTheDocument();
    const link = screen.getByRole("link", { name: "these instructions" });
    expect(link).toHaveAttribute("href", URL);
    await user.click(link);
    expect(openExternal).toHaveBeenCalledWith(URL);
  }, 15000);
});
