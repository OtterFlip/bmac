<!--
Copyright (c) 2026 BEENTHERE VENTURES, INC.
SPDX-License-Identifier: GPL-3.0-only
-->

# Agent instructions

## Running tests

Run the unit tests with `dev/run_tests.py` from the repository root, not
with `python3 -m unittest`. It runs every test in its own process, one worker
per CPU, and finishes the full suite in seconds instead of minutes.

```bash
dev/run_tests.py                                   # the whole suite
dev/run_tests.py hosts/test_add_proxmox_host.py    # specific files
dev/run_tests.py lib.test_rpool_mirror.RpoolMirrorToolTest.test_add_uses_shared_crypttab_form_and_verifies_boot_unlock
dev/run_tests.py -k reservation                    # filter by test name
```

- Run the whole suite before reporting work as finished; it is fast enough
  that there is no reason to run only a subset at the end.
- `-v` lists every test with its duration and the ten slowest; `-f` starts
  no new tests after the first failure; `-j N` sets the worker count.
- Also run `bash -n` on any shell script you changed.
- Tests must stay independent of order and of each other: build fixtures
  under a per-test temporary directory, never write to the checkout or a
  fixed path, and do not add `setUpClass` or `setUpModule`. A fake command
  that is fed by a pipe must read its stdin, or it causes intermittent
  `SIGPIPE` failures under `set -o pipefail` on a busy machine.
- If a test fails only under parallel runs, fix the test's race rather than
  serializing it.

See the Tests section of [`MAIN_DESIGN.md`](MAIN_DESIGN.md#tests) for
details, and [`DEVELOPMENT.md`](DEVELOPMENT.md) for running the complete
suite on a Mac.
