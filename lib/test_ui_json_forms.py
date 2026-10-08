# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""JSON-mode forms that operator scripts validate as a whole.

Each test sources one script in source-only mode from a small harness,
answers its form through the dashboard protocol, and checks the values the
script would go on to use.
"""

from __future__ import annotations

import os
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(REPO_ROOT / "lib"))

from ui_test_driver import run_json, scripted  # noqa: E402

GIB = 2**30


class JsonFormTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def harness(self, source_only: str, script: str, body: str) -> list[str]:
        path = self.root / "harness.sh"
        path.write_text(
            "#!/usr/bin/env bash\n"
            "set -Eeuo pipefail\n"
            f"export {source_only}=1\n"
            f"source {REPO_ROOT / script}\n"
            'bmac_ui_bootstrap "$@"\n'
            f"RUN_DIR={self.root}\n" + textwrap.dedent(body)
        )
        path.chmod(0o755)
        return [str(path)]

    def run_form(self, argv: list[str], *replies):
        env = {key: value for key, value in os.environ.items() if not key.startswith("BMAC_UI")}
        run = run_json(argv, scripted(*replies), env=env, timeout=60)
        self.assertEqual(run.returncode, 0, run.describe())
        return run

    def test_extend_disk_increase_is_revalidated_until_within_the_allowance(self) -> None:
        argv = self.harness(
            "EXTEND_PROD_DISK_SOURCE_ONLY",
            "guests/prod/extend_prod_vm_disk.sh",
            f"""
            RESOURCE_NAME=prod1
            CURRENT_DISK_BYTES={20 * GIB}
            ALLOWED_BYTES={10 * GIB}
            POOL_RESERVE_PERCENT=20
            prompt_increase_json
            bmac_ui_result increase "$INCREASE_BYTES"
            """,
        )
        seen = []

        def too_big(request):
            seen.append(request)
            return {"values": {"unit": "GiB", "amount": "50"}}

        def fits(request):
            seen.append(request)
            return {"values": {"unit": "GiB", "amount": "2.5"}}

        run = self.run_form(argv, too_big, fits)
        form, retry = seen
        self.assertEqual(form["type"], "input_group")
        self.assertEqual([field["id"] for field in form["fields"]], ["unit", "amount"])
        self.assertIn("10.000 GiB", next(f for f in form["fields"] if f["id"] == "amount")["help"])
        self.assertIn("amount", retry["validation_error"]["field_errors"])
        self.assertEqual(run.of("result")[-1]["data"]["increase"], str(int(2.5 * GIB)))

    def test_staging_options_validate_domain_and_sanitizer(self) -> None:
        bad_sanitizer = self.root / "startup.sh"
        bad_sanitizer.write_text("#!/bin/sh\ntrue\n")
        argv = self.harness(
            "APP_HA_STAGING_VM_SOURCE_ONLY",
            "guests/staging/add_staging_vm.sh",
            """
            SOURCE_NAME=prod1
            SOURCE_PRIMARY_DOMAIN=example.com
            STAGING_NODE=mox2
            STAGING_CANDIDATE_INDEX=1
            CORES_OVERRIDE=""
            MEMORY_GIB_OVERRIDE=""
            SANITIZER_FILE=""
            STAGING_VM_CORES=2
            default_memory_gib=4
            collect_options_json
            bmac_ui_result cores "$STAGING_VM_CORES" memory "$requested_memory_gib" \\
              start "$START_AFTER_CREATION" jump "$SETUP_WORKSTATION_JUMP_SSH" \\
              domain "$DOMAIN_OVERRIDE" link_down "$NETWORK_LINK_DOWN" \\
              sanitizer "$SANITIZER_FILE"
            """,
        )
        answers = {
            "cores": 0,
            "memory_gib": 8,
            "start_after_creation": False,
            "setup_jump_ssh": True,
            "domain_override": "https://bad/",
            "link_down": True,
            "sanitizer": str(bad_sanitizer),
        }
        seen = []

        def first(request):
            seen.append(request)
            return {"values": dict(answers)}

        def fixed(request):
            seen.append(request)
            return {"values": {**answers, "cores": 3, "domain_override": "Stage.Example.COM.", "sanitizer": ""}}

        run = self.run_form(argv, first, fixed)
        errors = seen[1]["validation_error"]["field_errors"]
        self.assertEqual(set(errors), {"cores", "domain_override", "sanitizer"})
        self.assertIn("Bash shebang", errors["sanitizer"])
        self.assertEqual(
            run.of("result")[-1]["data"],
            {
                "cores": "3",
                "memory": "8",
                "start": "false",
                "jump": "true",
                "domain": "stage.example.com",
                "link_down": "true",
                "sanitizer": "",
            },
        )
        self.assertFalse(run.of("error"), run.describe())

    def test_staging_request_asks_for_memory_only_in_the_options_form(self) -> None:
        (self.root / "resources.json").write_text(
            '[{"kind": "production", "state": "active", "index": 1, "name": "prod1"}]'
        )
        argv = self.harness(
            "APP_HA_STAGING_VM_SOURCE_ONLY",
            "guests/staging/add_staging_vm.sh",
            """
            bmac_ui_choose() { printf -v "$1" '%s' "$3"; }
            parse_selected_source() { SOURCE_PRIMARY_DOMAIN=example.com; }
            validate_live_source_and_ha() { :; }
            load_replication_jobs_and_choose_standby() { STAGING_NODE=mox2; }
            verify_source_vm_contract() { :; }
            query_next_vmid() { STAGING_VMID=200; }
            confirm_exact_local() { :; }
            STAGING_CANDIDATE_INDEX=1
            CORES_OVERRIDE=""
            MEMORY_GIB_OVERRIDE=""
            SANITIZER_FILE=""
            STAGING_VM_CORES=2
            STAGING_VM_MEMORY_GIB=4
            collect_request
            bmac_ui_result cores "$STAGING_VM_CORES" memory_mb "$STAGING_VM_MEMORY_MB"
            """,
        )
        run = self.run_form(
            argv,
            {
                "values": {
                    "cores": 10,
                    "memory_gib": 16,
                    "start_after_creation": True,
                    "setup_jump_ssh": True,
                    "domain_override": "stage.example.com",
                    "link_down": True,
                    "sanitizer": "",
                }
            },
        )
        self.assertEqual(
            run.of("result")[-1]["data"], {"cores": "10", "memory_mb": str(16 * 1024)}
        )

    def test_new_production_form_checks_placement_and_domains(self) -> None:
        argv = self.harness(
            "PRODUCTION_VM_SOURCE_ONLY",
            "guests/prod/add_prod_vm.sh",
            """
            ONLINE_NODES=(mox1 mox2 mox3)
            PURPOSE_SLUG=production
            PROD_VM_CORES=2
            PROD_VM_MEMORY_GIB=4
            PROD_VM_DISK_GB=32
            DEFAULT_REPLICATION_MINUTES=15
            STORED_STARTUP_SHA256=""
            STORED_FINAL_NETWORK_ENABLED=""
            INSTALL_PHASE=unstarted
            PROD_GUEST_OS_INSTALL_MODE=unattended
            collect_new_request
            bmac_ui_result placement "${PLACEMENT_NODES[*]}" initial "$INITIAL_NODE" \\
              primary "$PRIMARY_DOMAIN" aliases "${ALIAS_DOMAINS[*]}" \\
              staging "$STAGING_DNS_BASE" memory_mb "$VM_MEMORY_MB" \\
              allocation "$DISK_ALLOCATION" network "$FINAL_NETWORK_ENABLED" \\
              continue_after_install "$CONTINUE_AFTER_INSTALL" \\
              delete_source_iso_cache "$DELETE_SOURCE_ISO_CACHE"
            """,
        )
        values = {
            "placement": ["mox1"],
            "initial_node": "mox3",
            "primary_domain": "Example.COM",
            "aliases": "example.com",
            "cores": 2,
            "memory_gib": 8,
            "disk_gib": 40,
            "allocation": "full",
            "replication_minutes": 15,
        }
        seen = []

        def first(request):
            seen.append(request)
            return {"values": dict(values)}

        def fixed(request):
            seen.append(request)
            return {"values": {**values, "placement": ["mox1", "mox3"], "aliases": "www.example.com"}}

        run = self.run_form(
            argv,
            first,
            fixed,
            {"values": {"startup_script": ""}},
            {"values": {"final_network_enabled": True}},
            {"values": {"continue_after_install": True}},
            {"values": {"delete_source_iso_cache": False}},
        )
        errors = seen[1]["validation_error"]["field_errors"]
        self.assertEqual(set(errors), {"placement", "aliases"}, errors)
        self.assertIn("at least two", errors["placement"])
        self.assertEqual(
            run.of("result")[-1]["data"],
            {
                "placement": "mox1 mox3",
                "initial": "mox3",
                "primary": "example.com",
                "aliases": "www.example.com",
                "staging": "staging.example.com",
                "memory_mb": "8192",
                "allocation": "reserved",
                "network": "true",
                "continue_after_install": "true",
                "delete_source_iso_cache": "false",
            },
        )


if __name__ == "__main__":
    unittest.main()
