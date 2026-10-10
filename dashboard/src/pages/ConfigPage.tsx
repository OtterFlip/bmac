import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import {
  BookOpen,
  CheckCircle2,
  Circle,
  CopyPlus,
  ExternalLink,
  FileCode2,
  FolderInput,
  FolderOpen,
  GitCompareArrows,
  KeyRound,
  Lock,
  Redo2,
  RefreshCw,
  RotateCcw,
  Save,
  Search,
  TriangleAlert,
  Undo2,
} from "lucide-react";
import { configAttention, useStore } from "@/state/store";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Dialog } from "@/components/ui/dialog";
import { Input, Label } from "@/components/ui/inputs";
import { Tooltip } from "@/components/ui/tooltip";
import { PageHeader } from "@/components/layout/Page";
import { FileBrowserDialog } from "@/components/workflow/FileBrowserDialog";
import { ConfigEditor, type ConfigEditorHandle, type EditorStatus } from "@/components/config/ConfigEditor";
import { api, errorCode, errorMessage } from "@/lib/api";
import { toast } from "@/state/toast";
import { cn } from "@/lib/utils";
import type { ConfigFile, ConfigListing, ConfigText } from "@/protocol/types";

const idle: EditorStatus = { dirty: false, canUndo: false, canRedo: false, line: 1, column: 1, lines: 0 };

function StatePill({ file }: { file: ConfigFile }) {
  if (file.kind === "example") return <span className="text-[10.5px] font-medium uppercase tracking-[0.06em] text-fg-subtle">example</span>;
  switch (file.state) {
    case "missing":
      return <Badge tone={file.essential ? "danger" : "neutral"}>missing</Badge>;
    case "unchanged":
      return <Badge tone="warn">not edited</Badge>;
    case "customized":
      return <CheckCircle2 className="size-3.5 text-ok" aria-label="customized" />;
    default:
      return <Circle className="size-2.5 text-fg-subtle" aria-label="no example" />;
  }
}

function FileIcon({ file }: { file: ConfigFile }) {
  if (file.kind === "secret") return <KeyRound />;
  if (file.kind === "example") return <BookOpen />;
  return <FileCode2 />;
}

function FileList({ listing, selected, onSelect }: { listing: ConfigListing; selected: string | null; onSelect(name: string): void }) {
  const mine = listing.files.filter((f) => f.kind !== "example");
  const examples = listing.files.filter((f) => f.kind === "example");
  const group = (title: string, files: ConfigFile[], hint?: string) => (
    <div className="mb-3">
      <div className="flex items-baseline justify-between px-3 pb-1.5 pt-1">
        <span className="text-[10.5px] font-semibold uppercase tracking-[0.09em] text-fg-subtle">{title}</span>
        {hint && <span className="text-[10.5px] text-fg-subtle/80">{hint}</span>}
      </div>
      {files.map((file) => {
        const on = file.name === selected;
        return (
          <button
            key={file.name}
            onClick={() => onSelect(file.name)}
            aria-current={on ? "true" : undefined}
            className={cn(
              "group relative flex h-8 w-full items-center gap-2.5 rounded-md px-3 text-left [&_svg]:size-3.5",
              on ? "bg-surface-3 text-fg shadow-[inset_0_1px_0_rgb(255_255_255/0.04)]" : "text-fg-muted hover:bg-surface-2 hover:text-fg",
              !file.present && "opacity-75",
            )}
          >
            {on && <span className="absolute left-0 top-1.5 bottom-1.5 w-[2px] rounded-full bg-accent" />}
            <span className={cn("shrink-0", on ? "text-accent" : file.kind === "secret" ? "text-warn/80" : "text-fg-subtle")}>
              <FileIcon file={file} />
            </span>
            <span className={cn("min-w-0 flex-1 truncate font-mono text-[12.5px]", !file.present && "italic")}>{file.name}</span>
            <span className="flex shrink-0 items-center">
              <StatePill file={file} />
            </span>
          </button>
        );
      })}
    </div>
  );
  return (
    <div className="flex h-full flex-col">
      <div className="min-h-0 flex-1 overflow-y-auto px-2 pt-2">
        {group("Your files", mine)}
        {examples.length > 0 && group("Examples", examples, "read-only")}
      </div>
      <p className="border-t border-line px-3 py-2.5 text-[11px] leading-relaxed text-fg-subtle">
        Examples come with BMAC and are replaced when it is updated. Your own files are never overwritten.
      </p>
    </div>
  );
}

