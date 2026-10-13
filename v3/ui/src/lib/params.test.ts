// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

import { describe, expect, it } from "vitest";
import { freeHostSlots, validateParam } from "./params";

describe("validateParam", () => {
  const p = (type: string, required = false) => ({ id: "x", label: "Thing", type, required }) as never;
  it("checks BMAC names", () => {
    expect(validateParam(p("host"), "mox3")).toBeNull();
    expect(validateParam(p("host"), "mox10")).toBeNull();
    expect(validateParam(p("host"), "mox0")).not.toBeNull();
    expect(validateParam(p("host"), "mox32")).toBeNull();
    expect(validateParam(p("host"), "mox33")).not.toBeNull();
    expect(validateParam(p("production"), "prod12")).toBeNull();
    expect(validateParam(p("staging"), "stage1prod2")).toBeNull();
    expect(validateParam(p("guest"), "--evil")).not.toBeNull();
  });
  it("enforces required values", () => {
    expect(validateParam(p("host", true), "")).toMatch(/required/);
    expect(validateParam(p("host"), "")).toBeNull();
  });
  it("requires positive integers", () => {
    expect(validateParam(p("integer"), "0")).not.toBeNull();
    expect(validateParam(p("integer"), "600")).toBeNull();
  });
});

describe("freeHostSlots", () => {
  it("suggests the lowest slot with saved settings that is not a member", () => {
    const { options, suggested } = freeHostSlots(["mox1", "mox2", "mox3", "mox5"], ["mox1", "mox2"], 6);
    expect(suggested).toBe("mox3");
    expect(options.map((o) => o.value)).toEqual(["mox3", "mox5", "mox4", "mox6"]);
  });
  it("offers slots without settings, and no default, when every slot with settings is taken", () => {
    const { options, suggested } = freeHostSlots(["mox1", "mox2"], ["mox1", "mox2"], 4);
    expect(suggested).toBeUndefined();
    expect(options.map((o) => o.value)).toEqual(["mox3", "mox4"]);
  });
});
