# Task: add pveproxy/spiceproxy lock-file workaround to `add_proxmox_host.sh` (bmac)

## Goal

Make every Proxmox VE host provisioned by bmac's `add_proxmox_host.sh` immune to a
PVE 9 / Debian trixie bug where `pveproxy` (web UI, port 8006) and `spiceproxy`
(port 3128) fail to start after a reboot because they cannot create their lock
files in `/run/lock`. The fix is two systemd drop-in files, installed
idempotently, plus verification.

This was diagnosed and fixed by hand on the production cluster (mox1 + mox2)
on 2026-10-30. The script change should reproduce that fix automatically for
all future hosts.

## Before editing: things to confirm in the repo

- Repo: `github.com/OtterFlip/bmac`. The v3 codebase is isolated under `v3/`;
  everything outside `v3/` is v2. **Find which codebase(s) contain
  `add_proxmox_host.sh` and confirm with the user which one(s) to change.
  Don't change v3 when working on a v2 task (and vice versa).**
- Determine how the script executes commands on the new host:
  - runs **on the host itself** → call the function directly, or
  - runs **from the workstation and SSHes in** → ship the function over SSH
    (see "Integration" below) and check the exit code.
- Find where the script does `apt update` / `apt full-upgrade` and any final
  reboot + health-check step; the fix hooks into both.
- Follow the script's existing conventions (logging helpers, error handling,
  `set -euo pipefail` or not, function naming, how it reports failures).

## Environment

- Proxmox VE 9.x on Debian 13 (trixie), x86_64. Both nodes on identical
  package versions (`pveversion -v` checksums matched).
- 2-node cluster (mox1, mox2) + qdevice → 3 votes total; one node can reboot
  while keeping quorum.
- Hosts reached over Tailscale by short name (`ssh mox1`, `https://mox1:8006/`).
- Other apt repos present on hosts: Tailscale, Cloudflare (`cloudflared`).
- Local tooling on hosts under `/usr/local/lib/app-ha-proxmox/lib/`.

## Symptom

After `apt update && apt full-upgrade` and a reboot, mox1's web UI
(`https://mox1:8006/`) stopped responding. mox2, upgraded and rebooted the
same way, was fine. SSH, cluster, replication and the VM were all healthy.

## How it was diagnosed (evidence)

1. **Network path was fine.** From the workstation:
   - `curl -vk https://mox1:8006/` → `Connection refused` in 5 ms
   - `tailscale ping mox1` → pong in 4 ms; name resolved to its Tailscale IP
   - Fast refusal = nothing listening on 8006.
2. **Service state on mox1:**
   - `pve-cluster` and `pvedaemon`: active.
   - `pveproxy`: stuck in a restart loop (restart counter > 450).
   - `ss -tlnp | grep 8006`: nothing.
   - Journal, repeated every ~2 s:
     ```
     pveproxy[...]: start failed - unable to open lock file '/var/lock/pveproxy.lck' - Permission denied
     ```
   - `pvecm updatecerts` (ExecStartPre) succeeded, so certs / `/etc/pve` were fine.
3. **Comparison of mox1 vs mox2:**
   ```
   mox1: drwxr-xr-x root root /run/lock          (no pveproxy.lck)
   mox2: drwxr-xr-x root root /run/lock
         -rw-r--r-- www-data www-data /run/lock/pveproxy.lck  (created 17:05)
   both: /var/lock -> /run/lock (symlink), /run/lock is tmpfs
   ```
   `/run/lock` is mode **0755 root:root** on both nodes, so `www-data` cannot
   create files there. mox2 only worked because its lock file already existed.
4. **Ruled out:** no tmpfiles rule for `pveproxy.lck` on either node;
   `dpkg --audit` clean on both; identical `pveversion -v`;
   `systemd-tmpfiles-setup.service` active on both.
