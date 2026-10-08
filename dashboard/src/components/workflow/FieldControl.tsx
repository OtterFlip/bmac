import { useState } from "react";
import { Eye, EyeOff, FolderOpen, Lock } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Checkbox, Input, Label, Select, Switch, Textarea } from "@/components/ui/inputs";
import { FileBrowserDialog } from "./FileBrowserDialog";
import { cn } from "@/lib/utils";
import type { Field } from "@/protocol/types";

export type FieldValue = string | boolean | string[];

const MONO_TYPES = new Set(["hostname", "ip_address", "cidr", "mac_address", "path", "file", "directory", "ssh_public_key"]);
const NUMERIC = new Set(["integer", "number", "bytes"]);

export function initialValue(field: Field): FieldValue {
  const d = field.default;
  switch (field.type) {
    case "boolean":
      return d === true;
    case "multiselect":
      return Array.isArray(d) ? d.map(String) : [];
    default:
      return d === undefined || d === null ? "" : String(d);
  }
}

/** Client-side syntax checks. The script remains the authority. */
export function validateField(field: Field, value: FieldValue): string | null {
  if (field.type === "multiselect") {
    const n = (value as string[]).length;
    if (field.min_selected && n < field.min_selected) return `Select at least ${field.min_selected}.`;
    if (field.max_selected && n > field.max_selected) return `Select at most ${field.max_selected}.`;
    if (field.required && n === 0) return "Select at least one.";
    return null;
  }
  if (field.type === "boolean") return null;
  const text = String(value).trim();
  if (!text) return field.required ? "Required." : null;
  if (NUMERIC.has(field.type)) {
    const n = Number(text);
    if (!Number.isFinite(n)) return "Enter a number.";
    if (field.type === "integer" && !Number.isInteger(n)) return "Enter a whole number.";
    if (field.min !== undefined && n < field.min) return `At least ${field.min}.`;
    if (field.max !== undefined && n > field.max) return `At most ${field.max}.`;
  }
  if (field.pattern) {
    try {
      if (!new RegExp(`^(?:${field.pattern})$`).test(text)) return "This value is not in the expected format.";
    } catch {
      /* the script validates */
    }
  }
  if ((field.type === "path" || field.type === "file" || field.type === "directory") && !text.startsWith("/")) {
    return "Enter an absolute path.";
  }
  return null;
}

/** Turn form state into protocol values. */
export function toWire(field: Field, value: FieldValue): unknown {
  if (field.type === "boolean") return value === true;
  if (field.type === "multiselect") return value;
  const text = String(value).trim();
  if (NUMERIC.has(field.type) && text !== "") return Number(text);
  return field.type === "password" || field.type === "multiline" ? String(value) : text;
}

