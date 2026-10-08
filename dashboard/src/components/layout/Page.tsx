import type { ReactNode } from "react";
import { AlertTriangle, ChevronRight } from "lucide-react";
import { cn } from "@/lib/utils";
import { NextSteps } from "@/components/workflow/RunParts";
import type { NextStep } from "@/protocol/types";

export function PageHeader({ title, subtitle, actions }: { title: string; subtitle?: ReactNode; actions?: ReactNode }) {
  return (
    <header className="flex shrink-0 flex-wrap items-end gap-x-4 gap-y-3 px-7 pt-6 pb-4">
      <div className="min-w-[260px] flex-1">
        <h1 className="text-[21px] font-semibold tracking-[-0.02em] text-fg">{title}</h1>
        {subtitle && <p className="mt-1 text-[13px] text-fg-muted">{subtitle}</p>}
      </div>
      {actions && <div className="flex shrink-0 items-center gap-2">{actions}</div>}
    </header>
  );
}

export function PageBody({ children, className }: { children: ReactNode; className?: string }) {
  return <div className={cn("min-h-0 flex-1 overflow-y-auto px-7 pb-8", className)}>{children}</div>;
}

/** Problems a diagnostic reported, with its suggested next steps. */
export function AttentionBanner({ problems: rawProblems, steps: rawSteps, title = "Needs attention" }: { problems: string[]; steps?: NextStep[]; title?: string }) {
  const problems = [...new Set(rawProblems)];
  const seen = new Set<string>();
  const steps = (rawSteps ?? []).filter((s) => {
    const key = `${s.text}\n${s.command ?? ""}\n${s.workflow ?? ""}`;
    return seen.has(key) ? false : (seen.add(key), true);
  });
  if (!problems.length && !steps.length) return null;
  return (
    <div className="mb-5 overflow-hidden rounded-xl border border-warn/30 bg-gradient-to-br from-warn/[0.07] to-transparent">
      {problems.length > 0 && (
        <div className="px-4 pt-3 pb-3">
          <div className="flex items-center gap-2 text-[13px] font-semibold text-warn">
            <AlertTriangle className="size-4" /> {title}
            <span className="font-normal text-fg-subtle">· {problems.length}</span>
          </div>
          <ul className="mt-2 space-y-1">
            {problems.map((p, i) => (
              <li key={i} className="selectable flex gap-2 text-[12.5px] text-fg">
                <ChevronRight className="mt-0.5 size-3 shrink-0 text-warn/70" />
                {p}
              </li>
            ))}
          </ul>
        </div>
      )}
      {steps.length > 0 && (
        <div className={cn("px-4 pb-3", problems.length > 0 ? "border-t border-warn/15 pt-3" : "pt-3")}>
          <div className="mb-2 text-[11px] font-semibold uppercase tracking-[0.08em] text-fg-subtle">Suggested next steps</div>
          <NextSteps steps={steps} compact />
        </div>
      )}
    </div>
  );
}
