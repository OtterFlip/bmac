// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// Pages are paths (`/hosts`, `/settings`) so the back button and bookmarks
// work; bmacd answers every page path with index.html. Nothing secret goes
// in a URL.

import { useEffect } from "react";
import { useStore, type Page } from "../state/store";

const PAGES: Page[] = ["overview", "hosts", "production", "staging", "storage", "qdevice", "backups", "alerts", "activity", "logs", "diagnostics", "settings"];

export function pageFromPath(path: string): Page {
  const name = path.replace(/^\/+|\/+$/g, "");
  return (PAGES as string[]).includes(name) ? (name as Page) : "overview";
}

export function pathForPage(page: Page): string {
  return page === "overview" ? "/" : `/${page}`;
}

/** Keeps the store's page and the address bar in step. */
export function useRouting() {
  useEffect(() => {
    useStore.getState().setPage(pageFromPath(location.pathname));
    const onPop = () => useStore.getState().setPage(pageFromPath(location.pathname));
    window.addEventListener("popstate", onPop);
    const stop = useStore.subscribe((s, prev) => {
      if (s.page !== prev.page && pageFromPath(location.pathname) !== s.page) history.pushState(null, "", pathForPage(s.page));
    });
    return () => {
      window.removeEventListener("popstate", onPop);
      stop();
    };
  }, []);
}

/** Asks before the tab closes or reloads while a job waits for this user's answer. */
export function useLeaveWarning() {
  useEffect(() => {
    const onBeforeUnload = (e: BeforeUnloadEvent) => {
      const waiting = Object.values(useStore.getState().jobs).some((r) => r.pending_request || r.summary.pending_request);
      if (!waiting) return;
      e.preventDefault();
      e.returnValue = "";
    };
    window.addEventListener("beforeunload", onBeforeUnload);
    return () => window.removeEventListener("beforeunload", onBeforeUnload);
  }, []);
}