export function FieldControl({
  field,
  value,
  onChange,
  error,
  autoFocus,
}: {
  field: Field;
  value: FieldValue;
  onChange: (v: FieldValue) => void;
  error?: string | null;
  autoFocus?: boolean;
}) {
  const id = `field-${field.id}`;
  const [reveal, setReveal] = useState(false);
  const [browsing, setBrowsing] = useState(false);
  const help = field.help && <p className="mt-1.5 text-[12px] leading-relaxed text-fg-subtle">{field.help}</p>;
  const err = error && <p className="mt-1.5 text-[12px] font-medium text-danger">{error}</p>;
  const disabled = field.disabled;

  if (field.type === "boolean") {
    return (
      <div className={cn("flex items-start gap-3 rounded-lg border border-line bg-surface-2/50 px-3 py-2.5", error && "border-danger/50")}>
        <Switch id={id} checked={value === true} onCheckedChange={onChange} disabled={disabled} />
        <div className="min-w-0 flex-1">
          <label htmlFor={id} className="block text-[13px] font-medium text-fg">
            {field.label}
          </label>
          {field.help && <p className="mt-0.5 text-[12px] leading-relaxed text-fg-subtle">{field.help}</p>}
          {err}
        </div>
      </div>
    );
  }

  if (field.type === "select") {
    const options = field.options ?? [];
    const cards = options.length <= 5 && options.some((o) => o.help);
    return (
      <div>
        <Label htmlFor={id} required={field.required}>
          {field.label}
        </Label>
        {cards ? (
          <div role="radiogroup" aria-labelledby={id} className="grid gap-1.5">
            {options.map((o) => {
              const on = value === o.value;
              return (
                <button
                  key={o.value}
                  role="radio"
                  aria-checked={on}
                  disabled={disabled}
                  onClick={() => onChange(o.value)}
                  className={cn(
                    "flex items-start gap-2.5 rounded-lg border px-3 py-2 text-left transition-colors",
                    on ? "border-accent/60 bg-accent/8" : "border-line-strong bg-sunken hover:border-fg-subtle/40",
                  )}
                >
                  <span className={cn("mt-0.5 flex size-3.5 shrink-0 items-center justify-center rounded-full border", on ? "border-accent" : "border-fg-subtle/60")}>
                    {on && <span className="size-1.5 rounded-full bg-accent" />}
                  </span>
                  <span className="min-w-0">
                    <span className="block text-[13px] text-fg">{o.label}</span>
                    {o.help && <span className="mt-0.5 block text-[12px] text-fg-subtle">{o.help}</span>}
                  </span>
                </button>
              );
            })}
          </div>
        ) : (
          <Select id={id} value={String(value)} onValueChange={onChange} options={options} disabled={disabled} invalid={!!error} />
        )}
        {help}
        {err}
      </div>
    );
  }

  if (field.type === "multiselect") {
    const chosen = value as string[];
    const toggle = (v: string, on: boolean) => onChange(on ? [...chosen, v] : chosen.filter((x) => x !== v));
    return (
      <div>
        <Label required={field.required || !!field.min_selected}>
          {field.label}
          {(field.min_selected || field.max_selected) && (
            <span className="ml-2 font-normal text-fg-subtle">
              {field.min_selected && field.max_selected
                ? `${field.min_selected}–${field.max_selected}`
                : field.min_selected
                  ? `at least ${field.min_selected}`
                  : `at most ${field.max_selected}`}
              , {chosen.length} selected
            </span>
          )}
        </Label>
        <div className={cn("grid gap-1.5", (field.options?.length ?? 0) > 4 && "grid-cols-2")}>
          {(field.options ?? []).map((o) => {
            const on = chosen.includes(o.value);
            return (
              <label
                key={o.value}
                className={cn(
                  "flex cursor-default items-start gap-2.5 rounded-lg border px-3 py-2 transition-colors",
                  on ? "border-accent/50 bg-accent/8" : "border-line-strong bg-sunken hover:border-fg-subtle/40",
                )}
              >
                <Checkbox checked={on} onCheckedChange={(v) => toggle(o.value, v)} disabled={disabled} />
                <span className="-mt-0.5 min-w-0">
                  <span className="block text-[13px] text-fg">{o.label}</span>
                  {o.help && <span className="block text-[12px] text-fg-subtle">{o.help}</span>}
                </span>
              </label>
            );
          })}
        </div>
        {help}
        {err}
      </div>
    );
  }

  const isPath = field.type === "path" || field.type === "file" || field.type === "directory";
  const control =
    field.type === "multiline" || field.type === "ssh_public_key" ? (
      <Textarea id={id} value={String(value)} onChange={(e) => onChange(e.target.value)} placeholder={field.placeholder} aria-invalid={!!error || undefined} disabled={disabled} autoFocus={autoFocus} />
    ) : field.type === "password" ? (
      <div className="relative">
        <Lock className="pointer-events-none absolute left-2.5 top-2.5 size-3.5 text-fg-subtle" />
        <Input
          id={id}
          type={reveal ? "text" : "password"}
          value={String(value)}
          onChange={(e) => onChange(e.target.value)}
          placeholder={field.placeholder}
          autoComplete="off"
          spellCheck={false}
          className="pl-8 pr-9 font-mono"
          aria-invalid={!!error || undefined}
          disabled={disabled}
          autoFocus={autoFocus}
        />
        <button
          type="button"
          onClick={() => setReveal(!reveal)}
          className="absolute right-1.5 top-1.5 rounded p-1 text-fg-subtle hover:bg-surface-3 hover:text-fg"
          aria-label={reveal ? "Hide value" : "Show value"}
        >
          {reveal ? <EyeOff className="size-3.5" /> : <Eye className="size-3.5" />}
        </button>
      </div>
    ) : (
      <div className="flex gap-2">
        <div className="min-w-0 flex-1">
          <Input
            id={id}
            value={String(value)}
            onChange={(e) => onChange(e.target.value)}
            placeholder={field.placeholder}
            inputMode={NUMERIC.has(field.type) ? "decimal" : undefined}
            mono={MONO_TYPES.has(field.type)}
            suffix={field.suffix}
            aria-invalid={!!error || undefined}
            disabled={disabled}
            autoFocus={autoFocus}
            spellCheck={false}
          />
        </div>
        {isPath && (
          <>
            <Button variant="secondary" onClick={() => setBrowsing(true)} disabled={disabled}>
              <FolderOpen /> Browse…
            </Button>
            <FileBrowserDialog
              open={browsing}
              onOpenChange={setBrowsing}
              mode={field.type === "directory" ? "directory" : "file"}
              initialPath={String(value) || undefined}
              extensions={field.filters?.flatMap((f) => f.extensions)}
              title={field.label}
              onSelect={(p) => onChange(p)}
            />
          </>
        )}
      </div>
    );

  return (
    <div>
      <Label htmlFor={id} required={field.required}>
        {field.label}
        {field.sensitive && field.type !== "password" && <span className="ml-2 text-[11px] font-normal text-fg-subtle">sensitive</span>}
      </Label>
      {control}
      {help}
      {field.sensitive && (
        <p className="mt-1.5 flex items-center gap-1 text-[11.5px] text-fg-subtle">
          <Lock className="size-3" /> Sent only to the script. Never logged or saved in history.
        </p>
      )}
      {err}
    </div>
  );
}
