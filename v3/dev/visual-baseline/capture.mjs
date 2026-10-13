#!/usr/bin/env node
// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// Captures the v2 Dashboard's mock preview as the visual baseline for v3.
//
//   node dev/visual-baseline/capture.mjs [--url URL] [--out DIR] [--chrome PATH] [--only NAME-PREFIX]
//
// Without --url it starts `pnpm web:dev` in ../dashboard (v2) and stops it
// afterwards. The clock, run IDs, and process IDs are pinned so repeated
// captures of the same v2 commit produce the same images.

import { spawn } from "node:child_process";
import { mkdir, writeFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";
import { chromium } from "playwright-core";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "../../..");
const FIXED_TIME = new Date("2026-10-10T16:00:00Z");
const VIEWPORT = { width: 1440, height: 960 };

const { values: opts } = parseArgs({
  options: {
    url: { type: "string" },
    out: { type: "string", default: join(HERE, "v2") },
    chrome: { type: "string", default: process.env.BMAC_CHROME ?? "/usr/bin/google-chrome" },
    only: { type: "string" },
  },
});

const shots = [];

async function reachable(url) {
  try {
    return (await fetch(url)).ok;
  } catch {
    return false;
  }
}

async function startPreview() {
  const url = "http://localhost:1420/";
  if (await reachable(url)) return { url, stop: () => {} };
  const child = spawn("pnpm", ["web:dev"], { cwd: join(REPO, "dashboard"), stdio: ["ignore", "ignore", "inherit"], detached: true });
  for (let i = 0; i < 120; i += 1) {
    if (await reachable(url)) return { url, stop: () => process.kill(-child.pid, "SIGTERM") };
    await new Promise((r) => setTimeout(r, 250));
  }
  process.kill(-child.pid, "SIGTERM");
  throw new Error("the v2 mock preview did not start on port 1420");
}

const PINNED = `
  (() => {
    let seed = 20261010;
    Math.random = () => ((seed = (seed * 16807) % 2147483647) - 1) / 2147483646;
    let n = 0;
    Object.defineProperty(crypto, "randomUUID", { value: () => "00000000-0000-4000-8000-" + String(++n).padStart(12, "0") });
  })();
`;

const STILL = `*, *::before, *::after {
  animation-duration: 0s !important; animation-delay: 0s !important; animation-iteration-count: 1 !important;
  transition: none !important; caret-color: transparent !important;
}`;

async function openApp(browser, query = "") {
  const context = await browser.newContext({ viewport: VIEWPORT, deviceScaleFactor: 1, reducedMotion: "reduce", colorScheme: "dark", timezoneId: "UTC", locale: "en-US" });
  const page = await context.newPage();
  await page.clock.setFixedTime(FIXED_TIME);
  await page.addInitScript(PINNED);
  await page.goto(`${opts.url}${query}`);
  await page.addStyleTag({ content: STILL });
  await page.evaluate(async () => {
    const [{ useStore }, { api }] = await Promise.all([import("/src/state/store.ts"), import("/src/lib/api.ts")]);
    window.__bmac = { useStore, api };
  });
  await idle(page);
  return { context, page };
}

const app = (page, fn, arg) => page.evaluate(fn, arg);

async function poll(what, check, timeout = 30_000) {
  const until = Date.now() + timeout;
  for (;;) {
    const value = await check();
    if (value) return value;
    if (Date.now() > until) throw new Error(`timed out waiting for ${what}`);
    await new Promise((r) => setTimeout(r, 100));
  }
}

async function idle(page) {
  const settled = () =>
    app(page, async () => {
      const s = window.__bmac.useStore.getState();
      try {
        return s.ready && !s.preflight.loading && !s.launch && (await window.__bmac.api().getActiveRuns()).length === 0;
      } catch {
        return false;
      }
    });
  await poll("the app to settle", async () => (await settled()) && (await page.waitForTimeout(800), await settled()));
}

const record = (page, runId) => app(page, async (id) => (await window.__bmac.api().getRunDetails(id)).record, runId);

const waitRequest = (page, runId, requestId) =>
  poll(`request ${requestId}`, async () => {
    const r = await record(page, runId);
    return r.pending_request?.request_id === requestId ? r : null;
  });

const FINISHED = ["succeeded", "failed", "cancelled"];

async function waitStatus(page, runId, statuses) {
  const r = await poll(`run status ${statuses}`, async () => {
    const current = await record(page, runId);
    return statuses.includes(current.status) ? current : null;
  });
  if (FINISHED.includes(r.status)) await idle(page);
  return r;
}

const startRun = (page, workflowId, values) =>
  app(page, async ([id, v]) => (await window.__bmac.useStore.getState().startRun(id, v, { focus: true })).run_id, [workflowId, values]);

