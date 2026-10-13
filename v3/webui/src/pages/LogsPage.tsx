// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// Every stored record, from this host and its copies of the others: job
// events, audit events, bmacd's log, and the journal collector's entries,
// searched by host, job, kind, severity, unit, text, and time, newest
// first, with paging. A search exports as a file: its pages are fetched
// with the session token and handed to the browser as a Blob (SECURITY.md).

import { useCallback, useEffect, useState, type FormEvent } from "react";
import { ChevronDown, ChevronRight, Download, ScrollText, Search, X, XCircle } from "lucide-react";
import { Badge, Button, Card, EmptyState, Input, Label, Select, dateTimeMs, type LogKind, type LogPage, type LogQuery, type LogRecord, type LogSeverity } from "@bmac/ui";
import { PageBody, PageHeader } from "../components/layout/Page";
import { errorMessage } from "../lib/backend";
import { api } from "../state/store";
import { toast } from "../state/toast";
import { Table } from "./common";

const ANY = "__any__";
const PAGE = 100;
/** The most records one export fetches. */
export const EXPORT_MAX = 50_000;

const KINDS: { value: LogKind; label: string }[] = [
  { value: "job_event", label: "Job events" },
  { value: "audit", label: "Audit" },
  { value: "journal", label: "Journal" },
  { value: "log", label: "bmacd log" },
];
const KIND_LABEL = Object.fromEntries(KINDS.map((k) => [k.value, k.label])) as Record<LogKind, string>;

interface Filters {
  host: string;
  kind: string;
  severity: string;
  unit: string;
  job: string;
  text: string;
  from: string;
  to: string;
}

const EMPTY: Filters = { host: ANY, kind: ANY, severity: ANY, unit: "", job: "", text: "", from: "", to: "" };

function toQuery(f: Filters): LogQuery {
  const time = (value: string) => (value ? new Date(value).getTime() : undefined);
  return {
    host: f.host === ANY ? undefined : (f.host as LogQuery["host"]),
    kinds: f.kind === ANY ? [] : [f.kind as LogKind],
    min_severity: f.severity === ANY ? undefined : (f.severity as LogSeverity),
    unit: f.unit.trim() || undefined,
    job: f.job.trim() || undefined,
    text: f.text.trim() || undefined,
    from_ms: time(f.from),
    to_ms: time(f.to),
  };
}

function SeverityBadge({ severity }: { severity: LogSeverity }) {
  return <Badge tone={severity === "error" ? "danger" : severity === "warning" ? "warn" : "neutral"}>{severity}</Badge>;
}

function Row({ record }: { record: LogRecord }) {
  const [open, setOpen] = useState(false);
  const key = `${record.origin} ${record.stream} #${record.seq}`;
  return (
    <>
      <tr>
        <td className="w-8">
          <Button size="sm" variant="ghost" aria-expanded={open} aria-label={`${open ? "Hide" : "Show"} the record ${key}`} onClick={() => setOpen(!open)}>
            {open ? <ChevronDown /> : <ChevronRight />}
          </Button>
        </td>
        <td className="whitespace-nowrap text-fg-subtle tabular-nums">{dateTimeMs(record.time_ms)}</td>
        <td className="font-mono text-[12px]">{record.origin}</td>
        <td className="whitespace-nowrap text-fg-muted">{KIND_LABEL[record.kind]}</td>
        <td>
          <SeverityBadge severity={record.severity} />
        </td>
        <td className={record.evicted ? "text-fg-subtle italic" : "selectable text-fg"}>{record.summary}</td>
      </tr>
      {open && (
        <tr>
          <td />
          <td colSpan={5}>
            <dl className="mb-2 flex flex-wrap gap-x-4 gap-y-1 text-[11.5px] text-fg-subtle">
              <div>
                <dt className="inline">Key: </dt>
                <dd className="inline font-mono">{key}</dd>
              </div>
              {record.job && (
                <div>
                  <dt className="inline">Job: </dt>
                  <dd className="inline font-mono">{record.job}</dd>
                </div>
              )}
              {record.unit && (
                <div>
                  <dt className="inline">Unit: </dt>
                  <dd className="inline font-mono">{record.unit}</dd>
                </div>
              )}
            </dl>
            {record.evicted ? (
              <p className="text-[12px] text-fg-subtle">Evicted to keep log storage within its budget; only its key, time, and hash are kept.</p>
            ) : (
              <pre className="selectable max-h-80 overflow-auto rounded-lg border border-line bg-surface-2 p-3 font-mono text-[11.5px] leading-relaxed whitespace-pre-wrap text-fg">{JSON.stringify(record.body, null, 2)}</pre>
            )}
          </td>
        </tr>
      )}
    </>
  );
}

