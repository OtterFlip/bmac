<!--
Copyright (c) 2026 BEENTHERE VENTURES, INC.
SPDX-License-Identifier: GPL-3.0-only
-->

# Developing BMAC on a Mac

BMAC's scripts run in two very different places: an administrator
workstation (Linux or macOS) and the Proxmox hosts themselves (Debian 13,
amd64). The unit suite covers both, so part of it only runs on Linux. This
guide sets up a Mac for day-to-day work on BMAC itself: native tools for the
workstation scripts and a quick test loop, plus a small Lima VM that runs the
complete suite. It ends with what this setup still cannot do.

Operating a cluster from a Mac (SSH agent, host-key pinning, which operator
scripts are verified on macOS) is covered in
[`MAIN_DESIGN.md`](MAIN_DESIGN.md#administrator-workstation-linux-or-macos).

This setup was verified on 2026-09-24 on an Apple Silicon Mac running macOS
26 with Homebrew and Lima 2.2.

## What runs where

| Work | Native macOS | Lima VM | Otherwise |
|---|---|---|---|
| Editing, `bash -n` | yes | yes | |
| Unit suite | all but one test; some suites skip | complete, nothing skipped | |
| `guests/`, `app/`, `diagnostics/` against a cluster | yes | not configured | |
| `hosts/setup_proxmox_host.sh` | no | no | amd64 Linux |
| Real Proxmox, ZFS, HA, LUKS, or network behavior | no | no | the cluster |

## 1. Native macOS setup

```bash
brew install bash openssl@3 flock lima
```

- **Bash 4.4 or newer.** macOS ships Bash 3.2 as `/bin/bash`, and the scripts
  use newer features such as `mapfile -d`. Keep Homebrew's `bin`
  (`/opt/homebrew/bin` on Apple Silicon) ahead of `/bin` on `PATH`.
- **OpenSSL 3.** Apple's `/usr/bin/openssl` is LibreSSL, which lacks
  `openssl passwd -6`; the creators use it to hash console passwords.
- **`flock`.** macOS has none, and the lifecycle-hook tests call it. The
  Homebrew build supports every form the scripts use (`-w`, `-n`, `-u`,
  `-x`).
- **Lima** runs the test VM in step 2.
- **Python 3.9 or newer.** The Command Line Tools' `python3` qualifies.

Check the result:

```bash
bash --version | head -1   # 4.4 or newer
openssl version            # OpenSSL 3.x, not LibreSSL
python3 --version          # 3.9 or newer
```

Do not install HAProxy natively. `lib/test_haproxy_routes.py` runs
`haproxy -c` only when a `haproxy` binary is on `PATH`, and the rendered
configuration names a `haproxy` user that macOS does not have, so installing
it turns a skipped test into a failing one. The VM runs that check properly.

The tests also read two files from your workstation:

- `env/mox1.conf`. `lib/test_shared_libs.py` and
  `hosts/test_setup_proxmox_host.py` take example host values from it, and
  both fail on a fresh clone without it. If you have not already created it
  as part of cluster setup, copy the template:

  ```bash
  cp env/mox1_dot_conf env/mox1.conf
  ```

  If it already holds your real host's values, that is fine: the tests only
  copy them into temporary fixtures and never contact the host.
- `~/.ssh/known_hosts`. The production and staging dry-run tests require it
  to exist but never read it. Anyone who has used SSH has one; otherwise
  `install -d -m 700 ~/.ssh && install -m 600 /dev/null ~/.ssh/known_hosts`.

For a quick loop while editing, run one module natively:

```bash
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest lib/test_shared_libs.py
```

A full native run of all 241 tests reports two failures (lifecycle-hook
tests that need GNU `stat -c`) and 27 skips (Proxmox-host-only suites such as
the deferred-cleanup worker and `lib/rpool_mirror.sh`). Those are the gaps the
VM closes.

## 2. The Lima VM: the complete suite

```bash
dev/run-tests-in-vm.sh
```

On first use the runner creates a Lima instance named `bmac` from
[`dev/lima-bmac.yaml`](dev/lima-bmac.yaml). It downloads a Debian 13 cloud
image and installs the packages, which takes about four minutes. The VM uses
Apple's Virtualization framework (arm64 on Apple Silicon), 4 vCPUs, 8 GiB of
RAM, and a sparse 30 GiB disk. Later runs take under a minute.

Each run starts the VM if it is stopped, runs `bash -n` on every tracked
`*.sh`, then runs every tracked `test_*.py`. Once `env/mox1.conf` exists (see
above), the expected result is every test passing with nothing skipped. Extra
arguments go to `unittest`:

```bash
dev/run-tests-in-vm.sh -v -k reservation
dev/run-tests-in-vm.sh lib.test_haproxy_routes
```

Why it is built this way:

- **Debian 13** is the userland of Proxmox VE 9 and of the fixed HAProxy LXC,
  so GNU tool behavior and the HAProxy version match what the hosts run.
- **Only this checkout is mounted**, writable, at the same absolute path as
  on macOS. The config loader rejects symlinked path components, so the path
  must not change, and the checkout must not sit under a symlinked macOS
  directory such as `/tmp` or `/var`. The rest of your home directory is not
  visible to the VM.
- **Your UID carries into the VM**, so Git works on the mounted checkout and
  files created there belong to you on macOS.
- **`TMPDIR=/var/tmp`.** Debian 13 mounts `/tmp` as tmpfs, and
  `prepare_prod_iso.sh` refuses a tmpfs cache. If you run `unittest` by hand
  inside the VM, set it yourself, or the ISO-preparer test fails with
  `ISO cache must be disk-backed, not tmpfs`.
- **An empty `~/.ssh/known_hosts`** is created at provisioning for the
  dry-run tests.

Managing the VM:

```bash
limactl shell --workdir "$PWD" bmac   # interactive shell in the checkout
limactl stop bmac                     # free its RAM when not testing
limactl delete --force bmac           # the next run recreates it
```

Lima copies the template when it creates an instance. After editing
`dev/lima-bmac.yaml`, or after moving the checkout, delete the instance and
run the runner again; it detects an instance that does not mount this
checkout and says so. If you already use a Lima instance called `bmac` for
something else, set `BMAC_LIMA_INSTANCE` to another name.

## Known shortcomings

### Testing

- **Everything is mocked.** The suites stub Proxmox, ZFS, QEMU, HA, iDRAC,
  QDevice, and the network. A green run in the VM proves nothing about real
  media, Layer-2 behavior, LUKS boot, quorum, replication, failover, or
  Cloudflare; those still need the drills in
  [`MAIN_DESIGN.md`](MAIN_DESIGN.md#required-failure-drills).
- **The VM is arm64; the hosts are amd64.** The suites are Bash and Python
  with mocked commands, so this does not affect them, but nothing here
  executes an amd64 binary.
- **Native macOS is partial by design.** The lifecycle hook needs GNU
  `stat`, and `test_process_deferred_cleanup.py`, `test_rpool_mirror.py`,
  and the production ISO-preparer tests skip on non-Linux platforms. Treat the native run as a
  smoke test and use the VM before pushing.
- **The tests depend on workstation state.** They need `env/mox1.conf` and
  `~/.ssh/known_hosts` as described above. Both are test bugs: the tests
  should use `env/mox1_dot_conf` and a temporary known-hosts file. Until they
  are fixed, a fresh clone reports 25 errors (`FileNotFoundError` for
  `env/mox1.conf`) in the VM as well as natively, until the file exists.

### Operator workflows

- **`hosts/setup_proxmox_host.sh` cannot run here.** It needs Linux with
  `flock` and `ip`, and its install-ISO phase executes Proxmox's amd64-only
  `proxmox-auto-install-assistant`. Untested options are Rosetta in an arm64
  Lima VM (`vmOpts.vz.rosetta`), an emulated x86_64 Lima VM (`vmType: qemu`,
  `arch: x86_64`; slow, but workable for a command-line tool), or an amd64
  Linux machine. Whichever is used must also reach Tailscale and the
  provider's IPMI VPN and hold `env/secrets.env` and `hosts/artifacts/`
  under the loader's ownership and mode checks. This VM sets up none of that.
- **The VM has no cluster access.** It has no Tailscale, SSH agent, or
  pinned host keys. Run the `guests/`, `app/`, and `diagnostics/` scripts from
  macOS itself.
- **No local Proxmox.** Proxmox VE is amd64-only. On Apple Silicon it would
  need full x86 emulation with its own guests emulated again inside that,
  which is far too slow to be useful. Integration testing uses the real
  cluster, or rented x86 hardware with nested virtualization.
