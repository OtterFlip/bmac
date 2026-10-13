#!/usr/bin/env node
// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// Drives the Web Dashboard in Chrome against a lab host's bmacd (M3):
// logs in as alice with TOTP (or a recovery key), runs lab.interactive
// through every request kind, and fails on any Content Security Policy
// violation, console error, cookie, or browser storage use.
//
//   ssh -L 127.0.0.1:18443:100.64.0.11:8443 ...   (see lab/README.md)
//   node lab/browser/check-webui.mjs --url https://127.0.0.1:18443 [--recovery N] [--out DIR]
//
// --recovery N logs in with alice's Nth recovery key (0-9) instead of TOTP.
// Proxmox deletes a recovery key once it is used, so each N works once.
//
// --rotate then rotates the certificate of the host behind --url from the
// Settings page: the new fingerprint shown before the switch must be the
// one the host serves afterwards, and after logging in again Settings must
// show it as current and as the host's published fingerprint.
//
// Secrets are read from the lab's users.json and never printed.

import { X509Certificate, createHmac } from "node:crypto";
import { mkdir, readFile } from "node:fs/promises";
import { homedir, tmpdir } from "node:os";
import { join } from "node:path";
import { connect } from "node:tls";
import { parseArgs } from "node:util";
import { chromium } from "playwright-core";

const { values: opts } = parseArgs({
  options: {
    url: { type: "string", default: "https://127.0.0.1:18443" },
    users: { type: "string", default: join(homedir(), ".local/share/bmac-lab/bmaclab/users.json") },
    chrome: { type: "string", default: process.env.BMAC_CHROME ?? "/usr/bin/google-chrome" },
    out: { type: "string", default: join(tmpdir(), "bmac-webui-check") },
    host: { type: "string", default: "mox2" },
    recovery: { type: "string" },
    rotate: { type: "boolean", default: false },
    headed: { type: "boolean", default: false },
  },
});

function base32(text) {
  const alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
  let bits = "";
  for (const c of text.toUpperCase().replace(/=+$/, "")) bits += alphabet.indexOf(c).toString(2).padStart(5, "0");
  return Buffer.from(bits.match(/.{8}/g).map((b) => parseInt(b, 2)));
}

/** RFC 6238 with SHA-1, as Proxmox uses; waits for a window no earlier run used. */
async function freshTotp(secret) {
  const window = () => Math.floor(Date.now() / 30000);
  const start = window();
  while (window() === start) await new Promise((r) => setTimeout(r, 500));
  const counter = Buffer.alloc(8);
  counter.writeBigUInt64BE(BigInt(window()));
  const digest = createHmac("sha1", base32(secret)).update(counter).digest();
  const offset = digest[digest.length - 1] & 0x0f;
  return String((digest.readUInt32BE(offset) & 0x7fffffff) % 1e6).padStart(6, "0");
}

/** The SHA-256 fingerprint of the certificate served at `url`, as browsers show it. */
function servedFingerprint(url) {
  const { hostname, port } = new URL(url);
  return new Promise((resolve, reject) => {
    const socket = connect({ host: hostname, port: Number(port), rejectUnauthorized: false }, () => {
      const der = socket.getPeerCertificate().raw;
      socket.end();
      resolve(new X509Certificate(der).fingerprint256);
    });
    socket.on("error", reject);
  });
}

const FINGERPRINT = /(?:[0-9A-F]{2}:){31}[0-9A-F]{2}/;

const problems = [];
const step = (text) => console.log(`- ${text}`);

