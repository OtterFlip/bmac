import { useEffect, useMemo, useState } from "react";
import { ChevronRight, Eye, FlaskConical, FolderOpen, Play, ShieldAlert, TriangleAlert, Wrench } from "lucide-react";
import { Dialog } from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Input, Label, Select, Switch } from "@/components/ui/inputs";
import { FileBrowserDialog } from "./FileBrowserDialog";
import { useStore } from "@/state/store";
import { cn } from "@/lib/utils";
import { freeHostSlots, previewArgs, shellQuote, validateParam, type NameOption } from "@/lib/params";
import type { Param } from "@/protocol/types";

function useKnownNames() {
  const hosts = useStore((s) => s.sources.list_hosts.data);
  const guests = useStore((s) => s.sources.list_guests.data);
  const repo = useStore((s) => s.repo);
  return useMemo(() => {
    const max = Number(repo?.cluster_settings.find((c) => c.key === "MAX_MOX_HOSTS")?.value) || 10;
    return {
      host: (hosts?.hosts ?? []).map((h) => ({ value: h.name, label: h.name, help: h.online ? `online${h.is_control ? " · control node" : ""}` : "offline" })),
      production: (guests?.production ?? []).map((g) => ({ value: g.name, label: g.name, help: [g.domain, g.node && `on ${g.node}`, g.live_status].filter(Boolean).join(" · ") })),
      staging: (guests?.staging ?? []).map((g) => ({ value: g.name, label: g.name, help: [g.source && `from ${g.source}`, g.node && `on ${g.node}`, g.live_status].filter(Boolean).join(" · ") })),
      freeHostSlots: freeHostSlots(repo?.host_configs ?? [], (hosts?.hosts ?? []).map((h) => h.name), max),
    };
  }, [hosts, guests, repo]);
}

const NAME_HINT: Record<string, string> = { host: "mox1", production: "prod1", staging: "stage1prod1", guest: "prod1 or stage1prod1", hostname: "qdevice" };