function EmptyState({ icon, title, children, actions }: { icon: ReactNode; title: string; children: ReactNode; actions?: ReactNode }) {
  return (
    <div className="grid-bg flex h-full flex-col items-center justify-center px-10 text-center">
      <div className="flex size-12 items-center justify-center rounded-xl border border-line-strong bg-surface-2 text-fg-muted [&_svg]:size-5">{icon}</div>
      <div className="mt-4 text-[15px] font-semibold text-fg">{title}</div>
      <div className="mt-1.5 max-w-[460px] text-[13px] leading-relaxed text-fg-muted">{children}</div>
      {actions && <div className="mt-5 flex flex-wrap justify-center gap-2">{actions}</div>}
    </div>
  );
}

function SecretPanel({ file, onCreate, onOpen, onReveal }: { file: ConfigFile; onCreate(): void; onOpen(): void; onReveal(): void }) {
  if (!file.present) {
    return (
      <EmptyState icon={<KeyRound />} title={`${file.name} is missing`} actions={file.example && <Button variant="primary" onClick={onCreate}><CopyPlus /> Create from {file.example}</Button>}>
        Creating hosts and VMs needs it. The dashboard creates it as a private copy of the example, which you then fill in with your own editor.
      </EmptyState>
    );
  }
  return (
    <EmptyState
      icon={<Lock />}
      title={`${file.name} stays private`}
      actions={
        <>
          <Button variant="primary" onClick={onOpen}><ExternalLink /> Open in text editor</Button>
          <Button onClick={onReveal}><FolderOpen /> Show folder</Button>
        </>
      }
    >
      It holds passwords, so the dashboard never displays it. Edit it with your own text editor and keep it readable only by you (<span className="font-mono text-[12px]">chmod 600</span>).
      {file.state === "unchanged" && (
        <span className="mt-3 flex items-center justify-center gap-1.5 font-medium text-warn">
          <TriangleAlert className="size-3.5" /> It is still identical to {file.example}.
        </span>
      )}
      {file.state === "customized" && (
        <span className="mt-3 flex items-center justify-center gap-1.5 font-medium text-ok">
          <CheckCircle2 className="size-3.5" /> Customized.
        </span>
      )}
    </EmptyState>
  );
}

function ToolbarButton({ label, shortcut, onClick, disabled, active, children }: { label: string; shortcut?: string; onClick(): void; disabled?: boolean; active?: boolean; children: ReactNode }) {
  return (
    <Tooltip content={shortcut ? `${label} · ${shortcut}` : label}>
      <Button size="icon" variant="ghost" aria-label={label} aria-pressed={active} disabled={disabled} onClick={onClick} className={cn(active && "bg-accent/12 text-accent hover:bg-accent/18 hover:text-accent")}>
        {children}
      </Button>
    </Tooltip>
  );
}