5. **Root cause found in tmpfiles config** (both files shipped by the
   `systemd` package):
   ```
   /usr/lib/tmpfiles.d/debian.conf:13:  d /run/lock    1777 root root -   -
   /usr/lib/tmpfiles.d/legacy.conf:14:  d /run/lock    0755 root root -
   ```
   The `0755` rule is what ends up applied.
6. **Timing explains why only one node broke** (`stat -c %z /run/lock`):
   - mox1: `/run/lock` changed to 0755 at **16:55:21** (boot), before
     `pveproxy` started → it could never create its lock file.
   - mox2: `pveproxy` created its lock file at **17:05**; `/run/lock` was
     tightened at **17:06:51**, after that → worked by luck. Its next reboot
     could have failed the same way.

   So this is a boot-ordering race. It probably appeared after the upgrade
   because the upgrade brought in the systemd version with the conflicting
   `legacy.conf`, but the failure itself happens at service start.

## Upstream status

- Same failure reported on the Proxmox forum against PVE 9.0.5 in mid-August
  2025 (thread "PVE 9.0.5 pveproxy not working, no webgui"), affecting both
  `pveproxy` and `spiceproxy`. That user's workaround edited the packaged unit
  files in `/usr/lib/systemd/system/` and made `/var/lock` world-writable.
  **Don't copy that**: package upgrades overwrite those unit files, and loosening
  `/run/lock` is unnecessary.
- No Proxmox bug report or packaged fix was found for this specific problem;
  still occurring on fully updated PVE 9 in October 2026.

## The fix

One systemd drop-in per service, in the admin override area
`/etc/systemd/system/<svc>.service.d/lockfile.conf`:

```ini
[Service]
ExecStartPre=/bin/sh -c "touch /run/lock/pveproxy.lck && chown www-data:www-data /run/lock/pveproxy.lck"
```

(and the same with `spiceproxy`).

Why this works and keeps working:

- `pveproxy.service`/`spiceproxy.service` have no `User=`; they start as root
  and drop privileges themselves, so `ExecStartPre` runs as root and can create
  the file regardless of `/run/lock`'s mode.
- It runs on **every** service start: boot, manual restart, and the restart
  that `pve-manager`'s postinst does during `apt full-upgrade`.
- apt never touches `/etc/systemd/system/`, so the drop-ins survive all
  package upgrades. It's safe to install them **before** `apt full-upgrade`.
- `touch` + `chown` is used deliberately instead of `install`: `install`
  replaces the file with a new inode, which could break lock semantics
  across a restart. `touch` on an existing correctly-owned file is a no-op.
- If Proxmox/systemd later fix this properly, the drop-ins remain harmless.
- `/run/lock` is left at its 0755 mode; nothing is loosened.

## Reference implementation (adapt to the script's conventions)

```bash
# Workaround: on PVE 9 / Debian trixie, /run/lock can end up 0755 root:root
# (legacy.conf vs debian.conf tmpfiles conflict), so pveproxy/spiceproxy
# (running as www-data) can't create their lock files and fail to start
# after a reboot. Pre-create the lock files before each service starts.
# Diagnosed on the mox1/mox2 production cluster 2026-10-30.
install_proxy_lockfile_dropins() {
  local svc dropin
  for svc in pveproxy spiceproxy; do
    dropin=/etc/systemd/system/$svc.service.d/lockfile.conf
    mkdir -p "$(dirname "$dropin")"
    cat > "$dropin" <<EOF
[Service]
ExecStartPre=/bin/sh -c "touch /run/lock/$svc.lck && chown www-data:www-data /run/lock/$svc.lck"
EOF
  done
  systemctl daemon-reload
  systemctl restart pveproxy spiceproxy

  # Verify: services up and pveproxy listening, retrying briefly
  local i
  for i in $(seq 1 15); do
    if systemctl is-active --quiet pveproxy spiceproxy && ss -tln | grep -q ':8006 '; then
      echo "pveproxy/spiceproxy lockfile drop-ins installed and services healthy"
      return 0
    fi
    sleep 1
  done
  echo "ERROR: pveproxy/spiceproxy not healthy after installing lockfile drop-ins" >&2
  systemctl --no-pager status pveproxy spiceproxy >&2 || true
  return 1
}
```

