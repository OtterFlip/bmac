import { useEffect } from "react";
import { FolderGit2, Loader2, RefreshCw, TriangleAlert, Unplug } from "lucide-react";
import { useStore, type Page } from "@/state/store";
import { CLUSTER_SOURCE_IDS } from "@/protocol/state";
import { TooltipProvider } from "@/components/ui/tooltip";
import { Button } from "@/components/ui/button";
import { Sidebar } from "@/components/layout/Sidebar";
import { Toaster } from "@/components/layout/Toaster";
import { CloseDialog } from "@/components/layout/CloseDialog";
import { UnderTheHood } from "@/components/console/UnderTheHood";
import { WorkflowPanel } from "@/components/workflow/WorkflowPanel";
import { LaunchDialog } from "@/components/workflow/LaunchDialog";
import { DashboardPage } from "@/pages/DashboardPage";
import { HostsPage } from "@/pages/HostsPage";
import { ProductionPage } from "@/pages/ProductionPage";
import { StagingPage } from "@/pages/StagingPage";
import { StoragePage } from "@/pages/StoragePage";
import { QDevicePage } from "@/pages/QDevicePage";
import { DiagnosticsPage } from "@/pages/DiagnosticsPage";
import { OperationsPage } from "@/pages/OperationsPage";
import { SettingsPage } from "@/pages/SettingsPage";
import { ConfigPage } from "@/pages/ConfigPage";

const PAGES: Record<Page, () => React.JSX.Element> = {
  dashboard: DashboardPage,
  hosts: HostsPage,
  production: ProductionPage,
  staging: StagingPage,
  storage: StoragePage,
  qdevice: QDevicePage,
  diagnostics: DiagnosticsPage,
  operations: OperationsPage,
  config: ConfigPage,
  settings: SettingsPage,
};

function NoRepository() {
  const setPage = useStore((s) => s.setPage);
  return (
    <div className="mx-7 mt-2 mb-4 flex items-center gap-3 rounded-xl border border-warn/30 bg-warn/[0.07] px-4 py-3">
      <FolderGit2 className="size-4 text-warn" />
      <div className="flex-1 text-[13px] text-fg">
        The dashboard couldn't find the BMAC repository, so no workflows can run.
        <span className="text-fg-muted"> Choose it in Settings.</span>
      </div>
      <Button size="sm" onClick={() => setPage("settings")}>Open Settings</Button>
    </div>
  );
}

const CLUSTER_PAGES: Page[] = ["dashboard", "hosts", "production", "staging", "storage", "qdevice"];

/**
 * Shown while every cluster read so far has failed, e.g. when all hosts are off.
 * The QDevice check reaches its own host directly, so it can't vouch for the cluster.
 */
export function ClusterUnreachable() {
  const sources = useStore((s) => s.sources);
  const refreshSources = useStore((s) => s.refreshSources);
  const entries = CLUSTER_SOURCE_IDS.map((id) => [id, sources[id]] as const);
  const failed = entries.filter(([, e]) => e.status === "error");
  const loading = entries.some(([, e]) => e.status === "loading");
  if (!failed.length || entries.some(([, e]) => e.status === "ok")) return null;
  return (
    <div role="alert" className="mx-7 mt-2 mb-4 flex items-center gap-3 rounded-xl border border-danger/30 bg-danger/[0.07] px-4 py-3">
      <Unplug className="size-4 shrink-0 text-danger" />
      <div className="min-w-0 flex-1 text-[13px] text-fg">
        <span className="font-semibold">Can't reach the cluster.</span>
        <span className="text-fg-muted"> No state is shown until a host answers; nothing here is cached.</span>
        <div className="selectable mt-0.5 truncate text-[12px] text-fg-subtle" title={failed[0][1].error ?? undefined}>{failed[0][1].error}</div>
      </div>
      <Button size="sm" disabled={loading} onClick={() => refreshSources(failed.map(([id]) => id))}>
        <RefreshCw className={loading ? "animate-spin" : undefined} /> Try again
      </Button>
    </div>
  );
}

export function App() {
  const ready = useStore((s) => s.ready);
  const bootError = useStore((s) => s.bootError);
  const bootstrap = useStore((s) => s.bootstrap);
  const page = useStore((s) => s.page);
  const panelOpen = useStore((s) => s.panelOpen);
  const repoValid = useStore((s) => s.repo?.valid ?? false);
  const PageView = PAGES[page];

  useEffect(() => void bootstrap(), [bootstrap]);

  if (!ready) {
    return (
      <div className="flex h-full items-center justify-center gap-3 text-fg-muted">
        <Loader2 className="size-5 animate-spin text-accent" /> Starting…
      </div>
    );
  }
  if (bootError) {
    return (
      <div className="flex h-full flex-col items-center justify-center gap-3 px-8 text-center">
        <TriangleAlert className="size-7 text-danger" />
        <div className="text-[15px] font-semibold text-fg">The dashboard couldn't start</div>
        <div className="selectable max-w-[520px] text-[13px] text-fg-muted">{bootError}</div>
        <Button onClick={() => location.reload()}>Try again</Button>
      </div>
    );
  }

  return (
    <TooltipProvider>
      <div className="flex h-full flex-col">
        <div className="flex min-h-0 flex-1">
          <Sidebar />
          <main key={page} className="flex min-w-0 flex-1 flex-col animate-fade-in">
            {!repoValid && page !== "settings" && page !== "config" && <NoRepository />}
            {repoValid && CLUSTER_PAGES.includes(page) && <ClusterUnreachable />}
            <PageView />
          </main>
          {panelOpen && <WorkflowPanel />}
        </div>
        <UnderTheHood />
      </div>
      <LaunchDialog />
      <CloseDialog />
      <Toaster />
    </TooltipProvider>
  );
}