async function main() {
  const users = JSON.parse(await readFile(opts.users, "utf8"));
  await mkdir(opts.out, { recursive: true });
  const browser = await chromium.launch({ executablePath: opts.chrome, headless: !opts.headed });
  const context = await browser.newContext({ ignoreHTTPSErrors: true, viewport: { width: 1440, height: 960 } });
  const page = await context.newPage();
  // While the front end restarts after a certificate switch, the event
  // stream's dropped connection is expected.
  let restarting = false;
  page.on("console", (m) => {
    if (m.type() === "error" && !restarting) problems.push(`console error: ${m.text()}`);
  });
  page.on("pageerror", (e) => problems.push(`page error: ${e.message}`));
  await page.addInitScript(() => {
    document.addEventListener("securitypolicyviolation", (e) => {
      console.error(`CSP violation: ${e.violatedDirective} blocked ${e.blockedURI || "inline"} (${e.sourceFile}:${e.lineNumber})`);
    });
  });
  const shot = async (name) => {
    await page.waitForTimeout(400);
    await page.screenshot({ path: join(opts.out, `${name}.png`) });
  };

  const login = async (recovery) => {
    await page.getByLabel("Proxmox user").fill("alice@pve");
    await page.getByLabel("Password").fill(users.users["alice@pve"].password);
    await shot("01-login");
    await page.getByRole("button", { name: "Log in" }).click();
    if (recovery !== undefined) {
      step(`second factor: recovery key ${recovery}`);
      await page.getByRole("radio", { name: "Recovery key" }).click();
      await page.getByLabel("One of your recovery keys").fill(users.alice_recovery_keys[Number(recovery)]);
    } else {
      step("second factor: TOTP (waiting for a fresh code)");
      await page.getByLabel("Code from your authenticator app").fill(await freshTotp(users.alice_totp_secret));
    }
    await page.getByRole("button", { name: "Continue" }).click();
    await page.getByRole("button", { name: "Log out" }).waitFor({ timeout: 20000 });
  };

  async function rotate() {
    const before = await servedFingerprint(opts.url);
    step("rotate the certificate from Settings");
    await page.goto(`${opts.url}/settings`);
    await login();
    await page.getByText(before).last().waitFor({ timeout: 20000 });
    await page.getByRole("button", { name: "Make a new certificate" }).click();
    const fresh = page.getByRole("region", { name: "New certificate" });
    await fresh.waitFor({ timeout: 20000 });
    const shown = (await fresh.textContent()).match(FINGERPRINT)?.[0];
    if (!shown || shown === before) throw new Error(`the new certificate's fingerprint is ${shown}, the old one ${before}`);
    await shot("10-certificate-pending");
    await fresh.getByRole("button", { name: "Switch to the new certificate" }).click();
    restarting = true;
    await page.getByRole("dialog").getByRole("button", { name: "Switch", exact: true }).click();
    await page.getByRole("status", { name: "Certificate switched" }).waitFor({ timeout: 20000 });
    await shot("11-certificate-switched");

    step("the host serves the certificate that was shown");
    let served = before;
    for (const until = Date.now() + 30000; served !== shown && Date.now() < until; ) {
      await new Promise((r) => setTimeout(r, 1000));
      served = await servedFingerprint(opts.url).catch(() => served);
    }
    if (served !== shown) throw new Error(`the host serves ${served}, not the ${shown} shown before the switch`);

    step("reload, log in again, and see the new fingerprint");
    await page.reload();
    restarting = false;
    await login();
    await page.getByRole("button", { name: "Settings", exact: true }).click();
    await page.getByRole("button", { name: "Make a new certificate" }).waitFor({ timeout: 20000 });
    const published = page.getByRole("table", { name: "Host keys" }).getByRole("row").filter({ hasText: shown });
    for (const until = Date.now() + 20000; !(await published.count()) && Date.now() < until; ) {
      await page.waitForTimeout(2000);
      await page.getByRole("button", { name: "Reload" }).click();
    }
    if (!(await published.count())) problems.push("the new fingerprint isn't in the published host keys");
    if ((await page.getByText(shown, { exact: true }).count()) < 2) problems.push("the certificate card doesn't show the new fingerprint");
    await shot("12-certificate-after");
  }

  try {
    step(`open ${opts.url}/activity`);
    await page.goto(`${opts.url}/activity`);
    await login(opts.recovery);
    if (new URL(page.url()).pathname !== "/activity") problems.push(`after login the page is ${page.url()}, not /activity`);
    await shot("02-activity");

    const interrupted = page.getByRole("row").filter({ hasText: "Lab:" }).filter({ hasText: "Interrupted" });
    for (let n = await interrupted.count(); n > 0; n = await interrupted.count()) {
      step("mark an interrupted lab job failed (it holds its locks until then)");
      await interrupted.first().click();
      await page.getByRole("button", { name: "Mark failed" }).click();
      await page.getByRole("dialog").getByRole("button", { name: /I checked/ }).click();
      await page.waitForFunction((before) => document.querySelectorAll("tr").length && [...document.querySelectorAll("tr")].filter((r) => /Lab:.*Interrupted/s.test(r.textContent ?? "")).length < before, n, { timeout: 20000 });
    }

    step(`start lab.interactive on ${opts.host}`);
    await page.getByRole("button", { name: "Lab: interactive" }).first().click();
    const launch = page.getByRole("dialog");
    const dryRun = launch.getByRole("switch", { name: "Dry run" });
    if ((await dryRun.count()) && (await dryRun.getAttribute("aria-checked")) === "true") await dryRun.click();
    const host = launch.getByLabel("Host");
    if ((await host.evaluate((el) => el.tagName)) === "INPUT") await host.fill(opts.host);
    else {
      await host.click();
      await page.getByRole("option", { name: opts.host }).click();
    }
    await shot("03-launch");
    await launch.getByRole("button", { name: /^Start/ }).click();

    step("answer the input group, first with an odd core count");
    const panel = page.locator("aside").last();
    await page.getByLabel("vCPU cores").waitFor({ timeout: 30000 });
    await page.getByLabel("vCPU cores").fill("3");
    for (const name of ["mox1", "mox3"]) await page.getByRole("checkbox", { name }).click();
    await shot("04-input-group");
    await panel.getByRole("button", { name: /Continue|Submit/ }).click();
    await page.getByText("Use an even number of cores.").waitFor({ timeout: 30000 });
    await shot("05-validation-error");
    await page.getByLabel("vCPU cores").fill("4");
    await panel.getByRole("button", { name: /Continue|Submit/ }).click();

    step("type the confirmation phrase");
    await page.getByText("Create the VM?").waitFor({ timeout: 30000 });
    await page.getByPlaceholder("GO").or(page.getByRole("textbox", { name: /GO/ })).first().fill("GO");
    await shot("06-confirm");
    await panel.getByRole("button", { name: /Confirm|Continue/ }).click();

    step("acknowledge the manual action");
    await page.getByText("Unlock the disk").waitFor({ timeout: 30000 });
    await shot("07-manual-action");
    await panel.getByRole("button", { name: /Done|Acknowledge|Continue/ }).click();

    step("wait for the result");
    await page.getByText(/Completed successfully/).waitFor({ timeout: 60000 });
    await page.getByRole("button", { name: "Show output" }).click();
    await shot("08-succeeded");

    // Every v2 page's equivalent (M3 done-when); Backups, Alerts, and Logs
    // come later and are not checked.
    const pages = ["Overview", "Hosts", "Storage", "Production VMs", "Staging VMs", "QDevice", "Activity", "Diagnostics", "Settings"];
    for (const [i, name] of pages.entries()) {
      step(`page ${name}`);
      await page.getByRole("button", { name, exact: true }).click();
      await page.waitForFunction(() => !document.querySelector("[aria-busy='true']"), null, { timeout: 20000 }).catch(() => {});
      await page.waitForTimeout(1500);
      const broken = page.getByText(/isn't built yet|isn't available|aren't available/);
      for (const text of await broken.allTextContents()) problems.push(`${name}: ${text}`);
      await shot(`09-page-${String(i).padStart(2, "0")}-${name.toLowerCase().replace(/ /g, "-")}`);
    }

    const cookies = await context.cookies();
    if (cookies.length) problems.push(`cookies set: ${cookies.map((c) => c.name).join(", ")}`);
    const storage = await page.evaluate(async () => ({
      local: localStorage.length,
      session: sessionStorage.length,
      databases: (await indexedDB.databases()).length,
    }));
    if (storage.local || storage.session || storage.databases) problems.push(`browser storage used: ${JSON.stringify(storage)}`);

    step("reload returns to the login page");
    await page.reload();
    await page.getByLabel("Proxmox user").waitFor({ timeout: 10000 });
    await page.goto(`${opts.url}/activity`);
    await page.getByLabel("Proxmox user").waitFor({ timeout: 10000 });

    if (opts.rotate) await rotate();
  } catch (error) {
    await page.screenshot({ path: join(opts.out, "failure.png") }).catch(() => {});
    problems.push(`step failed: ${error.message.split("\n")[0]}`);
  } finally {
    await browser.close();
  }

  console.log(`screenshots in ${opts.out}`);
  if (problems.length) {
    console.log("PROBLEMS:");
    for (const p of problems) console.log(`  ${p}`);
    process.exit(1);
  }
  console.log("OK");
}

await main();
