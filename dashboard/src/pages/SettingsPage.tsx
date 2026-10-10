import { useEffect, useState } from "react";
import { ArrowRight, CheckCircle2, Copy, CopyPlus, ExternalLink, FileCog, FolderGit2, FolderOpen, Info, PartyPopper, SlidersHorizontal, Stethoscope, TriangleAlert, XCircle } from "lucide-react";
import { configAttention, useStore } from "@/state/store";
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
import type { ConfigFile, PreflightCheck, Settings } from "@/protocol/types";

const INTERVALS = [
  { value: "0", label: "Only when I refresh" },
  { value: "60", label: "Every minute" },
  { value: "300", label: "Every 5 minutes" },
  { value: "900", label: "Every 15 minutes" },
  { value: "3600", label: "Every hour" },
];

function usePreflight() {
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
  return { checks, loading, updatedAt, error, run };
}

type PreflightState = ReturnType<typeof usePreflight>;

function Preflight({ checks, loading, updatedAt, error, run }: PreflightState) {
  const icon = { ok: <CheckCircle2 className="size-4 text-ok" />, warning: <TriangleAlert className="size-4 text-warn" />, error: <XCircle className="size-4 text-danger" /> };
  return (
    <Card>
      <CardHeader icon={<Stethoscope />} title="This computer" subtitle="What the scripts need to run from here" actions={<RefreshControl updatedAt={updatedAt} loading={loading} error={error} onRefresh={() => void run()} label="Check again" />} />
      <div className="divide-y divide-line/70">
        {!checks && [0, 1, 2, 3].map((i) => <div key={i} className="px-4 py-3"><Skeleton className="w-1/2" /></div>)}
        {checks?.filter((c) => !c.config_file).map((c) => (
          <div key={c.id} className="flex items-start gap-3 px-4 py-2.5">
            <span className="mt-0.5">{icon[c.status]}</span>
            <div className="min-w-0 flex-1">
              <div className="text-[13px] text-fg">
                {c.label}
                {c.purpose && <span className="ml-2 text-[12px] text-fg-muted">{c.purpose}</span>}
              </div>
              <div className="selectable text-[12px] text-fg-subtle">{c.detail}</div>
              {c.fix && c.status !== "ok" && <div className="selectable mt-1 font-mono text-[11.5px] text-fg-muted">{c.fix}</div>}
              {c.command && c.status !== "ok" && (
                <div className="mt-1.5 flex max-w-md items-center gap-2 rounded-md border border-line bg-sunken/60 py-1 pl-2.5 pr-1">
                  <code className="selectable min-w-0 flex-1 truncate font-mono text-[11.5px] text-fg">{c.command}</code>
                  <Button
                    size="sm"
                    variant="ghost"
                    aria-label={`Copy ${c.command}`}
                    onClick={() =>
                      void navigator.clipboard.writeText(c.command!).then(
                        () => toast.success("Copied to Clipboard - press CTRL+SHIFT+V to paste"),
                        () => toast.error("Could not copy to the clipboard"),
                      )
                    }
                  >
                    <Copy /> Copy
                  </Button>
                </div>
              )}
            </div>
            {c.link && c.status !== "ok" && (
              <Button size="sm" variant="primary" className="mt-0.5 shrink-0" onClick={() => void api().openExternal(c.link!.url)}>
                <ExternalLink /> {c.link.label}
              </Button>
            )}
          </div>
        ))}
      </div>
    </Card>
  );
}

const STATE_TEXT: Record<string, string> = {
  "cluster.conf": "Cluster-wide settings every workflow reads.",
  "mox1.conf": "The cluster's first host.",
  "mox2.conf": "The cluster's second host.",
  "secrets.env": "Passwords. Never shown by the dashboard; edit it in your own editor.",
};

