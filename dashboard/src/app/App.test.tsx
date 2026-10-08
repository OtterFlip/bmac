import { describe, expect, it } from "vitest";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { App } from "./App";

// Runs the whole app against the in-browser mock backend.
describe("App", () => {
  it("loads cluster state and opens a workflow with its exact command", async () => {
    const user = userEvent.setup();
    render(<App />);
    expect(await screen.findByRole("heading", { name: "Overview" })).toBeInTheDocument();
    expect(await screen.findByText("Quorate", {}, { timeout: 5000 })).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: /^Staging/ }));
    await user.click(await screen.findByRole("button", { name: /New staging VM/ }));
    const dialog = await screen.findByRole("dialog");
    expect(within(dialog).getByText(/add_staging_vm\.sh/)).toBeInTheDocument();
    expect(within(dialog).getByRole("button", { name: /Start dry run/ })).toBeInTheDocument();

    await user.click(within(dialog).getByRole("switch"));
    expect(within(dialog).getByRole("button", { name: /^Start$/ })).toBeInTheDocument();
  }, 15000);
});
