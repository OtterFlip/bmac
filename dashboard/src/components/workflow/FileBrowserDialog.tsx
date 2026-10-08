import { useCallback, useEffect, useMemo, useState } from "react";
import { ArrowUp, File, FileArchive, FileCode2, Folder, FolderOpen, HardDrive, Home, Loader2, Search } from "lucide-react";
import { Dialog } from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { Input, Switch } from "@/components/ui/inputs";
import { api, errorMessage } from "@/lib/api";
import { bytes, dateTime } from "@/lib/format";
import { cn } from "@/lib/utils";
import type { DirEntry, DirListing } from "@/protocol/types";

/**
 * The dashboard's own file chooser (no OS dialogs). It lists directory
 * entries through a narrow, read-only engine command and returns a path; it
 * never reads file contents.
 */
export function FileBrowserDialog({
  open,
  onOpenChange,
  mode,
  initialPath,
  extensions,
  title,
  onSelect,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  mode: "file" | "directory";
  initialPath?: string;
  extensions?: string[];
  title?: string;
  onSelect: (path: string) => void;
}) {
  const [listing, setListing] = useState<DirListing | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [pathInput, setPathInput] = useState("");
  const [showHidden, setShowHidden] = useState(false);
  const [filter, setFilter] = useState("");

  const load = useCallback(async (path?: string | null) => {
    setLoading(true);
    setError(null);
    try {
      const next = await api().browseDirectory(path);
      setListing(next);
      setPathInput(next.path);
      setSelected(null);
      setFilter("");
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    if (open) {
      const start = initialPath ? (mode === "file" ? initialPath.replace(/\/[^/]*$/, "") || "/" : initialPath) : null;
      void load(start);
    }
  }, [open, initialPath, mode, load]);

  const entries = useMemo(() => {
    if (!listing) return [];
    const lower = filter.toLowerCase();
    return listing.entries.filter((e) => {
      if (!showHidden && e.hidden) return false;
      if (lower && !e.name.toLowerCase().includes(lower)) return false;
      if (mode === "directory") return e.kind === "directory";
      if (e.kind === "file" && extensions?.length) {
        return extensions.some((ext) => e.name.toLowerCase().endsWith(`.${ext.toLowerCase()}`));
      }
      return true;
    });
  }, [listing, showHidden, filter, mode, extensions]);

  const choose = (path: string) => {
    onSelect(path);
    onOpenChange(false);
  };

  const activate = (entry: DirEntry) => {
    if (entry.kind === "directory") void load(entry.path);
    else if (mode === "file") choose(entry.path);
  };

  const target = mode === "directory" ? selected ?? listing?.path ?? null : selected;

  return (
    <Dialog
      open={open}
      onOpenChange={onOpenChange}
      title={title ?? (mode === "file" ? "Choose a file" : "Choose a folder")}
      icon={<FolderOpen />}
      tone="accent"
      className="w-[min(860px,94vw)]"
      footer={
        <>
          <div className="mr-auto min-w-0 truncate font-mono text-[12px] text-fg-subtle">{target ?? (mode === "file" ? "No file selected" : "")}</div>
          <Button variant="ghost" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button variant="primary" disabled={!target} onClick={() => target && choose(target)}>
            {mode === "file" ? "Choose file" : "Choose this folder"}
          </Button>
        </>
      }
    >
      <div className="flex h-[440px] gap-3">
        <nav className="w-[150px] shrink-0 space-y-0.5">
          {listing?.shortcuts.map((s) => (
            <button
              key={s.path}
              onClick={() => void load(s.path)}
              className={cn(
                "flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left text-[12.5px] text-fg-muted hover:bg-surface-3 hover:text-fg",
                listing.path === s.path && "bg-accent/10 text-accent",
              )}
            >
              {s.label === "Home" ? <Home className="size-3.5" /> : s.label === "Computer" ? <HardDrive className="size-3.5" /> : <Folder className="size-3.5" />}
              <span className="truncate">{s.label}</span>
            </button>
          ))}
        </nav>
        <div className="flex min-w-0 flex-1 flex-col overflow-hidden rounded-lg border border-line">
          <div className="flex shrink-0 items-center gap-2 border-b border-line bg-surface-2/60 p-2">
            <Button size="icon" variant="ghost" disabled={!listing?.parent || listing.parent === listing.path} onClick={() => void load(listing?.parent)} aria-label="Up one folder">
              <ArrowUp />
            </Button>
            <form
              className="min-w-0 flex-1"
              onSubmit={(e) => {
                e.preventDefault();
                void load(pathInput);
              }}
            >
              <Input mono value={pathInput} onChange={(e) => setPathInput(e.target.value)} aria-label="Folder path" />
            </form>
            <div className="relative w-[150px]">
              <Search className="pointer-events-none absolute left-2 top-2 size-3.5 text-fg-subtle" />
              <Input value={filter} onChange={(e) => setFilter(e.target.value)} placeholder="Filter" className="pl-7" />
            </div>
          </div>
          <div className="min-h-0 flex-1 overflow-y-auto">
            {loading && (
              <div className="flex h-full items-center justify-center text-fg-subtle">
                <Loader2 className="mr-2 size-4 animate-spin" /> Loading…
              </div>
            )}
            {!loading && error && <div className="p-4 text-[12.5px] text-danger">{error}</div>}
            {!loading && !error && entries.length === 0 && (
              <div className="p-6 text-center text-[12.5px] text-fg-subtle">
                {mode === "file" && extensions?.length ? `No folders or .${extensions.join(", .")} files here.` : "This folder is empty."}
              </div>
            )}
            {!loading && !error && (
              <table className="w-full text-[12.5px]">
                <tbody>
                  {entries.map((e) => (
                    <tr
                      key={e.path}
                      onClick={() => (e.kind === "file" || mode === "directory" ? setSelected(e.path) : undefined)}
                      onDoubleClick={() => activate(e)}
                      className={cn(
                        "cursor-default border-b border-line/50 hover:bg-surface-2",
                        selected === e.path && "bg-accent/12 hover:bg-accent/15",
                        e.hidden && "opacity-70",
                      )}
                    >
                      <td className="w-8 py-1.5 pl-3">{entryIcon(e)}</td>
                      <td className="max-w-0 truncate py-1.5 pr-3 text-fg">
                        {e.name}
                        {e.symlink && <span className="ml-1.5 text-fg-subtle">↗</span>}
                      </td>
                      <td className="w-24 py-1.5 pr-3 text-right tabular-nums text-fg-subtle">{e.kind === "file" ? bytes(e.size) : ""}</td>
                      <td className="w-36 py-1.5 pr-3 text-right tabular-nums text-fg-subtle">{dateTime(e.modified)}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
          </div>
          <div className="flex shrink-0 items-center gap-2 border-t border-line px-3 py-2 text-[12px] text-fg-subtle">
            <Switch id="show-hidden" checked={showHidden} onCheckedChange={setShowHidden} />
            <label htmlFor="show-hidden">Show hidden</label>
            <span className="ml-auto">
              {entries.length} item{entries.length === 1 ? "" : "s"}
              {listing?.truncated && " (truncated)"}
              {mode === "file" && extensions?.length ? ` · .${extensions.join(", .")}` : ""}
            </span>
          </div>
        </div>
      </div>
    </Dialog>
  );
}

function entryIcon(e: DirEntry) {
  if (e.kind === "directory") return <Folder className="size-4 text-accent/80" />;
  if (/\.(iso|img|zip|gz|xz|tar)$/i.test(e.name)) return <FileArchive className="size-4 text-warn/80" />;
  if (/\.(sh|py|pem|key|cer|conf|json)$/i.test(e.name)) return <FileCode2 className="size-4 text-info/80" />;
  return <File className="size-4 text-fg-subtle" />;
}
