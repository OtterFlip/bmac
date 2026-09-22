# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

import os
import shlex
import subprocess
import tempfile
import textwrap
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("setup_proxmox_host.sh")
INVENTORY_SCRIPT = Path(__file__).with_name("inventory_disks.sh")
MOX1_CONFIG = SCRIPT.parent.parent / "env" / "mox1.conf"
TEST_IDRAC_IP = "192.0.2.63"


def config_value(path: Path, key: str) -> str:
    prefix = f"{key}="
    for line in path.read_text(encoding="utf-8").splitlines():
        if line.startswith(prefix):
            value = line.removeprefix(prefix)
            if value:
                return value
    raise AssertionError(f"{key} is missing from {path}")


class SetupProxmoxHostTests(unittest.TestCase):
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
            qdevice_status_is_healthy "$status" 2
            """
        )

        not_voting = status.replace("A,V,NMW mox2", "A,NV,NMW mox2")
        self.run_bash(
            f"""
            status={shlex.quote(not_voting)}
            if qdevice_status_is_healthy "$status" 2; then
              exit 9
            fi
            """
        )

        wrong_total = status.replace("Total votes:      3", "Total votes:      2")
        self.run_bash(
            f"""
            status={shlex.quote(wrong_total)}
            if qdevice_status_is_healthy "$status" 2; then
              exit 9
            fi
            """
        )

    def test_odd_cluster_status_requires_qdevice_absent_and_exact_votes(self) -> None:
        self.run_bash(
            """
            status=$'Quorate:          Yes\\nExpected votes:   3\\nTotal votes:      3\\nFlags:            Quorate'
            qdevice_absent_status_is_healthy "$status" 3
            status+=$' Qdevice'
            if qdevice_absent_status_is_healthy "$status" 3; then
              exit 9
            fi
            """
        )

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
            if source_iso_is_required; then
              exit 9
            fi
            rm -f "$(state_path iso-built)"
            source_iso_is_required
            """
        )

    def test_downloaded_assistant_checksum_output_does_not_pollute_path(self) -> None:
        self.run_bash(
            """
            ARTIFACTS_DIR="$(mktemp -d)"
            trap 'rm -rf "$ARTIFACTS_DIR"' EXIT
            curl() {
              : >"${@: -1}"
            }
            sha256sum() {
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

    def test_installation_gate_warns_before_idrac_steps_and_accepts_go(self) -> None:
        completed = self.run_bash(
            f"""
            STATE_DIR="$(mktemp -d)"
            trap 'rm -rf "$STATE_DIR"' EXIT
            prepared="$STATE_DIR/prepared.iso"
            : >"$prepared"
            write_state prepared-iso "$prepared"
            HOST_ID=mox1
            PROXMOX_FQDN=mox1.example.com
            PROXMOX_IP={shlex.quote(config_value(MOX1_CONFIG, "PROXMOX_IP"))}
            NVME_MIRROR_1_SERIAL_1=disk-a
            NVME_MIRROR_1_SERIAL_2=disk-b
            MIRROR_PAIR_COUNT=1
            HARDWARE_INVENTORY_MODE=idrac
            PROXMOX_PUBLIC_MAC=00:11:22:33:44:55
            PROXMOX_SECONDARY_MAC=00:11:22:33:44:66
            IDRAC_IP={shlex.quote(TEST_IDRAC_IP)}
            printf 'GO\\n' | installation_gate
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

    def test_manual_installation_gate_uses_generic_console_instructions(self) -> None:
        completed = self.run_bash(
            """
            STATE_DIR="$(mktemp -d)"
            trap 'rm -rf "$STATE_DIR"' EXIT
            prepared="$STATE_DIR/prepared.iso"
            : >"$prepared"
            write_state prepared-iso "$prepared"
            HOST_ID=mox1
            PROXMOX_FQDN=mox1.example.com
            PROXMOX_IP=192.0.2.10
            NVME_MIRROR_1_SERIAL_1=disk-a
            NVME_MIRROR_1_SERIAL_2=disk-b
            NVME_MIRROR_1_CAPACITY_BYTES_1=1000204886016
            NVME_MIRROR_1_CAPACITY_BYTES_2=1000204886016
            MIRROR_PAIR_COUNT=1
            HARDWARE_INVENTORY_MODE=manual
            PROXMOX_PUBLIC_MAC=00:11:22:33:44:55
            PROXMOX_SECONDARY_MAC=00:11:22:33:44:66
            printf 'GO\\n' | installation_gate
            """
        )
        self.assertIn("physical or remote console", completed.stdout)
        self.assertIn("bootable media supported by the target host", completed.stdout)
        self.assertNotIn("iDRAC", completed.stdout)

    def test_manual_inventory_requires_exact_pair_capacities(self) -> None:
        self.run_bash(
            """
            STATE_DIR="$(mktemp -d)"
            trap 'rm -rf "$STATE_DIR"' EXIT
            HARDWARE_INVENTORY_MODE=manual
            HOST_SETUP_CONFIG_SHA256=config-hash
            MIRROR_PAIR_COUNT=2
            NVME_MIRROR_1_CAPACITY_BYTES_1=1000204886016
            NVME_MIRROR_1_CAPACITY_BYTES_2=1000204886016
            NVME_MIRROR_2_CAPACITY_BYTES_1=2000398934016
            NVME_MIRROR_2_CAPACITY_BYTES_2=2000398934016
            discover_hardware
            [[ "$(read_state mirror-1-minimum-capacity-bytes)" == 1000204886016 ]]
            [[ "$(read_state mirror-2-minimum-capacity-bytes)" == 2000398934016 ]]
            [[ "$(read_state hardware-verified)" == manual ]]
            [[ "$(read_state zfs-hdsize-gib)" =~ ^[1-9][0-9]*$ ]]
            """
        )

        completed = self.run_bash(
            """
            STATE_DIR="$(mktemp -d)"
            HARDWARE_INVENTORY_MODE=manual
            HOST_SETUP_CONFIG_SHA256=config-hash
            MIRROR_PAIR_COUNT=1
            NVME_MIRROR_1_CAPACITY_BYTES_1=1000204886016
            NVME_MIRROR_1_CAPACITY_BYTES_2=1000204886017
            discover_hardware
            """,
            expected=1,
        )
        self.assertIn("identical byte capacities", completed.stderr)

    def test_inventory_helper_reports_exact_bytes_and_serials_read_only(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fake_bin = Path(directory)
            fake_lsblk = fake_bin / "lsblk"
            fake_lsblk.write_text(
                """#!/usr/bin/env bash
case "$*" in
  *"PATH,TYPE"*)
    printf '/dev/nvme0n1 disk\\n/dev/nvme1n1 disk\\n'
    ;;
  *"-o SIZE"*)
    printf '1000204886016\\n'
    ;;
  *"-o SERIAL"*)
    [[ "${*: -1}" == /dev/nvme0n1 ]] && printf 'SERIAL-A\\n' || printf 'SERIAL-B\\n'
    ;;
  *"-o MODEL"*)
    printf 'Example NVMe\\n'
    ;;
  *"-o VENDOR"*)
    printf 'Example\\n'
    ;;
  *"-o TRAN"*)
    printf 'nvme\\n'
    ;;
  *"-o ROTA"*)
    printf '0\\n'
    ;;
  *)
    exit 2
    ;;
esac
""",
                encoding="utf-8",
            )
            fake_lsblk.chmod(0o755)
            environment = os.environ.copy()
            environment["PATH"] = f"{fake_bin}:{environment['PATH']}"
            completed = subprocess.run(
                [str(INVENTORY_SCRIPT)],
                env=environment,
                check=False,
                capture_output=True,
                text=True,
            )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(completed.stdout.count("Class:          NVMe SSD"), 2)
        self.assertIn("NVME_MIRROR_N_SERIAL_M=SERIAL-A", completed.stdout)
        self.assertIn("NVME_MIRROR_N_SERIAL_M=SERIAL-B", completed.stdout)
        self.assertEqual(
            completed.stdout.count(
                "NVME_MIRROR_N_CAPACITY_BYTES_M=1000204886016"
            ),
            2,
        )
        for destructive_command in ("wipefs", "mkfs", "sgdisk", "parted"):
            self.assertNotIn(destructive_command, INVENTORY_SCRIPT.read_text())

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
        self.assertIn("pvecm qdevice setup '${QDEVICE_IPV4}' --force", source)
        self.assertNotIn(
            "pvecm qdevice setup '${PROXMOX_QDEVICE_HOST}' --force", source
        )
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
        ignore = SCRIPT.parent.parent.joinpath(".gitignore").read_text(encoding="utf-8")
        self.assertNotIn("\nartifacts/used-tailscale-auth-key-sha256\n", ignore)
        self.assertIn("artifacts/used-tailscale-auth-key-sha256.lock", ignore)
        ledger = SCRIPT.parent / "artifacts" / "used-tailscale-auth-key-sha256"
        self.assertTrue(ledger.is_file())

    def test_first_node_prepares_qdevice_without_violating_vote_parity(self) -> None:
        source = SCRIPT.read_text(encoding="utf-8")
        self.assertIn("dpkg-query -W -f='${Status}", source)
        self.assertIn("systemctl enable --now corosync-qnetd", source)
        self.assertIn("/run/lock/app-ha-qdevice-provision.lock", source)
        self.assertIn("/var/cache/apt/pkgcache.bin.*", source)
        self.assertIn("$4 ~ /:5403$/", source)
        self.assertIn("awk -v key=\"$key\" '$0 != key { print }'", source)
        main = source[source.index("main() {") :]
        self.assertLess(
            main.index("prepare_qdevice"),
            main.index("reconcile_cluster_control_plane"),
        )
        self.assertIn("if ((node_count % 2 == 1)); then", source)

    def test_private_identity_join_and_ssh_trust_are_reconciled_before_qdevice(self) -> None:
        source = SCRIPT.read_text(encoding="utf-8")
        main = source[source.index("main() {") :]
        self.assertIn("mox1.${PROXMOX_INTERNAL_DOMAIN}", source)
        self.assertIn("PROXMOX_INTERNAL_DOMAIN", source)
        self.assertIn("# BEGIN app-ha managed mox private identities", source)
        self.assertIn("/etc/pve/nodes/${peer_node}/ssh_known_hosts", source)
        self.assertIn("pvecm updatecerts --unmerge-known-hosts", source)
        self.assertNotIn(">/etc/pve/priv/known_hosts", source)
        self.assertIn("/etc/ssh/ssh_host_ed25519_key.pub", source)
        self.assertIn("--fingerprint '${cluster_fingerprint}'", source)
        self.assertIn('openssl s_client -connect "${fqdn}:8006"', source)
        join_preflight = source[source.index('existing_fqdn="mox1.${PROXMOX_INTERNAL_DOMAIN}"') :]
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
        helper_prompt = self.run_bash(
            """
            HOST_ID=mox2
            wait_for_helper_success /root/example-helper 'example completed' <<<'GO'
            """
        )
        self.assertIn("TARGET-HOST HELPER SCRIPT STATUS", helper_prompt.stdout)
        self.assertIn("Target host: mox2", helper_prompt.stdout)
        self.assertIn("Helper script: /root/example-helper", helper_prompt.stdout)
        self.assertIn("SCRIPT WORKED?", helper_prompt.stdout + helper_prompt.stderr)
        self.assertEqual(source.count("wait_for_helper_success \"$helper\""), 4)

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
        self.assertIn("Tailscale/SSH steps 6-10 are complete", source)

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



if __name__ == "__main__":
    unittest.main()
