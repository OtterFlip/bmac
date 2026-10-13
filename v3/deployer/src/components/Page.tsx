// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

import type { ReactNode } from "react";
import { cn } from "@bmac/ui";

export function PageHeader({ title, subtitle, actions }: { title: ReactNode; subtitle?: ReactNode; actions?: ReactNode }) {
  return (
    <header className="flex min-h-14 items-center gap-4 border-b border-line px-6 py-3">
      <div className="min-w-0 flex-1">
        <h1 className="truncate text-[16px] font-semibold tracking-[-0.01em] text-fg">{title}</h1>
        {subtitle && <p className="mt-0.5 truncate text-[12.5px] text-fg-muted">{subtitle}</p>}
      </div>
      {actions && <div className="flex shrink-0 items-center gap-2">{actions}</div>}
    </header>
  );
}

export function PageBody({ children, className }: { children: ReactNode; className?: string }) {
  return <div className={cn("min-h-0 flex-1 overflow-y-auto px-6 py-5", className)}>{children}</div>;
}
