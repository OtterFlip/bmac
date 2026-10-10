<!--
Copyright (c) 2026 BEENTHERE VENTURES, INC.
SPDX-License-Identifier: GPL-3.0-only
-->

# BMAC Dashboard

A desktop control panel for a BMAC cluster. It runs the same operator scripts
you run from a terminal, in their `--json` mode, and turns their prompts,
plans, progress, results, and suggested next steps into native-looking forms
and dialogs. The scripts remain the authority: the dashboard never talks to
Proxmox, SSH, or the registry itself.

The wire protocol, which is the call contract between the Dashboard and the
scripts that it calls, is described by
[`protocol/bmac-ui-v2.schema.json`](protocol/bmac-ui-v2.schema.json).

## Screenshots

[Here's](../docs/DASHBOARD_SCREENSHOTS.md) what the BMAC Dashboard looks like.

## What it does

- **Overview pages** for hosts, production VMs, staging VMs, storage, and the
  QDevice. Each list is filled by the fast `scripts/user_callable/diagnostics/list_*.sh` scripts
  and has a refresh button; Settings can also refresh them on a timer.
- **Operations** for every workflow in
  [`engine/workflows.json`](engine/workflows.json), launched from a dialog that
  shows the exact command line before it runs.
- **Requests** from a running script (inputs, forms, confirmations, and
  manual actions such as typing a LUKS passphrase at a host console) appear
  as in-app dialogs. Typed-phrase confirmations stay typed-phrase
  confirmations. There are no OS dialogs; file fields use an in-app browser.
- **The activity bar** at the bottom always shows what is running: the
  script, its arguments, the current step, and a one-click expansion into the
  live output. Every run keeps its full output, plan, result, and next steps
  in History, and each run's terminal-equivalent command can be copied.
- **Next steps** a script suggests are listed with the result. Those that name
  a workflow can be launched directly with the suggested values.
- **Config** (above Settings in the menu) lists `cluster.conf`, every
  `moxN.conf`, `secrets.env`, and the read-only `*_dot_*` examples, with an
  editor (undo/redo, find, compare with the example, save). For an installed
  package it can also move the config directory, optionally taking the files
  along. Settings warns about each essential file that is missing or still
  identical to its example.

## Safety model

- The frontend can only ask the native side to start an allowlisted workflow,
  answer the request a script is waiting on, or cancel. There is no shell,
  SSH, or general file API, and arguments are always passed as argv.
- Mutating workflows run one at a time. Read-only ones may run alongside.
- Cancel sends SIGINT to the script's process group, and is refused during
  phases a script marks as not cancellable. A script that exits without a
  `completed` event is reported as failed.
- Secret field values (passwords, Tailscale auth keys) go only to the
  script's stdin. They are redacted from history and logs, and never used as
  defaults. The dashboard compares `secrets.env` with its example in process
  to warn when it is unedited, but never displays it or sends its contents to
  the frontend; it can only open it in your own editor or show its folder.
- Config file reads and writes are limited to the known file names in the
  config directory. Writes are atomic, create files at mode `0600`, and are
  refused if the file changed on disk since it was loaded.
- LUKS passphrases are never entered in the dashboard; the scripts ask you to
  type them at the host console.
- Workflows the scripts support only on Linux (adding or removing a host or
  QDevice) are shown but disabled elsewhere, with the reason. Adding a host
  also requires x86-64 Debian or Ubuntu.

## Running it

From this directory:

```bash
nvm install && nvm use   # the Node major in .nvmrc
npm install -g pnpm@12   # once per nvm Node install; packageManager in package.json pins the exact version
pnpm install
pnpm dev             # browser preview at http://127.0.0.1:1420 with a mock backend
pnpm app:dev         # the desktop app against a real repository checkout
pnpm app:build       # release .deb under target/release/bundle/deb/
pnpm app:build:rpm       # opt-in: .rpm
pnpm app:build:appimage  # opt-in: AppImage
```

The build stages `scripts/` (without tests, caches, or artifacts), the
`config/*_dot_*` examples, and a `bmac-installed` marker into
`src-tauri/bundle-resources/`, and the package installs them under
`/usr/lib/BMAC_Dashboard/`. The `.deb` depends on the tools the scripts use
(`bash`, `python3`, `openssh-client`, `openssl`, `curl`, `jq`, ...) and
recommends `git` and `xorriso`.

The browser preview runs entirely on fixtures in `src/lib/mock/`, so you can
try every screen, including destructive confirmations, without a cluster.

The desktop app needs a stable Rust toolchain and, on Debian or Ubuntu:

```bash
sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev libsoup-3.0-dev \
  libjavascriptcoregtk-4.1-dev librsvg2-dev libayatana-appindicator3-dev
```

### Toolchain versions

`pnpm dev`, `pnpm build`, `pnpm app:dev`, and `pnpm app:build` first run
`scripts/check-toolchain.mjs`. It is silent when everything matches, warns
about drift, and stops the build only for problems that would break it,
printing the exact fix in each case. `pnpm check:toolchain` shows the full
report. It checks:

- Node against `.nvmrc`, and `.nvmrc`, `engines.node`, and `@types/node` against
  each other. Move all three together when you adopt a new Node LTS, so the
  type definitions describe the Node that actually runs the tooling.
- pnpm against `packageManager`, and whether `node_modules` is older than
  `pnpm-lock.yaml`.
- For the desktop app: `rustc` against `rust-version` in `Cargo.toml` (the
  highest minimum among the locked crates; raise it if `cargo update` pulls in
  a crate that needs a newer compiler), and the WebKitGTK development libraries.

Set `BMAC_SKIP_TOOLCHAIN_CHECK=1` to bypass it. `@types/node` deliberately
stays on the `.nvmrc` major, so `pnpm outdated` will keep listing a newer one.

TypeScript is split in two: `tsconfig.app.json` covers the app, which runs in
the Tauri WebKit window and gets no Node types, so `process` or `require` there
is a type error. `tsconfig.node.json` covers `vite.config.ts`, `scripts/`, and
the tests, which run under Node.

Settings live in the platform config directory
(`~/.config/com.btvcorp.bmac.dashboard/` on Linux) and run history in its data
directory (`~/.local/share/com.btvcorp.bmac.dashboard/runs/`).

The app works in one of two modes:

- **Dev**: the executable is inside a BMAC checkout (`pnpm app:dev`, or a
  binary under `target/`), or no bundled scripts are present. It uses the
  checkout's `scripts/` and `config/`; on first start without a checkout, choose
  one in Settings. The config directory is fixed to `<checkout>/config`.
- **Installed**: the bundled scripts under `/usr/lib/BMAC_Dashboard/` are used,
  and the config directory defaults to
  `~/.config/com.btvcorp.bmac.dashboard/config/`. Moving it from the Config
  page records the new path in `~/.config/com.btvcorp.bmac.dashboard/config-location`,
  which the scripts read too. A package cannot write into home directories, so
  the first start of each version copies any missing `cluster.conf`,
  `mox1.conf`, `mox2.conf`, and `secrets.env` from the bundled examples; it
  never overwrites existing files.

The app opens on Settings on first launch and whenever a config file still
needs attention.

On Linux, a build that was not installed from a package (`pnpm app:dev` or a
bare `target/release` binary) writes a hidden
`~/.local/share/applications/bmac-dashboard.desktop` and
`~/.local/share/icons/hicolor/256x256/apps/bmac-dashboard.png` at startup,
so GNOME and other desktops show the BMAC icon in Alt+Tab and the dock instead
of a generic one. Delete both files to undo it.

## Layout

| Path | Contents |
| --- | --- |
| `engine/` | Rust: protocol parsing and validation, the run supervisor, history, the workflow registry, preflight checks |
| `engine/workflows.json` | The workflow allowlist: scripts, parameters, and how they map to flags. Compiled into the app |
| `engine/src/config.rs` | Config directory location, file listing and state, guarded read/write, seeding, and relocation |
| `src-tauri/` | The Tauri shell that exposes the engine to the frontend as a narrow command set |
| `src-tauri/build.rs` | Stages the bundled scripts and examples into `src-tauri/bundle-resources/` (ignored) |
| `src/` | React frontend |
| `mock-scripts/` | Small protocol scripts used by the engine tests |

## Tests

```bash
pnpm test                                 # frontend (vitest)
pnpm typecheck
cargo test --manifest-path engine/Cargo.toml
dev/run_tests.py dashboard/test_workflow_registry.py      # from the repository root
```

## Adding a workflow

1. Make the script support `--json`: source `scripts/lib/ui_protocol.sh`, call
   `bmac_ui_bootstrap "$@"` before `main`, and give each prompt a JSON branch
   (`if bmac_ui_is_json; then ...; else <unchanged read>; fi`). Terminal output
   must not change. See [`scripts/lib/README.md`](../scripts/lib/README.md#ui_protocolsh).
2. Emit `bmac_ui_plan_*` before changing anything, `bmac_ui_result` at the
   end, and `bmac_ui_next_step` for what the operator should do afterward.
3. Add the workflow to `engine/workflows.json`, with its id equal to the
   script's basename.
4. Add a JSON-mode test with `scripts/tests/ui_test_driver.py`.
