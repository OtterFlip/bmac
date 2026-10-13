// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

import { CheckCircle2, Info, X, XCircle } from "lucide-react";
import { cn } from "@bmac/ui";
import { useToasts } from "../../state/toast";
import { useStore } from "../../state/store";

export function Toaster() {
  const toasts = useToasts((s) => s.toasts);
  const dismiss = useToasts((s) => s.dismiss);
  const focusJob = useStore((s) => s.focusJob);
  return (
    <div className="pointer-events-none fixed right-4 bottom-14 z-[90] flex w-[360px] flex-col gap-2">
      {toasts.map((t) => (
        <div
          key={t.id}
          role="status"
          className={cn(
            "pointer-events-auto flex items-start gap-2.5 rounded-lg border bg-surface-2 px-3 py-2.5 shadow-pop animate-fade-in",
            t.kind === "error" ? "border-danger/35" : t.kind === "success" ? "border-ok/30" : "border-line-strong",
          )}
        >
          {t.kind === "success" ? <CheckCircle2 className="mt-0.5 size-4 shrink-0 text-ok" /> : t.kind === "error" ? <XCircle className="mt-0.5 size-4 shrink-0 text-danger" /> : <Info className="mt-0.5 size-4 shrink-0 text-info" />}
          <div className="min-w-0 flex-1">
            <div className="text-[13px] font-medium text-fg">{t.title}</div>
            {t.body && <div className="selectable mt-0.5 line-clamp-3 break-words text-[12px] text-fg-muted">{t.body}</div>}
            {t.jobId && (
              <button
                onClick={() => {
                  focusJob(t.jobId!);
                  dismiss(t.id);
                }}
                className="mt-1 text-[12px] font-medium text-accent hover:underline"
              >
                View job
              </button>
            )}
          </div>
          <button onClick={() => dismiss(t.id)} className="rounded p-0.5 text-fg-subtle hover:text-fg" aria-label="Dismiss">
            <X className="size-3.5" />
          </button>
        </div>
      ))}
    </div>
  );
}
