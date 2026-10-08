import { useEffect, useState } from "react";
import { CheckCircle2, FolderGit2, FolderOpen, Info, SlidersHorizontal, Stethoscope, TriangleAlert, XCircle } from "lucide-react";
import { useStore } from "@/state/store";
import { Card, CardHeader } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Input, Label, Select, Switch } from "@/components/ui/inputs";
import { KV, RefreshControl, Skeleton } from "@/components/status";
import { PageBody, PageHeader } from "@/components/layout/Page";
import { FileBrowserDialog } from "@/components/workflow/FileBrowserDialog";
import { api, errorMessage } from "@/lib/api";
import { toast } from "@/state/toast";
import { cn } from "@/lib/utils";
import type { PreflightCheck, Settings } from "@/protocol/types";

const INTERVALS = [
  { value: "0", label: "Only when I refresh" },
  { value: "60", label: "Every minute" },
  { value: "300", label: "Every 5 minutes" },
  { value: "900", label: "Every 15 minutes" },
  { value: "3600", label: "Every hour" },
];

function Preflight() {
  const [checks, setChecks] = useState<PreflightCheck[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [updatedAt, setUpdatedAt] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);
  const run = async () => {
    setLoading(true);
    try {
      setChecks(await api().runPreflight());
      setUpdatedAt(Date.now());
      setError(null);
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setLoading(false);
    }
  };
  useEffect(() => void run(), []);
  const icon = { ok: <CheckCircle2 className="size-4 text-ok" />, warning: <TriangleAlert className="size-4 text-warn" />, error: <XCircle className="size-4 text-danger" /> };
  return (
    <Card>
      <CardHeader icon={<Stethoscope />} title="This computer" subtitle="What the scripts need to run from here" actions={<RefreshControl updatedAt={updatedAt} loading={loading} error={error} onRefresh={() => void run()} label="Check again" />} />
      <div className="divide-y divide-line/70">
        {!checks && [0, 1, 2, 3].map((i) => <div key={i} className="px-4 py-3"><Skeleton className="w-1/2" /></div>)}
        {checks?.map((c) => (
          <div key={c.id} className="flex items-start gap-3 px-4 py-2.5">
            <span className="mt-0.5">{icon[c.status]}</span>
            <div className="min-w-0 flex-1">
              <div className="text-[13px] text-fg">{c.label}</div>
              <div className="selectable text-[12px] text-fg-subtle">{c.detail}</div>
              {c.fix && c.status !== "ok" && <div className="selectable mt-1 font-mono text-[11.5px] text-fg-muted">{c.fix}</div>}
            </div>
          </div>
        ))}
      </div>
    </Card>
  );
}

function Repository() {
  const repo = useStore((s) => s.repo);
  const reloadCatalog = useStore((s) => s.reloadCatalog);
  const [browsing, setBrowsing] = useState(false);
  const [path, setPath] = useState(repo?.root ?? "");
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => setPath(repo?.root ?? ""), [repo?.root]);

  const apply = async (p: string) => {
    setSaving(true);
    setError(null);
    try {
      await api().setRepository(p);
      await reloadCatalog();
      toast.success("Repository updated", p);
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setSaving(false);
    }
  };

  return (
    <Card>
      <CardHeader icon={<FolderGit2 />} title="BMAC repository" subtitle="The checkout whose scripts the dashboard runs" />
      <div className="space-y-4 p-4">
        <div>
          <Label htmlFor="repo-path">Location</Label>
          <div className="flex gap-2">
            <Input id="repo-path" mono value={path} onChange={(e) => setPath(e.target.value)} spellCheck={false} />
            <Button onClick={() => setBrowsing(true)}><FolderOpen /> Browse…</Button>
            <Button variant="primary" disabled={saving || !path || path === repo?.root} onClick={() => void apply(path)}>{saving ? "Checking…" : "Use"}</Button>
          </div>
          {error && <p className="mt-1.5 text-[12px] font-medium text-danger">{error}</p>}
          <FileBrowserDialog open={browsing} onOpenChange={setBrowsing} mode="directory" initialPath={path || undefined} title="Choose the BMAC repository" onSelect={(p) => (setPath(p), void apply(p))} />
        </div>
        {repo && (
          <KV
            items={[
              ["Version", <span key="v" className="font-mono">{repo.git_describe ?? repo.git_commit ?? "unknown"}{repo.git_dirty && <Badge tone="warn" className="ml-2">uncommitted changes</Badge>}</span>],
              ["Branch", <span key="b" className="font-mono">{repo.git_branch ?? "—"}</span>],
              ["Protocol", repo.protocol_version ? `bmac-ui v${repo.protocol_version}` : "unknown"],
              ["cluster.conf", repo.cluster_conf_present ? "present" : <span key="c" className="text-warn">missing</span>],
              ["secrets.env", repo.secrets_env_present ? "present (never read by the dashboard)" : <span key="s" className="text-warn">missing</span>],
              ...repo.cluster_settings.map((c) => [c.key, <span key={c.key} className="font-mono">{c.value}</span>] as [string, React.ReactNode]),
            ]}
          />
        )}
        {repo && repo.problems.length > 0 && (
          <ul className="space-y-1 rounded-lg border border-warn/30 bg-warn/6 px-3 py-2 text-[12.5px] text-warn">
            {repo.problems.map((p) => <li key={p}>{p}</li>)}
          </ul>
        )}
      </div>
    </Card>
  );
}

