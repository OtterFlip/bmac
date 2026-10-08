import { existsSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";
import registry from "@registry";
import { freeHostSlots, previewArgs, shellQuote, validateParam } from "./params";
import type { Workflow } from "@/protocol/types";

const workflows = (registry as { workflows: Workflow[] }).workflows;
const byId = Object.fromEntries(workflows.map((w) => [w.id, w]));
const repoRoot = resolve(__dirname, "../../..");

describe("workflow registry", () => {
  it("only names scripts that exist in the repository", () => {
    for (const w of workflows) expect(existsSync(resolve(repoRoot, w.script)), w.script).toBe(true);
  });

  it("gives every flag-like parameter its flag", () => {
    for (const w of workflows)
      for (const p of w.params) {
        if (p.type === "flag") expect(p.flag, `${w.id}.${p.id}`).toMatch(/^--[a-z]/);
        if (p.type === "flag_choice") for (const c of p.choices ?? []) expect(c.flag, `${w.id}.${p.id}`).toMatch(/^--[a-z]/);
      }
  });

  it("starts dry-run parameters switched on", () => {
    for (const w of workflows) {
      const dry = w.params.find((p) => p.id === "dry_run");
      if (dry) expect(dry.default, w.id).toBe(true);
    }
  });
});

describe("previewArgs", () => {
  it("puts options before positionals and skips unset values", () => {
    expect(previewArgs(byId.remove_prod_vm, { dry_run: true, resource: "prod2" })).toEqual(["--dry-run", "prod2"]);
    expect(previewArgs(byId.remove_prod_vm, { dry_run: false, resource: "prod2" })).toEqual(["prod2"]);
    expect(previewArgs(byId.remove_prod_vm, {})).toEqual([]);
  });

  it("maps choices to their flags", () => {
    const w = byId.add_proxmox_host;
    const param = w.params.find((p) => p.type === "flag_choice")!;
    const choice = param.choices![0];
    expect(previewArgs(w, { [param.id]: choice.value })).toContain(choice.flag);
  });
});

describe("validateParam", () => {
  const p = (type: string, required = false) => ({ id: "x", label: "Thing", type, required }) as never;
  it("checks BMAC names", () => {
    expect(validateParam(p("host"), "mox3")).toBeNull();
    expect(validateParam(p("host"), "mox0")).not.toBeNull();
    expect(validateParam(p("production"), "prod12")).toBeNull();
    expect(validateParam(p("staging"), "stage1prod2")).toBeNull();
    expect(validateParam(p("guest"), "--evil")).not.toBeNull();
  });
  it("enforces required values", () => {
    expect(validateParam(p("host", true), "")).toMatch(/required/);
    expect(validateParam(p("host"), "")).toBeNull();
  });
  it("requires absolute paths and positive integers", () => {
    expect(validateParam(p("path"), "relative/file")).not.toBeNull();
    expect(validateParam(p("path"), "/tmp/sanitize.sh")).toBeNull();
    expect(validateParam(p("integer"), "0")).not.toBeNull();
    expect(validateParam(p("integer"), "600")).toBeNull();
  });
});

describe("freeHostSlots", () => {
  it("suggests the lowest configured slot that is not a member", () => {
    const { options, suggested } = freeHostSlots(["mox1", "mox2", "mox3", "mox5"], ["mox1", "mox2"], 6);
    expect(suggested).toBe("mox3");
    expect(options.map((o) => o.value)).toEqual(["mox3", "mox5", "mox4", "mox6"]);
  });
  it("offers unconfigured slots without a default when every configured slot is taken", () => {
    const { options, suggested } = freeHostSlots(["mox1", "mox2"], ["mox1", "mox2"], 4);
    expect(suggested).toBeUndefined();
    expect(options.map((o) => o.value)).toEqual(["mox3", "mox4"]);
  });
});

describe("shellQuote", () => {
  it("leaves plain words alone and quotes the rest", () => {
    expect(shellQuote("prod1")).toBe("prod1");
    expect(shellQuote("a b")).toBe("'a b'");
    expect(shellQuote("it's")).toBe("'it'\\''s'");
  });
});