function RelocateDialog({ open, onOpenChange, listing, onDone }: { open: boolean; onOpenChange(open: boolean): void; listing: ConfigListing; onDone(next: ConfigListing): void }) {
  const [path, setPath] = useState("");
  const [browsing, setBrowsing] = useState(false);
  const [asking, setAsking] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    if (open) {
      setPath(listing.dir);
      setAsking(false);
      setError(null);
    }
  }, [open, listing.dir]);

  const apply = async (moveFiles: boolean) => {
    setBusy(true);
    setError(null);
    try {
      const next = await api().relocateConfig(path.trim(), moveFiles);
      onDone(next);
      onOpenChange(false);
      toast.success("Config directory changed", moveFiles ? `Your files were moved to ${next.dir}.` : next.dir);
    } catch (e) {
      setError(errorMessage(e));
      setAsking(false);
    } finally {
      setBusy(false);
    }
  };
  const target = path.trim().replace(/\/$/, "");
  const unchanged = !target || target === listing.dir;

  return (
    <>
      <Dialog
        open={open}
        onOpenChange={onOpenChange}
        icon={<FolderInput />}
        tone="accent"
        title={asking ? "Move your files too?" : "Change the config directory"}
        description={
          asking ? (
            <>
              Move everything in <span className="selectable font-mono text-[12px] text-fg">{listing.dir}</span>, including secrets.env and the host artifacts (LUKS header backups), to the new directory?
            </>
          ) : (
            "Where BMAC keeps your cluster.conf, host files, secrets.env, and the artifacts the scripts create. The scripts follow this choice too."
          )
        }
        footer={
          asking ? (
            <>
              <Button variant="ghost" disabled={busy} onClick={() => setAsking(false)}>Back</Button>
              <Button disabled={busy} onClick={() => void apply(false)}>Don't move, start fresh</Button>
              <Button variant="primary" disabled={busy} onClick={() => void apply(true)}>{busy ? "Moving…" : "Move my files"}</Button>
            </>
          ) : (
            <>
              <Button variant="ghost" onClick={() => onOpenChange(false)}>Cancel</Button>
              <Button variant="primary" disabled={unchanged} onClick={() => setAsking(true)}>Continue</Button>
            </>
          )
        }
      >
        {!asking && (
          <div className="space-y-3">
            <div>
              <Label htmlFor="config-dir">Directory</Label>
              <div className="flex gap-2">
                <Input id="config-dir" mono value={path} onChange={(e) => setPath(e.target.value)} spellCheck={false} />
                <Button onClick={() => setBrowsing(true)}><FolderOpen /> Browse…</Button>
              </div>
            </div>
            {listing.dir !== listing.default_dir && (
              <Button variant="link" size="sm" onClick={() => setPath(listing.default_dir)}>Use the default location</Button>
            )}
          </div>
        )}
        {asking && <p className="text-[12.5px] text-fg-muted">If you don't move them, the dashboard creates fresh copies of the examples and new, unedited files there, and leaves the old directory as it is.</p>}
        {error && <p className="selectable mt-3 text-[12.5px] font-medium text-danger">{error}</p>}
      </Dialog>
      <FileBrowserDialog open={browsing} onOpenChange={setBrowsing} mode="directory" initialPath={path || undefined} title="Choose the config directory" onSelect={(p) => setPath(p)} />
    </>
  );
}