function Preferences() {
  const settings = useStore((s) => s.settings);
  const setSettings = useStore((s) => s.setSettings);
  if (!settings) return null;
  const update = async (patch: Partial<Settings>) => {
    try {
      setSettings(await api().updateSettings({ ...settings, ...patch }));
    } catch (e) {
      toast.error("Could not save settings", errorMessage(e));
    }
  };
  const row = "flex items-center justify-between gap-6 px-4 py-3";
  return (
    <Card>
      <CardHeader icon={<SlidersHorizontal />} title="Preferences" />
      <div className="divide-y divide-line/70">
        <div className={row}>
          <div>
            <div id="pref-refresh" className="text-[13px] text-fg">Refresh cluster state automatically</div>
            <div id="pref-refresh-help" className="text-[12px] text-fg-subtle">Runs the quick read-only state scripts in the background.</div>
          </div>
          <div className="w-[200px]">
            <Select labelledBy="pref-refresh" describedBy="pref-refresh-help" value={String(settings.refresh_interval_seconds)} onValueChange={(v) => void update({ refresh_interval_seconds: Number(v) })} options={INTERVALS} />
          </div>
        </div>
        <div className={row}>
          <div>
            <div id="pref-dry-run" className="text-[13px] text-fg">Start with dry run turned on</div>
            <div id="pref-dry-run-help" className="text-[12px] text-fg-subtle">For workflows that can preview their changes.</div>
          </div>
          <Switch labelledBy="pref-dry-run" describedBy="pref-dry-run-help" checked={settings.default_dry_run} onCheckedChange={(v) => void update({ default_dry_run: v })} />
        </div>
        <div className={row}>
          <div>
            <div id="pref-notifications" className="text-[13px] text-fg">Notifications</div>
            <div id="pref-notifications-help" className="text-[12px] text-fg-subtle">Tell me when an operation finishes or needs input while the window is in the background.</div>
          </div>
          <Switch labelledBy="pref-notifications" describedBy="pref-notifications-help" checked={settings.notifications} onCheckedChange={(v) => void update({ notifications: v })} />
        </div>
      </div>
    </Card>
  );
}

export function SettingsPage() {
  const appInfo = useStore((s) => s.appInfo);
  const platform = useStore((s) => s.platform);
  return (
    <>
      <PageHeader title="Settings" />
      <PageBody className="space-y-4">
        <Repository />
        <Preflight />
        <Preferences />
        <Card>
          <CardHeader icon={<Info />} title="About" />
          <div className="p-4">
            <KV
              items={[
                ["Dashboard", `${appInfo?.name ?? "BMAC Dashboard"} ${appInfo?.version ?? ""}`],
                ["Protocol", appInfo ? `bmac-ui v${appInfo.protocol_version}` : "—"],
                ["Platform", platform ? `${platform.os_name ?? platform.os} · ${platform.arch}${platform.hostname ? ` · ${platform.hostname}` : ""}` : "—"],
                ["Run history", <span key="h" className={cn("selectable font-mono text-[12px]")}>{appInfo?.history_dir ?? "—"}</span>],
              ]}
            />
            <p className="mt-4 max-w-[640px] text-[12px] leading-relaxed text-fg-subtle">
              The dashboard only runs the curated BMAC workflows, with your choices passed as plain arguments. The scripts do all the checking and are the final authority. Secrets you type go straight to the script and are never logged or saved.
            </p>
          </div>
        </Card>
      </PageBody>
    </>
  );
}
