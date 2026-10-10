import { describe, expect, it } from "vitest";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { App } from "@/app/App";
import { setBackend } from "@/lib/api";
import { createMockBackend } from "@/lib/mock/backend";

describe("QDevice page operations", () => {
  it("offers the setup guide ahead of Add QDevice", async () => {
    setBackend(createMockBackend());
    const user = userEvent.setup();
    render(<App />);
    await user.click((await screen.findAllByRole("button", { name: "QDevice" }))[0]);
    const operations = (await screen.findByText("QDevice operations")).nextElementSibling as HTMLElement;
    const titles = within(operations).getAllByRole("button").map((b) => b.textContent ?? "");
    const prereq = titles.findIndex((t) => t.includes("QDevice Setup Prereq"));
    expect(prereq).toBeGreaterThanOrEqual(0);
    expect(prereq).toBeLessThan(titles.findIndex((t) => t.includes("Add QDevice")));
  }, 15000);
});
