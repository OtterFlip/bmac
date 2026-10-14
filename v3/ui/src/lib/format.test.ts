// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

import { describe, expect, it } from "vitest";
import { ago, bytes, dateTime, duration, percent, plural } from "./format";

describe("format", () => {
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
  it("shows times in the cluster's zone, with the date unless it is that zone's today", () => {
    const iso = "2026-11-01T05:30:00Z";
    const now = new Date("2026-11-01T12:00:00Z");
    expect(dateTime(iso, "America/Denver", now)).toMatch(/Oct 31/);
    expect(dateTime(iso, "America/Denver", now)).toMatch(/11:30/);
    expect(dateTime(iso, "UTC", now)).toMatch(/^0?5:30/);
  });
  it("formats fractions and plurals", () => {
    expect(percent(0.456)).toBe("46%");
    expect(plural(1, "host")).toBe("1 host");
    expect(plural(3, "host")).toBe("3 hosts");
  });
});
