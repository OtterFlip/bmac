// Launch parameter helpers. The engine's registry::build_invocation is the
// authority; these only drive the form and the command preview.

import type { Param, WorkflowInfo } from "@/protocol/types";

export const shellQuote = (arg: string) => (/^[A-Za-z0-9_./=:,@%+-]+$/.test(arg) ? arg : `'${arg.replace(/'/g, "'\\''")}'`);

/** Options first, then positionals, the same order the engine uses. */
export function previewArgs(workflow: Pick<WorkflowInfo, "params">, values: Record<string, unknown>): string[] {
  const options: string[] = [];
  const positional: string[] = [];
  for (const p of workflow.params) {
    const v = values[p.id];
    if (v === undefined || v === null || v === "" || v === false) continue;
    if (p.type === "flag") options.push(p.flag!);
    else if (p.type === "flag_choice") {
      const c = p.choices?.find((c) => c.value === v);
      if (c?.flag) options.push(c.flag);
    } else if (p.flag) options.push(p.flag, String(v).trim());
    else positional.push(String(v).trim());
  }
  return [...options, ...positional];
}

const CHECKS: Partial<Record<Param["type"], [RegExp, string]>> = {
  host: [/^mox[1-9][0-9]{0,3}$/, "Expected a host name like mox1."],
  production: [/^prod[1-9][0-9]{0,3}$/, "Expected a production VM like prod1."],
  staging: [/^stage[1-9][0-9]{0,3}prod[1-9][0-9]{0,3}$/, "Expected a staging VM like stage1prod1."],
  guest: [/^(prod[1-9][0-9]{0,3}|stage[1-9][0-9]{0,3}prod[1-9][0-9]{0,3})$/, "Expected prodN or stageNprodN."],
  integer: [/^[1-9][0-9]{0,8}$/, "Expected a positive whole number."],
  path: [/^\/[^\n]*$/, "Expected an absolute path."],
  hostname: [/^[A-Za-z0-9]([A-Za-z0-9.-]*[A-Za-z0-9])?$/, "Expected a host name."],
};

export interface NameOption {
  value: string;
  label: string;
  help?: string;
}

/**
 * Host slots for adding a host: every moxN up to `max` that is not a cluster
 * member. Slots with an env/moxN.conf come first, and the lowest of them is
 * the suggested default; the others need that file before the script accepts
 * them.
 */
export function freeHostSlots(configs: string[], members: string[], max: number): { options: NameOption[]; suggested?: string } {
  const taken = new Set(members);
  const hasConfig = new Set(configs);
  const slot = (n: number) => `mox${n}`;
  const numbers = new Set<number>();
  for (let n = 1; n <= max; n++) numbers.add(n);
  for (const c of configs) numbers.add(Number(c.slice(3)));
  const free = [...numbers].sort((a, b) => a - b).map(slot).filter((s) => !taken.has(s));
  const ready = free.filter((s) => hasConfig.has(s)).map((s) => ({ value: s, label: s, help: `env/${s}.conf is ready` }));
  const later = free.filter((s) => !hasConfig.has(s)).map((s) => ({ value: s, label: s, help: `create env/${s}.conf first` }));
  return { options: [...ready, ...later], suggested: ready[0]?.value };
}

export function validateParam(param: Param, value: unknown): string | null {
  if (param.type === "flag" || param.type === "flag_choice" || param.type === "choice") return null;
  const text = value === undefined || value === null ? "" : String(value).trim();
  if (!text) return param.required ? `${param.label} is required.` : null;
  const check = CHECKS[param.type];
  return check && !check[0].test(text) ? check[1] : null;
}
