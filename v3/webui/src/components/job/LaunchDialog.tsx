// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

import { useEffect, useState } from "react";
import { ChevronRight, Eye, FlaskConical, Play, ShieldAlert, TriangleAlert, Wrench } from "lucide-react";
import { Badge, Button, Dialog, Input, Label, Select, Switch, cn, validateParam, type NameOption, type OperationView, type Param, type TargetRule } from "@bmac/ui";
import { useStore } from "../../state/store";

const NAME_HINT: Partial<Record<Param["type"], string>> = { host: "mox1", production: "prod1", staging: "stage1prod1", guest: "prod1 or stage1prod1", hostname: "qdevice" };

function targetText(target: TargetRule, values: Record<string, unknown>): string {
  const named = (param: string) => (typeof values[param] === "string" && values[param] ? String(values[param]) : `the ${param} you choose`);
  switch (target.rule) {
    case "any":
      return "Runs on the host you're connected to.";
    case "duty":
      return "Runs on the cluster's duty host.";
    case "host":
      return `Runs on ${named(target.param)}.`;
    case "any_except":
      return `Runs on any host except ${named(target.param)}.`;
    case "owner":
      return `Runs on the host that owns ${named(target.param)}.`;
    case "standby":
      return `Runs on the standby host for ${named(target.param)}.`;
    case "placement":
      return "Runs on the host BMAC places it on.";
  }
}

function ParamInput({ param, value, onChange }: { param: Param; value: unknown; onChange: (v: unknown) => void }) {
  const known = useStore((s) => s.known);
  const [manual, setManual] = useState(false);
  const id = `param-${param.id}`;
  const help = param.help && <p className="mt-1.5 text-[12px] text-fg-subtle">{param.help}</p>;
  const text = value === undefined || value === null ? "" : String(value);
  const slots = param.suggest === "free_host_slots" ? known.freeHostSlots : undefined;
  const suggested = slots?.suggested;
  useEffect(() => {
    if (suggested && !manual && !text) onChange(suggested);
  }, [suggested, manual, text]);

  if (param.type === "flag") {
    return (
      <div className="flex items-start gap-3 rounded-lg border border-line bg-surface-2/50 px-3 py-2.5">
        <Switch id={id} checked={value === true} onCheckedChange={onChange} />
        <div className="min-w-0 flex-1">
          <label htmlFor={id} className="text-[13px] font-medium text-fg">
            {param.label}
          </label>
          {help}
        </div>
      </div>
    );
  }

  if (param.type === "choice" || param.type === "flag_choice") {
    return (
      <div>
        <Label>{param.label}</Label>
        <div className="flex flex-wrap gap-1.5" role="radiogroup" aria-label={param.label}>
          {[{ value: "", label: param.type === "flag_choice" ? "Ask me" : "Any" }, ...param.choices].map((o) => {
            const on = (value ?? "") === o.value;
            return (
              <button
                key={o.value || "none"}
                role="radio"
                aria-checked={on}
                onClick={() => onChange(o.value || undefined)}
                className={cn("rounded-md border px-2.5 py-1.5 text-[12.5px] transition-colors", on ? "border-accent/60 bg-accent/10 text-fg" : "border-line-strong bg-sunken text-fg-muted hover:text-fg")}
              >
                {o.label}
              </button>
            );
          })}
        </div>
        {help}
      </div>
    );
  }

  const names: NameOption[] =
    slots?.options ??
    (param.type === "guest" ? [...known.production, ...known.staging] : param.type === "host" || param.type === "production" || param.type === "staging" ? known[param.type] : []);
  const picking = names.length > 0 && !manual && (!text || names.some((n) => n.value === text));
  return (
    <div>
      <div className="flex items-baseline justify-between">
        <Label htmlFor={id} required={param.required}>
          {param.label}
          {!param.required && <span className="ml-1.5 font-normal text-fg-subtle">optional</span>}
        </Label>
        {names.length > 0 && (
          <button className="text-[11.5px] text-fg-subtle hover:text-fg" onClick={() => setManual(picking)}>
            {picking ? "Enter a name" : slots ? "Choose a free slot" : "Choose from the cluster"}
          </button>
        )}
      </div>
      {picking ? (
        <Select id={id} value={text || undefined} onValueChange={onChange} options={names} placeholder={param.required ? "Choose…" : "Let the script ask"} />
      ) : (
        <Input
          id={id}
          mono
          value={text}
          onChange={(e) => onChange(e.target.value)}
          placeholder={param.placeholder ? `default ${param.placeholder}` : (NAME_HINT[param.type] ?? "")}
          inputMode={param.type === "integer" ? "numeric" : undefined}
          spellCheck={false}
        />
      )}
      {help}
    </div>
  );
}

function initialValues(operation: OperationView, given: Record<string, unknown>): Record<string, unknown> {
  const values: Record<string, unknown> = {};
  for (const p of operation.params) if (p.default !== undefined && p.default !== null) values[p.id] = p.default;
  for (const [key, value] of Object.entries(given)) {
    const param = operation.params.find((p) => p.id === key);
    if (!param) continue;
    values[key] = param.type === "flag" && typeof value === "string" ? value === "true" : value;
  }
  return values;
}

export function initialDryRun(operation: OperationView, given: Record<string, unknown>): boolean {
  return operation.supports_dry_run && given.dry_run !== "false" && given.dry_run !== false;
}

