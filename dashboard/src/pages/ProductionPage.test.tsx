import { describe, expect, it } from "vitest";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { App } from "@/app/App";
import { setBackend } from "@/lib/api";
import { createMockBackend } from "@/lib/mock/backend";

describe("Production page monitoring", () => {
  it("offers the Monitored services tile", async () => {
    setBackend(createMockBackend());
    const user = userEvent.setup();
    render(<App />);
    await user.click((await screen.findAllByRole("button", { name: "Production VMs" }))[0]);
    const operations = (await screen.findByText("Production operations")).nextElementSibling as HTMLElement;
    const titles = within(operations).getAllByRole("button").map((b) => b.textContent ?? "");
    expect(titles.some((t) => t.includes("Monitored services"))).toBe(true);
  }, 15000);
});
