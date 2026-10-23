#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""The registry's optional production monitoring (ingress probe) settings."""

from __future__ import annotations

import json
from pathlib import Path
import subprocess
import tempfile
import unittest


# The registry rejects paths with symlinked components (macOS /var).
tempfile.tempdir = str(Path(tempfile.gettempdir()).resolve())

REGISTRY = Path(__file__).resolve().parent.parent / "host_runtime" / "cluster_registry.py"

PROBE = {
    "enabled": True,
    "path": "/api/v1/healthz",
    "expected_status": 200,
    "expected_text": None,
}


class RegistryMonitoringTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.state = Path(self.temporary.name) / "registry"
        self.run_registry(
            "init", "--network", "10.213.0.0/24", "--guest-gateway", "10.213.0.10",
            "--mox-ip-start", "10.213.0.11", "--mox-ip-end", "10.213.0.20",
            "--vm-slot-start", "10.213.0.110", "--max-production", "10",
            "--max-staging-per-production", "9",
        )
        result, _ = self.run_registry(
            "allocate-prod", "--vmid", "100", "--purpose", "myapp",
            "--primary-domain", "prod1.example.com", "--alias", "www.example.org",
            "--placement", "mox1,mox2", "--initial-node", "mox1",
        )
        self.prod = result["resource"]

    def run_registry(self, *arguments: str, expected: int = 0):
        completed = subprocess.run(
            [str(REGISTRY), "--state-dir", str(self.state), *arguments],
            text=True, capture_output=True, check=False,
        )
        self.assertEqual(
            completed.returncode, expected,
            msg=f"stdout:\n{completed.stdout}\nstderr:\n{completed.stderr}",
        )
        parsed = json.loads(completed.stdout) if completed.stdout.strip() else None
        return parsed, completed

    def set_probe(self, probe: dict, *extra: str, expected: int = 0):
        return self.run_registry(
            "update", "prod1", *extra, "--monitoring-json", json.dumps({"probe": probe}),
            expected=expected,
        )

    def test_new_records_have_no_field_and_probes_default_to_off(self) -> None:
        self.assertNotIn("monitoring", self.prod)
        rows, _ = self.run_registry("monitoring-probes")
        self.assertEqual(rows, [{
            "name": "prod1", "state": self.prod["state"], "primary": "prod1.example.com",
            "aliases": ["www.example.org"],
            "probe": {"enabled": False, "path": None, "expected_status": None,
                      "expected_text": None, "fqdns": []},
        }])

    def test_update_sets_probe_defaults_fqdns_and_bumps_revision(self) -> None:
        updated, _ = self.set_probe(PROBE, "--expected-revision", str(self.prod["revision"]))
        self.assertEqual(updated["revision"], self.prod["revision"] + 1)
        self.assertEqual(updated["monitoring"]["probe"], {**PROBE, "fqdns": ["prod1.example.com"]})
        self.set_probe({**PROBE, "fqdns": []})
        record, _ = self.run_registry("get", "prod1")
        self.assertEqual(record["monitoring"]["probe"]["fqdns"], ["prod1.example.com"])
        updated, _ = self.set_probe(
            {**PROBE, "expected_text": '"status":"ok"', "fqdns": ["WWW.example.org.", "prod1.example.com"]}
        )
        self.assertEqual(updated["monitoring"]["probe"]["fqdns"], ["www.example.org", "prod1.example.com"])
        rows, _ = self.run_registry("monitoring-probes")
        self.assertEqual(rows[0]["probe"], updated["monitoring"]["probe"])

    def test_expected_revision_conflict_changes_nothing(self) -> None:
        _, completed = self.set_probe(PROBE, "--expected-revision", "99", expected=1)
        self.assertIn("revision conflict", completed.stderr)
        record, _ = self.run_registry("get", "prod1")
        self.assertNotIn("monitoring", record)

    def test_invalid_probe_settings_are_rejected(self) -> None:
        cases = {
            "fqdns must be this resource's domains": {**PROBE, "fqdns": ["other.example.com"]},
            "path must start with /": {**PROBE, "path": "api/v1/healthz"},
            "without spaces": {**PROBE, "path": "/api v1"},
            "expected_status must be an integer": {**PROBE, "expected_status": 99},
            "expected_status must be": {**PROBE, "expected_status": True},
            "expected_text must be null": {**PROBE, "expected_text": "x" * 201},
            "expected_text must be": {**PROBE, "expected_text": "two\nlines"},
            "enabled must be true or false": {**PROBE, "enabled": "yes"},
            "missing fields: expected_text": {k: v for k, v in PROBE.items() if k != "expected_text"},
        }
        for message, probe in cases.items():
            with self.subTest(message=message):
                _, completed = self.set_probe(probe, expected=1)
                self.assertIn(message, completed.stderr)
        _, completed = self.run_registry(
            "update", "prod1", "--monitoring-json", "{not json", expected=1
        )
        self.assertIn("not valid JSON", completed.stderr)
        _, completed = self.run_registry(
            "update", "prod1", "--monitoring-json", json.dumps({**PROBE}), expected=1
        )
        self.assertIn('one "probe" field', completed.stderr)
        _, completed = self.set_probe({**PROBE, "unknown": 1}, expected=1)
        self.assertIn("unknown fields: unknown", completed.stderr)

    def test_staging_records_never_have_monitoring(self) -> None:
        self.run_registry(
            "allocate-staging", "--source", "prod1", "--vmid", "200",
            "--placement", "mox2", "--placement-limit", "5",
        )
        _, completed = self.run_registry(
            "update", "stage1prod1", "--monitoring-json", json.dumps({"probe": PROBE}), expected=1
        )
        self.assertIn("only production resources", completed.stderr)
        rows, _ = self.run_registry("monitoring-probes")
        self.assertEqual([row["name"] for row in rows], ["prod1"])

    def test_clear_monitoring_removes_the_field(self) -> None:
        self.set_probe(PROBE)
        cleared, _ = self.run_registry("update", "prod1", "--clear-monitoring")
        self.assertNotIn("monitoring", cleared)
        self.run_registry(
            "update", "prod1", "--clear-monitoring",
            "--monitoring-json", json.dumps({"probe": PROBE}), expected=2,
        )

    def test_domain_changes_prune_probe_fqdns(self) -> None:
        self.set_probe({**PROBE, "fqdns": ["prod1.example.com", "www.example.org"]})
        updated, _ = self.run_registry("update", "prod1", "--clear-aliases")
        self.assertEqual(updated["monitoring"]["probe"]["fqdns"], ["prod1.example.com"])
        updated, _ = self.run_registry("update", "prod1", "--primary-domain", "app.example.com")
        self.assertEqual(updated["monitoring"]["probe"]["fqdns"], ["app.example.com"])

    def test_existing_on_disk_record_without_the_field_still_validates(self) -> None:
        self.set_probe(PROBE)
        path = self.state / "resources" / "prod1.json"
        record = json.loads(path.read_text(encoding="utf-8"))
        record.pop("monitoring")
        path.write_text(json.dumps(record, sort_keys=True) + "\n", encoding="utf-8")
        listed, _ = self.run_registry("list", "--record-type", "resources")
        self.assertNotIn("monitoring", listed[0])
        rows, _ = self.run_registry("monitoring-probes")
        self.assertFalse(rows[0]["probe"]["enabled"])
        # A stored record with an invalid probe is rejected on read.
        record["monitoring"] = {"probe": {**PROBE, "fqdns": ["other.example.com"]}}
        path.write_text(json.dumps(record, sort_keys=True) + "\n", encoding="utf-8")
        self.run_registry("monitoring-probes", expected=1)


if __name__ == "__main__":
    unittest.main()
