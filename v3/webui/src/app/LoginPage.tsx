// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// Logging in: a Proxmox password, then a TOTP code or a recovery key when
// the account has a second factor (SECURITY.md, "Login"). Nothing typed here
// is kept once it is sent.

import { useState, type FormEvent } from "react";
import { AlertTriangle, ChevronRight, KeyRound, Loader2, LogIn, ShieldCheck } from "lucide-react";
import { Button, Input, Label, cn } from "@bmac/ui";
import { ApiFailure, errorMessage } from "../lib/backend";
import { api, useStore } from "../state/store";

type Method = "totp" | "recovery";

function failureText(error: unknown): string {
  if (error instanceof ApiFailure && error.code === "rate_limited") {
    return error.retryAfterSeconds ? `${error.message} Try again in ${error.retryAfterSeconds} seconds.` : error.message;
  }
  return errorMessage(error);
}

export function LoginPage() {
  const notice = useStore((s) => s.loginNotice);
  const begin = useStore((s) => s.begin);
  const [user, setUser] = useState("");
  const [password, setPassword] = useState("");
  const [challenge, setChallenge] = useState<{ id: string; methods: Method[] } | null>(null);
  const [method, setMethod] = useState<Method>("totp");
  const [code, setCode] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const submitPassword = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const result = await api().login({ step: "password", user: user.trim(), password });
      setPassword("");
      if (result.step === "second_factor") {
        const methods = result.methods.filter((m): m is Method => m === "totp" || m === "recovery");
        if (!methods.length) {
          setError("This account's second factor isn't one BMAC supports yet. Add a TOTP app or recovery keys in Proxmox.");
          return;
        }
        setChallenge({ id: result.challenge, methods });
        setMethod(methods[0]);
      } else {
        await begin();
      }
    } catch (err) {
      setPassword("");
      setError(failureText(err));
    } finally {
      setBusy(false);
    }
  };

  const submitCode = async (e: FormEvent) => {
    e.preventDefault();
    if (!challenge) return;
    setBusy(true);
    setError(null);
    try {
      const result = await api().login({ step: "second_factor", challenge: challenge.id, method, response: code.trim() });
      setCode("");
      if (result.step === "session") await begin();
    } catch (err) {
      setCode("");
      setError(failureText(err));
      if (err instanceof ApiFailure && err.code === "unauthenticated" && /password/i.test(err.message)) setChallenge(null);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="grid-bg flex h-full items-center justify-center overflow-y-auto px-6 py-10">
      <div className="w-full max-w-[400px]">
        <div className="mb-6 flex items-center gap-3">
          <img src="/bmac.svg" alt="" className="size-10 rounded-[11px]" />
          <div>
            <h1 className="text-[18px] font-semibold tracking-[-0.015em] text-fg">BMAC Dashboard</h1>
            <p className="text-[12.5px] text-fg-subtle">{location.hostname}</p>
          </div>
        </div>

        <div className="rounded-xl border border-line bg-surface p-5 shadow-panel">
          {notice && (
            <div role="status" className="mb-4 flex items-start gap-2 rounded-lg border border-info/30 bg-info/8 px-3 py-2 text-[12.5px] text-fg">
              <AlertTriangle className="mt-0.5 size-3.5 shrink-0 text-info" /> {notice}
            </div>
          )}
          {error && (
            <div role="alert" className="mb-4 flex items-start gap-2 rounded-lg border border-danger/40 bg-danger/8 px-3 py-2 text-[12.5px] text-danger">
              <AlertTriangle className="mt-0.5 size-3.5 shrink-0" /> {error}
            </div>
          )}

          {!challenge ? (
            <form onSubmit={submitPassword} className="space-y-4">
              <div>
                <Label htmlFor="login-user" required>
                  Proxmox user
                </Label>
                <Input id="login-user" value={user} onChange={(e) => setUser(e.target.value)} placeholder="alice@pve" autoComplete="username" spellCheck={false} autoFocus mono />
              </div>
              <div>
                <Label htmlFor="login-password" required>
                  Password
                </Label>
                <Input id="login-password" type="password" value={password} onChange={(e) => setPassword(e.target.value)} autoComplete="current-password" />
              </div>
              <Button type="submit" variant="primary" size="lg" className="w-full" disabled={busy || !user.trim() || !password}>
                {busy ? <Loader2 className="animate-spin" /> : <LogIn />} Log in
              </Button>
            </form>
          ) : (
            <form onSubmit={submitCode} className="space-y-4">
              <p className="text-[12.5px] text-fg-muted">
                <span className="font-mono text-fg">{user.trim()}</span> has a second factor.
              </p>
              {challenge.methods.length > 1 && (
                <div role="radiogroup" aria-label="Second factor" className="grid grid-cols-2 gap-1.5">
                  {challenge.methods.map((m) => (
                    <button
                      key={m}
                      type="button"
                      role="radio"
                      aria-checked={method === m}
                      onClick={() => setMethod(m)}
                      className={cn("rounded-lg border px-3 py-2 text-[12.5px]", method === m ? "border-accent/60 bg-accent/8 text-fg" : "border-line-strong bg-sunken text-fg-muted hover:text-fg")}
                    >
                      {m === "totp" ? "Authenticator code" : "Recovery key"}
                    </button>
                  ))}
                </div>
              )}
              <div>
                <Label htmlFor="login-code" required>
                  {method === "totp" ? "Code from your authenticator app" : "One of your recovery keys"}
                </Label>
                <Input
                  id="login-code"
                  value={code}
                  onChange={(e) => setCode(e.target.value)}
                  inputMode={method === "totp" ? "numeric" : undefined}
                  autoComplete="one-time-code"
                  spellCheck={false}
                  autoFocus
                  mono
                />
              </div>
              <div className="flex gap-2">
                <Button
                  type="button"
                  variant="ghost"
                  onClick={() => {
                    setChallenge(null);
                    setCode("");
                    setError(null);
                  }}
                >
                  Back
                </Button>
                <Button type="submit" variant="primary" size="lg" className="flex-1" disabled={busy || !code.trim()}>
                  {busy ? <Loader2 className="animate-spin" /> : <KeyRound />} Continue
                </Button>
              </div>
            </form>
          )}
          <p className="mt-4 text-[11.5px] leading-relaxed text-fg-subtle">
            Second factors: authenticator (TOTP) codes and recovery keys. Security keys (WebAuthn) aren't supported yet. You log in again after reloading or in a new tab; BMAC keeps no login in the browser.
          </p>
        </div>

        <CertificateGuidance />
      </div>
    </div>
  );
}

