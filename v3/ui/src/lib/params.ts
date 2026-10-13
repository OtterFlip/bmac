// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// Launch parameter helpers. bmacd checks every parameter again; these only
// drive the form.

import type { Param } from "../generated";

const CHECKS: Partial<Record<Param["type"], [RegExp, string]>> = {
  host: [/^mox([1-9]|[12][0-9]|3[0-2])$/, "Expected a host name from mox1 to mox32."],
  production: [/^prod[1-9][0-9]{0,3}$/, "Expected a production VM like prod1."],
  staging: [/^stage[1-9][0-9]{0,3}prod[1-9][0-9]{0,3}$/, "Expected a staging VM like stage1prod1."],
  guest: [/^(prod[1-9][0-9]{0,3}|stage[1-9][0-9]{0,3}prod[1-9][0-9]{0,3})$/, "Expected prodN or stageNprodN."],
  integer: [/^[1-9][0-9]{0,8}$/, "Expected a positive whole number."],
  hostname: [/^[A-Za-z0-9]([A-Za-z0-9.-]*[A-Za-z0-9])?$/, "Expected a host name."],
};

export interface NameOption {
  value: string;
  label: string;
  help?: string;
}

/**
 * Host slots for adding a host: every moxN up to `max` that is not a cluster
 * member. Slots whose host settings are saved come first, and the lowest of
 * them is the suggested default; the others need their settings first.
 */
export function freeHostSlots(configured: string[], members: string[], max: number): { options: NameOption[]; suggested?: string } {
  const taken = new Set(members);
  const ready = new Set(configured);
  const numbers = new Set<number>();
  for (let n = 1; n <= max; n++) numbers.add(n);
  for (const c of configured) numbers.add(Number(c.slice(3)));
  const free = [...numbers].sort((a, b) => a - b).map((n) => `mox${n}`).filter((s) => !taken.has(s));
  const withSettings = free.filter((s) => ready.has(s)).map((s) => ({ value: s, label: s, help: `${s}'s settings are saved` }));
  const later = free.filter((s) => !ready.has(s)).map((s) => ({ value: s, label: s, help: `save ${s}'s settings first` }));
  return { options: [...withSettings, ...later], suggested: withSettings[0]?.value };
}

export function validateParam(param: Param, value: unknown): string | null {
  if (param.type === "flag" || param.type === "flag_choice" || param.type === "choice") return null;
  const text = value === undefined || value === null ? "" : String(value).trim();
  if (!text) return param.required ? `${param.label} is required.` : null;
  const check = CHECKS[param.type];
  return check && !check[0].test(text) ? check[1] : null;
}
