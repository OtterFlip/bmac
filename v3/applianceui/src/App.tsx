// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// The appliance UI: the console's unlock comes first, then the
// administrator's password, then the pages. Nothing is kept across a reload.

import { useCallback, useEffect, useState, type FormEvent, type ReactNode } from "react";
import { Archive, Gauge, GitBranch, KeyRound, Lock, LogIn, LogOut, ScrollText, Settings, Share2, Shield, TrendingUp, Loader2 } from "lucide-react";
import { Button, Input, Label, cn, type ApplianceHello } from "@bmac/ui";
import { ApiContext, Problem, useApi } from "./parts";
import { errorMessage, type Api } from "./lib/api";
import { OverviewPage } from "./pages/Overview";
import { PitsPage } from "./pages/Pits";
import { ThroughputPage } from "./pages/Throughput";
import { RetentionPage } from "./pages/Retention";
import { GrantsPage } from "./pages/Grants";
import { PeersPage } from "./pages/Peers";
import { SettingsPage } from "./pages/Settings";
import { AuditPage } from "./pages/Audit";

export type Page = "overview" | "pits" | "throughput" | "retention" | "grants" | "peers" | "settings" | "audit";

const NAV: { id: Page; label: string; icon: ReactNode }[] = [
  { id: "overview", label: "Overview", icon: <Gauge /> },
  { id: "pits", label: "PITs", icon: <GitBranch /> },
  { id: "throughput", label: "Throughput", icon: <TrendingUp /> },
  { id: "retention", label: "Retention", icon: <Archive /> },
  { id: "grants", label: "Restore grants", icon: <KeyRound /> },
  { id: "peers", label: "Peers", icon: <Share2 /> },
  { id: "settings", label: "Settings", icon: <Settings /> },
  { id: "audit", label: "Audit log", icon: <ScrollText /> },
];

function Frame({ children }: { children: ReactNode }) {
  return (
    <div className="grid-bg flex h-full items-center justify-center overflow-y-auto px-6 py-10">
      <div className="w-full max-w-[420px]">{children}</div>
    </div>
  );
}

function Brand({ hello }: { hello: ApplianceHello | null }) {
  return (
    <div className="mb-6 flex items-center gap-3">
      <img src="/bmac.svg" alt="" className="size-10 rounded-[11px]" />
      <div>
        <h1 className="text-[18px] font-semibold tracking-[-0.015em] text-fg">BMAC backup appliance</h1>
        <p className="text-[12.5px] text-fg-subtle">{hello ? hello.name : location.hostname}</p>
      </div>
    </div>
  );
}

function Fingerprint({ hello }: { hello: ApplianceHello }) {
  return (
    <div className="mt-4 rounded-lg border border-line bg-sunken px-3 py-2 text-[12px] text-fg-muted">
      <div className="flex items-center gap-1.5 font-medium text-fg">
        <Shield className="size-3.5 text-accent" /> Check this certificate
      </div>
      <p className="mt-1">Your browser's certificate details must show this SHA-256 fingerprint, the same one the appliance's console shows:</p>
      <p className="selectable mt-1 break-all font-mono text-[11.5px] text-fg">{hello.fingerprint}</p>
    </div>
  );
}

function Locked({ hello, retry }: { hello: ApplianceHello; retry: () => void }) {
  return (
    <Frame>
      <Brand hello={hello} />
      <div className="rounded-xl border border-line bg-surface p-5 shadow-panel">
        <div className="flex items-center gap-2 text-[14px] font-semibold text-fg">
          <Lock className="size-4 text-warn" /> The backup pool is locked
        </div>
        <p className="mt-2 text-[13px] text-fg-muted">
          Unlock it at the appliance's own console with <span className="font-mono text-fg">bmac-backupd unlock</span>. The passphrase is never typed here, and nobody can log in until the pool is open, since the administrator's password is kept on it.
        </p>
        <Button className="mt-4" onClick={retry}>
          Check again
        </Button>
        <Fingerprint hello={hello} />
      </div>
    </Frame>
  );
}

