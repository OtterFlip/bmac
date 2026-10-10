<!--
Copyright (c) 2026 BEENTHERE VENTURES, INC.
SPDX-License-Identifier: GPL-3.0-only
-->

# BMAC scripts: the engine

This directory is the engine of BMAC. The scripts here do all of the real
work: building and joining Proxmox hosts, managing disks and ZFS mirrors,
creating and moving production and staging guests, setting up the QDevice,
routing ingress through HAProxy, and reporting on the state of the cluster.
Everything else in the repository, including the Dashboard, is a way of
driving these scripts.

## Two ways to run a script

**Human mode.** Most scripts are meant to be run directly from a shell on an
administrator workstation, from the repository root:

```bash
./scripts/user_callable/diagnostics/show_cluster_health.sh
./scripts/user_callable/hosts/add_proxmox_host.sh --host mox2
```

In this mode a script draws its own terminal UI. It prompts for anything it
needs, shows a plan before doing anything destructive, asks for confirmation,
and prints progress and suggested next steps.

**Machine mode.** Pass `--json` and the same script speaks the bmac-ui v2
NDJSON protocol
([`dashboard/protocol/bmac-ui-v2.schema.json`](../dashboard/protocol/bmac-ui-v2.schema.json))
on stdin and stdout instead of drawing a terminal UI. Prompts become typed
input requests, and plans, confirmations, progress, results, and next steps
become structured events. The script's behavior and safety checks don't
change, only how it talks to its caller.

In machine mode the script library works as an API: any program that can
start a process and exchange JSON lines with it can use BMAC's functionality
without reimplementing it. The [BMAC Dashboard](../dashboard/README.md) works
this way. Its workflows are these scripts, run with `--json`, with their
requests and events rendered as forms, dialogs, and progress views.

## Layout

- `user_callable/` holds the scripts an operator (or the Dashboard) runs,
  grouped by area: `diagnostics/`, `hosts/`, `guests/`, and `qdevice/`.
- `lib/` holds the shared libraries those scripts source or import, including
  the configuration loader and the `--json` protocol support. See
  [`lib/README.md`](lib/README.md).
- `utilities/` holds helpers that only other scripts run, either on the
  workstation or piped or copied to a host for a single run.
- `host_runtime/` holds the tools installed on every Proxmox host, such as the
  cluster registry, the HAProxy route synchronizer, and the deferred-cleanup
  worker.
- `tests/` holds the unit tests. Run them with `dev/run_tests.py` from the
  repository root.

For the full design and the list of every script, see
[`docs/MAIN_DESIGN.md`](../docs/MAIN_DESIGN.md) and the
[top-level README](../README.md).