const respond = (page, runId, requestId, payload) =>
  app(page, ([run, req, p]) => window.__bmac.useStore.getState().respond(run, req, p), [runId, requestId, payload]);

async function setPage(page, id) {
  await app(page, (p) => window.__bmac.useStore.getState().setPage(p), id);
  await idle(page);
}

const call = (page, action, ...args) => app(page, ([a, rest]) => window.__bmac.useStore.getState()[a](...rest), [action, args]);

async function snap(page, name, description) {
  if (opts.only && !name.startsWith(opts.only)) return;
  await page.mouse.move(VIEWPORT.width / 2, 4);
  await page.waitForTimeout(400);
  await page.screenshot({ path: join(opts.out, `${name}.png`), animations: "disabled", caret: "hide" });
  shots.push({ name: `${name}.png`, description });
  console.log(`  ${name}.png`);
}

const PAGES = [
  ["dashboard", "Dashboard"],
  ["hosts", "Hosts"],
  ["storage", "Storage"],
  ["production", "Production VMs"],
  ["staging", "Staging VMs"],
  ["qdevice", "QDevice"],
  ["diagnostics", "Diagnostics"],
  ["operations", "Operations"],
  ["config", "Config"],
  ["settings", "Settings"],
];

async function pages(page, prefix, mode) {
  for (const [id, label] of PAGES) {
    await setPage(page, id);
    await snap(page, `${prefix}page-${id}`, `${label} page, ${mode}`);
  }
}

