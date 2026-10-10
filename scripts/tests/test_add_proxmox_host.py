# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

import os
import shlex
import subprocess
import tempfile
import textwrap
import unittest
from pathlib import Path


HOSTS_DIR = Path(__file__).resolve().parent.parent / "user_callable" / "hosts"
SCRIPT = HOSTS_DIR / "add_proxmox_host.sh"
INVENTORY_SCRIPT = HOSTS_DIR / "inventory_disks.sh"
QDEVICE_LIB = HOSTS_DIR.parents[1] / "lib" / "qdevice.sh"
MOX1_CONFIG = HOSTS_DIR.parents[2] / "config" / "mox1.conf"
TEST_IDRAC_IP = "192.0.2.63"


def config_value(path: Path, key: str) -> str:
    prefix = f"{key}="
    for line in path.read_text(encoding="utf-8").splitlines():
        if line.startswith(prefix):
            value = line.removeprefix(prefix)
            if value:
                return value
    raise AssertionError(f"{key} is missing from {path}")


class AddProxmoxHostTests(unittest.TestCase):
    def run_bash(self, body: str, expected: int = 0) -> subprocess.CompletedProcess[str]:
        completed = subprocess.run(
            [
                "bash",
                "-c",
                f"source {SCRIPT!s}\n{textwrap.dedent(body)}",
            ],
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(
            completed.returncode,
            expected,
            msg=f"stdout:\n{completed.stdout}\nstderr:\n{completed.stderr}",
        )
        return completed

    def test_production_iso_tools_are_installed_on_every_online_node(self) -> None:
        source = SCRIPT.read_text(encoding="utf-8")
        for value in (
            "PROD_ISO_BUILDER_SOURCE",
            "PROD_ISO_PREPARER_SOURCE",
            "guests/prod/build_ubuntu_autoinstall.py",
            "guests/prod/prepare_prod_iso.sh",
            "/var/lib/app-ha-proxmox/iso-cache",
            "curl psmisc util-linux xorriso",
        ):
            self.assertIn(value, source)

    def test_qdevice_status_requires_alive_voting_nodes_and_vote_totals(self) -> None:
        status = """\
Quorate:          Yes
Expected votes:   3
Total votes:      3
Flags:            Quorate Qdevice
    Nodeid      Votes    Qdevice Name
0x00000001          1    A,V,NMW mox1 (local)
0x00000002          1    A,V,NMW mox2
0x00000000          1            Qdevice
"""
        self.run_bash(
            f"""
            status={shlex.quote(status)}
            qd_status_is_healthy "$status" 2
            """
        )

        not_voting = status.replace("A,V,NMW mox2", "A,NV,NMW mox2")
        self.run_bash(
            f"""
            status={shlex.quote(not_voting)}
            if qd_status_is_healthy "$status" 2; then
              exit 9
            fi
            """
        )

        wrong_total = status.replace("Total votes:      3", "Total votes:      2")
        self.run_bash(
            f"""
            status={shlex.quote(wrong_total)}
            if qd_status_is_healthy "$status" 2; then
              exit 9
            fi
            """
        )

    def test_odd_cluster_status_requires_qdevice_absent_and_exact_votes(self) -> None:
        self.run_bash(
            """
            status=$'Quorate:          Yes\\nExpected votes:   3\\nTotal votes:      3\\nFlags:            Quorate'
            qd_status_absent_is_healthy "$status" 3
            status+=$' Qdevice'
            if qd_status_absent_is_healthy "$status" 3; then
              exit 9
            fi
            """
        )

    def test_qdevice_hooks_reach_members_through_the_control_node(self) -> None:
        completed = self.run_bash(
            r"""
            cluster_node_control() { printf 'node=%s\n' "$1" >&2; shift; "$@"; }
            cluster_control() {
              printf '%s\n' '[{"node":"mox3","status":"offline"},{"node":"mox1","status":"online"}]'
            }
            MAX_MOX_HOSTS=10
            qd_exec mox2 printf '<%s>' 'a b' '$HOME' "it's"
            printf '\n'
            qd_member_states
            """
        )
        self.assertEqual(completed.stdout, "<a b><$HOME><it's>\nmox1 online\nmox3 offline\n")
        self.assertIn("node=mox2", completed.stderr)

    def test_host_apt_commands_wait_for_the_apt_lock(self) -> None:
        source = SCRIPT.read_text(encoding="utf-8")
        lines = source.splitlines()
        checked = 0
        for number, line in enumerate(lines):
            if not line.lstrip().startswith(("apt-get", "DEBIAN_FRONTEND=noninteractive apt-get")):
                continue
            if "tailscale" in line:
                continue
            checked += 1
            self.assertIn("-o DPkg::Lock::Timeout=60", line, f"line {number + 1}")
            if " update" in line:
                self.assertEqual(lines[number - 1].strip(), "apt_wait_for_locks", f"line {number + 1}")
        self.assertEqual(checked, 12)
        self.assertEqual(source.count("remote_apt_script "), 5)
        self.assertEqual(source.count("declare -f apt_lock_holder apt_wait_for_locks"), 2)

    def test_current_prepared_iso_defers_source_iso_requirement(self) -> None:
        self.run_bash(
            """
            STATE_DIR="$(mktemp -d)"
            trap 'rm -rf "$STATE_DIR"' EXIT
            HOST_SETUP_CONFIG_SHA256=abc123
            prepared="$STATE_DIR/prepared.iso"
            : >"$prepared"
            write_state iso-built
            write_state iso-config-sha256 "$HOST_SETUP_CONFIG_SHA256"
            write_state prepared-iso "$prepared"
            HOST_KEY_PUB="$STATE_DIR/host.pub"
            source_iso_is_required
            printf 'ssh-ed25519 AAAA\\n' >"$HOST_KEY_PUB"
            if source_iso_is_required; then
              exit 9
            fi
            rm -f "$(state_path iso-built)"
            source_iso_is_required
            """
        )

    SOURCE_ISO_LAYOUT = """
            ARTIFACTS_DIR="$(mktemp -d)"
            trap 'rm -rf "$ARTIFACTS_DIR"' EXIT
            SOURCE_ISO_DIR="$ARTIFACTS_DIR/source-iso"
            PROXMOX_ISO_FILE_URL=https://example.test/iso/proxmox-ve_9.2-1.iso
            served=good
            PROXMOX_ISO_FILE_SHA256="$(printf good | sha256sum | awk '{print toupper($1)}')"
            curl() {
              local output=""
              while (($#)); do
                [[ "$1" == --output ]] && output="$2"
                shift
              done
              printf 'curl\\n' >>"$ARTIFACTS_DIR/curl-calls"
              printf '%s' "$served" >"$output"
            }
            curl_calls() { cat "$ARTIFACTS_DIR/curl-calls" 2>/dev/null | wc -l; }
            set_source_iso_path
            """

    def test_source_iso_is_cached_for_every_host_under_its_url_file_name(self) -> None:
        self.run_bash(
            self.SOURCE_ISO_LAYOUT
            + """
            [[ "$SOURCE_ISO" == "$ARTIFACTS_DIR/source-iso/proxmox-ve_9.2-1.iso" ]]
            PROXMOX_ISO_FILE_URL='https://example.test/dl/proxmox.iso?mirror=1'
            set_source_iso_path
            [[ "$SOURCE_ISO" == "$ARTIFACTS_DIR/source-iso/proxmox.iso" ]]
            PROXMOX_ISO_FILE_URL='https://example.test/dl/.iso'
            ( set_source_iso_path ) 2>/dev/null && exit 9
            exit 0
            """
        )

    def test_source_iso_is_downloaded_once_then_reused(self) -> None:
        self.run_bash(
            self.SOURCE_ISO_LAYOUT
            + """
            ensure_source_iso
            [[ "$(cat "$SOURCE_ISO")" == good && "$(curl_calls)" == 1 ]]
            [[ "$(stat -c %a "$SOURCE_ISO")" == 600 ]]
            ensure_source_iso
            [[ "$(curl_calls)" == 1 ]]
            printf corrupt >"$SOURCE_ISO"
            ensure_source_iso 2>"$ARTIFACTS_DIR/stderr"
            [[ "$(cat "$SOURCE_ISO")" == good && "$(curl_calls)" == 2 ]]
            grep -q 'downloading it again' "$ARTIFACTS_DIR/stderr"
            """
        )

    def test_source_iso_with_wrong_hash_is_not_kept(self) -> None:
        completed = self.run_bash(
            self.SOURCE_ISO_LAYOUT
            + """
            served=tampered
            ensure_source_iso
            """,
            expected=1,
        )
        self.assertIn("Downloaded Proxmox VE ISO SHA-256 mismatch", completed.stderr)
        self.run_bash(
            self.SOURCE_ISO_LAYOUT
            + """
            served=tampered
            ( ensure_source_iso ) >/dev/null 2>&1 && exit 9
            [[ ! -e "$SOURCE_ISO" ]]
            [[ -z "$(find "$SOURCE_ISO_DIR" -name '*.partial.*')" ]]
            """
        )

    def test_source_iso_is_kept_by_default_after_media_is_built(self) -> None:
        completed = self.run_bash(
            self.SOURCE_ISO_LAYOUT
            + """
            HOST_ID=mox2
            ensure_source_iso
            offer_to_delete_source_iso </dev/null
            [[ -f "$SOURCE_ISO" ]]
            printf '\\n' | offer_to_delete_source_iso
            [[ -f "$SOURCE_ISO" ]]
            printf 'n\\n' | offer_to_delete_source_iso
            [[ ! -e "$SOURCE_ISO" ]]
            """
        )
        self.assertEqual(completed.stdout.count("Kept "), 2)
        self.assertIn("Deleted ", completed.stdout)

    def test_downloaded_assistant_checksum_output_does_not_pollute_path(self) -> None:
        self.run_bash(
            """
            ARTIFACTS_DIR="$(mktemp -d)"
            trap 'rm -rf "$ARTIFACTS_DIR"' EXIT
            curl() {
              : >"${@: -1}"
            }
            sha256sum() {
              cat >/dev/null
              printf '%s: OK\\n' "$ARTIFACTS_DIR/tools/assistant.deb"
            }
            dpkg-deb() {
              local destination="$3"
              mkdir -p "$destination/usr/bin"
              printf '#!/bin/sh\\n' \
                >"$destination/usr/bin/proxmox-auto-install-assistant"
              chmod +x "$destination/usr/bin/proxmox-auto-install-assistant"
            }

            expected="$ARTIFACTS_DIR/tools/proxmox-auto-install-assistant-9.2.5/usr/bin/proxmox-auto-install-assistant"
            resolved="$(assistant_binary)"
            [[ "$resolved" == "$expected" ]]
            """
        )

    GATE_LAYOUT = """
            HOST_ARTIFACTS="$(mktemp -d)"
            trap 'rm -rf "$HOST_ARTIFACTS"' EXIT
            STATE_DIR="$HOST_ARTIFACTS/state"
            GENERATED_DIR="$HOST_ARTIFACTS/generated"
            SSH_DIR="$HOST_ARTIFACTS/ssh"
            mkdir -p "$STATE_DIR" "$GENERATED_DIR" "$SSH_DIR"
            HOST_KEY="$SSH_DIR/mox1_ssh_host_ed25519_key"
            HOST_KEY_PUB="$HOST_KEY.pub"
            ssh-keygen -q -t ed25519 -N '' -f "$HOST_KEY"
            prepared="$HOST_ARTIFACTS/prepared.iso"
            : >"$prepared"
            : >"$GENERATED_DIR/answer.toml"
            : >"$GENERATED_DIR/first-boot.sh"
            write_state prepared-iso "$prepared"
            TAILSCALE_TAG=tag:proxmox-host
            """

    def test_installation_gate_warns_before_idrac_steps_and_accepts_go(self) -> None:
        completed = self.run_bash(
            self.GATE_LAYOUT
            + f"""
            HOST_ID=mox1
            PROXMOX_FQDN=mox1.example.com
            PROXMOX_IP={shlex.quote(config_value(MOX1_CONFIG, "PROXMOX_IP"))}
            NVME_MIRROR_0_SERIAL_1=disk-a
            NVME_MIRROR_0_SERIAL_2=disk-b
            CONFIGURED_MIRROR_PAIRS=(0)
            HARDWARE_INVENTORY_MODE=idrac
            PROXMOX_PUBLIC_MAC=00:11:22:33:44:55
            PROXMOX_SECONDARY_MAC=00:11:22:33:44:66
            IDRAC_IP={shlex.quote(TEST_IDRAC_IP)}
            fingerprint="$(ssh-keygen -lf "$HOST_KEY_PUB" | awk '{{print $2}}')"
            printf 'GO\\nGO\\n' | installation_gate
            has_state installed
            [[ ! -e "$prepared" && ! -e "$HOST_KEY" && -s "$HOST_KEY_PUB" ]]
            [[ ! -e "$GENERATED_DIR/answer.toml" && ! -e "$GENERATED_DIR/first-boot.sh" ]]
            printf 'FINGERPRINT=%s\\n' "$fingerprint"
            """
        )
        self.assertLess(
            completed.stdout.index("DESTRUCTIVE ACTION"),
            completed.stdout.index("Using the iDRAC/IPMI HTML5 console"),
        )
        self.assertIn(
            f"iDRAC console: https://{TEST_IDRAC_IP}/restgui/start.html",
            completed.stdout,
        )
        self.assertNotIn("ERASE mox1 AND INSTALL PROXMOX", completed.stdout)
        fingerprint = completed.stdout.split("FINGERPRINT=", 1)[1].strip()
        self.assertIn(f"Ed25519 {fingerprint}", completed.stdout)
        self.assertIn("No SSH fingerprint check is needed", completed.stdout)
        for removed in ("ssh-keygen -lf", "~/.ssh/config entry using", "100.87.74.124"):
            self.assertNotIn(removed, completed.stdout)
        self.assertIn("step 6 is complete", completed.stderr)
        self.assertLess(
            completed.stdout.index("REMOVING THE INSTALLATION ISO"),
            completed.stdout.index("Shredding the spent installation ISO"),
        )
        self.assertIn("iDRAC/IPMI virtual media mapping for mox1, detach it now", completed.stdout)
        self.assertIn("Confirm nothing is using the installation ISO.", completed.stderr)

    def test_iso_is_kept_until_detach_is_confirmed(self) -> None:
        completed = self.run_bash(
            self.GATE_LAYOUT
            + """
            HOST_ID=mox1
            write_state installed
            installation_gate </dev/null
            """,
            expected=1,
        )
        self.assertIn("detach it now", completed.stdout)
        self.assertNotIn("Shredding the spent installation ISO", completed.stdout)
        self.assertIn("Input ended", completed.stderr)

    def test_installed_resume_shreds_leftover_installation_secrets(self) -> None:
        self.run_bash(
            self.GATE_LAYOUT
            + """
            HOST_ID=mox1
            write_state installed
            printf 'GO\\n' | installation_gate
            [[ ! -e "$prepared" && ! -e "$HOST_KEY" && -s "$HOST_KEY_PUB" ]]
            [[ ! -e "$GENERATED_DIR/answer.toml" ]]
            """
        )
        no_iso = self.run_bash(
            self.GATE_LAYOUT
            + """
            rm -f "$prepared"
            write_state installed
            installation_gate </dev/null
            [[ ! -e "$HOST_KEY" && ! -e "$GENERATED_DIR/answer.toml" ]]
            """
        )
        self.assertNotIn("detach it now", no_iso.stdout)
        outside = self.run_bash(
            self.GATE_LAYOUT
            + """
            elsewhere="$(mktemp)"
            trap 'rm -rf "$HOST_ARTIFACTS" "$elsewhere"' EXIT
            write_state prepared-iso "$elsewhere"
            write_state installed
            installation_gate
            """,
            expected=1,
        )
        self.assertIn("outside", outside.stderr)

    def test_manual_installation_gate_uses_generic_console_instructions(self) -> None:
        completed = self.run_bash(
            self.GATE_LAYOUT
            + """
            HOST_ID=mox1
            PROXMOX_FQDN=mox1.example.com
            PROXMOX_IP=192.0.2.10
            NVME_MIRROR_0_SERIAL_1=disk-a
            NVME_MIRROR_0_SERIAL_2=disk-b
            NVME_MIRROR_0_CAPACITY_BYTES_1=1000204886016
            NVME_MIRROR_0_CAPACITY_BYTES_2=1000204886016
            CONFIGURED_MIRROR_PAIRS=(0)
            HARDWARE_INVENTORY_MODE=manual
            PROXMOX_PUBLIC_MAC=00:11:22:33:44:55
            PROXMOX_SECONDARY_MAC=00:11:22:33:44:66
            printf 'GO\\nGO\\n' | installation_gate
            """
        )
        self.assertIn("physical or remote console", completed.stdout)
        self.assertIn("bootable media supported by the target host", completed.stdout)
        self.assertNotIn("iDRAC", completed.stdout)

    def test_installation_gate_explains_duplicate_rpool_recovery(self) -> None:
        completed = self.run_bash(
            self.GATE_LAYOUT
            + """
            HOST_ID=mox1
            PROXMOX_FQDN=mox1.example.com
            PROXMOX_IP=192.0.2.10
            NVME_MIRROR_0_SERIAL_1=disk-a
            NVME_MIRROR_0_SERIAL_2=disk-b
            NVME_MIRROR_0_CAPACITY_BYTES_1=1000204886016
            NVME_MIRROR_0_CAPACITY_BYTES_2=1000204886016
            CONFIGURED_MIRROR_PAIRS=(0 1)
            CONFIGURED_NVME_SERIALS=(disk-a disk-b disk-c disk-d)
            HARDWARE_INVENTORY_MODE=manual
            PROXMOX_PUBLIC_MAC=00:11:22:33:44:55
            PROXMOX_SECONDARY_MAC=00:11:22:33:44:66
            printf 'GO\\nGO\\n' | installation_gate
            """
        )
        out = completed.stdout
        heading = 'IF THE FIRST BOOT STOPS AT AN "(initramfs)" PROMPT'
        steps = [out.index(f"\n  {number}. ") for number in range(1, 7)]
        self.assertEqual(steps, sorted(steps))
        self.assertLess(out.index(heading + "...\n"), steps[0])
        self.assertLess(steps[-1], out.index("No SSH fingerprint check is needed"))
        self.assertIn("  6. Before reporting INSTALL COMPLETE, open the Tailscale", out)
        for expected in (
            "more than one matching pool",
            "NVME_MIRROR_0_SERIAL_2 from your mox1.conf file,",
            "zpool labelclear -f /dev/disk/by-id/NAME",
            "zpool import -N rpool",
            "zpool status",
        ):
            self.assertIn(expected, out)

    def test_json_installation_gate_sends_the_full_instructions_as_context(self) -> None:
        completed = self.run_bash(
            self.GATE_LAYOUT
            + """
            HOST_ID=mox1
            PROXMOX_FQDN=mox1.example.com
            PROXMOX_IP=192.0.2.10
            NVME_MIRROR_0_SERIAL_1=disk-a
            NVME_MIRROR_0_SERIAL_2=disk-b
            NVME_MIRROR_0_CAPACITY_BYTES_1=1000204886016
            NVME_MIRROR_0_CAPACITY_BYTES_2=1000204886016
            CONFIGURED_MIRROR_PAIRS=(0 1)
            CONFIGURED_NVME_SERIALS=(disk-a disk-b disk-c disk-d)
            HARDWARE_INVENTORY_MODE=manual
            PROXMOX_PUBLIC_MAC=00:11:22:33:44:55
            PROXMOX_SECONDARY_MAC=00:11:22:33:44:66
            bmac_ui_is_json() { return 0; }
            bmac_ui_manual_action() {
              printf '%s\\n' "$BMAC_UI_CONTEXT" >"$HOST_ARTIFACTS/context"
              printf '%s\\n' "$@" >"$HOST_ARTIFACTS/request"
            }
            scrub_installation_media() { :; }
            installation_gate >"$HOST_ARTIFACTS/stdout"
            has_state installed
            printf '%s\\n===CONTEXT===\\n' "$(cat "$HOST_ARTIFACTS/stdout")"
            cat "$HOST_ARTIFACTS/context"
            printf '===REQUEST===\\n'
            cat "$HOST_ARTIFACTS/request"
            """
        )
        printed, rest = completed.stdout.split("===CONTEXT===\n", 1)
        context, request = rest.split("===REQUEST===\n", 1)
        self.assertTrue(context.startswith("CUSTOM INSTALLER READY - INSTALL THE ISO ON THE HOST NOW\n"))
        self.assertIn(context.strip(), printed)
        for expected in (
            'IF THE FIRST BOOT STOPS AT AN "(initramfs)" PROMPT',
            "zpool labelclear -f /dev/disk/by-id/NAME",
            "DESTRUCTIVE ACTION",
            "  1. Keep the target host console open.",
            "  6. Before reporting INSTALL COMPLETE",
            "No SSH fingerprint check is needed",
        ):
            self.assertIn(expected, context)
        self.assertIn("--id\ninstall_complete\n", request)
        self.assertIn("--instruction\nKeep the target host console open.\n", request)
        self.assertNotIn("--instruction\n1. ", request)

    def test_manual_inventory_allows_pair_capacities_within_one_percent(self) -> None:
        self.run_bash(
            """
            STATE_DIR="$(mktemp -d)"
            trap 'rm -rf "$STATE_DIR"' EXIT
            HARDWARE_INVENTORY_MODE=manual
            HOST_SETUP_CONFIG_SHA256=config-hash
            CONFIGURED_MIRROR_PAIRS=(0 1)
            NVME_MIRROR_0_CAPACITY_BYTES_1=2000398934016
            NVME_MIRROR_0_CAPACITY_BYTES_2=1999998934016
            NVME_MIRROR_1_CAPACITY_BYTES_1=2000398934016
            NVME_MIRROR_1_CAPACITY_BYTES_2=2000398934016
            discover_hardware
            [[ "$(read_state mirror-0-minimum-capacity-bytes)" == 1999998934016 ]]
            [[ "$(read_state mirror-1-minimum-capacity-bytes)" == 2000398934016 ]]
            [[ "$(read_state hardware-verified)" == manual ]]
            [[ "$(read_state zfs-hdsize-gib)" =~ ^[1-9][0-9]*$ ]]
            """
        )

        # A decommissioned pair leaves a gap; later pairs keep their numbers.
        self.run_bash(
            """
            STATE_DIR="$(mktemp -d)"
            trap 'rm -rf "$STATE_DIR"' EXIT
            HARDWARE_INVENTORY_MODE=manual
            HOST_SETUP_CONFIG_SHA256=config-hash
            CONFIGURED_MIRROR_PAIRS=(0 2)
            NVME_MIRROR_0_CAPACITY_BYTES_1=1000204886016
            NVME_MIRROR_0_CAPACITY_BYTES_2=1000204886016
            NVME_MIRROR_2_CAPACITY_BYTES_1=4000787030016
            NVME_MIRROR_2_CAPACITY_BYTES_2=4000787030016
            discover_hardware
            [[ "$(read_state mirror-2-minimum-capacity-bytes)" == 4000787030016 ]]
            ! has_state mirror-1-minimum-capacity-bytes
            """
        )

        completed = self.run_bash(
            """
            STATE_DIR="$(mktemp -d)"
            trap 'rm -rf "$STATE_DIR"' EXIT
            HARDWARE_INVENTORY_MODE=manual
            HOST_SETUP_CONFIG_SHA256=config-hash
            CONFIGURED_MIRROR_PAIRS=(0)
            NVME_MIRROR_0_CAPACITY_BYTES_1=1000204886016
            NVME_MIRROR_0_CAPACITY_BYTES_2=980000000000
            discover_hardware
            """,
            expected=1,
        )
        self.assertIn("differ by more than 1%", completed.stderr)

    def test_inventory_helper_uses_live_host_state(self) -> None:
        text = INVENTORY_SCRIPT.read_text()
        self.assertIn('source "${SCRIPT_DIR}/../../lib/disk_workflows.sh"', text)
        self.assertIn('dw_select_host "$HOST_ARG"', text)
        self.assertIn('dw_inventory "$LAYOUT"', text)
        self.assertNotIn("NVME_MIRROR_", text)
        self.assertNotIn("moxN.conf", text)

    def test_reboot_wait_requires_changed_kernel_boot_id(self) -> None:
        self.run_bash(
            """
            STATE_DIR="$(mktemp -d)"
            trap 'rm -rf "$STATE_DIR"' EXIT
            write_state test-active-boot-id old-boot
            wait_for_exact() { :; }
            wait_for_host() { :; }
            remote() { printf '%s\\n' new-boot; }
            reboot_and_wait "test reboot" test-active
            """
        )
        completed = self.run_bash(
            """
            STATE_DIR="$(mktemp -d)"
            trap 'rm -rf "$STATE_DIR"' EXIT
            write_state test-active-boot-id same-boot
            wait_for_exact() { :; }
            wait_for_host() { :; }
            remote() { printf '%s\\n' same-boot; }
            reboot_and_wait "test reboot" test-active
            """,
            expected=1,
        )
        self.assertIn("kernel boot ID did not change", completed.stderr)

    def test_safety_mechanisms_are_present_in_generated_host_flow(self) -> None:
        source = SCRIPT.read_text(encoding="utf-8")
        library = QDEVICE_LIB.read_text(encoding="utf-8")
        self.assertIn("pvecm qdevice setup '${QD_IPV4}' --force", library)
        self.assertNotIn("pvecm qdevice setup '${PROXMOX_QDEVICE_HOST}' --force", library)
        self.assertNotIn("pvecm qdevice setup", source)
        self.assertIn('flock -x "$lock_fd"', source)
        self.assertIn('logical_sector_size="$(blockdev --getss "$disk")"', source)
        self.assertIn("app-ha-rpool-member-for-serial", source)
        self.assertIn(
            'cryptsetup open --test-passphrase --key-file="\\$key_file"',
            source,
        )
        self.assertIn("ip protocol 112 ip saddr { $peer_set }", source)
        self.assertIn("app-ha-haproxy-route-sync.timer", source)
        self.assertIn(
            "ExecStartPre=$install_root/lib/sync_haproxy_routes.sh "
            "--assert-local-current",
            source,
        )
        self.assertIn('UserKnownHostsFile=${pin}', source)
        self.assertIn('HostKeyAlias=${peer_node}', source)
        self.assertIn("GlobalKnownHostsFile=none", source)
        self.assertIn("printf -v remote_command '%q ' bash -s --", source)

    def test_rpool_member_helper_returns_zfs_stored_path(self) -> None:
        source = SCRIPT.read_text(encoding="utf-8")
        stored_path_lookup = (
            r"done < <(zpool status -P rpool | "
            r"awk '$1 ~ /^\/dev\// { print $1 }')"
        )
        resolved_path_lookup = (
            r"done < <(zpool status -LP rpool | "
            r"awk '$1 ~ /^\/dev\// { print $1 }')"
        )
        self.assertEqual(source.count(stored_path_lookup), 2)
        self.assertNotIn(resolved_path_lookup, source)

    def test_boot_tests_reselect_serial_bearing_zfs_path_after_reboot(self) -> None:
        source = SCRIPT.read_text(encoding="utf-8")
        research_proven_lookup = (
            'part="$(zpool status -P rpool | '
            "awk -v serial=\"$serial\" '$1 ~ serial { print $1; exit }')\""
        )
        self.assertEqual(source.count(research_proven_lookup), 2)
        self.assertIn('member="/dev/mapper/$mapper"', source)
        self.assertIn('zpool offline rpool "$member"', source)
        self.assertIn('zpool online rpool "$member"', source)

    def test_encryption_choice_precedes_three_reboot_test_choice(self) -> None:
        source = SCRIPT.read_text(encoding="utf-8")
        main = source[source.index("main() {") :]
        self.assertLess(
            main.index("choose_encryption_policy"),
            main.index("choose_boot_test_policy"),
        )
        self.assertIn("encrypted_boot_test A\n      encrypted_boot_test B", main)
        self.assertIn("raw_boot_test A\n      raw_boot_test B", main)
        self.assertEqual(main.count("final_normal_boot_test"), 2)
        self.assertNotIn("encrypted_a_boot_test", source)
        self.assertNotIn("encrypted_normal_boot_test", source)

    def test_reboots_use_short_orderly_transient_jobs(self) -> None:
        source = SCRIPT.read_text(encoding="utf-8")
        self.assertEqual(source.count("--on-active=1s"), 3)
        self.assertEqual(source.count("--timer-property=AccuracySec=100ms"), 3)
        self.assertEqual(source.count("--collect /usr/bin/systemctl reboot"), 3)
        self.assertNotIn("--on-active=10s", source)

    def test_private_nic_carrier_allows_slow_ten_gigabit_negotiation(self) -> None:
        source = SCRIPT.read_text(encoding="utf-8")
        self.assertIn("for attempt in $(seq 1 10); do", source)
        self.assertIn("retrying in 3 seconds", source)
        self.assertIn("sleep 3", source)
        self.assertIn("has no physical carrier after approximately 30 seconds", source)

    def test_private_bridge_has_one_dedicated_replication_address(self) -> None:
        source = SCRIPT.read_text(encoding="utf-8")
        self.assertIn('address $replication_cidr', source)
        self.assertIn('"$PROXMOX_SECONDARY_IP" "$MOX_REPLICATION_IP"', source)
        self.assertIn(
            "migration_address.network != migration",
            source,
        )
        self.assertIn(
            "if address in migration",
            source,
        )

    def test_existing_host_artifacts_are_offered_before_configuration_load(self) -> None:
        source = SCRIPT.read_text(encoding="utf-8")
        main = source[source.index("main() {") :]
        self.assertLess(
            main.index("reset_or_resume_existing_state"),
            main.index("load_configuration"),
        )
        self.assertIn('rm -rf -- "$HOST_ARTIFACTS"', source)
        self.assertIn("global record of exposed Tailscale auth-key digests", source)

    def test_reset_check_succeeds_when_host_artifacts_are_absent_or_empty(self) -> None:
        self.run_bash(
            """
            ARTIFACTS_DIR="$(mktemp -d)"
            trap 'rm -rf "$ARTIFACTS_DIR"' EXIT
            SELECTED_HOST=mox2
            reset_or_resume_existing_state
            mkdir -p "$ARTIFACTS_DIR/mox2"
            reset_or_resume_existing_state
            """
        )

    def test_custom_iso_is_host_scoped_but_auth_key_ledger_is_shared(self) -> None:
        source = SCRIPT.read_text(encoding="utf-8")
        self.assertIn(
            'output="${HOST_ARTIFACTS}/${iso_name}-${HOST_ID}-auto.iso"',
            source,
        )
        self.assertNotIn(
            'output="${ARTIFACTS_DIR}/${iso_name}-${HOST_ID}-auto.iso"',
            source,
        )
        self.assertIn('used_file="${ARTIFACTS_DIR}/used-tailscale-auth-key-sha256"', source)
        self.assertIn('ARTIFACTS_DIR="${PROXMOX_ARTIFACTS_DIR}/hosts"', source)
        ignore = HOSTS_DIR.parents[2].joinpath(".gitignore").read_text(encoding="utf-8")
        self.assertIn("\nconfig/artifacts/\n", ignore)

    def test_first_node_prepares_qdevice_without_violating_vote_parity(self) -> None:
        source = SCRIPT.read_text(encoding="utf-8")
        library = QDEVICE_LIB.read_text(encoding="utf-8")
        self.assertIn("dpkg-query -W -f='${Status}", library)
        self.assertIn("systemctl enable --now corosync-qnetd", library)
        self.assertIn("/run/lock/app-ha-qdevice-provision.lock", library)
        self.assertIn("/var/cache/apt/pkgcache.bin.*", library)
        self.assertIn("$4 ~ /:5403$/", library)
        self.assertIn("awk -v key=\"$key\" '$0 != key { print }'", library)
        prepare = source[source.index("prepare_qdevice() {") :]
        self.assertIn("qd_prepare_qnetd", prepare[: prepare.index("\n}\n")])
        main = source[source.index("main() {") :]
        self.assertLess(
            main.index("prepare_qdevice"),
            main.index("reconcile_cluster_control_plane"),
        )
        self.assertIn("if ((node_count % 2 == 1)); then", source)

    def test_private_identity_join_and_ssh_trust_are_reconciled_before_qdevice(self) -> None:
        source = SCRIPT.read_text(encoding="utf-8")
        main = source[source.index("main() {") :]
        self.assertIn("${EXISTING_NODE}.${PROXMOX_INTERNAL_DOMAIN}", source)
        self.assertIn("PROXMOX_INTERNAL_DOMAIN", source)
        self.assertIn("# BEGIN app-ha managed mox private identities", source)
        self.assertIn("/etc/pve/nodes/${peer_node}/ssh_known_hosts", source)
        self.assertIn("pvecm updatecerts --unmerge-known-hosts", source)
        self.assertNotIn(">/etc/pve/priv/known_hosts", source)
        self.assertNotIn("managed qdevice host key", source)
        self.assertIn("--fingerprint '${cluster_fingerprint}'", source)
        self.assertIn('openssl s_client -connect "${fqdn}:8006"', source)
        join_preflight = source[
            source.index('existing_fqdn="${EXISTING_NODE}.${PROXMOX_INTERNAL_DOMAIN}"') :
        ]
        self.assertLess(
            join_preflight.index("served_fingerprint"),
            join_preflight.index("remove_qdevice_before_membership_change"),
        )
        self.assertIn("pveproxy-ssl.pem", source)
        join = source[source.index("pvecm add '${existing_fqdn}'") :]
        self.assertLess(
            join.index("retire_setup_key_with_admin_identity"),
            join.index("wait_for_all_cluster_nodes_online"),
        )
        control = source[source.index("reconcile_cluster_control_plane()") :]
        self.assertLess(
            control.index("configure_cluster_host_resolution"),
            control.index("cluster_setup"),
        )
        self.assertLess(
            control.index("reconcile_private_cluster_ssh_trust"),
            control.index("reconcile_qdevice"),
        )
        self.assertIn("reconcile_cluster_control_plane", main)
        self.assertNotIn("cluster_has_qdevice", source)
        library = QDEVICE_LIB.read_text(encoding="utf-8")
        for function, first, then in (
            ("qd_reconcile() {", "qd_remove", "qd_clear_stale"),
            ("qd_remove() {", "pvecm qdevice remove", "qd_clear_stale"),
            ("qd_remove() {", "qd_confirm_forced_removal", "pvecm qdevice remove"),
        ):
            body = library[library.index(function) :]
            body = body[: body.index("\n}\n")]
            self.assertLess(body.index(first), body.rindex(then))
        before_join = source[source.index("remove_qdevice_before_membership_change() {") :]
        self.assertIn("  qd_remove ", before_join[: before_join.index("\n}\n")])
        self.assertLess(
            join_preflight.index('qd_require_access_for "$((expected_count + 1))"'),
            join_preflight.index("remove_qdevice_before_membership_change"),
        )
        reconcile = source[source.index("reconcile_qdevice() {") :]
        self.assertIn('qd_reconcile "$node_count"', reconcile[: reconcile.index("\n}\n")])
        remove = library[library.index("qd_remove() {") :]
        self.assertIn("pvecm qdevice remove", remove[: remove.index("\n}\n")])
        self.assertIn('flock -w 1800 "$cluster_lock_fd"', source)
        self.assertIn("cluster-control-plane.lock", source)
        self.assertIn("/run/lock/app-ha-cluster-control-plane", source)
        self.assertIn("Managed mox identity markers in /etc/hosts", source)
        self.assertIn("admin-ssh-enabled", source)
        self.assertIn("setup-key-retired", source)

    def test_vrrp_health_and_final_verification_require_one_vip_owner(self) -> None:
        source = SCRIPT.read_text(encoding="utf-8")
        self.assertEqual(
            source.count("nft -nn list chain ip app_ha_guest_egress"),
            2,
        )
        self.assertIn("Could not prove exactly one stable, reachable online mox", source)
        self.assertIn("vip_owner_count == 1", source)

    def test_haproxy_is_reconciled_before_strict_ingress_start(self) -> None:
        source = SCRIPT.read_text(encoding="utf-8")
        start = source.index("systemctl enable app-ha-haproxy-ingress.service")
        section = source[start : source.index("write_state haproxy-lxc-configured", start)]
        self.assertLess(
            section.index("--local-reconcile --lock-timeout 240"),
            section.index("systemctl restart app-ha-haproxy-ingress.service"),
        )
        self.assertIn("export LANG=C.UTF-8", source)
        self.assertIn("LC_ALL=C.UTF-8", source)
        self.assertEqual(source.count("--lock-timeout 240"), 2)
        self.assertNotIn("enable --now app-ha-haproxy-route-sync.timer", source)
        sync_section = source[
            source.index("sync_haproxy_routes()") : source.index("final_verify()")
        ]
        self.assertLess(
            sync_section.index("--lock-timeout 240"),
            sync_section.index("systemctl start app-ha-haproxy-route-sync.timer"),
        )

    def test_manual_mapper_helper_reports_all_outcomes(self) -> None:
        source = SCRIPT.read_text(encoding="utf-8")
        self.assertIn("is already open; no action was needed", source)
        self.assertIn("opened successfully", source)
        self.assertIn("Could not open encrypted rpool mapper", source)

    def test_confirmations_use_go_and_luks_password_uses_temporary_host_file(
        self,
    ) -> None:
        source = SCRIPT.read_text(encoding="utf-8")
        self.assertIn("Type GO to continue.", source)
        self.assertIn('[[ "$actual" == GO ]]', source)
        self.assertIn('LUKS_SECRET_FILE="/root/.app-ha-luks-passphrase"', source)
        self.assertIn("stage_luks_password_file", source)
        self.assertIn("verify_luks_password_file", source)
        self.assertIn("remove_luks_password_file", source)
        self.assertIn('unset PROXMOX_LUKS_PASSWORD', source)
        self.assertNotIn("read -r -s -p 'Shared rpool LUKS passphrase", source)
        self.assertIn('cryptsetup open --key-file="\\$key_file"', source)
        main = source[source.index("main() {") :]
        luks = main[main.index('if [[ "$ENCRYPTION_POLICY" == luks ]]') :]
        self.assertLess(
            luks.index("stage_luks_password_file"),
            luks.index("verify_luks_password_file existing"),
        )
        self.assertLess(
            luks.index("verify_luks_password_file existing"),
            luks.index("rebuild_luks_member A"),
        )
        self.assertLess(
            luks.index("final_normal_boot_test"),
            luks.index("remove_luks_password_file"),
        )
        self.assertLess(
            luks.index("verify_luks_password_file all"),
            luks.index("remove_luks_password_file"),
        )
        self.assertLess(
            luks.index("remove_luks_password_file"),
            luks.index("configure_private_network"),
        )
        self.run_bash("printf 'GO\\n' | confirm_exact 'continue?' 'OLD PHRASE'")
        self.run_bash(
            "printf 'OLD PHRASE\\n' | confirm_exact 'continue?' 'OLD PHRASE'",
            expected=1,
        )
        self.run_bash("printf 'GO\\n' | wait_for_exact 'ready.' 'OLD PHRASE'")

    def test_setup_runs_key_file_helpers_over_ssh_after_confirmation(self) -> None:
        source = SCRIPT.read_text(encoding="utf-8")
        fake_remote = """
            HOST_ID=mox2
            remote() {
              printf 'REMOTE %s\\n' "$*"
              printf 'STDIN<%s>\\n' "$(cat)"
              return "${REMOTE_STATUS:-0}"
            }
        """
        plain = self.run_bash(
            fake_remote
            + "run_target_helper /root/example-helper 'example completed' <<<'y'\n"
        )
        self.assertIn("TARGET-HOST HELPER SCRIPT", plain.stdout)
        self.assertIn("Target host: mox2", plain.stdout)
        self.assertIn("Helper script: /root/example-helper", plain.stdout)
        self.assertIn("Expected result: example completed", plain.stdout)
        self.assertIn("REMOTE /root/example-helper\nSTDIN<>", plain.stdout)

        formatting = self.run_bash(
            fake_remote
            + "run_target_helper /root/example-helper 'formatted' 'Erase it.' <<<'GO'\n"
        )
        self.assertIn("Erase it.", formatting.stderr)
        self.assertIn("REMOTE /root/example-helper\nSTDIN<GO>", formatting.stdout)

        declined = self.run_bash(
            fake_remote
            + "run_target_helper /root/example-helper 'example completed' <<<'n'\n",
            expected=1,
        )
        self.assertNotIn("REMOTE", declined.stdout)
        self.assertIn("was not run; rerun this script", declined.stderr)

        not_confirmed = self.run_bash(
            fake_remote
            + "run_target_helper /root/example-helper 'formatted' 'Erase it.' <<<'y'\n",
            expected=1,
        )
        self.assertNotIn("REMOTE", not_confirmed.stdout)

        failed = self.run_bash(
            fake_remote
            + "REMOTE_STATUS=3 run_target_helper /root/example-helper 'example completed' <<<'y'\n",
            expected=1,
        )
        self.assertIn("/root/example-helper failed on mox2", failed.stderr)

        self.assertNotIn("wait_for_helper_success", source)
        self.assertNotIn("MANUAL LUKS ACTION REQUIRED", source)
        self.assertEqual(source.count('run_target_helper "$helper"'), 4)
        member = source[source.index("rebuild_luks_member() {") : source.index("encrypted_boot_test() {")]
        extra = source[source.index("configure_extra_mirror() {") : source.index("configure_raw_extra_mirror() {")]
        # Helpers that format LUKS take a GO confirmation; the others do not.
        self.assertIn('"Format partition 3 of detached rpool member $member', member)
        self.assertIn('"Prepare extra mirror $pair on disks', extra)

    def test_new_storage_paths_keep_resume_evidence_distinct_and_live_checked(self) -> None:
        source = SCRIPT.read_text(encoding="utf-8")
        self.assertIn('state="encrypted-final-${survivor}-only-passed"', source)
        self.assertIn(
            "Cannot skip boot tests after the three-reboot drill has started",
            source,
        )
        self.assertIn('remote_script "${CONFIGURED_NVME_SERIALS[@]}"', source)
        self.assertIn("count_a == 1 && count_b == 1 && a != \"\" && a == b", source)
        self.assertIn("verify_clear_mirrors", source)
        self.assertIn("the login prompt is visible, and step ${#steps[@]} is complete", source)

    def test_extra_mirrors_use_the_one_shared_rpool_mirror_tool(self) -> None:
        source = SCRIPT.read_text(encoding="utf-8")
        tool = (HOSTS_DIR.parents[1] / "host_runtime" / "rpool_mirror.sh").read_text(encoding="utf-8")
        # Nate asked for one copy of the add-a-mirror logic: setup and
        # scripts/user_callable/hosts/add_new_disk_vdev.sh both run scripts/host_runtime/rpool_mirror.sh on the host.
        self.assertNotIn("zpool add", source)
        self.assertIn("zpool add", tool)
        for call in (
            '"$RPOOL_MIRROR_TOOL" check-new',
            'luks-prepare --pair %q --key-file %q',
            '"$RPOOL_MIRROR_TOOL" luks-check-prepared',
            '"$RPOOL_MIRROR_TOOL" luks-add',
            '"$RPOOL_MIRROR_TOOL" clear-add',
        ):
            self.assertIn(call, source)

    def test_boot_member_luks_uses_the_shared_rpool_mirror_tool(self) -> None:
        source = SCRIPT.read_text(encoding="utf-8")
        # The same member LUKS code serves scripts/user_callable/hosts/add_replacement_disk.sh.
        luks = source[source.index("rebuild_luks_member() {") : source.index("encrypted_boot_test() {")]
        self.assertNotIn("luksFormat", source)
        self.assertNotIn("update-initramfs", luks)
        self.assertNotIn("crypttab.app-ha-new", source)
        for call in (
            "install_rpool_mirror_tool",
            "luks-prepare-member --member %q --key-file %q",
            '"$RPOOL_MIRROR_TOOL" luks-check-member --member',
            '"$RPOOL_MIRROR_TOOL" luks-register --member',
            '"$RPOOL_MIRROR_TOOL" luks-backup-headers --member',
        ):
            self.assertIn(call, luks)
        self.assertLess(
            luks.index("luks-register --member"), luks.index("zpool attach rpool")
        )

    def test_verified_bootstrap_retires_proxmox_first_boot_payload(self) -> None:
        source = SCRIPT.read_text(encoding="utf-8")
        self.assertIn(
            "rm -f /var/lib/proxmox-first-boot/pending-first-boot-setup",
            source,
        )
        self.assertIn(
            "[[ -f /var/lib/app-ha-bootstrap-complete ]]\n"
            "rm -f /var/lib/proxmox-first-boot/pending-first-boot-setup",
            source,
        )
        self.assertNotIn(
            "systemctl start --no-block app-ha-bootstrap.service\nEOF\n'''",
            source,
        )

    def test_shared_passphrase_helper_writes_nonempty_marker(self) -> None:
        source = SCRIPT.read_text(encoding="utf-8")
        self.assertIn(
            r"""printf 'verified-mappers=%s\n' "\${#mappers[@]}" >"\$marker" """.strip(),
            source,
        )
        self.assertNotIn(r'touch "\$marker"', source)

    def test_hyphenated_bridge_is_not_sent_to_pve_iface_validator(self) -> None:
        source = SCRIPT.read_text(encoding="utf-8")
        self.assertNotIn('--enable 1 --iface "$bridge"', source)
        self.assertIn(
            'iifname != "$bridge" ip saddr '
            "$production_start-$staging_end counter drop",
            source,
        )
        self.assertIn('(.iface // "") == "" and .source == $source', source)

    def test_keepalived_does_not_track_its_own_vrrp_interface_twice(self) -> None:
        source = SCRIPT.read_text(encoding="utf-8")
        self.assertIn("    interface $bridge", source)
        self.assertIn(
            "    track_interface {\n"
            "        $public_if\n"
            "    }",
            source,
        )
        self.assertNotIn(
            "    track_interface {\n"
            "        $bridge\n",
            source,
        )

    def test_storage_content_uses_supported_proxmox_api(self) -> None:
        source = SCRIPT.read_text(encoding="utf-8")
        self.assertNotIn("pvesm config", source)
        self.assertEqual(
            source.count("pvesh get /storage/local --output-format json"),
            2,
        )

    def test_haproxy_lxc_is_pinned_to_amd64_with_guarded_partial_recovery(self) -> None:
        source = SCRIPT.read_text(encoding="utf-8")
        self.assertNotIn(
            "awk '$2 ~ /^debian-13-standard_/ {print $2}'",
            source,
        )
        self.assertIn("_amd64[.]tar[.]zst$", source)
        self.assertIn("--arch amd64 --tags app-ha-fixed-haproxy", source)
        self.assertIn('grep -Fxq "arch: amd64" <<<"$config"', source)
        self.assertIn('[[ "$existing_arch" == arm64 ]]', source)
        self.assertIn('[[ "$(pct status "$vmid")" == "status: stopped" ]]', source)
        self.assertIn('pct destroy "$vmid" --purge 1', source)

    def test_generated_host_names_are_application_agnostic(self) -> None:
        source = SCRIPT.read_text(encoding="utf-8")
        for expected in (
            "/usr/local/sbin/app-ha-disk-by-serial",
            "/etc/systemd/system/app-ha-host-guard.service",
            "/root/.app-ha-bootstrap",
            "/etc/network/interfaces.d/app-ha-private.cfg",
            "/etc/crypttab.app-ha-before-shared-unlock",
            'if ($3 != "app-ha-rpool"',
            "table inet app_ha_host_guard",
            "table ip app_ha_guest_egress",
            "vrrp_script app_ha_egress_ready",
        ):
            self.assertIn(expected, source)

    def test_host_credentials_are_not_process_arguments_or_plaintext_answers(self) -> None:
        source = SCRIPT.read_text(encoding="utf-8")
        self.assertIn("root-password-hashed =", source)
        self.assertNotIn("root-password =", source)
        self.assertIn("curl --config -", source)
        self.assertNotIn('--user "$IDRAC_USER:$IDRAC_PASSWORD"', source)

    def choose_role_body(self, host: str, control: str, members: str, extra: str = "") -> str:
        return f"""
            STATE_DIR="$(mktemp -d)"
            trap 'rm -rf "$STATE_DIR"' EXIT
            HOST_ID={host}
            PROXMOX_CLUSTER_NAME=MyAppCloud
            resolve_control_node() {{
              CONTROL_NODE={control}
              CONTROL_MEMBER_NODES=({members})
              CONTROL_ONLINE_NODES=({members})
              CONTROL_PROBE_NODE="${{CONTROL_MEMBER_NODES[0]:-}}"
            }}
            confirm_exact() {{ printf 'CONFIRM %s\\n' "$1"; }}
            {extra}
            choose_role
            printf 'ROLE=%s EXISTING=%s CONTROL=%s\\n' \\
              "$SETUP_ROLE" "$EXISTING_NODE" "$(cluster_control_node)"
            """

    def test_control_node_creates_cluster_only_when_no_member_is_reachable(self) -> None:
        created = self.run_bash(self.choose_role_body("mox1", "mox1", ""))
        self.assertIn("CONFIRM No member of an existing Proxmox cluster", created.stdout)
        self.assertIn("ROLE=first EXISTING=none CONTROL=mox1", created.stdout)

        assumed = self.run_bash(self.choose_role_body("mox2", "mox1", ""))
        self.assertIn("Assuming control node mox1 will create the cluster", assumed.stdout)
        self.assertIn("ROLE=join EXISTING=mox1 CONTROL=mox1", assumed.stdout)
        self.assertNotIn("CONFIRM", assumed.stdout)

        resumed = self.run_bash(
            self.choose_role_body(
                "mox2", "mox1", "", "write_state setup-role join; write_state existing-node mox1"
            )
        )
        self.assertIn("ROLE=join EXISTING=mox1 CONTROL=mox1", resumed.stdout)

    def test_hosts_join_through_the_control_node_and_slots_may_have_gaps(self) -> None:
        joined = self.run_bash(self.choose_role_body("mox1", "mox3", "mox3 mox4"))
        self.assertIn("ROLE=join EXISTING=mox3 CONTROL=mox3", joined.stdout)
        self.assertNotIn("CONFIRM", joined.stdout)

        member = self.run_bash(self.choose_role_body("mox3", "mox3", "mox3 mox4"))
        self.assertIn("ROLE=join EXISTING=mox3 CONTROL=mox3", member.stdout)

        stale_first = self.run_bash(
            self.choose_role_body(
                "mox1",
                "mox3",
                "mox3 mox4",
                "write_state setup-role first; write_state existing-node none",
            ),
            expected=1,
        )
        self.assertIn("already exist without it", stale_first.stderr)

        resumed = self.run_bash(
            self.choose_role_body(
                "mox5",
                "mox4",
                "mox3 mox4",
                "write_state setup-role join; write_state existing-node mox1",
            )
        )
        self.assertIn("ROLE=join EXISTING=mox4 CONTROL=mox4", resumed.stdout)

    def test_cluster_wide_loops_use_live_members_instead_of_contiguous_slots(self) -> None:
        source = SCRIPT.read_text(encoding="utf-8")
        self.assertNotIn("Configured nodes must be contiguous", source)
        self.assertNotIn("root@mox1", source)
        self.assertNotIn('[[ "$HOST_ID" == mox1 ]]', source)
        self.assertIn('"root@${lock_host}"', source)
        self.assertIn("host-sync --live", source)
        self.assertIn('control-set "$control_node" --expected-node none', source)
        self.assertIn("reserve_registry_host_slot", source)
        self.assertIn("/etc/pve/nodes/${HOST_ID} still exists", source)

    @staticmethod
    def tailscale_peer(
        host: str,
        ip: str,
        created: str,
        tags: list[str] | None = None,
        online: bool = True,
    ) -> dict[str, object]:
        peer: dict[str, object] = {
            "HostName": host,
            "DNSName": f"{host}.example.ts.net.",
            "Online": online,
            "Created": created,
            "TailscaleIPs": [ip, "fd7a:115c:a1e0::1"],
        }
        if tags is not None:
            peer["Tags"] = tags
        return peer

    def peer_selection(self, *peers: dict[str, object]) -> str:
        import json

        status = json.dumps({"Peer": {str(index): peer for index, peer in enumerate(peers)}})
        since = "$(date -u -d 2026-10-06T16:56:07Z +%s)"
        completed = self.run_bash(
            f"""
            TAILSCALE_HOSTNAME=mox1
            TAILSCALE_TAG=tag:proxmox-host
            tailscale_peer_selection "{since}" <<<{shlex.quote(status)}
            """
        )
        return completed.stdout.strip()

    def test_tailscale_peer_selection_requires_one_fresh_tagged_online_peer(self) -> None:
        tag = ["tag:proxmox-host"]
        fresh = self.tailscale_peer("mox1", "100.64.0.10", "2026-10-06T17:12:28.407677017Z", tag)
        stale = self.tailscale_peer("mox1", "100.64.0.9", "2026-10-05T01:00:00.1Z", tag)
        untagged = self.tailscale_peer("mox1", "100.64.0.8", "2026-10-06T17:13:00Z")
        other_tag = self.tailscale_peer(
            "mox1", "100.64.0.7", "2026-10-06T17:13:00Z", ["tag:laptop"]
        )
        similar = self.tailscale_peer("mox10", "100.64.0.6", "2026-10-06T17:14:00Z", tag)
        self.assertEqual(
            self.peer_selection(stale, untagged, other_tag, similar, fresh),
            "ip 100.64.0.10",
        )
        self.assertEqual(self.peer_selection(stale, untagged, similar), "absent")
        self.assertEqual(
            self.peer_selection({**fresh, "Online": False}, stale), "offline"
        )
        second = self.tailscale_peer("mox1", "100.64.0.11", "2026-10-06T17:20:00Z", tag)
        self.assertEqual(self.peer_selection(fresh, second), "ambiguous 2")
        self.assertEqual(self.run_bash(
            """
            TAILSCALE_HOSTNAME=mox1
            TAILSCALE_TAG=tag:proxmox-host
            tailscale_peer_selection 0 <<<'{"Peer": null}'
            """
        ).stdout.strip(), "absent")

    def test_resolve_tailscale_ip_fails_closed_on_ambiguous_peers(self) -> None:
        import json

        tag = ["tag:proxmox-host"]
        peers = {
            "a": self.tailscale_peer("mox1", "100.64.0.10", "2026-10-06T17:12:28Z", tag),
            "b": self.tailscale_peer("mox1", "100.64.0.11", "2026-10-06T17:20:00Z", tag),
        }
        body = """
            STATE_DIR="$(mktemp -d)"
            trap 'rm -rf "$STATE_DIR"' EXIT
            TAILSCALE_HOSTNAME=mox1
            TAILSCALE_TAG=tag:proxmox-host
            write_state iso-built 2026-10-06T16:56:07Z
            tailscale() {{ printf '%s\\n' {status}; }}
            resolve_tailscale_ip
            [[ "$TAILSCALE_IP" == 100.64.0.10 ]]
            [[ "$(read_state tailscale-ip)" == 100.64.0.10 ]]
            """
        self.run_bash(body.format(status=shlex.quote(json.dumps({"Peer": {"a": peers["a"]}}))))
        ambiguous = self.run_bash(
            body.format(status=shlex.quote(json.dumps({"Peer": peers}))), expected=1
        )
        self.assertIn("expected exactly one", ambiguous.stderr)

    def test_pinned_setup_ssh_rejects_an_unexpected_host_key(self) -> None:
        completed = self.run_bash(
            """
            HOST_ARTIFACTS="$(mktemp -d)"
            trap 'rm -rf "$HOST_ARTIFACTS"' EXIT
            STATE_DIR="$HOST_ARTIFACTS"
            HOST_ID=mox1
            SSH_KEY="$HOST_ARTIFACTS/setup"
            KNOWN_HOSTS="$HOST_ARTIFACTS/known_hosts"
            HOST_KEY="$HOST_ARTIFACTS/host"
            HOST_KEY_PUB="$HOST_KEY.pub"
            TAILSCALE_IP=100.64.0.10
            ssh-keygen -q -t ed25519 -N '' -f "$HOST_KEY"
            options="$(ssh_options)"
            grep -Fxq HostKeyAlias=mox1 <<<"$options"
            grep -Fxq HostKeyAlgorithms=ssh-ed25519 <<<"$options"
            ssh() { printf 'Host key verification failed.\\n' >&2; return 255; }
            wait_for_pinned_setup_ssh
            """,
            expected=1,
        )
        self.assertIn("man-in-the-middle", completed.stderr)

    def test_iso_embeds_a_fresh_pinned_host_key_and_drops_local_secrets(self) -> None:
        source = SCRIPT.read_text(encoding="utf-8")
        build = source[source.index("build_iso() {") : source.index("host_key_fingerprint() {")]
        self.assertLess(
            build.index('rm -f -- "$HOST_KEY" "$HOST_KEY_PUB"'),
            build.index("ssh-keygen -q -t ed25519"),
        )
        for value in ('"__HOST_KEY_B64__"', '"__HOST_KEY_PUB_B64__"'):
            self.assertIn(value, build)
        first_boot = build[build.index("first_boot = r'''") :]
        self.assertLess(
            first_boot.index("/etc/ssh/ssh_host_ed25519_key"),
            first_boot.index("app-ha-bootstrap.service"),
        )
        self.assertIn("systemctl try-restart ssh.service", first_boot)
        self.assertIn('ordering = "before-network"', build)
        inspected = build[build.index('inspect-iso "$output"') :]
        for secret in ('"$answer"', '"$first_boot"', '"$HOST_KEY"'):
            self.assertIn(f"shred_secret_file {secret}", inspected)
        self.assertIn("grep -rlZF -- TAILSCALE_AUTH_KEY_B64= /var/lib/proxmox-first-boot", source)
        self.assertIn('[[ -s "$HOST_KEY_PUB" ]]', source[source.index("installation_media_is_current() {") :])
        main = source[source.index("main() {") :]
        self.assertLess(main.index("installation_gate"), main.index("configure_workstation_ssh"))
        self.assertLess(main.index("configure_workstation_ssh"), main.index("bootstrap_host"))

    WORKSTATION_LAYOUT = r"""
            work="$(mktemp -d)"
            trap 'rm -rf "$work"' EXIT
            HOME="$work/home"
            ARTIFACTS_DIR="$work"
            STATE_DIR="$work/state"
            mkdir -p "$HOME/.ssh" "$STATE_DIR"
            HOST_ID=mox1
            TAILSCALE_IP=100.64.0.10
            HOST_KEY="$work/host"
            HOST_KEY_PUB="$HOST_KEY.pub"
            ssh-keygen -q -t ed25519 -N '' -f "$HOST_KEY"
            ssh-keygen -q -t ed25519 -N '' -f "$work/old"
            cat >"$HOME/.ssh/config" <<'EOF'
User operator
ServerAliveInterval 30

Host mox1
    HostName 192.0.2.63
    ProxyJump bastion
EOF
            {
              printf 'mox1 %s\n' "$(awk '{print $1 " " $2}' "$work/old.pub")"
              printf 'unrelated %s\n' "$(awk '{print $1 " " $2}' "$work/old.pub")"
            } >"$HOME/.ssh/known_hosts"
            cp "$HOME/.ssh/config" "$work/config.before"
            cp "$HOME/.ssh/known_hosts" "$work/known_hosts.before"
            sleep() { :; }
            ssh() {
              if [[ "$1" == -G ]]; then
                command ssh -F "$HOME/.ssh/config" -G "$2"
                return
              fi
              printf '%s\n' "$*" >>"$work/probes"
              case "$PROBE" in
                ok) return 0 ;;
                denied) printf 'root@100.64.0.10: Permission denied (publickey).\n' >&2; return 255 ;;
                mismatch) printf 'Host key verification failed.\n' >&2; return 255 ;;
              esac
            }
            """

    def test_workstation_ssh_config_pins_the_embedded_host_key(self) -> None:
        completed = self.run_bash(
            self.WORKSTATION_LAYOUT
            + r"""
            PROBE=ok
            write_workstation_ssh_config mox1-test mox1
            [[ -z "$WORKSTATION_SSH_FAILURE" ]]
            write_workstation_ssh_config mox1-test mox1
            [[ -z "$WORKSTATION_SSH_FAILURE" ]]
            grep -Fq 'ControlMaster=no' "$work/probes"
            [[ "$(stat -c %a "$HOME/.ssh/config")" == 600 ]]
            [[ "$(stat -c %a "$HOME/.ssh/known_hosts")" == 600 ]]
            printf 'PIN=%s\n' "$(awk '{print $1 " " $2}' "$HOST_KEY_PUB")"
            printf '==CONFIG==\n'; cat "$HOME/.ssh/config"
            printf '==KNOWN==\n'; cat "$HOME/.ssh/known_hosts"
            printf '==OTHER==\n'; command ssh -F "$HOME/.ssh/config" -G otherhost
            printf '==MOX1==\n'; command ssh -F "$HOME/.ssh/config" -G mox1
            """
        )
        out = completed.stdout
        pin = out.split("PIN=", 1)[1].splitlines()[0]
        config = out.split("==CONFIG==\n", 1)[1].split("==KNOWN==\n", 1)[0]
        known = out.split("==KNOWN==\n", 1)[1].split("==OTHER==\n", 1)[0]
        other = out.split("==OTHER==\n", 1)[1].split("==MOX1==\n", 1)[0]
        mox1 = out.split("==MOX1==\n", 1)[1]
        self.assertTrue(config.startswith("# BEGIN app-ha managed proxmox host mox1\nHost mox1-test mox1\n"))
        self.assertEqual(config.count("# BEGIN app-ha managed proxmox host mox1"), 1)
        self.assertIn("Host *\n# END app-ha managed proxmox host mox1\n", config)
        self.assertIn("User operator\nServerAliveInterval 30\n", config)
        self.assertIn("    HostName 192.0.2.63\n", config)
        self.assertEqual(known.splitlines().count(f"mox1 {pin}"), 1)
        self.assertNotIn("mox1 ssh-ed25519", known.replace(f"mox1 {pin}", ""))
        self.assertIn("unrelated ssh-ed25519", known)
        self.assertIn("user operator\n", other)
        self.assertIn("serveraliveinterval 30\n", other)
        for line in (
            "hostname 100.64.0.10",
            "user root",
            "hostkeyalias mox1",
            "hostkeyalgorithms ssh-ed25519",
            "stricthostkeychecking true",
            "updatehostkeys false",
        ):
            self.assertIn(line + "\n", mox1)
        self.assertNotIn("proxyjump", mox1)

    def test_workstation_ssh_config_restores_files_when_login_fails(self) -> None:
        denied = self.run_bash(
            self.WORKSTATION_LAYOUT
            + r"""
            PROBE=denied
            write_workstation_ssh_config mox1
            [[ "$WORKSTATION_SSH_FAILURE" == *"not authorized for root"* ]]
            cmp "$HOME/.ssh/config" "$work/config.before"
            cmp "$HOME/.ssh/known_hosts" "$work/known_hosts.before"
            ! compgen -G "$STATE_DIR/workstation-ssh.*" >/dev/null
            """
        )
        self.assertEqual(denied.stderr.count("Restoring the previous"), 1)
        mismatch = self.run_bash(
            self.WORKSTATION_LAYOUT
            + r"""
            PROBE=mismatch
            trap 'cmp "$HOME/.ssh/config" "$work/config.before" && cmp "$HOME/.ssh/known_hosts" "$work/known_hosts.before" && echo RESTORED; rm -rf "$work"' EXIT
            write_workstation_ssh_config mox1
            """,
            expected=1,
        )
        self.assertIn("man-in-the-middle", mismatch.stderr)
        self.assertIn("RESTORED", mismatch.stdout)

    def test_workstation_ssh_prompt_defaults_to_yes_and_host_name(self) -> None:
        completed = self.run_bash(
            self.WORKSTATION_LAYOUT
            + r"""
            PROBE=ok
            HARDWARE_INVENTORY_MODE=idrac
            resolve_tailscale_ip() { :; }
            printf '\n\n' | configure_workstation_ssh
            [[ "$(read_state workstation-ssh-config)" == mox1 ]]
            head -2 "$HOME/.ssh/config"
            configure_workstation_ssh </dev/null
            """
        )
        self.assertIn("Host mox1\n", completed.stdout)
        self.assertIn("Configured and verified pinned workstation SSH: ssh mox1", completed.stdout)
        declined = self.run_bash(
            self.WORKSTATION_LAYOUT
            + r"""
            HARDWARE_INVENTORY_MODE=manual
            resolve_tailscale_ip() { :; }
            printf 'n\n' | configure_workstation_ssh
            [[ "$(read_state workstation-ssh-config)" == declined ]]
            cmp "$HOME/.ssh/config" "$work/config.before"
            """
        )
        self.assertIn("HostKeyAlias mox1", declined.stdout)
        self.assertIn("HostName 100.64.0.10", declined.stdout)



if __name__ == "__main__":
    unittest.main()
