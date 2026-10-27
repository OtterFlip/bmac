import { describe, expect, it } from "vitest";
import { ago, bytes, duration, localDateTime, percent, plural } from "./format";

describe("format", () => {
  it("shows a full date and time in this workstation's time zone", () => {
    const zone = process.env.TZ;
    process.env.TZ = "America/Denver";
    try {
      // 2026-10-27T16:04:23Z is 10:04 in Denver, still on daylight time.
      const text = localDateTime(Date.UTC(2026, 9, 27, 16, 4, 23) / 1000);
      expect(text).toMatch(/2026/);
      expect(text).toMatch(/10:04/);
      expect(text).toMatch(/MDT|GMT-6/);
    } finally {
      process.env.TZ = zone;
    }
    expect(localDateTime(null)).toBe("—");
  });
  it("formats bytes in binary units", () => {
    expect(bytes(null)).toBe("—");
    expect(bytes(512)).toMatch(/512 B/);
    expect(bytes(1024 ** 3 * 2)).toMatch(/^2(\.0)? GiB$/);
  });
  it("formats durations compactly", () => {
    expect(duration(45)).toMatch(/45s/);
    expect(duration(3600 * 26)).toBe("26h 0m");
    expect(duration(3600 * 50)).toBe("2d 2h");
  });
  it("formats relative times", () => {
    const now = Date.now();
    expect(ago(now - 2000, now)).toBe("just now");
    expect(ago(now - 5 * 60_000, now)).toBe("5m ago");
  });
  it("formats fractions and plurals", () => {
    expect(percent(0.456)).toBe("46%");
    expect(plural(1, "host")).toBe("1 host");
    expect(plural(3, "host")).toBe("3 hosts");
  });
});