export function ConfigPage() {
  const listing = useStore((s) => s.config);
  const refreshConfig = useStore((s) => s.refreshConfig);
  const focus = useStore((s) => s.configFocus);
  const installed = useStore((s) => s.appInfo?.installed ?? false);
  const editor = useRef<ConfigEditorHandle>(null);
  const [selected, setSelected] = useState<string | null>(focus);
  const [doc, setDoc] = useState<ConfigText | null>(null);
  const [loadNonce, setLoadNonce] = useState(0);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [status, setStatus] = useState<EditorStatus>(idle);
  const [example, setExample] = useState<string | null>(null);
  const [comparing, setComparing] = useState(false);
  const [pending, setPending] = useState<string | null>(null);
  const [relocating, setRelocating] = useState(false);
  const [saving, setSaving] = useState(false);

  useEffect(() => void refreshConfig(), [refreshConfig]);
  useEffect(() => {
    const onFocus = () => void refreshConfig();
    window.addEventListener("focus", onFocus);
    return () => window.removeEventListener("focus", onFocus);
  }, [refreshConfig]);

  const file = listing?.files.find((f) => f.name === selected) ?? null;

  // Select the linked file, else the first that needs attention, else the first.
  useEffect(() => {
    if (!listing) return;
    if (focus && listing.files.some((f) => f.name === focus)) {
      setSelected(focus);
      useStore.setState({ configFocus: null });
    } else if (!selected || !listing.files.some((f) => f.name === selected)) {
      setSelected((configAttention(listing)[0] ?? listing.files[0])?.name ?? null);
    }
  }, [listing, focus, selected]);

  const load = useCallback(async (name: string) => {
    setLoadError(null);
    setComparing(false);
    setExample(null);
    try {
      const text = await api().readConfigFile(name);
      setDoc(text);
      setLoadNonce((n) => n + 1);
    } catch (e) {
      setDoc(null);
      setLoadError(errorMessage(e));
    }
  }, []);

  useEffect(() => {
    setStatus(idle);
    if (file && file.present && file.kind !== "secret") void load(file.name);
    else setDoc(null);
    // Reload only when the selection or its presence changes.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selected, file?.present]);

  const choose = (name: string) => {
    if (name === selected) return;
    if (status.dirty) setPending(name);
    else setSelected(name);
  };

  const save = async () => {
    if (!doc || doc.read_only || !editor.current || saving) return;
    const text = editor.current.text();
    setSaving(true);
    try {
      const saved = await api().writeConfigFile(doc.name, text, doc.revision);
      setDoc((d) => (d ? { ...d, revision: saved.revision } : d));
      editor.current.markSaved(saved.text);
      toast.success(`Saved ${doc.name}`);
      void refreshConfig();
    } catch (e) {
      if (errorCode(e) === "unavailable") toast.error(`${doc.name} changed on disk`, "Reload it to see the other changes; your edits are still in the editor.");
      else toast.error(`Could not save ${doc.name}`, errorMessage(e));
    } finally {
      setSaving(false);
    }
  };

  const toggleCompare = async () => {
    if (comparing) return setComparing(false);
    const name = file?.example;
    if (!name) return;
    try {
      setExample((await api().readConfigExample(name)).text);
      setComparing(true);
    } catch (e) {
      toast.error(`Could not open ${name}`, errorMessage(e));
    }
  };

  const createFrom = async (exampleName: string) => {
    try {
      const created = await api().createConfigFromExample(exampleName);
      await refreshConfig();
      setSelected(created);
      toast.success(`Created ${created}`, `A copy of ${exampleName}. Fill in your own values.`);
    } catch (e) {
      toast.error("Could not create the file", errorMessage(e));
    }
  };

  const openExternally = async (name?: string) => {
    try {
      await api().openConfigLocation(name ?? null);
    } catch (e) {
      toast.error("Could not open it", errorMessage(e));
    }
  };

  const userCopy = file?.kind === "example" ? listing?.files.find((f) => f.name === file.user_file) : undefined;

  const pane = (() => {
    if (!listing || !file) return null;
    if (file.kind === "secret") return <SecretPanel file={file} onCreate={() => file.example && void createFrom(file.example)} onOpen={() => void openExternally(file.name)} onReveal={() => void openExternally()} />;
    if (!file.present) {
      return (
        <EmptyState icon={<FileCode2 />} title={`${file.name} doesn't exist yet`} actions={file.example && <Button variant="primary" onClick={() => void createFrom(file.example!)}><CopyPlus /> Create from {file.example}</Button>}>
          {file.essential ? "Setting up the cluster needs it. " : ""}
          {file.example ? `Start from the example, then fill in your own values.` : "Create it in the config directory."}
        </EmptyState>
      );
    }
    if (loadError) return <EmptyState icon={<TriangleAlert />} title={`Couldn't open ${file.name}`} actions={<Button onClick={() => void load(file.name)}><RefreshCw /> Try again</Button>}>{loadError}</EmptyState>;
    if (!doc || doc.name !== file.name) return <div className="h-full animate-pulse bg-sunken/40" />;
    return (
      <ConfigEditor
        ref={editor}
        name={doc.name}
        docKey={`${doc.name}:${loadNonce}`}
        initial={doc.text}
        readOnly={doc.read_only || comparing}
        compareWith={comparing ? example : null}
        onStatus={setStatus}
        onSave={() => void save()}
      />
    );
  })();

  const editable = !!doc && !doc.read_only && doc.name === file?.name && file?.kind === "config";

  return (
    <>
      <PageHeader
        title="Config"
        subtitle={
          listing ? (
            <span className="flex min-w-0 items-center gap-2">
              <span className="selectable truncate font-mono text-[12px] text-fg-muted">{listing.dir}</span>
              {!listing.relocatable && <Badge>checkout</Badge>}
            </span>
          ) : (
            "Your cluster configuration files"
          )
        }
        actions={
          <>
            <Button onClick={() => void openExternally()}><FolderOpen /> Open folder</Button>
            <Tooltip content={listing && !listing.relocatable ? "A BMAC checkout always uses its own config/ directory." : null}>
              <span>
                <Button disabled={!listing?.relocatable || !installed} onClick={() => setRelocating(true)}><FolderInput /> Change location…</Button>
              </span>
            </Tooltip>
          </>
        }
      />
      {listing?.problem && (
        <div className="mx-7 mb-3 flex items-center gap-2 rounded-lg border border-danger/30 bg-danger/8 px-3 py-2 text-[12.5px] text-danger">
          <TriangleAlert className="size-4 shrink-0" />
          <span className="selectable">{listing.problem}. Using the default location until you choose one again.</span>
        </div>
      )}
      <div className="min-h-0 flex-1 px-7 pb-6">
        <div className="flex h-full min-h-[420px] overflow-hidden rounded-xl border border-line bg-surface shadow-[inset_0_1px_0_rgb(255_255_255/0.03)]">
          <aside aria-label="Config files" className="w-[264px] shrink-0 border-r border-line bg-sunken/50">
            {listing ? <FileList listing={listing} selected={selected} onSelect={choose} /> : <div className="p-4 text-[12.5px] text-fg-subtle">Loading…</div>}
          </aside>
          <section className="flex min-w-0 flex-1 flex-col">
            {file && (
              <div className="flex h-12 shrink-0 items-center gap-3 border-b border-line px-4">
                <div className="flex min-w-0 flex-1 items-center gap-2">
                  <span className={cn("[&_svg]:size-4", file.kind === "secret" ? "text-warn/80" : "text-accent")}>
                    <FileIcon file={file} />
                  </span>
                  <span className="truncate font-mono text-[13.5px] font-medium text-fg">{file.name}</span>
                  {status.dirty && <span className="size-2 shrink-0 rounded-full bg-accent" aria-label="unsaved changes" />}
                  {file.kind === "example" && <Badge><Lock /> read-only</Badge>}
                  {file.state === "unchanged" && <Badge tone="warn"><TriangleAlert /> same as {file.example}</Badge>}
                  {file.state === "missing" && <Badge tone={file.essential ? "danger" : "neutral"}>missing</Badge>}
                  {comparing && <Badge tone="accent"><GitCompareArrows /> changes from {file.example}</Badge>}
                </div>
                <div className="flex shrink-0 items-center gap-0.5">
                  {file.kind === "example" && file.user_file && (
                    userCopy?.present ? (
                      <Button size="sm" variant="ghost" onClick={() => choose(file.user_file!)}>Open {file.user_file}</Button>
                    ) : (
                      <Button size="sm" variant="primary" onClick={() => void createFrom(file.name)}><CopyPlus /> Create {file.user_file}</Button>
                    )
                  )}
                  {editable && (
                    <>
                      <ToolbarButton label="Undo" shortcut="Ctrl+Z" disabled={!status.canUndo || comparing} onClick={() => editor.current?.undo()}><Undo2 /></ToolbarButton>
                      <ToolbarButton label="Redo" shortcut="Ctrl+Shift+Z" disabled={!status.canRedo || comparing} onClick={() => editor.current?.redo()}><Redo2 /></ToolbarButton>
                    </>
                  )}
                  {doc && doc.name === file.name && (
                    <ToolbarButton label="Find" shortcut="Ctrl+F" onClick={() => editor.current?.find()}><Search /></ToolbarButton>
                  )}
                  {editable && file.example && (
                    <ToolbarButton label={comparing ? "Back to editing" : `Compare with ${file.example}`} active={comparing} onClick={() => void toggleCompare()}><GitCompareArrows /></ToolbarButton>
                  )}
                  {editable && (
                    <>
                      <ToolbarButton label="Revert to saved" disabled={!status.dirty} onClick={() => void load(file.name)}><RotateCcw /></ToolbarButton>
                      <span className="mx-1.5 h-5 w-px bg-line" />
                      <Button size="sm" variant="primary" disabled={!status.dirty || saving || comparing} onClick={() => void save()}>
                        <Save /> {saving ? "Saving…" : "Save"}
                      </Button>
                    </>
                  )}
                </div>
              </div>
            )}
            <div className="min-h-0 flex-1 bg-[color-mix(in_oklab,var(--color-canvas)_60%,var(--color-surface))]">{pane}</div>
            {doc && doc.name === file?.name && (
              <div className="flex h-7 shrink-0 items-center gap-4 border-t border-line bg-sunken/60 px-4 font-mono text-[11px] text-fg-subtle">
                <span>Ln {status.line}, Col {status.column}</span>
                <span>{status.lines} lines</span>
                <span className="flex-1" />
                <span>{doc.read_only ? "read-only" : status.dirty ? "unsaved changes" : "saved"}</span>
              </div>
            )}
          </section>
        </div>
      </div>
      <Dialog
        open={pending !== null}
        onOpenChange={(open) => !open && setPending(null)}
        icon={<TriangleAlert />}
        tone="warn"
        title={`Discard your changes to ${selected}?`}
        description="They haven't been saved."
        footer={
          <>
            <Button variant="ghost" onClick={() => setPending(null)}>Keep editing</Button>
            <Button variant="danger" onClick={() => (setStatus(idle), setSelected(pending), setPending(null))}>Discard changes</Button>
          </>
        }
      />
      {listing && <RelocateDialog open={relocating} onOpenChange={setRelocating} listing={listing} onDone={(next) => (useStore.setState({ config: next }), void useStore.getState().refreshRepo())} />}
    </>
  );
}
