import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { App } from "@/app/App";
import { setBackend } from "@/lib/api";
import { createMockBackend } from "@/lib/mock/backend";
import { snapshotOf } from "./BackupsPage";

async function openBackups() {
  setBackend(createMockBackend());
  const user = userEvent.setup();
  render(<App />);
  await user.click((await screen.findAllByRole("button", { name: "Backups" }))[0]);
  return user;
}

// Vitest runs without globals here, so Testing Library cannot unmount on its
// own; each test renders the whole app.
afterEach(cleanup);

describe("Backups page", () => {
  it("offers the PBS server guide ahead of Setup PBS", async () => {
    await openBackups();
    const operations = (await screen.findByText("Backup operations")).nextElementSibling as HTMLElement;
    const titles = within(operations).getAllByRole("button").map((b) => b.textContent ?? "");
    const guide = titles.findIndex((t) => t.includes("PBS Server Setup"));
    expect(guide).toBeGreaterThanOrEqual(0);
    expect(guide).toBeLessThan(titles.findIndex((t) => t.includes("Setup PBS")));
    for (const title of ["Backup schedule", "Back up now", "Protect a restore point", "Full Restore to Existing VM", "Full Restore to New VM", "Instant Restore to New Permanent VM", "Instant Restore to Backup-Linked Staging VM"]) {
      expect(titles.some((t) => t.includes(title))).toBe(true);
    }
  }, 15000);

  it("shows the retention policy, schedules, and marks protected restore points", async () => {
    await openBackups();
    expect((await screen.findAllByText("keep-last 6", {}, { timeout: 5000 })).length).toBeGreaterThan(0);
    for (const text of ["keep-daily 3", "keep-weekly 4", "every 4h", "not backed up"]) {
      expect(screen.getAllByText(text).length).toBeGreaterThan(0);
    }
    // One of the four restore points is protected; the badge marks it.
    expect(screen.getAllByText("protected")).toHaveLength(1);
    expect(screen.getByText(/4 made by BMAC · 1 other backup not listed/)).toBeTruthy();
    // How each was backed up, from its task log: two incremental, one full read, one without a log.
    expect(screen.getAllByText("incremental")).toHaveLength(2);
    expect(screen.getAllByText("full read")).toHaveLength(1);
    expect(screen.getAllByText("no log")).toHaveLength(1);
    expect(screen.getAllByText("99% reused · ~61.4 MiB new")).toHaveLength(2);
    expect(screen.getByText("1.0 TiB read · 6m 54s")).toBeTruthy();
    // Only the commented restore point shows the comment marker.
    expect(screen.getAllByLabelText(/^Comment: /).map((e) => e.getAttribute("aria-label"))).toEqual([
      "Comment: Before upgrading Postgres",
    ]);
  }, 15000);

  it("names a restore point by its snapshot", () => {
    expect(
      snapshotOf({ volid: "bmac-pbs:backup/vm/100/2026-10-27T14:50:01Z" } as Parameters<typeof snapshotOf>[0]),
    ).toBe("vm/100/2026-10-27T14:50:01Z");
  });
});