/** First-visit guidance: compare this host's certificate with its host card (SECURITY.md, "Verification"). */
function CertificateGuidance() {
  const [open, setOpen] = useState(false);
  const host = `${location.hostname}:${location.port || "443"}`;
  return (
    <div className="mt-4 rounded-xl border border-line bg-surface/70">
      <button onClick={() => setOpen(!open)} className="flex w-full items-center gap-2 px-4 py-3 text-left text-[12.5px] font-medium text-fg-muted hover:text-fg">
        <ShieldCheck className="size-4 text-accent" />
        <span className="flex-1">First visit? Check this host's certificate before logging in.</span>
        <ChevronRight className={cn("size-3.5 transition-transform", open && "rotate-90")} />
      </button>
      {open && (
        <div className="space-y-3 border-t border-line px-4 py-3 text-[12px] leading-relaxed text-fg-muted">
          <p>
            Compare the certificate's SHA-256 fingerprint with the one on this host's card from the Deployer, or with the list on Settings in a Dashboard you already verified. Log in only if they match.
          </p>
          <ul className="space-y-1.5">
            <li>
              <span className="text-fg">Chrome or Edge:</span> click the icon left of the address, then Connection is not secure (or Certificate details), then the Details tab; read SHA-256 Fingerprint.
            </li>
            <li>
              <span className="text-fg">Firefox:</span> click the padlock, then Connection not secure, More information, View Certificate; read Fingerprints, SHA-256.
            </li>
            <li>
              <span className="text-fg">Safari:</span> on the warning page click Show Details, then view the certificate; read SHA-256 under Fingerprints.
            </li>
          </ul>
          <p>Or, from a terminal:</p>
          <pre className="selectable overflow-x-auto rounded-md bg-[#07090c] px-3 py-2 font-mono text-[11px] text-fg">{`openssl s_client -connect ${host} \\\n  </dev/null 2>/dev/null \\\n  | openssl x509 -noout -fingerprint -sha256`}</pre>
        </div>
      )}
    </div>
  );
}
