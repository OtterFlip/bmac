import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { App } from "@/app/App";
import { setBackend } from "@/lib/api";
import { createMockBackend } from "@/lib/mock/backend";

// Every config file is customized, but one dependency on this computer is not.
describe("settings banner", () => {
  it("reminds about missing dependencies after the config files are done", async () => {
    const backend = createMockBackend();
    const runPreflight = backend.runPreflight;
    backend.runPreflight = async () => [
      ...(await runPreflight()).filter((c) => c.id !== "jq"),
      { id: "jq", label: "jq", status: "warning", detail: "Not found.", command: "sudo apt install jq" },
    ];
    setBackend(backend);
    const user = userEvent.setup();
    render(<App />);
    await user.click(await screen.findByRole("button", { name: /^Settings/ }));
    expect(await screen.findByText(/Make sure all of the dependencies under "This computer" below are checked off before running workflows/)).toBeInTheDocument();
    expect(screen.queryByText(/Fill in your cluster's values/)).not.toBeInTheDocument();
  }, 15000);
});
