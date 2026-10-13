#!/usr/bin/env node
// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// Screenshots the v3 Web Dashboard's mock in every state v2's baseline
// shows, compares each with the v2 image, and writes a side-by-side report.
//
//   node dev/visual-baseline/compare.mjs [--url URL] [--out DIR] [--chrome PATH] [--only NAME-PREFIX]
//
// An image passes when it differs from v2's by at most the tolerance, or
// when intended.json says why it differs. It must also match the accepted
// v3 image in v3/, so a change after review is caught; --accept replaces
// those with this run's images. Without --url it starts the webui dev
// server and stops it afterwards.

import { spawn } from "node:child_process";
import { copyFile, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { existsSync } from "node:fs";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";
import pixelmatch from "pixelmatch";
import { PNG } from "pngjs";
import { chromium } from "playwright-core";

const HERE = dirname(fileURLToPath(import.meta.url));
const V3 = resolve(HERE, "../..");
const BASELINE = join(HERE, "v2");
const ACCEPTED = join(HERE, "v3");
const FIXED_TIME = new Date("2026-10-10T16:00:00Z");
const VIEWPORT = { width: 1440, height: 960 };
/** The share of pixels that may differ from v2 before an image needs a reason. */
const TOLERANCE = 0.01;
/** The share that may differ from the accepted v3 image. */
const DRIFT = 0.002;
const PORT = 5179;

const { values: opts } = parseArgs({
  options: {
    url: { type: "string" },
    out: { type: "string", default: join(V3, "target/visual") },
    chrome: { type: "string", default: process.env.BMAC_CHROME ?? "/usr/bin/google-chrome" },
    only: { type: "string" },
    accept: { type: "boolean", default: false },
  },
});

const shots = new Set();

async function reachable(url) {
  try {
    return (await fetch(url)).ok;
  } catch {
    return false;
  }
}

async function startDevServer() {
  const url = `http://localhost:${PORT}/`;
  if (await reachable(url)) return { url, stop: () => {} };
  const child = spawn("pnpm", ["exec", "vite", "--port", String(PORT), "--strictPort"], { cwd: join(V3, "webui"), stdio: ["ignore", "ignore", "inherit"], detached: true });
  for (let i = 0; i < 120; i += 1) {
    if (await reachable(url)) return { url, stop: () => process.kill(-child.pid, "SIGTERM") };
    await new Promise((r) => setTimeout(r, 250));
  }
  process.kill(-child.pid, "SIGTERM");
  throw new Error(`the webui dev server did not start on port ${PORT}`);
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
}
[data-radix-popper-content-wrapper]:has([role="tooltip"]) { display: none !important; }`;

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

async function openApp(browser, prepare) {
  const context = await browser.newContext({ viewport: VIEWPORT, deviceScaleFactor: 1, reducedMotion: "reduce", colorScheme: "dark", timezoneId: "UTC", locale: "en-US" });
  const page = await context.newPage();
  await page.clock.setFixedTime(FIXED_TIME);
  await page.addInitScript(PINNED);
  await page.goto(`${opts.url}?mock`);
  await page.addStyleTag({ content: STILL });
  await poll("the mock backend", () =>
    page.evaluate(async () => {
      const { useStore, api } = await import("/src/state/store.ts");
      try {
        window.__bmac = { useStore, mock: api() };
        return true;
      } catch {
        return false;
      }
    }),
  );
  if (prepare) await app(page, prepare);
  await page.getByLabel(/Proxmox user/).fill("alice@pve");
  await page.getByLabel(/Password/).fill("alice");
  await page.getByRole("button", { name: /Log in/ }).click();
  await page.getByLabel(/authenticator app/).fill("123456");
  await page.getByRole("button", { name: /Continue/ }).click();
  await page.getByRole("button", { name: "Log out" }).waitFor();
  await idle(page);
  return { context, page };
}

/** No status fetch in flight and nothing changed for a moment. */
async function idle(page) {
  const settled = () =>
    app(page, () => {
      const s = window.__bmac.useStore.getState();
      return s.phase === "ready" && Object.values(s.status).every((e) => !e.loading);
    });
  await poll("the app to settle", async () => (await settled()) && (await page.waitForTimeout(600), await settled()));
}

const job = (page, id) =>
  app(
    page,
    (jobId) => {
      const j = window.__bmac.mock.jobsById.get(jobId);
      return j && { state: j.summary.state, pending: j.summary.pending_request ?? null, events: j.events.map((e) => ({ type: e.type, id: e.id, status: e.status, request_id: e.request_id })) };
    },
    id,
  );

const waitRequest = (page, id, requestId) =>
  poll(`request ${requestId}`, async () => {
    const j = await job(page, id);
    return j?.pending === requestId ? j : null;
  });

async function waitState(page, id, states) {
  const j = await poll(`job state ${states}`, async () => {
    const current = await job(page, id);
    return current && states.includes(current.state) ? current : null;
  });
  await idle(page);
  return j;
}

const launch = (page, operation, parameters = {}, dryRun = false) =>
  app(page, async ([op, params, dry]) => (await window.__bmac.useStore.getState().launchJob(op, params, dry)).id, [operation, parameters, dryRun]);

/**
 * Pauses scripted jobs at their next step (true), or once they have that
 * many events (a number), so a running job looks the same in every capture.
 */
const hold = (page, value) => app(page, (v) => (window.__bmac.mock.holdJobs = v), value);

async function held(page, id, events) {
  await poll(`job ${id} to reach ${events} events`, async () => (await job(page, id))?.events.length >= events);
  await page.waitForTimeout(300);
}

const eventCount = async (page, id) => (await job(page, id)).events.length;

const answer = (page, id, requestId, value) => app(page, ([j, r, a]) => window.__bmac.useStore.getState().answer(j, r, a), [id, requestId, value]);

const call = (page, action, ...args) => app(page, ([a, rest]) => window.__bmac.useStore.getState()[a](...rest), [action, args]);

async function setPage(page, id) {
  await call(page, "setPage", id);
  await idle(page);
}

async function snap(page, name) {
  if (opts.only && !name.startsWith(opts.only)) return;
  await page.mouse.move(VIEWPORT.width / 2, 4);
  await page.waitForTimeout(400);
  await page.screenshot({ path: join(opts.out, "v3", `${name}.png`), animations: "disabled", caret: "hide" });
  shots.add(`${name}.png`);
  console.log(`  ${name}.png`);
}

/** v2's pages and the v3 page that replaces each. */
const PAGES = [
  ["dashboard", "overview"],
  ["hosts", "hosts"],
  ["storage", "storage"],
  ["production", "production"],
  ["staging", "staging"],
  ["qdevice", "qdevice"],
  ["diagnostics", "diagnostics"],
  ["operations", "activity"],
  ["config", "settings"],
  ["settings", "settings"],
];

async function capturePopulated(browser) {
  console.log("Populated cluster:");
  const { context, page } = await openApp(browser);
  for (const [v2, v3] of PAGES) {
    await setPage(page, v3);
    await snap(page, `page-${v2}`);
  }
  await setPage(page, "settings");
  const cores = page.getByLabel(/^PROD_VM_CORES/);
  await cores.fill("48");
  await snap(page, "page-config-editor");
  await page.getByRole("button", { name: "Discard changes" }).first().click();

  await setPage(page, "overview");
  await page.getByRole("button", { name: /Choose job/ }).first().click();
  await snap(page, "menu-run-switcher");
  await page.keyboard.press("Escape");
  await call(page, "setConsoleOpen", true);
  await snap(page, "console-open");
  await call(page, "setConsoleOpen", false);

  await setPage(page, "staging");
  await call(page, "openLaunch", "staging.add");
  await snap(page, "dialog-launch");
  await call(page, "closeLaunch");
  await call(page, "openLaunch", "staging.remove", { resource: "stage1prod1" });
  await page.getByRole("dialog").getByRole("switch").first().click().catch(() => {});
  await snap(page, "dialog-launch-destructive");
  await call(page, "closeLaunch");
  await setPage(page, "diagnostics");
  await call(page, "openLaunch", "diagnostics.host_state");
  await snap(page, "dialog-launch-read-only");
  await call(page, "closeLaunch");

  await hold(page, 30);
  let id = await launch(page, "diagnostics.cluster_health");
  await held(page, id, 30);
  await snap(page, "run-running-read-only");
  await call(page, "setConsoleOpen", true);
  await snap(page, "run-running-read-only-console");
  await call(page, "setConsoleOpen", false);
  await hold(page, false);
  await waitState(page, id, ["COMPLETED"]);
  await snap(page, "run-succeeded-read-only");

  await setPage(page, "staging");
  id = await launch(page, "staging.add", {}, true);
  await waitState(page, id, ["COMPLETED"]);
  await snap(page, "run-dry-run");

  id = await launch(page, "staging.add");
  await waitRequest(page, id, "req-1-source");
  await idle(page);
  await snap(page, "request-input-select");
  await answer(page, id, "req-1-source", { kind: "values", values: { source: "prod1" } });
  await waitRequest(page, id, "req-1b-snapshot");
  await idle(page);
  await snap(page, "request-input-select-default");
  await answer(page, id, "req-1b-snapshot", { kind: "values", values: { choice: "new" } });
  await waitRequest(page, id, "req-2-resources");
  await idle(page);
  await snap(page, "request-input-group");
  const panel = page.getByRole("complementary", { name: "Job panel" });
  await panel.getByRole("textbox", { name: /Memory/ }).fill("128");
  await panel.getByRole("button", { name: "Continue" }).click();
  await poll("the validation error", async () => (await job(page, id))?.events.some((e) => e.type === "validation_error"));
  await idle(page);
  await snap(page, "request-validation-error");
  await panel.getByRole("textbox", { name: /Memory/ }).fill("16");
  await panel.getByRole("button", { name: "Continue" }).click();
  await waitRequest(page, id, "req-3-confirm");
  await idle(page);
  await snap(page, "request-plan-confirm");
  // The answer, two phase changes, then a progress event and a line per step: four steps.
  const progressed = (await eventCount(page, id)) + 11;
  await hold(page, progressed);
  await answer(page, id, "req-3-confirm", { kind: "confirm", confirmed: true });
  await held(page, id, progressed);
  await snap(page, "run-running-progress");
  await hold(page, false);
  // The commit line is followed by a long pause, so holding then stops right there.
  await poll("the commit phase", async () => (await job(page, id))?.events.some((e) => e.type === "phase" && e.id === "commit" && e.status === "running"));
  await hold(page, true);
  await page.waitForTimeout(300);
  await snap(page, "run-running-no-cancel");
  await hold(page, false);
  await waitState(page, id, ["COMPLETED"]);
  await snap(page, "run-succeeded");

  id = await launch(page, "staging.remove", { resource: "stage1prod1" });
  await waitRequest(page, id, "req-3-confirm");
  await idle(page);
  await snap(page, "request-confirm-typed");
  await panel.getByRole("textbox").last().fill("DESTROY stage1prod1");
  await snap(page, "request-confirm-typed-filled");
  await call(page, "cancel", id);
  await waitState(page, id, ["CANCELLED"]);
  await snap(page, "run-cancelled");

  await setPage(page, "storage");
  id = await launch(page, "lab.wipe", { host: "mox3" });
  await waitRequest(page, id, "req-3-confirm");
  await idle(page);
  await snap(page, "request-confirm-destructive-storage");
  await answer(page, id, "req-3-confirm", { kind: "confirm", confirmed: true });
  await waitRequest(page, id, "req-4-console");
  await idle(page);
  await snap(page, "request-manual-action");
  await answer(page, id, "req-4-console", { kind: "acknowledge" });
  await waitState(page, id, ["COMPLETED"]);

  // As in v2's capture: an answer without values makes the scenario fail.
  await setPage(page, "staging");
  id = await launch(page, "staging.add");
  await waitRequest(page, id, "req-1-source");
  await answer(page, id, "req-1-source", { kind: "values", values: { source: "prod2" } });
  await waitRequest(page, id, "req-2-resources");
  await answer(page, id, "req-2-resources", { kind: "values", values: null });
  await waitState(page, id, ["FAILED"]);
  await snap(page, "run-failed");

  await setPage(page, "activity");
  await call(page, "setPanelOpen", false);
  await snap(page, "page-operations-history");
  await context.close();
}

async function captureFresh(browser) {
  console.log("No cluster settings yet:");
  const { context, page } = await openApp(browser, () => {
    window.__bmac.mock.settingsData.cluster = null;
  });
  await setPage(page, "settings");
  await snap(page, "fresh-landing");
  await snap(page, "fresh-page-settings");
  await context.close();
}

function difference(a, b, diffPath) {
  const left = PNG.sync.read(a);
  const right = PNG.sync.read(b);
  if (left.width !== right.width || left.height !== right.height) return { ratio: 1, diff: null };
  const diff = new PNG({ width: left.width, height: left.height });
  const pixels = pixelmatch(left.data, right.data, diff.data, left.width, left.height, { threshold: 0.1 });
  return { ratio: pixels / (left.width * left.height), diff: { path: diffPath, png: PNG.sync.write(diff) } };
}

const escape = (s) => String(s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]);

async function compare() {
  const manifest = JSON.parse(await readFile(join(BASELINE, "manifest.json"), "utf8"));
  const intended = JSON.parse(await readFile(join(HERE, "intended.json"), "utf8"));
  const rows = [];
  let failures = 0;
  for (const { name, description } of manifest.shots) {
    if (opts.only && !name.startsWith(opts.only)) continue;
    const v3 = join(opts.out, "v3", name);
    const reason = intended[name];
    let ratio = null;
    let diffPath = null;
    if (existsSync(v3)) {
      const result = difference(await readFile(join(BASELINE, name)), await readFile(v3), join(opts.out, "diff", name));
      ratio = result.ratio;
      if (result.diff) {
        await writeFile(result.diff.path, result.diff.png);
        diffPath = result.diff.path;
      }
    }
    const close = ratio !== null && ratio <= TOLERANCE;
    let status = close ? (reason ? "stale reason" : "match") : reason ? "intended" : "FAIL";
    let drift = null;
    const accepted = join(ACCEPTED, name);
    if (existsSync(v3) && !opts.accept) {
      drift = existsSync(accepted) ? difference(await readFile(accepted), await readFile(v3), join(opts.out, "drift", name)) : { ratio: 1, diff: null };
      if (drift.diff && drift.ratio > DRIFT) await writeFile(drift.diff.path, drift.diff.png);
      if (drift.ratio > DRIFT) status = "CHANGED";
    }
    if (status === "FAIL" || status === "CHANGED") failures += 1;
    rows.push({ name, description, ratio, status, reason, v3: existsSync(v3) ? v3 : null, diff: diffPath });
    const note = status === "FAIL" && ratio === null ? " (no v3 image)" : status === "CHANGED" ? ` (${existsSync(accepted) ? `${(drift.ratio * 100).toFixed(2)}% from the accepted image` : "no accepted image"})` : "";
    console.log(`${status.padEnd(12)} ${ratio === null ? "  n/a " : `${(ratio * 100).toFixed(1).padStart(5)}%`}  ${name}${note}`);
  }
  if (opts.accept) {
    await mkdir(ACCEPTED, { recursive: true });
    for (const r of rows) if (r.v3) await copyFile(r.v3, join(ACCEPTED, r.name));
    console.log(`Accepted ${rows.filter((r) => r.v3).length} images into ${ACCEPTED}`);
  }
  const rel = (p) => (p ? relative(opts.out, p) : null);
  const html = `<!doctype html><meta charset="utf-8"><title>BMAC visual comparison</title>
<style>body{font:13px system-ui;background:#111;color:#ddd;margin:20px}h2{font-size:14px;margin:28px 0 6px}.row{display:grid;grid-template-columns:repeat(3,1fr);gap:8px}img{width:100%;border:1px solid #333}.FAIL,.CHANGED{color:#f66}.intended{color:#fc6}.match{color:#6c6}p{margin:2px 0;color:#999}</style>
<h1>v2 baseline, v3, and their difference</h1><p>Tolerance ${(TOLERANCE * 100).toFixed(1)}% of pixels. ${failures} failing.</p>
${rows
  .map(
    (r) => `<h2>${escape(r.name)} <span class="${r.status.split(" ")[0]}">${escape(r.status)}${r.ratio === null ? "" : `, ${(r.ratio * 100).toFixed(1)}% differs`}</span></h2>
<p>${escape(r.description)}</p>${r.reason ? `<p>Why it differs: ${escape(r.reason)}</p>` : ""}
<div class="row"><img src="${escape(relative(opts.out, join(BASELINE, r.name)))}">${r.v3 ? `<img src="${escape(rel(r.v3))}">` : "<div>no v3 image</div>"}${r.diff ? `<img src="${escape(rel(r.diff))}">` : "<div></div>"}</div>`,
  )
  .join("\n")}`;
  await writeFile(join(opts.out, "report.html"), html);
  console.log(`\nReport: ${join(opts.out, "report.html")}`);
  return failures;
}

const server = opts.url ? { url: opts.url, stop: () => {} } : await startDevServer();
opts.url = server.url.replace(/\/$/, "") + "/";
await mkdir(join(opts.out, "v3"), { recursive: true });
for (const dir of ["diff", "drift"]) {
  await rm(join(opts.out, dir), { recursive: true, force: true });
  await mkdir(join(opts.out, dir), { recursive: true });
}
const browser = await chromium.launch({ executablePath: opts.chrome, headless: true, args: ["--hide-scrollbars"] });
try {
  await capturePopulated(browser);
  await captureFresh(browser);
} finally {
  await browser.close();
  server.stop();
}
const failures = await compare();
if (failures) {
  console.error(`${failures} image(s) differ from v2 without a reason in intended.json, or changed since they were accepted`);
  process.exit(1);
}