function Login({ hello, done, notice }: { hello: ApplianceHello; done: () => void; notice: string | null }) {
  const api = useApi();
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await api.login(password);
      setPassword("");
      done();
    } catch (err) {
      setPassword("");
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Frame>
      <Brand hello={hello} />
      <div className="rounded-xl border border-line bg-surface p-5 shadow-panel">
        {notice && <p className="mb-4 rounded-lg border border-info/30 bg-info/8 px-3 py-2 text-[12.5px] text-fg">{notice}</p>}
        {hello.no_password ? (
          <p className="text-[13px] text-fg-muted">
            No administrator password is set yet. Set one at the appliance's console with <span className="font-mono text-fg">bmac-backupd admin-password</span>, then reload this page.
          </p>
        ) : (
          <form onSubmit={submit} className="space-y-4">
            <Problem>{error}</Problem>
            <div>
              <Label htmlFor="password" required>
                Administrator password
              </Label>
              <Input id="password" type="password" value={password} onChange={(e) => setPassword(e.target.value)} autoComplete="current-password" autoFocus />
            </div>
            <Button type="submit" variant="primary" size="lg" className="w-full" disabled={busy || !password}>
              {busy ? <Loader2 className="animate-spin" /> : <LogIn />} Log in
            </Button>
          </form>
        )}
        <Fingerprint hello={hello} />
      </div>
    </Frame>
  );
}

function Shell({ hello, logout }: { hello: ApplianceHello; logout: () => void }) {
  const [page, setPage] = useState<Page>("overview");
  const pages: Record<Page, ReactNode> = {
    overview: <OverviewPage />,
    pits: <PitsPage />,
    throughput: <ThroughputPage />,
    retention: <RetentionPage />,
    grants: <GrantsPage />,
    peers: <PeersPage />,
    settings: <SettingsPage />,
    audit: <AuditPage />,
  };
  return (
    <div className="flex h-full">
      <aside className="flex w-[216px] shrink-0 flex-col border-r border-line bg-sunken">
        <div className="flex h-[60px] items-center gap-2.5 px-4">
          <img src="/bmac.svg" alt="" className="size-8 shrink-0 rounded-[9px]" />
          <div className="min-w-0">
            <div className="text-[14px] font-semibold tracking-[-0.01em] text-fg">BMAC appliance</div>
            <div className="truncate text-[11.5px] text-fg-subtle">{hello.name}</div>
          </div>
        </div>
        <nav className="flex-1 space-y-0.5 overflow-y-auto px-2 pt-2">
          {NAV.map((item) => (
            <button
              key={item.id}
              onClick={() => setPage(item.id)}
              aria-current={page === item.id ? "page" : undefined}
              className={cn(
                "flex h-8 w-full items-center gap-2.5 rounded-md px-2.5 text-[13px] transition-colors [&_svg]:size-4",
                page === item.id ? "bg-surface-3 text-fg" : "text-fg-muted hover:bg-surface-2 hover:text-fg",
              )}
            >
              <span className={page === item.id ? "text-accent" : "text-fg-subtle"}>{item.icon}</span>
              {item.label}
            </button>
          ))}
        </nav>
        <div className="border-t border-line p-2">
          <Button variant="ghost" className="w-full justify-start" onClick={logout}>
            <LogOut /> Log out
          </Button>
          <div className="px-2.5 pt-1 text-[11px] text-fg-subtle">bmac-backupd {hello.version}</div>
        </div>
      </aside>
      <main className="flex min-w-0 flex-1 flex-col">{pages[page]}</main>
    </div>
  );
}

export function App({ api }: { api: Api }) {
  const [hello, setHello] = useState<ApplianceHello | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loggedIn, setLoggedIn] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);

  const check = useCallback(async () => {
    try {
      setHello(await api.hello());
      setError(null);
    } catch (err) {
      setError(errorMessage(err));
    }
  }, [api]);

  useEffect(() => {
    void check();
    return api.onSessionEnded((message) => {
      setLoggedIn(false);
      setNotice(message);
      void check();
    });
  }, [api, check]);

  let body: ReactNode;
  if (!hello) {
    body = (
      <Frame>
        <Brand hello={null} />
        {error ? <Problem>{error}</Problem> : <Loader2 className="size-5 animate-spin text-fg-subtle" />}
      </Frame>
    );
  } else if (hello.locked) {
    body = <Locked hello={hello} retry={() => void check()} />;
  } else if (!loggedIn) {
    body = (
      <Login
        hello={hello}
        notice={notice}
        done={() => {
          setNotice(null);
          setLoggedIn(true);
        }}
      />
    );
  } else {
    body = (
      <Shell
        hello={hello}
        logout={() => {
          void api.logout().finally(() => {
            setLoggedIn(false);
            setNotice("You are logged out.");
          });
        }}
      />
    );
  }
  return <ApiContext.Provider value={api}>{body}</ApiContext.Provider>;
}