export function LaunchDialog() {
  const launch = useStore((s) => s.launch);
  const operation = useStore((s) => (s.launch ? s.operations[s.launch.operation] : undefined));
  const closeLaunch = useStore((s) => s.closeLaunch);
  const launchJob = useStore((s) => s.launchJob);
  const [values, setValues] = useState<Record<string, unknown>>({});
  const [dryRun, setDryRun] = useState(true);
  const [advanced, setAdvanced] = useState(false);
  const [touched, setTouched] = useState(false);
  const [starting, setStarting] = useState(false);

  useEffect(() => {
    if (!operation || !launch) return;
    setValues(initialValues(operation, launch.values ?? {}));
    setDryRun(initialDryRun(operation, launch.values ?? {}));
    setAdvanced(Object.keys(launch.values ?? {}).some((k) => operation.params.find((p) => p.id === k)?.advanced));
    setTouched(false);
    setStarting(false);
  }, [operation, launch]);

  if (!operation || !launch) return null;
  const basic = operation.params.filter((p) => !p.advanced);
  const extra = operation.params.filter((p) => p.advanced);
  const errors = Object.fromEntries(operation.params.map((p) => [p.id, validateParam(p, values[p.id])]));
  const invalid = Object.values(errors).some(Boolean);
  const dry = operation.supports_dry_run && dryRun;
  const destructive = operation.destructive && !dry;

  const start = async () => {
    setTouched(true);
    if (invalid || !operation.allowed) return;
    setStarting(true);
    const clean = Object.fromEntries(
      Object.entries(values)
        .filter(([, v]) => v !== "" && v !== undefined && v !== null)
        .map(([k, v]) => [k, operation.params.find((p) => p.id === k)?.type === "integer" ? Number(v) : v]),
    );
    const summary = await launchJob(operation.id, clean, dry);
    if (!summary) setStarting(false);
  };

  const field = (p: Param) => (
    <div key={p.id}>
      <ParamInput param={p} value={values[p.id]} onChange={(v) => setValues((cur) => ({ ...cur, [p.id]: v }))} />
      {touched && errors[p.id] && <p className="mt-1.5 text-[12px] font-medium text-danger">{errors[p.id]}</p>}
    </div>
  );

  return (
    <Dialog
      open
      onOpenChange={(o) => !o && closeLaunch()}
      title={operation.title}
      description={operation.summary}
      icon={operation.destructive ? <ShieldAlert /> : operation.mutating ? <Wrench /> : <Eye />}
      tone={destructive ? "danger" : operation.mutating ? "neutral" : "accent"}
      className="w-[min(620px,94vw)]"
      footer={
        <>
          <Button variant="ghost" onClick={closeLaunch}>
            Cancel
          </Button>
          <Button variant={destructive ? "danger" : "primary"} size="lg" onClick={() => void start()} disabled={starting || !operation.allowed || (touched && invalid)}>
            {dry ? <FlaskConical /> : <Play />}
            {starting ? "Starting…" : dry ? "Start dry run" : destructive ? "Start (destructive)" : "Start"}
          </Button>
        </>
      }
    >
      <div className="space-y-4">
        <div className="flex flex-wrap gap-1.5">
          <Badge tone={operation.mutating ? "neutral" : "accent"}>{operation.mutating ? "Changes the cluster" : "Read-only"}</Badge>
          {operation.destructive && <Badge tone="danger">Destructive</Badge>}
          {operation.supports_dry_run && <Badge tone="info">Supports dry run</Badge>}
        </div>
        <p className="text-[12.5px] leading-relaxed text-fg-muted">{operation.description}</p>
        {!operation.allowed && (
          <div className="flex items-start gap-2 rounded-lg border border-warn/35 bg-warn/8 px-3 py-2.5 text-[12.5px] text-warn">
            <TriangleAlert className="mt-0.5 size-3.5 shrink-0" /> Your role can't start this. It needs the {operation.min_role} role.
          </div>
        )}
        {operation.supports_dry_run && (
          <div className={cn("flex items-start gap-3 rounded-lg border px-3 py-2.5", dryRun ? "border-info/35 bg-info/6" : "border-warn/35 bg-warn/6")}>
            <Switch id="launch-dry-run" checked={dryRun} onCheckedChange={setDryRun} />
            <div className="min-w-0 flex-1">
              <label htmlFor="launch-dry-run" className="flex items-center gap-1.5 text-[13px] font-medium text-fg">
                <FlaskConical className="size-3.5 text-info" /> Dry run
              </label>
              <p className="mt-0.5 text-[12px] text-fg-subtle">{dryRun ? "The script validates everything and shows its plan without changing anything." : "This run will make real changes."}</p>
            </div>
          </div>
        )}
        {basic.map(field)}
        {extra.length > 0 && (
          <div>
            <button onClick={() => setAdvanced(!advanced)} className="flex items-center gap-1 text-[12.5px] font-medium text-fg-muted hover:text-fg">
              <ChevronRight className={cn("size-3.5 transition-transform", advanced && "rotate-90")} /> Advanced options
            </button>
            {advanced && <div className="mt-3 space-y-4 border-l border-line-strong pl-4">{extra.map(field)}</div>}
          </div>
        )}
        {operation.params.length === 0 && <p className="text-[12.5px] text-fg-subtle">This takes no options. It asks for anything it needs as it runs.</p>}
        <p className="text-[11.5px] text-fg-subtle">{targetText(operation.target, values)}</p>
      </div>
    </Dialog>
  );
}
