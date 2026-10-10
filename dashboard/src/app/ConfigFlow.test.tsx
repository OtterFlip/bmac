import { afterEach, describe, expect, it } from "vitest";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { App } from "./App";

// A fresh install, through the mock backend's ?fresh mode: every essential
// config file is still a copy of its example.
describe("first launch", () => {
  afterEach(() => window.history.replaceState({}, "", "/"));

  it("starts on Settings, warns about unedited files, and links to the config page", async () => {
    window.history.replaceState({}, "", "/?fresh");
    const user = userEvent.setup();
    render(<App />);
    expect(await screen.findByRole("heading", { name: "Settings" })).toBeInTheDocument();
    expect(await screen.findByText(/Welcome to BMAC/)).toBeInTheDocument();
    expect(screen.getAllByText("not edited yet")).toHaveLength(4);
    expect(await screen.findByRole("button", { name: /Install Tailscale/ })).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "mox2.conf" }));
    expect(await screen.findByRole("heading", { name: "Config" })).toBeInTheDocument();
    const list = screen.getByRole("complementary", { name: "Config files" });
    expect(within(list).getByRole("button", { name: /mox2\.conf/ })).toHaveAttribute("aria-current", "true");
    expect(await screen.findByLabelText("Contents of mox2.conf")).toBeInTheDocument();

    await user.click(within(list).getByRole("button", { name: /^secrets\.env/ }));
    expect(await screen.findByText("secrets.env stays private")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /Open in text editor/ })).toBeInTheDocument();
  }, 15000);
});
