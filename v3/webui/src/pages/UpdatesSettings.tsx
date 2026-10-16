// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// Cluster updates (DEPLOYMENT.md, "Upgrades"): upload a signed release
// bundle to this host, then run "Update the cluster", which installs it on
// every host one at a time, or "Roll back a cluster update".

import { useCallback, useEffect, useRef, useState } from "react";
import { PackageCheck, RotateCcw, Trash2, Upload } from "lucide-react";
import { Badge, Button, Card, CardHeader, Dialog, dateTimeMs, type UpdatesView } from "@bmac/ui";
import { errorMessage } from "../lib/backend";
import { api, useStore } from "../state/store";
import { toast } from "../state/toast";
import { Table } from "./common";

/** A chunk's bytes as base64, without one huge string argument. */
function base64(bytes: Uint8Array): string {
  let binary = "";
  for (let i = 0; i < bytes.length; i += 0x8000) {
    binary += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  }
  return btoa(binary);
}

function shortSha(sha: string): string {
  return `${sha.slice(0, 12)}…`;
}

function megabytes(bytes: number): string {
  return `${(bytes / 1_048_576).toFixed(1)} MB`;
}

export function UpdatesSettings() {
  const [view, setView] = useState<UpdatesView | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [progress, setProgress] = useState<number | null>(null);
  const [deleting, setDeleting] = useState<string | null>(null);
  const input = useRef<HTMLInputElement>(null);
  const update = useStore((s) => s.operations["cluster.update"]);
  const rollback = useStore((s) => s.operations["cluster.rollback"]);
  const startOperation = useStore((s) => s.startOperation);
  const canManage = update?.allowed === true;

  const load = useCallback(() => {
    api()
      .updates()
      .then(
        (next) => {
          setView(next);
          setError(null);
        },
        (e) => setError(errorMessage(e)),
      );
  }, []);
  useEffect(load, [load]);

  const upload = async (file: File) => {
    setProgress(0);
    try {
      const started = await api().startUpload(file.size);
      for (let offset = 0; offset < file.size; offset += started.chunk) {
        const piece = new Uint8Array(await file.slice(offset, offset + started.chunk).arrayBuffer());
        await api().uploadChunk(started.upload_id, offset, base64(piece));
        setProgress(Math.min(1, (offset + piece.length) / file.size));
      }
      const bundle = await api().finishUpload(started.upload_id);
      toast.success(`Bundle ${bundle.version} checks out`, `Signed with the ${bundle.signed_by} key`);
    } catch (e) {
      toast.error("The bundle was refused", errorMessage(e));
    } finally {
      setProgress(null);
      if (input.current) input.current.value = "";
      load();
    }
  };

  const remove = async (sha: string) => {
    try {
      await api().deleteBundle(sha);
      toast.success("Bundle deleted");
    } catch (e) {
      toast.error(errorMessage(e));
    }
    setDeleting(null);
    load();
  };

  return (
    <Card className="mb-4">
      <CardHeader
        icon={<PackageCheck />}
        title="Updates"
        subtitle={view ? `Release bundles uploaded to ${view.this_host}, and what each host runs` : "Release bundles, and what each host runs"}
        actions={
          canManage && (
            <div className="flex items-center gap-2">
              <input
                ref={input}
                type="file"
                accept=".bundle,.tar"
                aria-label="Release bundle file"
                className="hidden"
                onChange={(e) => {
                  const file = e.target.files?.[0];
                  if (file) void upload(file);
                }}
              />
              <Button size="sm" disabled={progress !== null} onClick={() => input.current?.click()}>
                <Upload /> {progress === null ? "Upload a bundle" : `Uploading ${Math.round(progress * 100)}%`}
              </Button>
              <Button size="sm" variant="primary" disabled={!view?.bundles.length} onClick={() => startOperation("cluster.update", {})}>
                <PackageCheck /> Update the cluster…
              </Button>
              {rollback?.allowed && (
                <Button size="sm" variant="ghost" onClick={() => startOperation("cluster.rollback", {})}>
                  <RotateCcw /> Roll back…
                </Button>
              )}
            </div>
          )
        }
      />
      {error ? (
        <p role="alert" className="px-4 py-3 text-[12.5px] text-danger">
          Updates aren't available: {error}
        </p>
      ) : (
        view && (
          <>
            <Table label="Host versions" head={["Host", "Runs", "Installed bundle", "Rolls back to"]}>
              {view.hosts.map((h) => (
                <tr key={h.host}>
                  <td className="font-mono font-medium text-fg">{h.host}</td>
                  <td className="font-mono">{h.version ?? <Badge tone="warn">no answer</Badge>}</td>
                  <td className="font-mono text-[11.5px]">{h.installed.current ? `${h.installed.current.version} (${shortSha(h.installed.current.sha256)})` : <span className="text-fg-subtle">as installed</span>}</td>
                  <td className="font-mono text-[11.5px]">{h.installed.previous ? h.installed.previous.version : <span className="text-fg-subtle">nothing kept</span>}</td>
                </tr>
              ))}
            </Table>
            {view.bundles.length === 0 ? (
              <p className="px-4 py-3 text-[12.5px] text-fg-subtle">
                No bundles uploaded. This host trusts bundles signed with the {view.trusted_keys.join(" or ")} key.
              </p>
            ) : (
              <Table label="Release bundles" head={["Version", "Signed with", "Built", "Size", "SHA-256", ""]}>
                {view.bundles.map((b) => (
                  <tr key={b.sha256}>
                    <td className="font-mono font-medium text-fg">{b.version}</td>
                    <td>
                      <Badge tone={b.signed_by === "release" ? "ok" : "warn"}>{b.signed_by}</Badge>
                    </td>
                    <td className="text-[11.5px]">{dateTimeMs(b.built_at_s * 1000)}</td>
                    <td className="text-[11.5px]">{megabytes(b.size)}</td>
                    <td className="selectable font-mono text-[11px] break-all">{b.sha256}</td>
                    <td>
                      {canManage && (
                        <div className="flex justify-end">
                          <Button size="sm" variant="ghost" aria-label={`Delete bundle ${b.version}`} onClick={() => setDeleting(b.sha256)}>
                            <Trash2 />
                          </Button>
                        </div>
                      )}
                    </td>
                  </tr>
                ))}
              </Table>
            )}
          </>
        )
      )}
      <Dialog
        open={deleting !== null}
        onOpenChange={(open) => !open && setDeleting(null)}
        tone="danger"
        icon={<Trash2 />}
        title="Delete this bundle?"
        description="It is removed from this host. A bundle a host runs, or keeps for a rollback, can't be deleted."
        footer={
          <>
            <Button variant="ghost" onClick={() => setDeleting(null)}>
              Keep it
            </Button>
            <Button variant="danger" onClick={() => deleting && void remove(deleting)}>
              Delete
            </Button>
          </>
        }
      />
    </Card>
  );
}