function download(records: LogRecord[]) {
  const text = records.map((r) => JSON.stringify(r)).join("\n") + (records.length ? "\n" : "");
  const url = URL.createObjectURL(new Blob([text], { type: "application/x-ndjson" }));
  const link = document.createElement("a");
  link.href = url;
  link.download = `bmac-logs-${new Date().toISOString().replace(/[:.]/g, "-")}.ndjson`;
  link.click();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

export function LogsPage() {
  const [draft, setDraft] = useState<Filters>(EMPTY);
  const [query, setQuery] = useState<LogQuery>(toQuery(EMPTY));
  const [records, setRecords] = useState<LogRecord[]>([]);
  const [next, setNext] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [exporting, setExporting] = useState<number | null>(null);

  const fetchPage = useCallback(async (q: LogQuery, before: string | undefined): Promise<LogPage | null> => {
    setLoading(true);
    try {
      const page = await api().searchLogs({ ...q, before, limit: PAGE });
      setError(null);
      return page;
    } catch (e) {
      setError(errorMessage(e));
      return null;
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    let current = true;
    void fetchPage(query, undefined).then((page) => {
      if (!current || !page) return;
      setRecords(page.records);
      setNext(page.next);
    });
    return () => {
      current = false;
    };
  }, [query, fetchPage]);

  const more = async () => {
    const page = await fetchPage(query, next ?? undefined);
    if (!page) return;
    setRecords((r) => [...r, ...page.records]);
    setNext(page.next);
  };

  const submit = (e: FormEvent) => {
    e.preventDefault();
    setQuery(toQuery(draft));
  };

  const exportAll = async () => {
    setExporting(0);
    const all: LogRecord[] = [];
    try {
      let before: string | undefined;
      do {
        const page: LogPage = await api().searchLogs({ ...query, before, limit: 500 });
        all.push(...page.records);
        before = page.next ?? undefined;
        setExporting(all.length);
      } while (before && all.length < EXPORT_MAX);
      download(all);
      toast.success("Logs exported", before ? `The first ${all.length} records; narrow the search to export the rest.` : `${all.length} records`);
    } catch (e) {
      toast.error("Could not export the logs", errorMessage(e));
    } finally {
      setExporting(null);
    }
  };

  const set = (key: keyof Filters) => (value: string) => setDraft((d) => ({ ...d, [key]: value }));
  return (
    <>
      <PageHeader
        title="Logs"
        subtitle="Job events, audit events, and journal entries from every host, newest first"
        actions={
          <Button size="sm" disabled={exporting !== null || !records.length} onClick={() => void exportAll()}>
            <Download /> {exporting !== null ? `Exporting… ${exporting}` : "Export"}
          </Button>
        }
      />
      <PageBody>
        <Card className="mb-5 p-4">
          <form aria-label="Search logs" onSubmit={submit} className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
            <div>
              <Label htmlFor="logs-host">Host</Label>
              <Select id="logs-host" value={draft.host} onValueChange={set("host")} options={[{ value: ANY, label: "Any host" }, ...Array.from({ length: 10 }, (_, i) => ({ value: `mox${i + 1}`, label: `mox${i + 1}` }))]} />
            </div>
            <div>
              <Label htmlFor="logs-kind">Kind</Label>
              <Select id="logs-kind" value={draft.kind} onValueChange={set("kind")} options={[{ value: ANY, label: "Any kind" }, ...KINDS]} />
            </div>
            <div>
              <Label htmlFor="logs-severity">Severity</Label>
              <Select
                id="logs-severity"
                value={draft.severity}
                onValueChange={set("severity")}
                options={[
                  { value: ANY, label: "Any" },
                  { value: "warning", label: "Warning and error" },
                  { value: "error", label: "Error" },
                ]}
              />
            </div>
            <div>
              <Label htmlFor="logs-unit">Unit</Label>
              <Input id="logs-unit" mono value={draft.unit} spellCheck={false} autoComplete="off" placeholder="corosync.service" onChange={(e) => set("unit")(e.target.value)} />
            </div>
            <div>
              <Label htmlFor="logs-job">Job</Label>
              <Input id="logs-job" mono value={draft.job} spellCheck={false} autoComplete="off" placeholder="job ID" onChange={(e) => set("job")(e.target.value)} />
            </div>
            <div>
              <Label htmlFor="logs-text">Contains</Label>
              <Input id="logs-text" value={draft.text} autoComplete="off" placeholder="any text" onChange={(e) => set("text")(e.target.value)} />
            </div>
            <div>
              <Label htmlFor="logs-from">From</Label>
              <Input id="logs-from" type="datetime-local" value={draft.from} onChange={(e) => set("from")(e.target.value)} />
            </div>
            <div>
              <Label htmlFor="logs-to">To</Label>
              <Input id="logs-to" type="datetime-local" value={draft.to} onChange={(e) => set("to")(e.target.value)} />
            </div>
            <div className="flex gap-2 sm:col-span-2 lg:col-span-4">
              <Button type="submit" variant="primary" size="sm">
                <Search /> Search
              </Button>
              <Button
                type="button"
                variant="ghost"
                size="sm"
                onClick={() => {
                  setDraft(EMPTY);
                  setQuery(toQuery(EMPTY));
                }}
              >
                <X /> Clear
              </Button>
            </div>
          </form>
        </Card>
        {error && (
          <div role="alert" className="mb-5 flex items-center gap-2 rounded-xl border border-danger/30 bg-danger/[0.06] px-4 py-3 text-[12.5px] text-danger">
            <XCircle className="size-4" /> The search failed: {error}
          </div>
        )}
        <Card>
          {records.length ? (
            <>
              <Table label="Log records" head={["", "Time", "Host", "Kind", "Severity", "Record"]}>
                {records.map((r) => (
                  <Row key={`${r.origin}/${r.stream}/${r.seq}`} record={r} />
                ))}
              </Table>
              <div className="flex items-center gap-3 border-t border-line/70 px-4 py-3 text-[12px] text-fg-subtle">
                <span>
                  {records.length} record{records.length === 1 ? "" : "s"}
                  {next ? " so far" : ""}
                </span>
                {next && (
                  <Button size="sm" disabled={loading} onClick={() => void more()}>
                    {loading ? "Loading…" : "Older records"}
                  </Button>
                )}
              </div>
            </>
          ) : loading ? (
            <div className="space-y-3 p-4" aria-busy="true">
              <div className="skeleton h-4 w-1/3" />
              <div className="skeleton h-4 w-full" />
            </div>
          ) : (
            <EmptyState icon={<ScrollText />} title="No records" body="Nothing stored matches this search." />
          )}
        </Card>
      </PageBody>
    </>
  );
}
