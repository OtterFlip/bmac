#!/usr/bin/env node
// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// Checks that the local toolchain matches what this project pins, and says
// how to fix any drift. Silent when everything matches.
//
//   node scripts/check-toolchain.mjs [--app] [--verbose]
//
// --app also checks the Rust toolchain and the native WebKitGTK libraries
// that `tauri dev` and `tauri build` need. Problems that would break the
// build exit 1; drift that only risks subtle issues is printed as a warning.
// Set BMAC_SKIP_TOOLCHAIN_CHECK=1 to skip the check entirely.

import { execFileSync } from "node:child_process";
import { existsSync, readFileSync, statSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const args = new Set(process.argv.slice(2));
const app = args.has("--app");
const verbose = args.has("--verbose");
const errors = [];
const warnings = [];
const notes = [];

if (process.env.BMAC_SKIP_TOOLCHAIN_CHECK === "1") process.exit(0);

/** @param {string} path */
const read = (path) => readFileSync(join(root, path), "utf8");
/** @param {unknown} version */
const major = (version) => Number.parseInt(String(version).replace(/^[^\d]*/, ""), 10);

/**
 * @param {string} command
 * @param {string[]} argv
 */
function run(command, argv) {
  try {
    return execFileSync(command, argv, { encoding: "utf8", stdio: ["ignore", "pipe", "ignore"] }).trim();
  } catch {
    return null;
  }
}

/**
 * @param {string} a
 * @param {string} b
 */
function compareVersions(a, b) {
  const left = a.split(".").map(Number);
  const right = b.split(".").map(Number);
  for (let index = 0; index < Math.max(left.length, right.length); index += 1) {
    const diff = (left[index] ?? 0) - (right[index] ?? 0);
    if (diff) return diff;
  }
  return 0;
}

// --- Node -------------------------------------------------------------------

const pkg = JSON.parse(read("package.json"));
const pinnedNode = major(read(".nvmrc").trim());
const nodeMajor = major(process.versions.node);
const pinFiles = ".nvmrc, package.json engines.node, and @types/node";

if (nodeMajor < pinnedNode) {
  errors.push(
    `Node ${process.versions.node} is older than the pinned Node ${pinnedNode} (.nvmrc).\n` +
      `  Fix: cd dashboard && nvm install && nvm use`,
  );
} else if (nodeMajor > pinnedNode) {
  warnings.push(
    `Node ${process.versions.node} is newer than the pinned Node ${pinnedNode} (.nvmrc).\n` +
      `  If this is the new LTS, move the project to it by updating ${pinFiles} together:\n` +
      `    echo ${nodeMajor} > .nvmrc, set engines.node to "^${nodeMajor}.0.0", then\n` +
      `    pnpm add -D @types/node@${nodeMajor}\n` +
      `  Otherwise switch back with: nvm use`,
  );
} else {
  notes.push(`Node ${process.versions.node} matches .nvmrc (${pinnedNode}).`);
}

const enginesNode = pkg.engines?.node ?? "";
if (major(enginesNode) !== pinnedNode) {
  warnings.push(
    `package.json engines.node is "${enginesNode}" but .nvmrc pins Node ${pinnedNode}.\n` +
      `  Fix: keep ${pinFiles} on the same major version.`,
  );
}

const typesPath = join(root, "node_modules/@types/node/package.json");
if (existsSync(typesPath)) {
  const typesVersion = JSON.parse(readFileSync(typesPath, "utf8")).version;
  if (major(typesVersion) !== pinnedNode) {
    warnings.push(
      `@types/node ${typesVersion} describes Node ${major(typesVersion)}, but the project runs on Node ${pinnedNode}.\n` +
        `  TypeScript would accept Node APIs that do not exist at runtime (or reject ones that do).\n` +
        `  Fix: pnpm add -D @types/node@${pinnedNode}`,
    );
  } else {
    notes.push(`@types/node ${typesVersion} matches Node ${pinnedNode}.`);
  }
}

// --- pnpm and installed packages --------------------------------------------

const pinnedPnpm = String(pkg.packageManager ?? "").replace(/^pnpm@/, "");
const agent = process.env.npm_config_user_agent ?? "";
const runningPnpm = /\bpnpm\/(\S+)/.exec(agent)?.[1];
if (runningPnpm && pinnedPnpm && runningPnpm !== pinnedPnpm) {
  warnings.push(
    `pnpm ${runningPnpm} is running, but package.json packageManager pins pnpm ${pinnedPnpm}.\n` +
      `  Fix: npm install -g pnpm@${pinnedPnpm}, or update packageManager after upgrading on purpose.`,
  );
} else if (runningPnpm) {
  notes.push(`pnpm ${runningPnpm} matches packageManager.`);
}

const installedLock = join(root, "node_modules/.pnpm/lock.yaml");
if (!existsSync(installedLock)) {
  errors.push("Dependencies are not installed.\n  Fix: cd dashboard && pnpm install");
} else if (statSync(join(root, "pnpm-lock.yaml")).mtimeMs > statSync(installedLock).mtimeMs + 1000) {
  warnings.push(
    "pnpm-lock.yaml changed after the last install, so node_modules may be stale.\n  Fix: pnpm install",
  );
}

// --- Rust and native libraries (desktop app only) ---------------------------

if (app) {
  const required = /^\s*rust-version\s*=\s*"([^"]+)"/m.exec(read("Cargo.toml"))?.[1];
  const rustc = run("rustc", ["--version"]);
  const rustVersion = rustc && /rustc (\d+\.\d+(?:\.\d+)?)/.exec(rustc)?.[1];
  if (!rustVersion) {
    errors.push("rustc was not found.\n  Fix: install Rust from https://rustup.rs (or your distribution's rustc and cargo).");
  } else if (required && compareVersions(rustVersion, required) < 0) {
    errors.push(
      `rustc ${rustVersion} is older than the rust-version ${required} in Cargo.toml.\n` +
        `  Fix: rustup update stable (or upgrade your distribution's rustc).`,
    );
  } else {
    notes.push(`rustc ${rustVersion} satisfies rust-version ${required ?? "(unset)"}.`);
  }

  if (process.platform === "linux") {
    const libraries = {
      "webkit2gtk-4.1": "libwebkit2gtk-4.1-dev",
      "javascriptcoregtk-4.1": "libjavascriptcoregtk-4.1-dev",
      "libsoup-3.0": "libsoup-3.0-dev",
      "gtk+-3.0": "libgtk-3-dev",
      "librsvg-2.0": "librsvg2-dev",
    };
    if (run("pkg-config", ["--version"]) === null) {
      errors.push("pkg-config was not found; Tauri needs it to locate WebKitGTK.\n  Fix: sudo apt install pkg-config");
    } else {
      const missing = Object.entries(libraries).filter(
        ([module]) => run("pkg-config", ["--exists", module]) === null,
      );
      if (missing.length) {
        errors.push(
          `Missing native development libraries: ${missing.map(([module]) => module).join(", ")}.\n` +
            `  Fix: sudo apt install ${missing.map(([, apt]) => apt).join(" ")}`,
        );
      } else {
        notes.push("WebKitGTK 4.1, GTK 3, libsoup 3, and librsvg development libraries are installed.");
      }
    }
  }
}

// --- Report -----------------------------------------------------------------

if (verbose) for (const note of notes) console.log(`toolchain ok: ${note}`);
for (const warning of warnings) console.warn(`toolchain warning: ${warning}\n`);
for (const error of errors) console.error(`toolchain error: ${error}\n`);
if (errors.length) {
  console.error("Fix the errors above, or set BMAC_SKIP_TOOLCHAIN_CHECK=1 to build anyway.");
  process.exit(1);
}
if (verbose && !warnings.length) console.log("toolchain ok: everything matches the pinned versions.");
