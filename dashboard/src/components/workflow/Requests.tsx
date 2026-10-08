import { useEffect, useMemo, useState } from "react";
import { AlertOctagon, AlertTriangle, ChevronRight, ClipboardCheck, Hand, ListChecks, ShieldAlert, TerminalSquare } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/inputs";
import { FieldControl, initialValue, toWire, validateField, type FieldValue } from "./FieldControl";
import { useStore } from "@/state/store";
import { cn } from "@/lib/utils";
import { logs } from "@/state/logs";
import type { ConfirmEvent, Field, InputEvent, InputGroupEvent, ManualActionEvent, RequestEvent, RunEvent } from "@/protocol/types";

export function RequestView({ runId, request }: { runId: string; request: RequestEvent }) {
  switch (request.type) {
    case "input":
    case "input_group":
      return <InputRequest key={request.request_id} runId={runId} request={request} />;
    case "confirm":
      return <ConfirmRequest key={request.request_id} runId={runId} request={request} />;
    case "manual_action":
      return <ManualActionRequest key={request.request_id} runId={runId} request={request} />;
  }
}

const RECENT_LINES = 40;

/** The output the script printed just before asking, which is what a
 *  terminal user would be looking at when answering. */
function recentOutput(runId: string, requestId: string): string {
  const lines = logs.get(runId);
  let end = lines.length - 1;
  while (end >= 0 && !(lines[end].stream === "protocol" && lines[end].text.endsWith(`[${requestId}]`))) end -= 1;
  if (end < 0) end = lines.length;
  const out: string[] = [];
  for (let i = end - 1; i >= 0 && out.length < RECENT_LINES; i -= 1) {
    const line = lines[i];
    if (line.stream === "protocol") {
      if (line.text.startsWith("waiting for") || line.text.startsWith("validation error")) break;
      continue;
    }
    out.push(line.text);
  }
  while (out.length && !out[out.length - 1].trim()) out.pop();
  while (out.length && !out[0].trim()) out.shift();
  return out.reverse().join("\n");
}

function Context({ text, runId, requestId }: { text?: string; runId: string; requestId: string }) {
  const [open, setOpen] = useState(true);
  const fallback = useMemo(() => (text?.trim() ? "" : recentOutput(runId, requestId)), [text, runId, requestId]);
  text = text?.trim() ? text : fallback;
  if (!text?.trim()) return null;
  return (
    <div className="mb-4 overflow-hidden rounded-lg border border-line bg-[#07090c]">
      <button onClick={() => setOpen(!open)} className="flex w-full items-center gap-1.5 px-3 py-1.5 text-left text-[11.5px] font-medium text-fg-subtle hover:text-fg">
        <ChevronRight className={cn("size-3 transition-transform", open && "rotate-90")} />
        <TerminalSquare className="size-3" />
        What the script showed
      </button>
      {open && <pre className="max-h-52 overflow-auto px-3 pb-2.5 font-mono text-[11.5px] leading-[1.5] text-fg-muted">{text}</pre>}
    </div>
  );
}

function Shell({ tone, icon, eyebrow, title, description, children }: {
  tone: "info" | "warn" | "danger";
  icon: React.ReactNode;
  eyebrow: string;
  title: string;
  description?: string;
  children: React.ReactNode;
}) {
  const ring = { info: "border-info/30", warn: "border-warn/35", danger: "border-danger/45" }[tone];
  const glow = {
    info: "from-info/10",
    warn: "from-warn/10",
    danger: "from-danger/14",
  }[tone];
  const chip = { info: "text-info bg-info/12", warn: "text-warn bg-warn/12", danger: "text-danger bg-danger/14" }[tone];
  return (
    <section className={cn("overflow-hidden rounded-xl border bg-surface shadow-panel", ring)} aria-live="polite">
      <div className={cn("bg-gradient-to-b to-transparent px-4 pt-4 pb-3", glow)}>
        <div className="flex items-center gap-2">
          <span className={cn("flex size-6 items-center justify-center rounded-md [&_svg]:size-3.5", chip)}>{icon}</span>
          <span className="text-[11px] font-semibold uppercase tracking-[0.08em] text-fg-muted">{eyebrow}</span>
        </div>
        <h3 className="mt-2 text-[15px] font-semibold tracking-[-0.01em] text-fg">{title}</h3>
        {description && <p className="mt-1 text-[12.5px] leading-relaxed text-fg-muted">{description}</p>}
      </div>
      <div className="px-4 pb-4">{children}</div>
    </section>
  );
}

