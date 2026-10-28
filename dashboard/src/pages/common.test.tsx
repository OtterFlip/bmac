import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { useStore } from "@/state/store";
import { TooltipProvider } from "@/components/ui/tooltip";
import { Table, WorkflowTiles } from "./common";

// Vitest runs without globals here, so Testing Library cannot unmount on its own.
afterEach(cleanup);

// jsdom does no layout: a tile title is as wide as 8px per character, in a
// 160px box, and table rows are 40px under a 36px header.
const saved = {
  scrollWidth: Object.getOwnPropertyDescriptor(HTMLElement.prototype, "scrollWidth"),
  clientWidth: Object.getOwnPropertyDescriptor(HTMLElement.prototype, "clientWidth"),
  rowRect: HTMLTableRowElement.prototype.getBoundingClientRect,
  tableRect: HTMLTableElement.prototype.getBoundingClientRect,
};
beforeEach(() => {
  Object.defineProperty(HTMLElement.prototype, "scrollWidth", { configurable: true, get() { return (this.textContent?.length ?? 0) * 8; } });
  Object.defineProperty(HTMLElement.prototype, "clientWidth", { configurable: true, get() { return 160; } });
  HTMLTableRowElement.prototype.getBoundingClientRect = function () {
    const bottom = this.parentElement?.tagName === "TBODY" ? 36 + (this.sectionRowIndex + 1) * 40 : 36;
    return { top: bottom - 40, bottom } as DOMRect;
  };
  HTMLTableElement.prototype.getBoundingClientRect = () => ({ top: 0, bottom: 0 }) as DOMRect;
});
afterEach(() => {
  if (saved.scrollWidth) Object.defineProperty(HTMLElement.prototype, "scrollWidth", saved.scrollWidth);
  if (saved.clientWidth) Object.defineProperty(HTMLElement.prototype, "clientWidth", saved.clientWidth);
  HTMLTableRowElement.prototype.getBoundingClientRect = saved.rowRect;
  HTMLTableElement.prototype.getBoundingClientRect = saved.tableRect;
});

const workflow = (id: string, title: string) =>
  ({ id, title, summary: "", mode: "read_only", destructive: false, available: true, params: [] }) as never;

describe("workflow tiles", () => {
  it("lets only a title that doesn't fit glide to its end", () => {
    useStore.setState({
      workflows: {
        short: workflow("short", "List hosts"),
        long: workflow("long", "Instant Restore to Backup-Linked Staging VM"),
      },
    });
    render(<TooltipProvider><WorkflowTiles ids={["short", "long"]} /></TooltipProvider>);
    const short = screen.getByText("List hosts").parentElement!;
    const long = screen.getByText("Instant Restore to Backup-Linked Staging VM").parentElement!;
    expect(short.hasAttribute("data-overflow")).toBe(false);
    expect(long.hasAttribute("data-overflow")).toBe(true);
    // 43 characters at 8px in 160px: it moves 184px, plus a 4px margin.
    expect(long.style.getPropertyValue("--tile-shift")).toBe("-188px");
    expect(long.style.getPropertyValue("--tile-glide")).toBe("2576ms");
  });
});

describe("table", () => {
  const rows = (count: number) =>
    Array.from({ length: count }, (_, i) => (
      <tr key={i}>
        <td>row {i}</td>
      </tr>
    ));

  it("scrolls with its header in place once it has more rows than maxRows, showing exactly that many", () => {
    const { container } = render(<Table head={["Time"]} maxRows={7}>{rows(9)}</Table>);
    const wrapper = container.firstElementChild as HTMLElement;
    expect(wrapper.style.maxHeight).toBe(`${36 + 7 * 40}px`);
    expect(wrapper.className).toContain("overflow-y-auto");
    expect(container.querySelector("thead")!.className).toContain("sticky");
  });

  it("keeps its full height with maxRows rows or fewer", () => {
    const { container } = render(<Table head={["Time"]} maxRows={7}>{rows(7)}</Table>);
    const wrapper = container.firstElementChild as HTMLElement;
    expect(wrapper.style.maxHeight).toBe("");
    expect(container.querySelector("thead")!.className).not.toContain("sticky");
  });
});
