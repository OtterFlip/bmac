// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

import { useMemo, type JSX } from "react";
import { Loader2 } from "lucide-react";
import { JobActionsProvider, JobPanel, TooltipProvider, type JobActions } from "@bmac/ui";
import { UnderTheHood } from "../components/console/UnderTheHood";
import { LaunchDialog } from "../components/job/LaunchDialog";
import { Sidebar, NAV } from "../components/layout/Sidebar";
import { Toaster } from "../components/layout/Toaster";
import { RunSwitcher } from "../components/RunSwitcher";
import { ActivityPage } from "../pages/ActivityPage";
import { AlertsPage } from "../pages/AlertsPage";
import { BackupsPage } from "../pages/BackupsPage";
import { DiagnosticsPage } from "../pages/DiagnosticsPage";
import { HostsPage } from "../pages/HostsPage";
import { LogsPage } from "../pages/LogsPage";
import { OverviewPage } from "../pages/OverviewPage";
import { PlaceholderPage } from "../pages/PlaceholderPage";
import { ProductionPage } from "../pages/ProductionPage";
import { QDevicePage } from "../pages/QDevicePage";
import { SettingsPage } from "../pages/SettingsPage";
import { StagingPage } from "../pages/StagingPage";
import { StoragePage } from "../pages/StoragePage";
import { useStore, type Page } from "../state/store";
import { toast } from "../state/toast";
import { LoginPage } from "./LoginPage";
import { SetupPage } from "./SetupPage";
import { useLeaveWarning, useRouting } from "./routing";

const TITLES: Record<Page, string> = { ...Object.fromEntries(NAV.map((n) => [n.id, n.label])), settings: "Settings" } as Record<Page, string>;

const PAGES: Partial<Record<Page, () => JSX.Element>> = {
  overview: OverviewPage,
  hosts: HostsPage,
  production: ProductionPage,
  staging: StagingPage,
  storage: StoragePage,
  qdevice: QDevicePage,
  backups: BackupsPage,
  alerts: AlertsPage,
  activity: ActivityPage,
  logs: LogsPage,
  diagnostics: DiagnosticsPage,
  settings: SettingsPage,
};

function useJobActions(): JobActions {
  const operations = useStore((s) => s.operations);
  return useMemo(() => {
    const s = useStore.getState();
    return {
      operations,
      answer: s.answer,
      cancel: s.cancel,
      resume: s.resume,
      reconcile: s.reconcile,
      openLaunch: s.openLaunch,
      notify: (kind, title, body) => (kind === "error" ? toast.error(title, body) : toast.info(title, body)),
    };
  }, [operations]);
}

function Panel() {
  const record = useStore((s) => (s.panelOpen && s.focusedJobId ? s.jobs[s.focusedJobId] : undefined));
  const setPanelOpen = useStore((s) => s.setPanelOpen);
  const showInConsole = useStore((s) => s.showInConsole);
  const showInPanel = useStore((s) => s.showInPanel);
  if (!record) return null;
  return (
    <JobPanel
      record={record}
      onClose={() => setPanelOpen(false)}
      onShowOutput={() => showInConsole(record.summary.id)}
      title={(content) => (
        <RunSwitcher currentId={record.summary.id} onSelect={showInPanel} tooltip="Show another running job in this panel">
          {content}
        </RunSwitcher>
      )}
    />
  );
}

function Shell() {
  const page = useStore((s) => s.page);
  const setupActive = useStore((s) => s.setup?.active ?? false);
  const actions = useJobActions();
  const PageView = PAGES[page];
  if (setupActive) {
    return (
      <JobActionsProvider value={actions}>
        <SetupPage panel={<Panel />} />
        <LaunchDialog />
      </JobActionsProvider>
    );
  }
  return (
    <JobActionsProvider value={actions}>
      <div className="flex h-full flex-col">
        <div className="flex min-h-0 flex-1">
          <Sidebar />
          <main key={page} className="flex min-w-0 flex-1 flex-col animate-fade-in">
            {PageView ? <PageView /> : <PlaceholderPage title={TITLES[page]} />}
          </main>
          <Panel />
        </div>
        <UnderTheHood />
      </div>
      <LaunchDialog />
    </JobActionsProvider>
  );
}

export function App() {
  const phase = useStore((s) => s.phase);
  useRouting();
  useLeaveWarning();
  return (
    <TooltipProvider>
      {phase === "login" ? (
        <LoginPage />
      ) : phase === "loading" ? (
        <div className="flex h-full items-center justify-center gap-3 text-fg-muted">
          <Loader2 className="size-5 animate-spin text-accent" /> Loading the cluster…
        </div>
      ) : (
        <Shell />
      )}
      <Toaster />
    </TooltipProvider>
  );
}