function useValidationErrors(runId: string, requestId: string, submittedAt: number) {
  const events = useStore((s) => s.events[runId]);
  return useMemo(() => {
    const list = (events ?? []).filter((e: RunEvent) => e.seq > submittedAt && e.event.type === "validation_error" && e.event.request_id === requestId);
    const last = list[list.length - 1]?.event;
    return last && last.type === "validation_error" ? last : null;
  }, [events, requestId, submittedAt]);
}

/** Non-secret answers already sent, so a request the script re-asks after a
 *  validation_error comes back filled in. Memory only; never persisted. */
const drafts = new Map<string, Record<string, FieldValue>>();

function InputRequest({ runId, request }: { runId: string; request: InputEvent | InputGroupEvent }) {
  const fields: Field[] = request.type === "input" ? [request.field] : request.fields;
  const respond = useStore((s) => s.respond);
  const lastSeq = useStore((s) => s.events[runId]?.[s.events[runId].length - 1]?.seq ?? 0);
  const draftKey = `${runId}:${request.request_id}`;
  const [values, setValues] = useState<Record<string, FieldValue>>(() => {
    const draft = drafts.get(draftKey);
    return Object.fromEntries(fields.map((f) => [f.id, !f.sensitive && draft && f.id in draft ? draft[f.id] : initialValue(f)]));
  });
  const [touched, setTouched] = useState(false);
  const [sending, setSending] = useState(false);
  const [baseline, setBaseline] = useState(0);
  const serverErrors = useValidationErrors(runId, request.request_id, baseline);

  useEffect(() => {
    if (serverErrors) setSending(false);
  }, [serverErrors]);

  const clientErrors = Object.fromEntries(fields.map((f) => [f.id, validateField(f, values[f.id])]));
  const hasClientErrors = Object.values(clientErrors).some(Boolean);

  const submit = async () => {
    setTouched(true);
    if (hasClientErrors) return;
    setSending(true);
    setBaseline(lastSeq);
    const wire = Object.fromEntries(fields.map((f) => [f.id, toWire(f, values[f.id])]));
    drafts.set(draftKey, Object.fromEntries(fields.filter((f) => !f.sensitive).map((f) => [f.id, values[f.id]])));
    if (drafts.size > 50) drafts.delete(drafts.keys().next().value!);
    const ok = await respond(runId, request.request_id, { kind: "values", values: wire });
    // Secrets are cleared from form state as soon as they are sent.
    setValues((v) => Object.fromEntries(Object.entries(v).map(([k, val]) => [k, fields.find((f) => f.id === k)?.sensitive ? "" : val])));
    if (!ok) setSending(false);
  };

  const title = request.type === "input" ? request.title ?? request.field.label : request.title;
  return (
    <Shell tone="info" icon={<Hand />} eyebrow="Input needed" title={title} description={request.description}>
      <Context text={request.context} runId={runId} requestId={request.request_id} />
      <form
        onSubmit={(e) => {
          e.preventDefault();
          void submit();
        }}
        className="space-y-4"
      >
        {serverErrors?.message && (
          <div className="flex items-start gap-2 rounded-lg border border-danger/40 bg-danger/8 px-3 py-2 text-[12.5px] text-danger">
            <AlertTriangle className="mt-0.5 size-3.5 shrink-0" /> {serverErrors.message}
          </div>
        )}
        {fields.map((f, i) => (
          <FieldControl
            key={f.id}
            field={f}
            value={values[f.id]}
            onChange={(v) => setValues((cur) => ({ ...cur, [f.id]: v }))}
            error={(touched && clientErrors[f.id]) || serverErrors?.field_errors[f.id] || null}
            autoFocus={i === 0}
          />
        ))}
        <div className="flex items-center justify-end gap-2 pt-1">
          {serverErrors && !sending && <span className="mr-auto text-[12px] text-danger">The script asked for corrections.</span>}
          <Button type="submit" variant="primary" size="lg" disabled={sending}>
            {sending ? "Sending…" : "Continue"}
            <ChevronRight />
          </Button>
        </div>
      </form>
    </Shell>
  );
}