function ConfigFiles({ dependenciesMissing, dependenciesChecked }: { dependenciesMissing: boolean; dependenciesChecked: boolean }) {
  const config = useStore((s) => s.config);
  const refreshConfig = useStore((s) => s.refreshConfig);
  const openConfig = useStore((s) => s.openConfig);
  const firstLaunch = useStore((s) => s.appInfo?.first_launch ?? false);
  const hostsDetected = useStore((s) => (s.sources.list_hosts.data?.hosts.length ?? 0) > 0);
  useEffect(() => {
    void refreshConfig();
    const onFocus = () => void refreshConfig();
    window.addEventListener("focus", onFocus);
    return () => window.removeEventListener("focus", onFocus);
  }, [refreshConfig]);
  if (!config) return null;
  const needs = configAttention(config);
  const awaitingFirstHost = needs.length === 0 && dependenciesChecked && !dependenciesMissing && !hostsDetected;
  const essential = config.files.filter((f) => f.essential);
  const open = async (name?: string) => {
    try {
      await api().openConfigLocation(name ?? null);
    } catch (e) {
      toast.error("Could not open it", errorMessage(e));
    }
  };
  const create = async (file: ConfigFile) => {
    try {
      const created = await api().createConfigFromExample(file.example!);
      await refreshConfig();
      toast.success(`Created ${created}`, `A copy of ${file.example}. Fill in your own values.`);
    } catch (e) {
      toast.error("Could not create the file", errorMessage(e));
    }
  };
  const status = (f: ConfigFile) =>
    f.state === "missing" ? (
      <Badge tone={f.name === "cluster.conf" ? "danger" : "warn"}>missing</Badge>
    ) : f.state === "unchanged" ? (
      <Badge tone="warn">not edited yet</Badge>
    ) : (
      <Badge tone="ok"><CheckCircle2 /> customized</Badge>
    );
  return (
    <Card className={cn((needs.length > 0 || dependenciesMissing || awaitingFirstHost) && "border-warn/35")}>
      <CardHeader
        icon={<FileCog />}
        title="Your configuration"
        subtitle={<span className="selectable font-mono">{config.dir}</span>}
        actions={
          <>
            <Button size="sm" onClick={() => void open()}><FolderOpen /> Open folder</Button>
            <Button size="sm" onClick={() => openConfig()}>Config page <ArrowRight /></Button>
          </>
        }
      />
      {(needs.length > 0 || dependenciesMissing) && (
        <div className="flex items-start gap-3 border-b border-warn/20 bg-gradient-to-br from-warn/[0.08] to-transparent px-4 py-3">
          {firstLaunch ? <PartyPopper className="mt-0.5 size-4 shrink-0 text-warn" /> : <TriangleAlert className="mt-0.5 size-4 shrink-0 text-warn" />}
          <div className="text-[12.5px] leading-relaxed text-fg">
            {needs.length > 0 ? (
              <>
                <span className="font-semibold">{firstLaunch ? "Welcome to BMAC. " : ""}Fill in your cluster's values before running workflows.</span>{" "}
                <span className="text-fg-muted">
                  {needs.length === 1 ? "One file is" : `${needs.length} files are`} still missing or identical to the example {needs.length === 1 ? "it starts" : "they start"} from.
                </span>
                {dependenciesMissing && (
                  <div className="mt-1 font-semibold">Also make sure all of the dependencies under "This computer" below are checked off.</div>
                )}
              </>
            ) : (
              <span className="font-semibold">{firstLaunch ? "Welcome to BMAC. " : ""}Make sure all of the dependencies under "This computer" below are checked off before running workflows.</span>
            )}
          </div>
        </div>
      )}
      {awaitingFirstHost && (
        <div className="flex items-start gap-3 border-b border-warn/20 bg-gradient-to-br from-warn/[0.08] to-transparent px-4 py-3">
          <TriangleAlert className="mt-0.5 size-4 shrink-0 text-warn" />
          <div className="text-[12.5px] font-semibold leading-relaxed text-fg">
            When you've finished customizing your config files and password file then deploy your first host using "Add Proxmox host" from the Hosts page.
          </div>
        </div>
      )}
      <div className="divide-y divide-line/70">
        {essential.map((f) => {
          const attention = f.state === "missing" || f.state === "unchanged";
          return (
            <div key={f.name} className={cn("flex items-center gap-3 px-4 py-2.5", attention && "bg-warn/[0.03]")}>
              <span className="mt-0.5 self-start">
                {f.state === "missing" ? <XCircle className={cn("size-4", f.name === "cluster.conf" ? "text-danger" : "text-warn")} /> : attention ? <TriangleAlert className="size-4 text-warn" /> : <CheckCircle2 className="size-4 text-ok" />}
              </span>
              <div className="min-w-0 flex-1">
                <div className="flex items-center gap-2">
                  {f.kind === "secret" ? (
                    <button className="font-mono text-[13px] text-fg underline-offset-2 hover:text-accent hover:underline" onClick={() => void open()} title="Show the folder that contains it">
                      {f.name}
                    </button>
                  ) : (
                    <button className="font-mono text-[13px] text-fg underline-offset-2 hover:text-accent hover:underline" onClick={() => openConfig(f.name)}>
                      {f.name}
                    </button>
                  )}
                  {status(f)}
                </div>
                <div className="text-[12px] text-fg-subtle">{STATE_TEXT[f.name]}</div>
              </div>
              <div className="flex shrink-0 items-center gap-1.5">
                {f.state === "missing" && f.example && (
                  <Button size="sm" variant={attention ? "primary" : "secondary"} onClick={() => void create(f)}><CopyPlus /> Create from example</Button>
                )}
                {f.state !== "missing" && f.kind === "secret" && (
                  <>
                    <Button size="sm" variant={attention ? "primary" : "secondary"} onClick={() => void open(f.name)}><ExternalLink /> Open in editor</Button>
                    <Button size="sm" variant="ghost" onClick={() => void open()}><FolderOpen /> Folder</Button>
                  </>
                )}
                {f.state !== "missing" && f.kind !== "secret" && (
                  <Button size="sm" variant={attention ? "primary" : "ghost"} onClick={() => openConfig(f.name)}>Edit <ArrowRight /></Button>
                )}
              </div>
            </div>
          );
        })}
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
  const repo = useStore((s) => s.repo);
  const platform = useStore((s) => s.platform);
  const preflight = usePreflight();
  const dependenciesMissing = preflight.checks?.some((c) => !c.config_file && c.status !== "ok") ?? false;
  return (
    <>
      <PageHeader title="Settings" />
      <PageBody className="space-y-4">
        <ConfigFiles dependenciesMissing={dependenciesMissing} dependenciesChecked={preflight.checks !== null} />
        {!repo?.bundled && <Repository />}
        <Preflight {...preflight} />
        <Preferences />
        <Card>
          <CardHeader icon={<Info />} title="About" />
          <div className="p-4">
            <KV
              items={[
                ["Dashboard", `${appInfo?.name ?? "BMAC Dashboard"} ${appInfo?.version ?? ""}`],
                ["Protocol", appInfo ? `bmac-ui v${appInfo.protocol_version}` : "—"],
                ["Scripts", <span key="s" className="selectable font-mono text-[12px]">{repo ? `${repo.root}${repo.bundled ? " (bundled)" : ""}` : "—"}</span>],
                ["Documentation", <Button key="d" variant="link" size="sm" onClick={() => void api().openExternal("https://github.com/OtterFlip/bmac")}>github.com/OtterFlip/bmac</Button>],
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