async function capturePopulated(browser) {
  console.log("Populated cluster:");
  const { context, page } = await openApp(browser);
  await pages(page, "", "populated cluster");

  const configFile = await app(page, async () => (await window.__bmac.api().getConfigListing()).files[0]?.name ?? null);
  await call(page, "openConfig", configFile);
  await snap(page, "page-config-editor", `Config page with ${configFile} open in the editor`);

  await setPage(page, "dashboard");
  await page.getByRole("button", { name: "Choose run" }).click();
  await snap(page, "menu-run-switcher", "Run switcher menu listing the startup refreshes");
  await page.keyboard.press("Escape");
  await call(page, "setConsoleOpen", true);
  await snap(page, "console-open", "Under-the-hood console expanded");
  await call(page, "setConsoleOpen", false);

  await setPage(page, "staging");
  await call(page, "openLaunch", "add_staging_vm");
  await snap(page, "dialog-launch", "Launch dialog for a mutating workflow with dry run on by default");
  await page.getByRole("button", { name: "Advanced options" }).click();
  await snap(page, "dialog-launch-advanced", "Launch dialog with advanced options expanded");
  await page.getByRole("button", { name: /browse/i }).first().click();
  await snap(page, "dialog-file-browser", "File browser dialog opened from a path field");
  await page.keyboard.press("Escape");
  await call(page, "closeLaunch");
  await call(page, "openLaunch", "remove_staging_vm", { resource: "stage1prod1", dry_run: false });
  await snap(page, "dialog-launch-destructive", "Launch dialog for a destructive workflow with dry run off");
  await call(page, "closeLaunch");
  await setPage(page, "diagnostics");
  await call(page, "openLaunch", "show_proxmox_host_state");
  await snap(page, "dialog-launch-read-only", "Launch dialog for a read-only workflow with a required field");
  await call(page, "closeLaunch");

  await setPage(page, "diagnostics");
  let run = await startRun(page, "show_cluster_health", {});
  await poll("read-only output", async () => (await record(page, run)).log_lines > 20);
  await snap(page, "run-running-read-only", "Read-only run in progress with phases");
  await call(page, "setConsoleOpen", true);
  await snap(page, "run-running-read-only-console", "Read-only run in progress with the console showing its output");
  await call(page, "setConsoleOpen", false);
  await waitStatus(page, run, ["succeeded"]);
  await snap(page, "run-succeeded-read-only", "Read-only run finished with a warning and a next step");

  await setPage(page, "staging");
  run = await startRun(page, "add_staging_vm", { dry_run: true });
  await waitStatus(page, run, ["succeeded"]);
  await snap(page, "run-dry-run", "Dry run finished with a plan-only result");

  run = await startRun(page, "add_staging_vm", { dry_run: false });
  await waitRequest(page, run, "req-1-source");
  await snap(page, "request-input-select", "Waiting for input: a select field with context");
  await respond(page, run, "req-1-source", { kind: "values", values: { source: "prod1" } });
  await waitRequest(page, run, "req-1b-snapshot");
  await snap(page, "request-input-select-default", "Waiting for input: a select field with a default");
  await respond(page, run, "req-1b-snapshot", { kind: "values", values: { choice: "new" } });
  await waitRequest(page, run, "req-2-resources");
  await snap(page, "request-input-group", "Waiting for input: an input group form");
  const panel = page.getByRole("complementary", { name: "Workflow panel" });
  await panel.getByRole("textbox", { name: /Memory/ }).fill("128");
  await panel.getByRole("button", { name: "Continue" }).click();
  await poll("the validation error", async () => {
    const r = await record(page, run);
    return r.pending_request?.request_id === "req-2-resources" && r.responses.length === 3 ? r : null;
  });
  await snap(page, "request-validation-error", "Waiting for input: the script rejected a field value");
  await panel.getByRole("textbox", { name: /Memory/ }).fill("16");
  await panel.getByRole("button", { name: "Continue" }).click();
  await waitRequest(page, run, "req-3-confirm");
  await snap(page, "request-plan-confirm", "Plan followed by a normal confirmation");
  await respond(page, run, "req-3-confirm", { kind: "confirm", confirmed: true });
  await poll("progress", async () => (await record(page, run)).current_activity?.includes("Replicating"));
  await page.waitForTimeout(1200);
  await snap(page, "run-running-progress", "Mutating run in progress with a progress bar");
  await poll("the commit phase", async () => (await record(page, run)).cancel_allowed === false && (await record(page, run)).status === "running");
  await snap(page, "run-running-no-cancel", "Run in a phase the script marked unsafe to interrupt");
  await waitStatus(page, run, ["succeeded"]);
  await snap(page, "run-succeeded", "Mutating run finished with a result and next steps");

  run = await startRun(page, "remove_staging_vm", { dry_run: false, resource: "stage1prod1" });
  await waitRequest(page, run, "req-3-confirm");
  await snap(page, "request-confirm-typed", "Destructive confirmation requiring a typed phrase");
  await page.getByRole("textbox").last().fill("DESTROY stage1prod1");
  await snap(page, "request-confirm-typed-filled", "Destructive confirmation with the phrase typed");
  await call(page, "cancel", run);
  await waitStatus(page, run, ["cancelled"]);
  await snap(page, "run-cancelled", "Run cancelled by the operator while waiting");

  await setPage(page, "storage");
  run = await startRun(page, "add_new_disk_vdev", { host: "mox3" });
  await waitRequest(page, run, "req-3-confirm");
  await snap(page, "request-confirm-destructive-storage", "Destructive storage confirmation");
  await respond(page, run, "req-3-confirm", { kind: "confirm", confirmed: true });
  await waitRequest(page, run, "req-4-console");
  await snap(page, "request-manual-action", "Waiting for a manual action at the host console");
  await respond(page, run, "req-4-console", { kind: "acknowledge" });
  await waitStatus(page, run, ["succeeded"]);

  // The mock has no failing scenario; an answer missing its values makes the
  // scenario throw, which the mock reports as a failed run.
  await setPage(page, "staging");
  run = await startRun(page, "add_staging_vm", { dry_run: false });
  await waitRequest(page, run, "req-1-source");
  await respond(page, run, "req-1-source", { kind: "values", values: { source: "prod2" } });
  await waitRequest(page, run, "req-2-resources");
  await respond(page, run, "req-2-resources", { kind: "values", values: null });
  await waitStatus(page, run, ["failed"]);
  await snap(page, "run-failed", "Run that failed");

  await setPage(page, "operations");
  await call(page, "setPanelOpen", false);
  await snap(page, "page-operations-history", "Operations page after every run state above");

  await call(page, "setCloseRequested", 1);
  await snap(page, "dialog-close-requested", "Quit confirmation while a run is active");
  await context.close();
}

async function captureFresh(browser) {
  console.log("First launch:");
  const { context, page } = await openApp(browser, "?fresh");
  await snap(page, "fresh-landing", "First launch lands on Settings with setup steps");
  await pages(page, "fresh-", "first launch with no cluster");
  await context.close();
}

const preview = opts.url ? { url: opts.url, stop: () => {} } : await startPreview();
opts.url = preview.url.replace(/\/$/, "") + "/";
await mkdir(opts.out, { recursive: true });
const browser = await chromium.launch({ executablePath: opts.chrome, headless: true, args: ["--hide-scrollbars"] });
try {
  await capturePopulated(browser);
  await captureFresh(browser);
} finally {
  await browser.close();
  preview.stop();
}
if (!opts.only) {
  const manifest = { source: "v2 Dashboard mock preview (dashboard/src/lib/mock)", viewport: VIEWPORT, fixed_time: FIXED_TIME.toISOString(), shots };
  await writeFile(join(opts.out, "manifest.json"), `${JSON.stringify(manifest, null, 2)}\n`);
}
console.log(`${shots.length} screenshots in ${opts.out}`);