function ConfirmRequest({ runId, request }: { runId: string; request: ConfirmEvent }) {
  const respond = useStore((s) => s.respond);
  const [typed, setTyped] = useState("");
  const [sending, setSending] = useState(false);
  const severe = request.severity === "destructive" || request.severity === "critical";
  const tone = severe ? "danger" : request.severity === "warning" ? "warn" : "info";
  const needsText = !!request.confirmation_text;
  const matches = !needsText || typed === request.confirmation_text;

  const answer = async (confirmed: boolean) => {
    setSending(true);
    const ok = await respond(runId, request.request_id, { kind: "confirm", confirmed });
    if (!ok) setSending(false);
  };

  return (
    <Shell
      tone={tone}
      icon={severe ? <ShieldAlert /> : request.severity === "warning" ? <AlertTriangle /> : <ClipboardCheck />}
      eyebrow={severe ? (request.severity === "critical" ? "Critical confirmation" : "Destructive action") : "Confirmation needed"}
      title={request.title}
      description={request.message}
    >
      <Context text={request.context} runId={runId} requestId={request.request_id} />
      {request.details && request.details.length > 0 && (
        <ul className="mb-4 space-y-1 rounded-lg border border-line bg-sunken px-3 py-2.5 text-[12.5px] text-fg-muted">
          {request.details.map((d, i) => (
            <li key={i} className="flex gap-2">
              <span className="text-fg-subtle">•</span>
              <span className="selectable">{d}</span>
            </li>
          ))}
        </ul>
      )}
      {needsText && (
        <div className="mb-4">
          <label htmlFor="confirm-text" className="mb-1.5 block text-[12.5px] text-fg-muted">
            Type <code className="rounded bg-surface-3 px-1.5 py-0.5 font-mono text-[12px] text-fg">{request.confirmation_text}</code> to confirm
          </label>
          <Input
            id="confirm-text"
            mono
            value={typed}
            onChange={(e) => setTyped(e.target.value)}
            autoFocus
            autoComplete="off"
            spellCheck={false}
            className={cn(severe && "focus:border-danger/70 focus:ring-danger/15")}
            onKeyDown={(e) => {
              if (e.key === "Enter" && matches && !sending) void answer(true);
            }}
          />
        </div>
      )}
      {severe && (
        <p className="mb-4 flex items-start gap-2 text-[12px] leading-relaxed text-fg-subtle">
          <AlertOctagon className="mt-0.5 size-3.5 shrink-0 text-danger" />
          The script repeats its own safety checks before acting. Confirming here is equivalent to confirming in the terminal.
        </p>
      )}
      <div className="flex items-center justify-end gap-2">
        <Button variant="ghost" onClick={() => void answer(false)} disabled={sending}>
          {request.cancel_label ?? "Cancel"}
        </Button>
        <Button variant={severe ? "danger" : "primary"} size="lg" onClick={() => void answer(true)} disabled={!matches || sending}>
          {request.confirm_label ?? (severe ? "Confirm" : "Continue")}
        </Button>
      </div>
    </Shell>
  );
}

function ManualActionRequest({ runId, request }: { runId: string; request: ManualActionEvent }) {
  const respond = useStore((s) => s.respond);
  const [done, setDone] = useState<Set<number>>(new Set());
  const [sending, setSending] = useState(false);
  return (
    <Shell tone="warn" icon={<ListChecks />} eyebrow="Manual action required" title={request.title}>
      <Context text={request.context} runId={runId} requestId={request.request_id} />
      <ol className="mb-4 space-y-1.5">
        {request.instructions.map((line, i) => {
          const on = done.has(i);
          return (
            <li key={i}>
              <button
                onClick={() => setDone((s) => {
                  const next = new Set(s);
                  if (next.has(i)) next.delete(i);
                  else next.add(i);
                  return next;
                })}
                className={cn(
                  "flex w-full items-start gap-3 rounded-lg border px-3 py-2 text-left text-[13px] transition-colors",
                  on ? "border-ok/30 bg-ok/6 text-fg-muted" : "border-line bg-sunken text-fg hover:border-line-strong",
                )}
              >
                <span className={cn("mt-px flex size-5 shrink-0 items-center justify-center rounded-full border text-[11px] font-semibold tabular-nums", on ? "border-ok/50 bg-ok/15 text-ok" : "border-line-strong text-fg-subtle")}>
                  {on ? "✓" : i + 1}
                </span>
                <span className={cn("selectable leading-relaxed", on && "line-through decoration-fg-subtle/50")}>{line}</span>
              </button>
            </li>
          );
        })}
      </ol>
      <div className="flex items-center justify-end gap-2">
        <span className="mr-auto text-[12px] text-fg-subtle">The script waits until you continue.</span>
        <Button
          variant="primary"
          size="lg"
          disabled={sending}
          onClick={async () => {
            setSending(true);
            if (!(await respond(runId, request.request_id, { kind: "acknowledge" }))) setSending(false);
          }}
        >
          {request.acknowledge_label ?? "Done — continue"}
        </Button>
      </div>
    </Shell>
  );
}
