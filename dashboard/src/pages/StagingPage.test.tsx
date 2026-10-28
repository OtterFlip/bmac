import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { App } from "@/app/App";
import { setBackend } from "@/lib/api";
import { createMockBackend } from "@/lib/mock/backend";

// Vitest runs without globals here, so Testing Library cannot unmount on its
// own; each test renders the whole app.
afterEach(cleanup);

describe("Staging page", () => {
  it("shows a backup-linked staging VM's restore point and offers both kinds", async () => {
    setBackend(createMockBackend());
    const user = userEvent.setup();
    render(<App />);
    await user.click((await screen.findAllByRole("button", { name: /^Staging/ }))[0]);
    expect(await screen.findByText("vm/100/2026-10-27T16:04:23Z", {}, { timeout: 5000 })).toBeTruthy();
    expect(screen.getByText("backup-linked")).toBeTruthy();
    // The prod-linked rows keep their snapshots.
    expect(screen.getAllByText("stg-base-stage1prod1-261010T040000Z")).toHaveLength(2);
    const operations = (await screen.findByText("Staging operations")).nextElementSibling as HTMLElement;
    const titles = within(operations).getAllByRole("button").map((b) => b.textContent ?? "");
    for (const title of ["Add prod-linked Staging VM", "Instant Restore to Backup-Linked Staging VM", "Remove staging VM"]) {
      expect(titles.some((t) => t.includes(title))).toBe(true);
    }
    expect(screen.getByRole("button", { name: /New prod-linked staging VM/ })).toBeTruthy();
  }, 15000);
});