Requirements for the integrated version:

- **Idempotent**: re-running `add_proxmox_host.sh` on an existing node must be
  safe (it just rewrites identical files).
- **Fails loudly**: non-zero exit with service status if the proxies don't
  come up; the calling script should abort or report clearly.
- **Polls instead of a fixed sleep.**
- **Commented** with the reason, so it isn't mistaken for mystery config and
  can be removed if fixed upstream.

## Integration

- Call it **after PVE packages are installed**, and it can go **before**
  `apt full-upgrade` (drop-ins survive the upgrade and protect the
  postinst restart). Running it again after the upgrade is harmless.
- If the script runs from the workstation over SSH:
  ```bash
  ssh "$host" "$(declare -f install_proxy_lockfile_dropins); install_proxy_lockfile_dropins" \
    || { echo "lockfile drop-in install failed on $host" >&2; exit 1; }
  ```
- **Post-reboot verification** (the real proof on each host). Add this to the
  script's final reboot/health-check step if it has one:
  ```bash
  systemctl is-active pveproxy spiceproxy        # both: active
  ls -l /run/lock/pveproxy.lck /run/lock/spiceproxy.lck   # both owned www-data, timestamp = boot
  ss -tln | grep -E ':8006 |:3128 '              # both listening
  pvecm status | grep -E 'Quorate|Total votes'   # Quorate: Yes, expected vote count
  ```

## How the manual fix was verified on mox1 and mox2

Done one node at a time:

1. Installed the drop-ins with the loop above → both services `active`, both
   `.lck` files owned `www-data:www-data`, listeners on `*:8006` and `*:3128`.
2. Rebooted the node.
3. Post-reboot check → both services `active`, lock files freshly created at
   boot (mox1 17:26, mox2 17:31), `Quorate: Yes`, `Total votes: 3`,
   `Flags: Quorate Qdevice`.
4. Web UI reachable again at `https://mox1:8006/`.

## Related notes for the same script

- **Use `apt full-upgrade` (= `dist-upgrade`), never plain `apt upgrade`**, on
  PVE/PBS hosts. Proxmox updates often involve package transitions (kernel
  meta-packages, `proxmox-ve`, library swaps) that plain `upgrade` holds back or
  half-applies. Before confirming, a plan that removes `proxmox-ve` or
  `pve-manager` signals a repo misconfiguration; in automation, consider
  checking for that (e.g. a dry run with `apt -s full-upgrade`) and aborting.
- **Repos on existing hosts**: no-subscription is configured in
  `/etc/apt/sources.list.d/pve-no-subscription.sources` (deb822, suite
  `trixie`, component `pve-no-subscription`, `Signed-By:
  /usr/share/keyrings/proxmox-archive-keyring.gpg`). `pve-enterprise.sources`
  and `ceph.sources` are absent (deleted rather than disabled). Adding a second
  file such as `proxmox.sources` with the same entry causes
  "configured multiple times" warnings; if the script manages repos, keep a
  single file and make that step idempotent.
- **Cluster rollout**: update and reboot one node at a time, confirm quorum
  and replication (`pvesr status`) before the next.

## Open item (not yet checked)

`/usr/local/lib/app-ha-proxmox/lib/{app-ha-guest-role-hook.sh,tunnel_sync.sh,process_deferred_cleanup.sh}`
reference `/run/lock` or `/var/lock`. Probably just `flock` use, but confirm
none of them change `/run/lock`'s mode or ownership:

```bash
grep -n "/run/lock\|/var/lock" /usr/local/lib/app-ha-proxmox/lib/*.sh
```

If those scripts come from bmac, check their source in the repo too.
