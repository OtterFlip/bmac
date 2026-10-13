<!--
Copyright (c) 2026 BEENTHERE VENTURES, INC.
SPDX-License-Identifier: GPL-3.0-only
-->

# Agent instructions for v3

Everything for BMAC v3 lives under `v3/`. Nothing outside it changes: v2's
files are read as reference and copied into `v3/` when reused, never moved or
edited in place. The design is in [`design/`](design/), and the order of work
is [`design/IMPLEMENTATION_PLAN.md`](design/IMPLEMENTATION_PLAN.md).

v3 is not compatible with v2, and there is no upgrade path from a v2
deployment. Don't keep v2's names, paths, locks, or record formats for
compatibility's sake, and don't add migration or mixed v2/v3 support.

## Platform

v3 development is amd64 Linux only (with KVM for the lab). Don't add
macOS or other platform workarounds to the code, the lab, or the dev
scripts.

## Checks

Run every check with `v3/dev/check`, unsandboxed (the Python tests need pty
devices and real timing). It runs, stopping at the first failure:

- the toolchain check (`node dev/check-toolchain.mjs`, `--deployer` for the
  Deployer's native libraries);
- the version check (`dev/set_version.py`);
- `bash -n` on every shell script, in parallel;

then these three groups at the same time, each stopping at its first
failure, with each group's output shown once all three finish:

- the Python tests (`dev/run_tests.py`, which takes the same options as v2's
  runner: `-k`, `-f`, `-j N`, `-v`, and test targets);
- `cargo fmt --check`, `cargo clippy -D warnings`, and the Rust tests:
  `cargo nextest run` (every test in its own process, across all test
  binaries at once) and `cargo test --doc`, or plain `cargo test` where
  cargo-nextest isn't installed (`cargo install cargo-nextest --locked`);
- the TypeScript lint, `pnpm typecheck`, and `pnpm test`.

Tests in every suite run in parallel, when possible, so each must stay independent of the
others (see Tests). Run the whole thing before reporting work as finished.
Run `pnpm install` in `v3/` once first.

`v3/dev/check` is the only suite v3 work needs. Don't run v2's suite (the
repository root's `dev/run_tests.py`) for changes under `v3/`: it collects
only committed test files, so until v3 is committed it tests v2's unchanged
scripts, and once v3 is committed it runs the same v3 Python tests that
`v3/dev/check` already ran. Run it only if a change touches something
outside `v3/`, which v3 work shouldn't.

## Tests

The v2 rules apply unchanged: tests stay independent of order and of each
other, build fixtures under a per-test temporary directory, never write to
the checkout or a fixed path, and don't use `setUpClass` or `setUpModule`. A
fake command fed by a pipe must read its stdin. If a test fails only under
parallel runs, fix the race rather than serializing it.

For all test suites, if it's possible to run tests in parallel then make
an effort to ensure that those tests are run in parallel.

For tests suites that include tests with multiple boot/reboot drills,
perform all other tests in the suite first, ensuring they pass first,
before adding/enabling the reboot/boot-test drills to ensure that they pass.
The principle here to follow is to get all of the faster/shorter tests
to pass first before getting the slower/longer tests to pass, because
when we run an entire test suite repeatedly, if we don't optimize how
we run the tests within this suite, then the repeated runs of the suite
itself, before everything passes in the suite, are less optimal when it
comes to the time consumed to run the suite.

When a lab scenario builds on a long stage that has already passed (a host
installed and set up, a cluster formed), save the VMs at that stage and let
later runs start from it, so work on what comes after doesn't repeat the
stage each time. Watch for these: whenever a scenario spends many minutes
reaching a state before the part being worked on, save that state as a
stage. `m6.staged(s, STAGE, build, then)` does this: it installs the
host, runs `build` to reach the stage, saves it, and runs `then`; later
runs restore the stage, install the current bmacd package on it, and run
only `then`. A stage keeps the host's tailnet device, installing that VM
anew drops its stages, and `BMAC_LAB_FRESH=1` builds the stage again.
Because a VM's stages share its tailnet name, they form a chain: build the
next stage on the last one (`m7_join` starts from `m7_storage`'s
`m7-first-host`), and never repeat a long scenario that a stage already
covers.

There are three labs, and they run at the same time (each takes up to
29 GiB of RAM):

- the NVMe lab (`lab/lab.toml`), whose hosts have NVMe boot disks like the
  real servers. QEMU can't save a running NVMe controller, so a stage is
  saved with the host shut down and boots when restored (an encrypted host
  then waits for its LUKS passphrase);
- the SCSI lab (`lab/lab --config lab/lab-scsi.toml`) and the SATA lab
  (`lab/lab --config lab/lab-sata.toml`), the same topology with the boot
  disks attached as SCSI or SATA. Their stages are saved live and resume in
  seconds, running, with the guest's clock set to now.

Develop a stage that hasn't passed yet on the SATA lab, from the live
stage of the one it builds on: each failed try restores that stage and goes
again. For the final runs, run the scenario on all three labs concurrently,
so the change is proved from each lab's stage, NVMe and SCSI included. When a change touches what
a stage itself covers, run with `BMAC_LAB_FRESH=1` on every lab, which
proves the stage from scratch and saves it again. Save the hosts of a
cluster together, one after another while nothing is changing, so they
agree with each other when restored. The stages saved so far are
listed in [`lab/README.md`](lab/README.md#stages).

v2's `dev/run_tests.py` also collects every committed `test_*.py` under
`v3/`, and runs it from the repository root. So a v3 Python test must find
what it tests by paths relative to its own file (for example, loading a
module with `importlib` from `Path(__file__)`), never by importing from the
repository root, where `dev` and `scripts` are v2's.

## Tailscale names

A v2 test cluster is deployed on the same tailnet as `qdevice`, `mox1`,
`mox2`, and `mox3`. Nothing in v3 development may register a tailnet node
under those names. Lab VMs use `Topology.tailnet_name()` (`bmaclab-mox1`,
and so on; see `lab/README.md`), and any other test that joins Tailscale
uses a name with a `bmac-` prefix or similar, ephemeral keys where
possible, and never a bare host name.

## Versions

`v3/dev/VERSION` is the single source of truth. Change it with
`v3/dev/set_version.py X.Y.Z`, which rewrites every Cargo and npm copy.
New crates use `version.workspace = true`; new pnpm packages are listed in
`pnpm-workspace.yaml` so the tool finds them.

## License headers

Every source file starts with the copyright line and
`SPDX-License-Identifier: GPL-3.0-only`, in the file's comment syntax.
`dev/test_license_headers.py` enforces it; JSON and generated lock files are
exempt.
