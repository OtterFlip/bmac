// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

import { createContext, useCallback, useContext, useEffect, useRef, useState, type ReactNode } from "react";
import { AlertTriangle, CheckCircle2 } from "lucide-react";
import { cn } from "@bmac/ui";
import { errorMessage, type Api } from "./lib/api";

export const ApiContext = createContext<Api | null>(null);

export function useApi(): Api {
  const api = useContext(ApiContext);
  if (!api) throw new Error("no API");
  return api;
}

/** Loads with `load`, again on `reload()` and every `everyMs` when given. */
export function useLoad<T>(load: (api: Api) => Promise<T>, everyMs?: number) {
  const api = useApi();
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<string | null>(null);
  const loadRef = useRef(load);
  loadRef.current = load;
  const reload = useCallback(async () => {
    try {
      setData(await loadRef.current(api));
      setError(null);
    } catch (err) {
      setError(errorMessage(err));
    }
  }, [api]);
  useEffect(() => {
    void reload();
    if (!everyMs) return;
    const timer = setInterval(() => void reload(), everyMs);
    return () => clearInterval(timer);
  }, [reload, everyMs]);
  return { data, error, reload };
}

/** Runs an action, keeping its error or success message for display. */
export function useAction() {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [done, setDone] = useState<string | null>(null);
  const run = useCallback(async (action: () => Promise<string | void>) => {
    setBusy(true);
    setError(null);
    setDone(null);
    try {
      const message = await action();
      if (message) setDone(message);
      return true;
    } catch (err) {
      setError(errorMessage(err));
      return false;
    } finally {
      setBusy(false);
    }
  }, []);
  return { busy, error, done, run, clear: () => (setError(null), setDone(null)) };
}

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
  return <div className={cn("min-h-0 flex-1 space-y-5 overflow-y-auto px-7 pb-8", className)}>{children}</div>;
}

export function Problem({ children }: { children: ReactNode }) {
  if (!children) return null;
  return (
    <div role="alert" className="flex items-start gap-2 rounded-lg border border-danger/40 bg-danger/8 px-3 py-2 text-[12.5px] text-danger">
      <AlertTriangle className="mt-0.5 size-3.5 shrink-0" /> <span className="selectable">{children}</span>
    </div>
  );
}

export function Done({ children }: { children: ReactNode }) {
  if (!children) return null;
  return (
    <div role="status" className="flex items-start gap-2 rounded-lg border border-ok/30 bg-ok/8 px-3 py-2 text-[12.5px] text-fg">
      <CheckCircle2 className="mt-0.5 size-3.5 shrink-0 text-ok" /> <span className="selectable">{children}</span>
    </div>
  );
}

export function Warning({ children }: { children: ReactNode }) {
  return (
    <div className="flex items-start gap-2 rounded-lg border border-warn/30 bg-warn/8 px-3 py-2 text-[12.5px] text-fg">
      <AlertTriangle className="mt-0.5 size-3.5 shrink-0 text-warn" /> <span>{children}</span>
    </div>
  );
}

export function short(uuid: string): string {
  return uuid.slice(0, 8);
}

/** A time in `timeZone` (a cluster's), or the browser's. */
export function when(ms: number | null | undefined, timeZone?: string): string {
  if (ms === null || ms === undefined) return "—";
  return new Date(ms).toLocaleString([], { timeZone });
}

export function whenSeconds(s: number, timeZone?: string): string {
  return when(s * 1000, timeZone);
}
