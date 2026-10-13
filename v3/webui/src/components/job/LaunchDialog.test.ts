// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

import { describe, expect, it } from "vitest";
import type { OperationView } from "@bmac/ui";
import { initialDryRun } from "./LaunchDialog";

const dryRunnable = { id: "prod.add", supports_dry_run: true, params: [] } as unknown as OperationView;
const always = { id: "prod.change_owner", supports_dry_run: false, params: [] } as unknown as OperationView;

describe("initialDryRun", () => {
  it("starts as a dry run when the operation supports one", () => {
    expect(initialDryRun(dryRunnable, {})).toBe(true);
    expect(initialDryRun(always, {})).toBe(false);
  });

  it("starts for real when a next step asks for it", () => {
    expect(initialDryRun(dryRunnable, { dry_run: "false" })).toBe(false);
    expect(initialDryRun(dryRunnable, { dry_run: false })).toBe(false);
    expect(initialDryRun(dryRunnable, { dry_run: "true" })).toBe(true);
    expect(initialDryRun(always, { dry_run: "true" })).toBe(false);
  });
});