function ParamInput({ param, value, onChange }: { param: Param; value: unknown; onChange: (v: unknown) => void }) {
  const known = useKnownNames();
  const [browsing, setBrowsing] = useState(false);
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
    const dry = param.id === "dry_run";
    return (
      <div className={cn("flex items-start gap-3 rounded-lg border px-3 py-2.5", dry ? (value ? "border-info/35 bg-info/6" : "border-warn/35 bg-warn/6") : "border-line bg-surface-2/50")}>
        <Switch id={id} checked={value === true} onCheckedChange={onChange} />
        <div className="min-w-0 flex-1">
          <label htmlFor={id} className="flex items-center gap-1.5 text-[13px] font-medium text-fg">
            {dry && <FlaskConical className="size-3.5 text-info" />}
            {param.label}
          </label>
          {dry ? (
            <p className="mt-0.5 text-[12px] text-fg-subtle">
              {value ? "The script validates everything and shows its plan without changing anything." : "This run will make real changes."}
            </p>
          ) : (
            help
          )}
        </div>
      </div>
    );
  }

  if (param.type === "choice" || param.type === "flag_choice") {
    const options = param.choices ?? [];
    return (
      <div>
        <Label>{param.label}</Label>
        <div className="flex flex-wrap gap-1.5" role="radiogroup">
          {[{ value: "", label: param.type === "flag_choice" ? "Ask me" : "Any" }, ...options].map((o) => {
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
    (param.type === "guest"
      ? [...known.production, ...known.staging]
      : param.type === "host" || param.type === "production" || param.type === "staging"
        ? known[param.type]
        : []);
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
      <div className="flex gap-2">
        {picking ? (
          <div className="min-w-0 flex-1">
            <Select id={id} value={text || undefined} onValueChange={onChange} options={names} placeholder={param.required ? "Choose…" : "Let the script ask"} />
          </div>
        ) : (
        <div className="min-w-0 flex-1">
          <Input
            id={id}
            mono
            value={text}
            onChange={(e) => onChange(e.target.value)}
            placeholder={param.placeholder ? `default ${param.placeholder}` : NAME_HINT[param.type] ?? (param.type === "path" ? "/absolute/path" : "")}
            inputMode={param.type === "integer" ? "numeric" : undefined}
            spellCheck={false}
          />
        </div>
        )}
        {param.type === "path" && (
          <>
            <Button onClick={() => setBrowsing(true)}>
              <FolderOpen /> Browse…
            </Button>
            <FileBrowserDialog open={browsing} onOpenChange={setBrowsing} mode="file" initialPath={text || undefined} title={param.label} onSelect={onChange} />
          </>
        )}
      </div>
      {help}
    </div>
  );
}

export function LaunchDialog() {
  const launch = useStore((s) => s.launch);
  const workflow = useStore((s) => (s.launch ? s.workflows[s.launch.workflowId] : undefined));
  const settings = useStore((s) => s.settings);
  const busy = useStore((s) =>
    Object.values(s.records).find(
      (r) =>
        r.mode === "mutating" &&
        !["succeeded", "failed", "cancelled", "interrupted"].includes(r.status) &&
        !(workflow?.concurrent && r.workflow_id === workflow.id),
    ),
  );
  const closeLaunch = useStore((s) => s.closeLaunch);
  const startRun = useStore((s) => s.startRun);
  const refreshRepo = useStore((s) => s.refreshRepo);
  const refreshSource = useStore((s) => s.refreshSource);
  const [values, setValues] = useState<Record<string, unknown>>({});
  const [advanced, setAdvanced] = useState(false);
  const [touched, setTouched] = useState(false);
  const [starting, setStarting] = useState(false);

  useEffect(() => {
    if (!workflow) return;
    const initial: Record<string, unknown> = {};
    for (const p of workflow.params) {
      if (p.id === "dry_run") initial[p.id] = settings?.default_dry_run ?? true;
      else if (p.default !== undefined) initial[p.id] = p.default;
    }
    const given: Record<string, unknown> = { ...(launch?.values ?? {}) };
    for (const p of workflow.params) {
      if (p.type === "flag" && typeof given[p.id] === "string") given[p.id] = given[p.id] === "true";
    }
    setValues({ ...initial, ...given });
    setAdvanced(Object.keys(launch?.values ?? {}).some((k) => workflow.params.find((p) => p.id === k)?.advanced));
    setTouched(false);
    setStarting(false);
  }, [workflow, launch, settings?.default_dry_run]);

  const wantsSlots = workflow?.params.some((p) => p.suggest === "free_host_slots");
  useEffect(() => {
    if (!wantsSlots) return;
    void refreshRepo();
    void refreshSource("list_hosts", { ifOlderThanMs: 60_000 });
  }, [wantsSlots, launch, refreshRepo, refreshSource]);

  if (!workflow || !launch) return null;
  const basic = workflow.params.filter((p) => !p.advanced);
  const extra = workflow.params.filter((p) => p.advanced);
  const errors = Object.fromEntries(workflow.params.map((p) => [p.id, validateParam(p, values[p.id])]));
  const invalid = Object.values(errors).some(Boolean);
  const dryRun = values.dry_run === true;
  const destructive = workflow.destructive && !dryRun;
  const blockedByBusy = workflow.mode === "mutating" && busy;
  const args = previewArgs(workflow, values);

  const start = async () => {
    setTouched(true);
    if (invalid || !workflow.available || blockedByBusy) return;
    setStarting(true);
    const clean = Object.fromEntries(Object.entries(values).filter(([, v]) => v !== "" && v !== undefined && v !== null));
    const record = await startRun(workflow.id, clean, { focus: true });
    if (!record) setStarting(false);
  };

  return (
    <Dialog
      open
      onOpenChange={(o) => !o && closeLaunch()}
      title={workflow.title}
      description={workflow.summary}
      icon={workflow.destructive ? <ShieldAlert /> : workflow.mode === "read_only" ? <Eye /> : <Wrench />}
      tone={destructive ? "danger" : workflow.mode === "read_only" ? "accent" : "neutral"}
      className="w-[min(620px,94vw)]"
      footer={
        <>
          <Button variant="ghost" onClick={closeLaunch}>
            Cancel
          </Button>
          <Button
            variant={destructive ? "danger" : "primary"}
            size="lg"
            onClick={() => void start()}
            disabled={starting || !workflow.available || !!blockedByBusy || (touched && invalid)}
          >
            {dryRun ? <FlaskConical /> : <Play />}
            {starting ? "Starting…" : dryRun ? "Start dry run" : destructive ? "Start (destructive)" : "Start"}
          </Button>
        </>
      }
    >
      <div className="space-y-4">
        <div className="flex flex-wrap gap-1.5">
          <Badge tone={workflow.mode === "read_only" ? "accent" : "neutral"}>{workflow.mode === "read_only" ? "Read-only" : "Changes the cluster"}</Badge>
          {workflow.destructive && <Badge tone="danger">Destructive</Badge>}
          {workflow.supports_dry_run && <Badge tone="info">Supports dry run</Badge>}
        </div>
        <p className="text-[12.5px] leading-relaxed text-fg-muted">{workflow.description}</p>
        {workflow.notes && <p className="text-[12px] leading-relaxed text-fg-subtle">{workflow.notes}</p>}
        {!workflow.available && (
          <div className="flex items-start gap-2 rounded-lg border border-warn/35 bg-warn/8 px-3 py-2.5 text-[12.5px] text-warn">
            <TriangleAlert className="mt-0.5 size-3.5 shrink-0" /> {workflow.unavailable_reason}
          </div>
        )}
        {blockedByBusy && (
          <div className="flex items-start gap-2 rounded-lg border border-warn/35 bg-warn/8 px-3 py-2.5 text-[12.5px] text-warn">
            <TriangleAlert className="mt-0.5 size-3.5 shrink-0" /> {busy!.workflow_title} is changing the cluster. Changes run one at a time; wait for it to finish.
          </div>
        )}
        {basic.map((p) => (
          <div key={p.id}>
            <ParamInput param={p} value={values[p.id]} onChange={(v) => setValues((cur) => ({ ...cur, [p.id]: v }))} />
            {touched && errors[p.id] && <p className="mt-1.5 text-[12px] font-medium text-danger">{errors[p.id]}</p>}
          </div>
        ))}
        {extra.length > 0 && (
          <div>
            <button onClick={() => setAdvanced(!advanced)} className="flex items-center gap-1 text-[12.5px] font-medium text-fg-muted hover:text-fg">
              <ChevronRight className={cn("size-3.5 transition-transform", advanced && "rotate-90")} /> Advanced options
            </button>
            {advanced && (
              <div className="mt-3 space-y-4 border-l border-line-strong pl-4">
                {extra.map((p) => (
                  <div key={p.id}>
                    <ParamInput param={p} value={values[p.id]} onChange={(v) => setValues((cur) => ({ ...cur, [p.id]: v }))} />
                    {touched && errors[p.id] && <p className="mt-1.5 text-[12px] font-medium text-danger">{errors[p.id]}</p>}
                  </div>
                ))}
              </div>
            )}
          </div>
        )}
        {workflow.params.length === 0 && <p className="text-[12.5px] text-fg-subtle">This workflow takes no options. It asks for anything it needs as it runs.</p>}
        <div>
          <div className="mb-1.5 text-[11px] font-semibold uppercase tracking-[0.08em] text-fg-subtle">Command the dashboard will run</div>
          <div className="selectable overflow-x-auto whitespace-nowrap rounded-md border border-line bg-[#07090c] px-3 py-2 font-mono text-[12px] text-fg-muted">
            <span className="text-accent">$ </span>
            {workflow.script} <span className="text-info">--json</span> {args.map(shellQuote).join(" ")}
          </div>
          <p className="mt-1.5 text-[11.5px] text-fg-subtle">Run from the repository root. Without --json, the same command works in a terminal.</p>
        </div>
      </div>
    </Dialog>
  );
}
