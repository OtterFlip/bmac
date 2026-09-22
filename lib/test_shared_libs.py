#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Non-destructive tests for config.sh and cluster_registry.py."""

from __future__ import annotations

import argparse
import contextlib
import copy
import datetime as dt
import errno
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock
import uuid


# The config loader and the registry reject paths with symlinked components.
# macOS keeps TMPDIR under /var -> /private/var, so hand out resolved temp
# paths. A no-op where the temp directory is already a real path.
tempfile.tempdir = str(Path(tempfile.gettempdir()).resolve())

LIB_DIR = Path(__file__).resolve().parent
REGISTRY = LIB_DIR / "cluster_registry.py"
CONFIG = LIB_DIR / "config.sh"
MOX1_CONFIG = LIB_DIR.parent / "env" / "mox1.conf"


def checked_in_config_value(path: Path, key: str) -> str:
    prefix = f"{key}="
    for line in path.read_text(encoding="utf-8").splitlines():
        if line.startswith(prefix):
            value = line.removeprefix(prefix)
            if value:
                return value
    raise AssertionError(f"{key} is missing from {path}")

REGISTRY_SPEC = importlib.util.spec_from_file_location("cluster_registry", REGISTRY)
assert REGISTRY_SPEC and REGISTRY_SPEC.loader
cluster_registry = importlib.util.module_from_spec(REGISTRY_SPEC)
REGISTRY_SPEC.loader.exec_module(cluster_registry)


class SimulatedPmxcfs:
    """Reject local-filesystem operations that pmxcfs does not support."""

    def __init__(self, root: Path) -> None:
        self.root = root
        self.lock_dir = root / "priv" / "lock"
        self.forbidden_calls: list[str] = []
        self.regular_replacements = 0
        self.stale_break_requests = 0
        self._stack = contextlib.ExitStack()
        self._open = os.open
        self._chmod = os.chmod
        self._chown = os.chown
        self._replace = os.replace
        self._rename = os.rename
        self._utime = os.utime

    def _contains(self, value: os.PathLike[str] | str | bytes) -> bool:
        try:
            Path(os.fsdecode(value)).relative_to(self.root)
        except ValueError:
            return False
        return True

    def _reject_path_operation(self, operation: str, function):
        def wrapper(path, *args, **kwargs):
            if self._contains(path):
                self.forbidden_calls.append(operation)
                raise OSError(errno.EOPNOTSUPP, f"pmxcfs rejects {operation}")
            return function(path, *args, **kwargs)

        return wrapper

    def _open_wrapper(self, path, flags, mode=0o777, *, dir_fd=None):
        if self._contains(path) and flags & os.O_EXCL:
            self.forbidden_calls.append("O_EXCL")
            raise OSError(errno.EOPNOTSUPP, "pmxcfs rejects O_EXCL")
        if dir_fd is None:
            return self._open(path, flags, mode)
        return self._open(path, flags, mode, dir_fd=dir_fd)

    def _replace_wrapper(self, source, destination, *args, **kwargs):
        if self._contains(source) or self._contains(destination):
            if Path(source).is_dir():
                self.forbidden_calls.append("directory rename")
                raise OSError(
                    errno.ENOTSUP,
                    "pmxcfs rejects non-empty directory replacement",
                )
            self.regular_replacements += 1
        return self._replace(source, destination, *args, **kwargs)

    def _rename_wrapper(self, source, destination, *args, **kwargs):
        if self._contains(source) or self._contains(destination):
            if Path(source).is_dir():
                self.forbidden_calls.append("directory rename")
                raise OSError(
                    errno.ENOTSUP,
                    "pmxcfs rejects non-empty directory rename",
                )
        return self._rename(source, destination, *args, **kwargs)

    def _utime_wrapper(
        self,
        path,
        times=None,
        *,
        ns=None,
        dir_fd=None,
        follow_symlinks=True,
    ):
        candidate = Path(path)
        if (
            self._contains(path)
            and candidate.parent == self.lock_dir
            and times == (0, 0)
        ):
            self.stale_break_requests += 1
            if candidate.is_dir() and not any(candidate.iterdir()):
                candidate.rmdir()
            return None
        keywords = {
            "dir_fd": dir_fd,
            "follow_symlinks": follow_symlinks,
        }
        if ns is not None:
            return self._utime(path, ns=ns, **keywords)
        return self._utime(path, times, **keywords)

    def __enter__(self) -> "SimulatedPmxcfs":
        self._stack.enter_context(
            mock.patch.object(cluster_registry, "PMXCFS_ROOT", self.root)
        )
        self._stack.enter_context(
            mock.patch.object(cluster_registry, "PMXCFS_LOCK_DIR", self.lock_dir)
        )
        self._stack.enter_context(mock.patch.object(os, "open", self._open_wrapper))
        self._stack.enter_context(
            mock.patch.object(
                os,
                "chmod",
                self._reject_path_operation("chmod", self._chmod),
            )
        )
        self._stack.enter_context(
            mock.patch.object(
                os,
                "chown",
                self._reject_path_operation("chown", self._chown),
            )
        )
        self._stack.enter_context(
            mock.patch.object(
                os,
                "fchmod",
                side_effect=AssertionError("pmxcfs code must not call fchmod"),
            )
        )
        self._stack.enter_context(
            mock.patch.object(
                os,
                "fchown",
                side_effect=AssertionError("pmxcfs code must not call fchown"),
            )
        )
        self._stack.enter_context(
            mock.patch.object(
                os,
                "fsync",
                side_effect=AssertionError("pmxcfs code must not call fsync"),
            )
        )
        self._stack.enter_context(
            mock.patch.object(os, "replace", self._replace_wrapper)
        )
        self._stack.enter_context(
            mock.patch.object(os, "rename", self._rename_wrapper)
        )
        self._stack.enter_context(mock.patch.object(os, "utime", self._utime_wrapper))
        return self

    def __exit__(self, *exc_info) -> None:
        self._stack.__exit__(*exc_info)


class RegistryCliTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.state = Path(self.temporary.name) / "registry"
        self.run_registry(
            "init",
            "--network",
            "10.213.0.0/24",
            "--guest-gateway",
            "10.213.0.10/24",
            "--mox-ip-start",
            "10.213.0.11",
            "--mox-ip-end",
            "10.213.0.20",
            "--haproxy-ip-start",
            "10.213.0.21",
            "--haproxy-ip-end",
            "10.213.0.30",
            "--production-ip-start",
            "10.213.0.31",
            "--production-ip-end",
            "10.213.0.50",
            "--staging-ip-start",
            "10.213.0.51",
            "--staging-ip-end",
            "10.213.0.200",
        )

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def run_registry(
        self,
        *arguments: str,
        expected: int = 0,
        stdin: dict | None = None,
        global_arguments: tuple[str, ...] = (),
    ) -> tuple[dict | list | None, subprocess.CompletedProcess[str]]:
        completed = subprocess.run(
            [
                str(REGISTRY),
                "--state-dir",
                str(self.state),
                *global_arguments,
                *arguments,
            ],
            input=json.dumps(stdin) if stdin is not None else None,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        self.assertEqual(
            completed.returncode,
            expected,
            msg=f"stdout:\n{completed.stdout}\nstderr:\n{completed.stderr}",
        )
        parsed = json.loads(completed.stdout) if completed.stdout.strip() else None
        return parsed, completed

    def allocate_prod(
        self,
        *,
        vmid: int = 100,
        purpose: str = "myapp",
        domain: str = "myapp.com",
        allocation_id: str | None = None,
        staging_base: str | None = None,
    ) -> dict:
        arguments = [
            "allocate-prod",
            "--vmid",
            str(vmid),
            "--purpose",
            purpose,
            "--primary-domain",
            domain,
            "--placement",
            "mox1,mox2",
            "--initial-node",
            "mox1",
        ]
        if allocation_id:
            arguments.extend(["--allocation-id", allocation_id])
        if staging_base:
            arguments.extend(["--staging-dns-base", staging_base])
        result, _ = self.run_registry(*arguments)
        assert isinstance(result, dict)
        return result

    def begin_ingress(
        self,
        generation: str,
        *,
        bundle_sha256: str = "b" * 64,
        route_count: int = 1,
    ) -> dict:
        arguments = [
            "ingress-begin",
            "--generation",
            generation,
            "--bundle-sha256",
            bundle_sha256,
            "--route-count",
            str(route_count),
        ]
        for index in range(1, 11):
            arguments.extend(["--node", f"mox{index}"])
        result, _ = self.run_registry(*arguments)
        assert isinstance(result, dict)
        return result

    def test_deterministic_allocations_release_and_reuse(self) -> None:
        first = self.allocate_prod(staging_base="preview.myapp.com")
        second = self.allocate_prod(vmid=101, purpose="docs", domain="docs.example.com")
        self.assertEqual((first["resource"]["name"], first["resource"]["ip"]), ("prod1", "10.213.0.31"))
        self.assertEqual((second["resource"]["name"], second["resource"]["ip"]), ("prod2", "10.213.0.32"))

        stage_one, _ = self.run_registry(
            "allocate-staging",
            "--source",
            "prod1",
            "--vmid",
            "200",
            "--placement",
            "mox2",
            "--placement-limit",
            "5",
        )
        stage_two, _ = self.run_registry(
            "allocate-staging",
            "--source",
            "prod1",
            "--vmid",
            "201",
            "--placement",
            "mox2",
            "--placement-limit",
            "5",
        )
        self.assertEqual(
            (stage_one["resource"]["name"], stage_one["resource"]["ip"]),
            ("stage1prod1", "10.213.0.51"),
        )
        self.assertEqual(
            stage_one["resource"]["domains"]["primary"],
            "stage1prod1.myapp.com",
        )
        self.assertEqual(
            (stage_two["resource"]["name"], stage_two["resource"]["ip"]),
            ("stage2prod1", "10.213.0.52"),
        )
        _, capacity_error = self.run_registry(
            "allocate-staging",
            "--source",
            "prod1",
            "--vmid",
            "202",
            "--placement",
            "mox2",
            "--placement-limit",
            "2",
            expected=1,
        )
        self.assertIn("limit 2", capacity_error.stderr)

        self.run_registry("release", "stage1prod1")
        replacement, _ = self.run_registry(
            "allocate-staging",
            "--source",
            "prod1",
            "--vmid",
            "203",
            "--placement",
            "mox2",
            "--placement-limit",
            "5",
        )
        self.assertEqual(
            (replacement["resource"]["name"], replacement["resource"]["ip"]),
            ("stage1prod1", "10.213.0.51"),
        )

    def test_registry_uses_configured_network_and_allocation_ranges(self) -> None:
        original_state = self.state
        with tempfile.TemporaryDirectory() as temporary:
            self.state = Path(temporary) / "registry"
            try:
                self.run_registry(
                    "init",
                    "--network",
                    "10.77.0.0/24",
                    "--guest-gateway",
                    "10.77.0.5/24",
                    "--mox-ip-start",
                    "10.77.0.40",
                    "--mox-ip-end",
                    "10.77.0.49",
                    "--haproxy-ip-start",
                    "10.77.0.60",
                    "--haproxy-ip-end",
                    "10.77.0.69",
                    "--production-ip-start",
                    "10.77.0.80",
                    "--production-ip-end",
                    "10.77.0.99",
                    "--staging-ip-start",
                    "10.77.0.120",
                    "--staging-ip-end",
                    "10.77.0.199",
                )
                production = self.allocate_prod()
                self.assertEqual(production["resource"]["ip"], "10.77.0.80")
                staging, _ = self.run_registry(
                    "allocate-staging",
                    "--source",
                    "prod1",
                    "--vmid",
                    "200",
                    "--placement",
                    "mox2",
                    "--placement-limit",
                    "5",
                )
                assert isinstance(staging, dict)
                self.assertEqual(staging["resource"]["ip"], "10.77.0.120")
            finally:
                self.state = original_state

    def test_stage_number_is_lowest_free_within_required_host_limit(self) -> None:
        production, _ = self.run_registry(
            "allocate-prod",
            "--vmid",
            "100",
            "--purpose",
            "myapp",
            "--primary-domain",
            "myapp.com",
            "--placement",
            "mox1,mox2,mox3",
            "--initial-node",
            "mox1",
        )
        self.assertEqual(production["resource"]["name"], "prod1")
        self.run_registry(
            "update",
            "prod1",
            "--state",
            "provisioning",
            "--owner-node",
            "mox1",
        )
        self.run_registry("update", "prod1", "--state", "stopped")

        for vmid, node, expected_name in (
            ("200", "mox2", "stage1prod1"),
            ("201", "mox3", "stage2prod1"),
        ):
            allocated, _ = self.run_registry(
                "allocate-staging",
                "--source",
                "prod1",
                "--vmid",
                vmid,
                "--placement",
                node,
                "--placement-limit",
                "2",
            )
            self.assertEqual(allocated["resource"]["name"], expected_name)

        _, exhausted = self.run_registry(
            "allocate-staging",
            "--source",
            "prod1",
            "--vmid",
            "202",
            "--placement",
            "mox2",
            "--placement-limit",
            "2",
            expected=1,
        )
        self.assertIn("stage number range 1-2 is exhausted", exhausted.stderr)

        self.run_registry("release", "stage1prod1")
        replacement, _ = self.run_registry(
            "allocate-staging",
            "--source",
            "prod1",
            "--vmid",
            "203",
            "--placement",
            "mox2",
            "--placement-limit",
            "2",
        )
        self.assertEqual(replacement["resource"]["name"], "stage1prod1")
        self.assertEqual(
            replacement["resource"]["domains"]["primary"],
            "stage1prod1.myapp.com",
        )

    def test_production_orchestration_lease_and_install_sentinel(self) -> None:
        allocation = self.allocate_prod()
        resource = allocation["resource"]
        nonce_one = "a" * 32
        nonce_two = "b" * 32

        acquired, _ = self.run_registry(
            "orchestration-acquire",
            "prod1",
            "--nonce",
            nonce_one,
            "--owner",
            "test-runner-1",
            "--ttl-seconds",
            "600",
            "--initial-install-phase",
            "unstarted",
            "--final-network-enabled",
            "no",
            "--startup-sha256",
            "c" * 64,
            "--source-iso-sha256",
            "d" * 64,
            "--install-mode",
            "ubuntu-autoinstall",
        )
        self.assertEqual(acquired["resource_id"], resource["id"])
        self.assertEqual(acquired["install_phase"], "unstarted")
        self.assertFalse(acquired["final_network_enabled"])
        self.assertEqual(acquired["startup_sha256"], "c" * 64)
        self.assertEqual(acquired["source_iso_sha256"], "d" * 64)
        self.assertEqual(acquired["install_mode"], "ubuntu-autoinstall")
        self.assertEqual(acquired["lease"]["nonce"], nonce_one)
        self.run_registry("release", "prod1", "--force", expected=1)

        self.run_registry(
            "orchestration-acquire",
            "prod1",
            "--nonce",
            nonce_two,
            "--owner",
            "test-runner-2",
            "--ttl-seconds",
            "600",
            "--final-network-enabled",
            "no",
            "--startup-sha256",
            "c" * 64,
            "--source-iso-sha256",
            "d" * 64,
            "--install-mode",
            "ubuntu-autoinstall",
            expected=1,
        )
        media, _ = self.run_registry(
            "orchestration-set-phase",
            "prod1",
            "--nonce",
            nonce_one,
            "--phase",
            "media-attached",
        )
        self.assertEqual(media["install_phase"], "media-attached")
        self.run_registry(
            "orchestration-set-phase",
            "prod1",
            "--nonce",
            nonce_one,
            "--phase",
            "installer-started",
        )
        self.run_registry(
            "orchestration-set-phase",
            "prod1",
            "--nonce",
            nonce_one,
            "--phase",
            "media-attached",
            expected=1,
        )
        reset, _ = self.run_registry(
            "orchestration-set-phase",
            "prod1",
            "--nonce",
            nonce_one,
            "--phase",
            "media-attached",
            "--confirm-wipe",
            "WIPE",
        )
        self.assertEqual(reset["install_phase"], "media-attached")
        self.run_registry(
            "orchestration-set-phase",
            "prod1",
            "--nonce",
            nonce_one,
            "--phase",
            "installer-started",
        )
        self.run_registry(
            "orchestration-set-phase",
            "prod1",
            "--nonce",
            nonce_one,
            "--phase",
            "installed-confirmed",
        )
        verified, _ = self.run_registry(
            "orchestration-set-phase",
            "prod1",
            "--nonce",
            nonce_one,
            "--phase",
            "guest-verified",
        )
        self.assertEqual(verified["install_phase"], "guest-verified")
        finalized, _ = self.run_registry(
            "orchestration-set-phase",
            "prod1",
            "--nonce",
            nonce_one,
            "--phase",
            "network-finalized",
        )
        self.assertEqual(finalized["install_phase"], "network-finalized")

        released, _ = self.run_registry(
            "orchestration-release",
            "prod1",
            "--nonce",
            nonce_one,
        )
        self.assertIsNone(released["lease"])
        reacquired, _ = self.run_registry(
            "orchestration-acquire",
            "prod1",
            "--nonce",
            nonce_two,
            "--owner",
            "test-runner-2",
            "--ttl-seconds",
            "600",
            "--final-network-enabled",
            "no",
            "--startup-sha256",
            "c" * 64,
            "--source-iso-sha256",
            "d" * 64,
            "--install-mode",
            "ubuntu-autoinstall",
        )
        self.assertEqual(reacquired["install_phase"], "network-finalized")
        self.assertEqual(reacquired["lease"]["nonce"], nonce_two)
        self.run_registry(
            "orchestration-acquire",
            "prod1",
            "--nonce",
            nonce_two,
            "--owner",
            "test-runner-2",
            "--ttl-seconds",
            "600",
            "--final-network-enabled",
            "no",
            "--startup-sha256",
            "c" * 64,
            "--source-iso-sha256",
            "e" * 64,
            "--install-mode",
            "manual",
            expected=1,
        )

    def test_allocation_is_idempotent_and_uniqueness_is_enforced(self) -> None:
        first = self.allocate_prod(allocation_id="request-1")
        retry = self.allocate_prod(allocation_id="request-1")
        self.assertTrue(first["created"])
        self.assertFalse(retry["created"])
        self.assertEqual(first["resource"]["id"], retry["resource"]["id"])

        _, duplicate_purpose = self.run_registry(
            "allocate-prod",
            "--vmid",
            "101",
            "--purpose",
            "myapp",
            "--primary-domain",
            "other.example.com",
            "--placement",
            "mox1",
            expected=1,
        )
        self.assertIn("duplicate production purpose", duplicate_purpose.stderr)
        _, reserved_vmid = self.run_registry(
            "allocate-prod",
            "--vmid",
            "9111",
            "--purpose",
            "other",
            "--primary-domain",
            "other.example.com",
            "--placement",
            "mox1",
            expected=1,
        )
        self.assertIn("reserved for fixed HAProxy", reserved_vmid.stderr)

        self.allocate_prod(vmid=102, purpose="docs", domain="docs.example.com")
        _, duplicate_domain = self.run_registry(
            "update",
            "prod2",
            "--primary-domain",
            "myapp.com",
            expected=1,
        )
        self.assertIn("duplicate domain", duplicate_domain.stderr)

        self.run_registry(
            "update",
            "prod1",
            "--state",
            "provisioning",
            "--owner-node",
            "mox1",
        )
        self.run_registry("update", "prod1", "--state", "stopped")
        stage = [
            "allocate-staging",
            "--source",
            "prod1",
            "--vmid",
            "200",
            "--placement",
            "mox2",
            "--placement-limit",
            "5",
            "--allocation-id",
            "stage-request",
        ]
        first_stage, _ = self.run_registry(*stage)
        retry_stage, _ = self.run_registry(*stage)
        self.assertEqual(first_stage["resource"]["id"], retry_stage["resource"]["id"])

    def test_transitions_routes_and_reconcile(self) -> None:
        self.allocate_prod()
        _, invalid = self.run_registry(
            "update", "prod1", "--routes-enabled", expected=1
        )
        self.assertIn("routes may only be enabled", invalid.stderr)
        self.run_registry(
            "update",
            "prod1",
            "--state",
            "provisioning",
            "--owner-node",
            "mox1",
            "--volume-id",
            "local-zfs:vm-100-disk-0",
        )
        self.run_registry(
            "update",
            "prod1",
            "--state",
            "stopped",
            "--ha-nodes",
            "mox1,mox2",
            "--replication-targets",
            "mox2",
            "--routes-enabled",
        )
        routes, _ = self.run_registry("list-routes")
        self.assertEqual([route["domain"] for route in routes], ["myapp.com"])

        observation = {
            "schema_version": 1,
            "nodes": [
                {"name": "mox1", "online": True},
                {"name": "mox2", "online": True},
            ],
            "vms": [
                {
                    "vmid": 100,
                    "name": "prod1",
                    "node": "mox1",
                    "status": "stopped",
                    "ip": "10.213.0.31",
                    "mac": None,
                    "tags": ["app-ha-production"],
                    "volume_ids": ["local-zfs:vm-100-disk-0"],
                }
            ],
            "ha_resources": [{"vmid": 100, "nodes": ["mox1", "mox2"]}],
            "replication_jobs": [{"vmid": 100, "target": "mox2", "healthy": True}],
            "volumes": [{"id": "local-zfs:vm-100-disk-0"}],
            "routes": [
                {
                    "domain": "myapp.com",
                    "resource": "prod1",
                    "ip": "10.213.0.31",
                }
            ],
            "cleanup_completed": [],
        }
        report, _ = self.run_registry("reconcile", "--observed", "-", stdin=observation)
        self.assertTrue(report["ok"])
        self.assertEqual(report["issues"], [])

    def test_snapshot_guid_mismatch_and_stale_reservation_reconcile(self) -> None:
        self.allocate_prod()
        self.run_registry(
            "update",
            "prod1",
            "--snapshot-name",
            "staging-base-1",
            "--snapshot-owner-node",
            "mox1",
            "--snapshot-guid",
            "mox1=100",
            "--snapshot-guid",
            "mox2=100",
        )
        observation = {
            "schema_version": 1,
            "snapshots": [
                {
                    "resource": "prod1",
                    "snapshot": "staging-base-1",
                    "node": "mox1",
                    "guid": "100",
                },
                {
                    "resource": "prod1",
                    "snapshot": "staging-base-1",
                    "node": "mox2",
                    "guid": "101",
                },
            ],
        }
        report, _ = self.run_registry("reconcile", "--observed", "-", stdin=observation)
        codes = {issue["code"] for issue in report["issues"]}
        self.assertIn("snapshot-guid-mismatch", codes)
        self.assertIn("snapshot-recorded-guid-mismatch", codes)

        resource_path = self.state / "resources" / "prod1.json"
        resource = json.loads(resource_path.read_text(encoding="utf-8"))
        resource["created_at"] = (
            dt.datetime.now(dt.timezone.utc) - dt.timedelta(hours=2)
        ).replace(microsecond=0).isoformat()
        resource_path.write_text(
            json.dumps(resource, sort_keys=True, separators=(",", ":")) + "\n",
            encoding="utf-8",
        )
        resource_path.chmod(0o600)
        stale_observation = {"schema_version": 1, "vms": []}
        report, _ = self.run_registry(
            "reconcile", "--observed", "-", "--apply", stdin=stale_observation
        )
        self.assertIn("stale-reservation", {issue["code"] for issue in report["issues"]})
        updated, _ = self.run_registry("get", "prod1")
        self.assertEqual(updated["state"], "failed")

    def test_staging_snapshot_dependency_is_complete_unique_and_idempotent(self) -> None:
        self.allocate_prod()
        self.run_registry(
            "update",
            "prod1",
            "--state",
            "provisioning",
            "--owner-node",
            "mox1",
            "--volume-id",
            "local-zfs:vm-100-disk-0",
        )
        self.run_registry(
            "update",
            "prod1",
            "--state",
            "stopped",
            "--ha-nodes",
            "mox1,mox2",
            "--replication-targets",
            "mox2",
        )
        stage, _ = self.run_registry(
            "allocate-staging",
            "--source",
            "prod1",
            "--vmid",
            "200",
            "--placement",
            "mox2",
            "--placement-limit",
            "5",
        )
        revision = stage["resource"]["revision"]
        self.run_registry("update", "stage1prod1", "--state", "snapshotting")
        intent_args = (
            "record-staging-snapshot-intent",
            "stage1prod1",
            "--snapshot-name",
            "stg-base-stage1prod1-260914T220000Z",
            "--snapshot-owner-node",
            "mox1",
            "--source-volume-id",
            "local-zfs:vm-100-disk-0",
            "--snapshotted-volume-id",
            "local-zfs:vm-100-disk-0",
            "--snapshotted-volume-id",
            "local-zfs:vm-100-disk-1",
        )
        intent, _ = self.run_registry(*intent_args)
        self.assertFalse(intent["proxmox"]["snapshot"]["verified"])
        snapshot_args = (
            "record-staging-snapshot",
            "stage1prod1",
            "--snapshot-name",
            "stg-base-stage1prod1-260914T220000Z",
            "--snapshot-owner-node",
            "mox1",
            "--source-volume-id",
            "local-zfs:vm-100-disk-0",
            "--snapshot-guid",
            "mox1=123456",
            "--snapshot-guid",
            "mox2=123456",
            "--snapshot-volume-guid",
            "local-zfs:vm-100-disk-0,mox1=123456",
            "--snapshot-volume-guid",
            "local-zfs:vm-100-disk-0,mox2=123456",
            "--snapshot-volume-guid",
            "local-zfs:vm-100-disk-1,mox1=789012",
            "--snapshot-volume-guid",
            "local-zfs:vm-100-disk-1,mox2=789012",
        )
        recorded, _ = self.run_registry(*snapshot_args)
        metadata = recorded["proxmox"]["snapshot"]
        self.assertEqual(metadata["source_resource"], "prod1")
        self.assertEqual(metadata["dependent_resources"], ["stage1prod1"])
        self.assertEqual(metadata["refcount"], 1)
        self.assertTrue(metadata["verified"])
        self.assertEqual(metadata["guids"], {"mox1": "123456", "mox2": "123456"})

        retry, _ = self.run_registry(*snapshot_args)
        self.assertEqual(retry["revision"], recorded["revision"])
        self.assertGreater(recorded["revision"], revision)
        _, pinned_source = self.run_registry(
            "update", "prod1", "--clear-volume", expected=1
        )
        self.assertIn(
            "staging snapshot dependencies exist", pinned_source.stderr
        )

        self.run_registry(
            "allocate-staging",
            "--source",
            "prod1",
            "--vmid",
            "201",
            "--placement",
            "mox2",
            "--placement-limit",
            "5",
        )
        self.run_registry("update", "stage2prod1", "--state", "snapshotting")
        _, duplicate = self.run_registry(
            "record-staging-snapshot-intent",
            "stage2prod1",
            "--snapshot-name",
            "stg-base-stage1prod1-260914T220000Z",
            "--snapshot-owner-node",
            "mox1",
            "--source-volume-id",
            "local-zfs:vm-100-disk-0",
            "--snapshotted-volume-id",
            "local-zfs:vm-100-disk-0",
            expected=1,
        )
        self.assertIn("already belongs to stage1prod1", duplicate.stderr)

        self.run_registry(
            "record-staging-snapshot-intent",
            "stage2prod1",
            "--snapshot-name",
            "stg-base-stage2prod1-260914T220001Z",
            "--snapshot-owner-node",
            "mox1",
            "--source-volume-id",
            "local-zfs:vm-100-disk-0",
            "--snapshotted-volume-id",
            "local-zfs:vm-100-disk-0",
        )
        _, incomplete = self.run_registry(
            "record-staging-snapshot",
            "stage2prod1",
            "--snapshot-name",
            "stg-base-stage2prod1-260914T220001Z",
            "--snapshot-owner-node",
            "mox1",
            "--source-volume-id",
            "local-zfs:vm-100-disk-0",
            "--snapshot-guid",
            "mox1=123456",
            "--snapshot-volume-guid",
            "local-zfs:vm-100-disk-0,mox1=123456",
            expected=1,
        )
        self.assertIn("must cover production placement", incomplete.stderr)

        _, mismatched = self.run_registry(
            "record-staging-snapshot",
            "stage2prod1",
            "--snapshot-name",
            "stg-base-stage2prod1-260914T220001Z",
            "--snapshot-owner-node",
            "mox1",
            "--source-volume-id",
            "local-zfs:vm-100-disk-0",
            "--snapshot-guid",
            "mox1=123456",
            "--snapshot-guid",
            "mox2=654321",
            "--snapshot-volume-guid",
            "local-zfs:vm-100-disk-0,mox1=123456",
            "--snapshot-volume-guid",
            "local-zfs:vm-100-disk-0,mox2=654321",
            expected=1,
        )
        self.assertIn("differs between nodes", mismatched.stderr)

    def test_deferred_cleanup_is_idempotent_and_blocks_reuse(self) -> None:
        allocated = self.allocate_prod()
        arguments = [
            "defer-cleanup",
            "--resource",
            "prod1",
            "--node",
            "mox2",
            "--action",
            "delete-snapshot",
            "--target",
            "vm-100@staging-base-1",
            "--reason",
            "mox2 is offline",
        ]
        first, _ = self.run_registry(*arguments)
        retry, _ = self.run_registry(*arguments)
        self.assertEqual(first["cleanup"]["id"], retry["cleanup"]["id"])
        _, blocked = self.run_registry("release", "prod1", expected=1)
        self.assertIn("deferred cleanup is pending", blocked.stderr)

        observation = {
            "schema_version": 1,
            "cleanup_completed": [first["cleanup"]["id"]],
        }
        self.run_registry(
            "reconcile", "--observed", "-", "--apply", stdin=observation
        )
        self.run_registry("release", "prod1")
        replacement = self.allocate_prod(
            vmid=101, purpose="replacement", domain="replacement.example.com"
        )
        self.assertEqual(replacement["resource"]["name"], "prod1")
        self.assertNotEqual(
            allocated["resource"]["id"], replacement["resource"]["id"]
        )

    def test_staging_cleanup_finalization_requires_every_snapshot_copy(self) -> None:
        self.allocate_prod()
        self.run_registry(
            "update",
            "prod1",
            "--state",
            "provisioning",
            "--owner-node",
            "mox1",
            "--volume-id",
            "local-zfs:vm-100-disk-0",
        )
        self.run_registry(
            "update",
            "prod1",
            "--state",
            "stopped",
            "--ha-nodes",
            "mox1,mox2",
            "--replication-targets",
            "mox2",
        )
        stage, _ = self.run_registry(
            "allocate-staging",
            "--source",
            "prod1",
            "--vmid",
            "200",
            "--placement",
            "mox2",
            "--placement-limit",
            "5",
        )
        self.run_registry("update", "stage1prod1", "--state", "snapshotting")
        self.run_registry(
            "record-staging-snapshot-intent",
            "stage1prod1",
            "--snapshot-name",
            "stg-base-stage1prod1-260914T220000Z",
            "--snapshot-owner-node",
            "mox1",
            "--source-volume-id",
            "local-zfs:vm-100-disk-0",
            "--snapshotted-volume-id",
            "local-zfs:vm-100-disk-0",
        )
        self.run_registry(
            "record-staging-snapshot",
            "stage1prod1",
            "--snapshot-name",
            "stg-base-stage1prod1-260914T220000Z",
            "--snapshot-owner-node",
            "mox1",
            "--source-volume-id",
            "local-zfs:vm-100-disk-0",
            "--snapshot-guid",
            "mox1=123456",
            "--snapshot-guid",
            "mox2=123456",
            "--snapshot-volume-guid",
            "local-zfs:vm-100-disk-0,mox1=123456",
            "--snapshot-volume-guid",
            "local-zfs:vm-100-disk-0,mox2=123456",
        )
        self.run_registry(
            "update", "stage1prod1", "--state", "cleanup_pending"
        )
        cleanup_ids = []
        for node in ("mox1", "mox2"):
            queued, _ = self.run_registry(
                "defer-cleanup",
                "--resource",
                "stage1prod1",
                "--node",
                node,
                "--action",
                "delete-snapshot",
                "--target",
                "vm-100@stg-base-stage1prod1-260914T220000Z",
                "--reason",
                "source copy cleanup",
            )
            cleanup_ids.append(queued["cleanup"]["id"])
        for node in ("mox1", "mox2"):
            route, _ = self.run_registry(
                "defer-cleanup",
                "--resource",
                "stage1prod1",
                "--node",
                node,
                "--action",
                "remove-route",
                "--target",
                "stage1prod1",
                "--reason",
                "route cleanup",
            )
            cleanup_ids.append(route["cleanup"]["id"])

        _, pending = self.run_registry(
            "finalize-staging-cleanup",
            "stage1prod1",
            "--resource-id",
            stage["resource"]["id"],
            expected=1,
        )
        self.assertIn("deferred cleanup remains pending", pending.stderr)
        self.run_registry(
            "reconcile",
            "--observed",
            "-",
            "--apply",
            stdin={
                "schema_version": 1,
                "cleanup_completed": cleanup_ids,
            },
        )
        finalized, _ = self.run_registry(
            "finalize-staging-cleanup",
            "stage1prod1",
            "--resource-id",
            stage["resource"]["id"],
        )
        self.assertTrue(finalized["released"])
        _, missing = self.run_registry("get", "stage1prod1", expected=1)
        self.assertIn("does not exist", missing.stderr)

        _, reserved_snapshot = self.run_registry(
            "defer-cleanup",
            "--resource",
            "prod1",
            "--node",
            "mox1",
            "--action",
            "delete-snapshot",
            "--target",
            "vm-100@__replicate_100-0_1",
            "--reason",
            "must be rejected",
            expected=1,
        )
        self.assertIn("reserved __replicate_ prefix", reserved_snapshot.stderr)

    def test_atomic_lock_times_out_and_can_explicitly_reap_stale_lock(self) -> None:
        lock = self.state / ".allocator.lock"
        lock.mkdir(mode=0o700)
        owner = self.state / ".allocator-lock-owner.json"
        owner.write_text(
            json.dumps(
                {
                    "schema_version": 1,
                    "record_type": "allocator-lock",
                    "nonce": "other",
                    "node": "mox1",
                    "pid": 1,
                    "created_at": "2000-01-01T00:00:00+00:00",
                }
            ),
            encoding="utf-8",
        )
        old = 946684800
        os.utime(lock, (old, old))
        _, timed_out = self.run_registry(
            "allocate-prod",
            "--vmid",
            "100",
            "--purpose",
            "myapp",
            "--primary-domain",
            "myapp.com",
            "--placement",
            "mox1",
            expected=1,
            global_arguments=("--lock-timeout", "0"),
        )
        self.assertIn("timed out waiting for allocator lock", timed_out.stderr)
        result, _ = self.run_registry(
            "allocate-prod",
            "--vmid",
            "100",
            "--purpose",
            "myapp",
            "--primary-domain",
            "myapp.com",
            "--placement",
            "mox1",
            global_arguments=(
                "--lock-timeout",
                "0",
                "--stale-lock-seconds",
                "1",
                "--break-stale-lock",
            ),
        )
        self.assertEqual(result["resource"]["name"], "prod1")

    def test_simulated_pmxcfs_uses_supported_atomic_file_and_lock_operations(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            pmxcfs_root = Path(temporary) / "etc" / "pve"
            state = pmxcfs_root / "priv" / "app-ha"
            pmxcfs_root.mkdir(parents=True)
            simulated = SimulatedPmxcfs(pmxcfs_root)
            with simulated:
                registry = cluster_registry.Registry(
                    state,
                    lock_timeout=0,
                    stale_lock_seconds=1,
                    break_stale_lock=False,
                )
                registry.ensure_layout()
                lock = registry.lock()
                with lock:
                    self.assertEqual(lock.path.parent, pmxcfs_root / "priv" / "lock")
                    self.assertEqual(list(lock.path.iterdir()), [])
                    self.assertEqual(lock.owner_path.parent, state)
                    self.assertFalse(lock.owner_path.is_relative_to(lock.path))
                    cluster_registry.atomic_write_json(
                        state / "probe.json", {"generation": 1}
                    )
                    cluster_registry.atomic_write_json(
                        state / "probe.json", {"generation": 2}
                    )
                    self.assertEqual(list(lock.path.iterdir()), [])
                self.assertFalse(lock.path.exists())
                self.assertFalse(lock.owner_path.exists())
                self.assertEqual(
                    cluster_registry.read_json_file(state / "probe.json", "probe"),
                    {"generation": 2},
                )
            self.assertEqual(simulated.forbidden_calls, [])
            self.assertGreaterEqual(simulated.regular_replacements, 3)

    def test_simulated_pmxcfs_stale_break_uses_safe_unlock_request(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            pmxcfs_root = Path(temporary) / "etc" / "pve"
            state = pmxcfs_root / "priv" / "app-ha"
            pmxcfs_root.mkdir(parents=True)
            simulated = SimulatedPmxcfs(pmxcfs_root)
            with simulated:
                registry = cluster_registry.Registry(
                    state,
                    lock_timeout=0,
                    stale_lock_seconds=1,
                    break_stale_lock=True,
                )
                registry.ensure_layout()
                stale = registry.lock()
                stale.path.parent.mkdir(parents=True, exist_ok=True)
                stale.path.mkdir()
                cluster_registry.atomic_write_json(
                    stale.owner_path,
                    {
                        "schema_version": 1,
                        "record_type": "allocator-lock",
                        "nonce": "stale-owner",
                        "node": "mox1",
                        "pid": 1,
                        "created_at": "2000-01-01T00:00:00+00:00",
                    },
                )
                os.utime(stale.path, (946684800, 946684800))
                with registry.lock() as acquired:
                    self.assertEqual(list(acquired.path.iterdir()), [])
                self.assertFalse(stale.path.exists())
            self.assertEqual(simulated.stale_break_requests, 1)
            self.assertEqual(simulated.forbidden_calls, [])

    def test_registry_enforces_renderer_route_limit(self) -> None:
        resource = self.allocate_prod()["resource"]
        resource["state"] = "stopped"
        resource["routes_enabled"] = True
        resource["domains"]["aliases"] = [
            f"route-{index}.example.com"
            for index in range(cluster_registry.MAX_ROUTES - 1)
        ]
        cluster_registry.validate_registry_invariants([resource])
        self.assertEqual(
            len(cluster_registry.build_routes([resource])),
            cluster_registry.MAX_ROUTES,
        )

        over_limit = copy.deepcopy(resource)
        over_limit["domains"]["aliases"].append("one-too-many.example.com")
        with self.assertRaisesRegex(
            cluster_registry.RegistryError,
            "renderer limit",
        ):
            cluster_registry.validate_registry_invariants([over_limit])
        with self.assertRaisesRegex(
            cluster_registry.RegistryError,
            "renderer limit",
        ):
            cluster_registry.build_routes([over_limit])

    def test_ingress_state_recovers_coordinator_crash_and_partial_commit(
        self,
    ) -> None:
        generation = "a" * 64
        first = self.begin_ingress(generation)
        self.assertEqual(first["desired"]["generation"], generation)
        self.run_registry(
            "ingress-mark",
            "--generation",
            generation,
            "--node",
            "mox1",
            "--state",
            "applied",
        )
        self.run_registry(
            "ingress-mark",
            "--generation",
            generation,
            "--node",
            "mox2",
            "--state",
            "staged",
        )

        # A new coordinator repeats ingress-begin after the prior process
        # disappears. Durable applied/staged states must survive that crash.
        recovered = self.begin_ingress(generation)
        states = {record["node"]: record for record in recovered["nodes"]}
        self.assertEqual(states["mox1"]["state"], "applied")
        self.assertEqual(states["mox1"]["applied_generation"], generation)
        self.assertEqual(states["mox2"]["state"], "staged")
        self.assertIsNone(states["mox2"]["applied_generation"])
        completed, _ = self.run_registry(
            "ingress-mark",
            "--generation",
            generation,
            "--node",
            "mox2",
            "--state",
            "applied",
        )
        self.assertEqual(completed["applied_generation"], generation)

    def test_offline_node_remains_pending_until_new_generation_applies(
        self,
    ) -> None:
        old_generation = "1" * 64
        new_generation = "2" * 64
        self.begin_ingress(old_generation)
        for node in ("mox1", "mox2"):
            self.run_registry(
                "ingress-mark",
                "--generation",
                old_generation,
                "--node",
                node,
                "--state",
                "applied",
            )

        changed = self.begin_ingress(
            new_generation, bundle_sha256="c" * 64, route_count=2
        )
        states = {record["node"]: record for record in changed["nodes"]}
        self.assertEqual(states["mox1"]["state"], "pending")
        self.assertEqual(states["mox2"]["state"], "pending")
        self.assertEqual(states["mox2"]["applied_generation"], old_generation)
        self.run_registry(
            "ingress-mark",
            "--generation",
            new_generation,
            "--node",
            "mox1",
            "--state",
            "applied",
        )
        self.run_registry(
            "ingress-mark",
            "--generation",
            new_generation,
            "--node",
            "mox2",
            "--state",
            "reject-only",
        )
        status, _ = self.run_registry("ingress-status")
        by_node = {record["node"]: record for record in status["nodes"]}
        self.assertEqual(by_node["mox1"]["state"], "applied")
        self.assertEqual(by_node["mox2"]["state"], "reject-only")
        self.assertEqual(by_node["mox2"]["applied_generation"], old_generation)

    def test_ingress_generation_metadata_retention_is_bounded(self) -> None:
        registry = cluster_registry.Registry(
            self.state,
            lock_timeout=1,
            stale_lock_seconds=1,
            break_stale_lock=False,
        )
        nodes = [f"mox{index}" for index in range(1, 11)]
        with mock.patch.object(cluster_registry, "MAX_INGRESS_GENERATIONS", 2):
            for index in range(4):
                cluster_registry.command_ingress_begin(
                    registry,
                    argparse.Namespace(
                        generation=f"{index + 1:x}" * 64,
                        bundle_sha256=f"{index + 5:x}" * 64,
                        route_count=index,
                        node=nodes,
                    ),
                )
        retained = sorted(registry.ingress_generations_dir.glob("*.json"))
        self.assertEqual(len(retained), 2)
        desired = registry.ingress_desired()
        assert desired is not None
        self.assertTrue(
            registry.ingress_generation_path(desired["generation"]).exists()
        )

    def test_release_bounds_history_and_completed_cleanup_retention(self) -> None:
        resource = self.allocate_prod()["resource"]
        registry = cluster_registry.Registry(
            self.state,
            lock_timeout=1,
            stale_lock_seconds=1,
            break_stale_lock=False,
        )
        for index in range(3):
            cluster_registry.atomic_write_json(
                registry.history_dir / f"000-old-{index}.json",
                {"record_type": "test-history", "sequence": index},
            )
            now = f"2026-09-14T00:00:0{index}+00:00"
            cluster_registry.atomic_write_json(
                registry.cleanup_path(f"cleanup-old-{index}"),
                {
                    "schema_version": 1,
                    "record_type": "deferred-cleanup",
                    "id": f"cleanup-old-{index}",
                    "resource": "prod9",
                    "resource_id": str(uuid.uuid4()),
                    "node": "mox1",
                    "action": "remove-route",
                    "target": "prod9",
                    "reason": "retention test",
                    "state": "completed",
                    "attempts": 1,
                    "created_at": now,
                    "updated_at": now,
                    "revision": 2,
                },
            )

        with (
            mock.patch.object(cluster_registry, "MAX_HISTORY_RECORDS", 2),
            mock.patch.object(
                cluster_registry, "MAX_COMPLETED_CLEANUP_RECORDS", 2
            ),
        ):
            result = cluster_registry.command_release(
                registry,
                argparse.Namespace(name=resource["name"], force=False),
            )
        self.assertEqual(result["released"], "prod1")
        self.assertEqual(len(list(registry.history_dir.glob("*.json"))), 2)
        self.assertEqual(len(registry.cleanup_records()), 2)

    def test_active_cleanup_metadata_has_a_hard_record_bound(self) -> None:
        resource = self.allocate_prod()["resource"]
        registry = cluster_registry.Registry(
            self.state,
            lock_timeout=1,
            stale_lock_seconds=1,
            break_stale_lock=False,
        )
        now = "2026-09-14T00:00:00+00:00"
        cluster_registry.atomic_write_json(
            registry.cleanup_path("cleanup-existing"),
            {
                "schema_version": 1,
                "record_type": "deferred-cleanup",
                "id": "cleanup-existing",
                "resource": resource["name"],
                "resource_id": resource["id"],
                "node": "mox1",
                "action": "remove-route",
                "target": resource["name"],
                "reason": "retained active completion",
                "state": "completed",
                "attempts": 1,
                "created_at": now,
                "updated_at": now,
                "revision": 2,
            },
        )
        with (
            mock.patch.object(cluster_registry, "MAX_CLEANUP_RECORDS", 1),
            mock.patch.object(
                cluster_registry, "MAX_COMPLETED_CLEANUP_RECORDS", 0
            ),
            self.assertRaisesRegex(
                cluster_registry.RegistryError,
                "1-record limit",
            ),
        ):
            cluster_registry.command_defer_cleanup(
                registry,
                argparse.Namespace(
                    resource=resource["name"],
                    node="mox1",
                    action="destroy-vm",
                    target=f"vm:{resource['vmid']}",
                    reason="hard-bound test",
                ),
            )
        self.assertEqual(len(registry.cleanup_records()), 1)

    def test_live_reconcile_uses_safe_pvesh_projection(self) -> None:
        self.allocate_prod()
        fake_pvesh = Path(self.temporary.name) / "pvesh"
        fake_pvesh.write_text(
            """#!/usr/bin/env python3
import json
import sys

path = sys.argv[2]
if path == "/nodes":
    value = [
        {"node": "mox1", "status": "online"},
        {"node": "mox2", "status": "online"},
    ]
elif path == "/cluster/resources":
    value = [
        {
            "vmid": 100,
            "name": "prod1",
            "node": "mox1",
            "status": "stopped",
            "type": "qemu",
        }
    ]
elif path == "/nodes/mox1/qemu/100/config":
    value = {
        "name": "prod1",
        "tags": "app-ha-production",
        "scsi0": "local-zfs:vm-100-disk-0,size=64G",
    }
else:
    raise SystemExit(2)
print(json.dumps(value))
""",
            encoding="utf-8",
        )
        fake_pvesh.chmod(0o755)
        report, _ = self.run_registry(
            "reconcile", "--live", "--pvesh", str(fake_pvesh)
        )
        self.assertTrue(report["ok"])
        self.assertEqual(report["checked_sections"], ["nodes", "vms", "volumes"])


class ConfigLoaderTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.env_dir = Path(self.temporary.name) / "env"
        self.env_dir.mkdir()
        self.cluster = self.env_dir / "cluster.conf"
        self.mox = self.env_dir / "mox10.conf"
        self.secrets = self.env_dir / "secrets.env"
        self.cluster.write_text(
            "\n".join(
                [
                    "MAX_MOX_HOSTS=10",
                    "PROXMOX_INTERNAL_DOMAIN=pve.internal",
                    "PRIVATE_SUBNET_CIDR=10.213.0.0/24",
                    "PROXMOX_MIGRATION_NETWORK=10.213.0.240/28",
                    "DATACENTER_PRIVATE_VLAN_GATEWAY=10.213.0.1",
                    "GUEST_EGRESS_VIP=10.213.0.10/24",
                    "MOX_IP_START=10.213.0.11",
                    "MOX_IP_END=10.213.0.20",
                    "MOX_REPLICATION_IP_START=10.213.0.241",
                    "MOX_REPLICATION_IP_END=10.213.0.250",
                    "HAPROXY_IP_START=10.213.0.21",
                    "HAPROXY_IP_END=10.213.0.30",
                    "PRODUCTION_IP_START=10.213.0.31",
                    "PRODUCTION_IP_END=10.213.0.50",
                    "STAGING_IP_START=10.213.0.51",
                    "STAGING_IP_END=10.213.0.200",
                    "PRODUCTION_VM_TAG=app-ha-production",
                    "STAGING_VM_TAG=app-ha-staging",
                    "EVICTABLE_VM_TAG=evictable",
                    "",
                ]
            ),
            encoding="utf-8",
        )
        self.mox.write_text(
            "\n".join(
                [
                    "IDRAC_IP=192.0.2.63",
                    f'PROXMOX_IP={checked_in_config_value(MOX1_CONFIG, "PROXMOX_IP")}',
                    f'PROXMOX_GATEWAY={checked_in_config_value(MOX1_CONFIG, "PROXMOX_GATEWAY")}',
                    "PROXMOX_PREFIX=24",
                    "PROXMOX_PUBLIC_MAC=02:00:00:00:00:10",
                    "PROXMOX_SECONDARY_MAC=02:00:00:00:10:10",
                    "NVME_MIRROR_1_SERIAL_1=disk-a",
                    "NVME_MIRROR_1_SERIAL_2=disk-b",
                    "MAX_PROD_VM_COUNT_ON_THIS_HOST=1",
                    "MAX_STAGING_VM_COUNT_ON_THIS_HOST=5",
                    "",
                ]
            ),
            encoding="utf-8",
        )
        self.secrets.write_text(
            "IDRAC_USER=test-user\n"
            "IDRAC_PASSWORD=must-not-appear\n"
            "PROXMOX_LUKS_PASSWORD=luks-must-not-appear\n",
            encoding="utf-8",
        )
        self.cluster.chmod(0o644)
        self.mox.chmod(0o640)
        self.secrets.chmod(0o600)

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def run_config(
        self,
        *arguments: str,
        expected: int = 0,
        environment_updates: dict[str, str] | None = None,
    ) -> subprocess.CompletedProcess[str]:
        environment = os.environ.copy()
        environment.update(
            {
                "APP_HA_CONFIG_TEST_MODE": "1",
                "APP_HA_ENV_DIR": str(self.env_dir),
            }
        )
        environment.update(environment_updates or {})
        completed = subprocess.run(
            [str(CONFIG), *arguments],
            env=environment,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        self.assertEqual(
            completed.returncode,
            expected,
            msg=f"stdout:\n{completed.stdout}\nstderr:\n{completed.stderr}",
        )
        self.assertNotIn("must-not-appear", completed.stdout + completed.stderr)
        return completed

    def test_layered_loading_formulas_and_secret_redaction(self) -> None:
        completed = self.run_config(
            "--check", "--host", "mox10", "--require-secrets"
        )
        self.assertIn("Derived mox FQDN: mox10.pve.internal", completed.stdout)
        self.assertIn("Derived mox address: 10.213.0.20/24", completed.stdout)
        self.assertIn(
            "Derived migration/replication address: 10.213.0.250/28",
            completed.stdout,
        )
        self.assertIn(
            "Derived HAProxy address/VMID: 10.213.0.30/24 / 9120",
            completed.stdout,
        )
        self.assertIn("Secret layer loaded: 1", completed.stdout)

    def test_private_role_addresses_are_derived_from_cluster_ranges(self) -> None:
        replacements = {
            "PRIVATE_SUBNET_CIDR=10.213.0.0/24": "PRIVATE_SUBNET_CIDR=10.77.0.0/24",
            "PROXMOX_MIGRATION_NETWORK=10.213.0.240/28": "PROXMOX_MIGRATION_NETWORK=10.77.0.240/28",
            "DATACENTER_PRIVATE_VLAN_GATEWAY=10.213.0.1": "DATACENTER_PRIVATE_VLAN_GATEWAY=10.77.0.2",
            "GUEST_EGRESS_VIP=10.213.0.10/24": "GUEST_EGRESS_VIP=10.77.0.5/24",
            "MOX_IP_START=10.213.0.11": "MOX_IP_START=10.77.0.40",
            "MOX_IP_END=10.213.0.20": "MOX_IP_END=10.77.0.49",
            "MOX_REPLICATION_IP_START=10.213.0.241": "MOX_REPLICATION_IP_START=10.77.0.241",
            "MOX_REPLICATION_IP_END=10.213.0.250": "MOX_REPLICATION_IP_END=10.77.0.250",
            "HAPROXY_IP_START=10.213.0.21": "HAPROXY_IP_START=10.77.0.60",
            "HAPROXY_IP_END=10.213.0.30": "HAPROXY_IP_END=10.77.0.69",
            "PRODUCTION_IP_START=10.213.0.31": "PRODUCTION_IP_START=10.77.0.80",
            "PRODUCTION_IP_END=10.213.0.50": "PRODUCTION_IP_END=10.77.0.99",
            "STAGING_IP_START=10.213.0.51": "STAGING_IP_START=10.77.0.120",
            "STAGING_IP_END=10.213.0.200": "STAGING_IP_END=10.77.0.199",
        }
        content = self.cluster.read_text(encoding="utf-8")
        for old, new in replacements.items():
            content = content.replace(old, new)
        self.cluster.write_text(content, encoding="utf-8")

        completed = self.run_config(
            "--check", "--host", "mox10", "--require-secrets"
        )
        self.assertIn("Derived mox address: 10.77.0.49/24", completed.stdout)
        self.assertIn(
            "Derived migration/replication address: 10.77.0.250/28",
            completed.stdout,
        )
        self.assertIn(
            "Derived HAProxy address/VMID: 10.77.0.69/24 / 9120",
            completed.stdout,
        )

    def test_internal_domain_must_be_private_and_fqdn_is_derived(self) -> None:
        self.cluster.write_text(
            self.cluster.read_text(encoding="utf-8").replace(
                "PROXMOX_INTERNAL_DOMAIN=pve.internal",
                "PROXMOX_INTERNAL_DOMAIN=myapp.com",
            ),
            encoding="utf-8",
        )
        completed = self.run_config(
            "--check", "--host", "mox10", "--require-secrets", expected=1
        )
        self.assertIn(
            "PROXMOX_INTERNAL_DOMAIN must be a lowercase private domain",
            completed.stderr,
        )

    def test_explicit_mox_fqdn_is_rejected_because_identity_is_derived(self) -> None:
        with self.mox.open("a", encoding="utf-8") as handle:
            handle.write("PROXMOX_FQDN=mox10.myapp.com\n")
        completed = self.run_config(
            "--check", "--host", "mox10", "--require-secrets", expected=1
        )
        self.assertIn("unknown or misplaced key PROXMOX_FQDN", completed.stderr)

    def test_first_load_clears_every_allowlisted_inherited_key(self) -> None:
        environment = os.environ.copy()
        environment.update(
            {
                "APP_HA_CONFIG_TEST_MODE": "1",
                "APP_HA_ENV_DIR": str(self.env_dir),
            }
        )
        completed = subprocess.run(
            [
                "bash",
                "-c",
                r'''
source "$1"
for key in "${!_PROXMOX_CONFIG_ALLOWED_SCOPE[@]}"; do
  printf -v "$key" '%s' inherited-poison
  export "$key"
done
load_proxmox_config --host mox10 --require-secrets
[[ "$MAX_MOX_HOSTS" == 10 ]]
[[ "$NVME_MIRROR_1_SERIAL_1" == disk-a ]]
[[ "$PROXMOX_FQDN" == mox10.pve.internal ]]
[[ "$PROD_GUEST_OS_ISO_URL+x" != x ]]
require_var IDRAC_PASSWORD
PROXMOX_DNS_SERVER=198.51.100.53
if require_var PROXMOX_DNS_SERVER 2>/dev/null; then
  exit 32
fi
unset PROXMOX_DNS_SERVER
for key in "${!_PROXMOX_CONFIG_ALLOWED_SCOPE[@]}"; do
  if [[ -z "${_PROXMOX_CONFIG_SOURCE_FILE[$key]+x}" &&
    -z "${_PROXMOX_CONFIG_DERIVED[$key]+x}" ]]; then
    [[ -z "${!key+x}" ]] || exit 31
  fi
done
''',
                "bash",
                str(CONFIG),
            ],
            env=environment,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        self.assertEqual(
            completed.returncode,
            0,
            msg=f"stdout:\n{completed.stdout}\nstderr:\n{completed.stderr}",
        )

    def test_inherited_required_key_cannot_replace_host_file_origin(self) -> None:
        self.mox.write_text(
            self.mox.read_text(encoding="utf-8").replace(
                f'PROXMOX_IP={checked_in_config_value(MOX1_CONFIG, "PROXMOX_IP")}\n',
                "",
            ),
            encoding="utf-8",
        )
        completed = self.run_config(
            "--check",
            "--host",
            "mox10",
            "--no-secrets",
            expected=1,
            environment_updates={
                "PROXMOX_IP": checked_in_config_value(MOX1_CONFIG, "PROXMOX_IP")
            },
        )
        self.assertIn("Required variable PROXMOX_IP is unset or empty", completed.stderr)

    def test_idrac_address_is_optional_for_manual_inventory_hosts(self) -> None:
        self.mox.write_text(
            self.mox.read_text(encoding="utf-8").replace(
                "IDRAC_IP=192.0.2.63\n",
                "",
            ),
            encoding="utf-8",
        )
        completed = self.run_config(
            "--check",
            "--host",
            "mox10",
            "--no-secrets",
        )
        self.assertIn("Configuration valid.", completed.stdout)

    def test_secret_values_are_shell_only_and_do_not_change_effective_hash(
        self,
    ) -> None:
        environment = os.environ.copy()
        environment.update(
            {
                "APP_HA_CONFIG_TEST_MODE": "1",
                "APP_HA_ENV_DIR": str(self.env_dir),
                "IDRAC_PASSWORD": "inherited-secret",
            }
        )
        completed = subprocess.run(
            [
                "bash",
                "-c",
                (
                    'source "$1"; '
                    "load_proxmox_config --host mox10 --require-secrets; "
                    '[[ "$IDRAC_PASSWORD" == must-not-appear ]]; '
                    'bash -c \'[[ -z "${IDRAC_USER+x}" && '
                    '-z "${IDRAC_PASSWORD+x}" ]]\''
                ),
                "bash",
                str(CONFIG),
            ],
            env=environment,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        self.assertEqual(
            completed.returncode,
            0,
            msg=f"stdout:\n{completed.stdout}\nstderr:\n{completed.stderr}",
        )
        self.assertNotIn(
            "must-not-appear",
            completed.stdout + completed.stderr,
        )

        first = self.run_config(
            "--check", "--host", "mox10", "--require-secrets"
        )
        self.secrets.write_text(
            "IDRAC_USER=other-user\nIDRAC_PASSWORD=different-secret-value\n",
            encoding="utf-8",
        )
        second = self.run_config(
            "--check", "--host", "mox10", "--require-secrets"
        )
        hash_prefix = "Effective non-secret SHA-256: "
        first_hash = next(
            line.removeprefix(hash_prefix)
            for line in first.stdout.splitlines()
            if line.startswith(hash_prefix)
        )
        second_hash = next(
            line.removeprefix(hash_prefix)
            for line in second.stdout.splitlines()
            if line.startswith(hash_prefix)
        )
        self.assertEqual(first_hash, second_hash)
        self.assertNotIn(
            "different-secret-value",
            second.stdout + second.stderr,
        )

    def test_exact_nvme_and_production_iso_key_names_remain_allowlisted(
        self,
    ) -> None:
        completed = subprocess.run(
            [
                "bash",
                "-c",
                (
                    'source "$1"; '
                    "_config_key_allowed mox NVME_MIRROR_5_SERIAL_1; "
                    "_config_key_allowed mox NVME_MIRROR_5_SERIAL_2; "
                    "_config_key_allowed mox NVME_MIRROR_5_CAPACITY_BYTES_1; "
                    "_config_key_allowed mox NVME_MIRROR_5_CAPACITY_BYTES_2; "
                    "_config_key_allowed cluster "
                    "PROD_GUEST_OS_ISO_URL; "
                    "_config_key_allowed cluster "
                    "PROD_GUEST_OS_ISO_SHA256; "
                    "_config_key_allowed cluster "
                    "PROD_GUEST_OS_INSTALL_MODE; "
                    "_config_key_allowed secrets PROXMOX_LUKS_PASSWORD; "
                    "! _config_key_allowed mox NVME_MIRROR_5_SERIAL1; "
                    "! _config_key_allowed cluster "
                    "PROD_GUEST_UBUNTU_ISO_FILE_URL"
                ),
                "bash",
                str(CONFIG),
            ],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)

    def test_production_iso_url_requires_public_https_iso(self) -> None:
        with self.cluster.open("a", encoding="utf-8") as handle:
            handle.write(
                "PROD_GUEST_OS_ISO_URL="
                "http://user:secret@example.test/source.iso#fragment\n"
            )
        completed = self.run_config("--check", "--no-secrets", expected=1)
        self.assertIn(
            "PROD_GUEST_OS_ISO_URL must be a public HTTPS URL "
            "ending in .iso",
            completed.stderr,
        )

    def test_production_install_mode_is_strictly_enumerated(self) -> None:
        with self.cluster.open("a", encoding="utf-8") as handle:
            handle.write("PROD_GUEST_OS_INSTALL_MODE=maybe\n")
        completed = self.run_config("--check", "--no-secrets", expected=1)
        self.assertIn(
            "PROD_GUEST_OS_INSTALL_MODE must be ubuntu-autoinstall or manual",
            completed.stderr,
        )

    def test_manual_inventory_capacities_must_be_complete_positive_pairs(
        self,
    ) -> None:
        original = self.mox.read_text(encoding="utf-8")
        self.mox.write_text(
            original + "NVME_MIRROR_1_CAPACITY_BYTES_1=1000204886016\n",
            encoding="utf-8",
        )
        completed = self.run_config("--check", "--host", "mox10", expected=1)
        self.assertIn(
            "must define both NVME_MIRROR_1_CAPACITY_BYTES_1 "
            "and NVME_MIRROR_1_CAPACITY_BYTES_2",
            completed.stderr,
        )

        self.mox.write_text(
            original
            + "NVME_MIRROR_1_CAPACITY_BYTES_1=1000204886016\n"
            + "NVME_MIRROR_1_CAPACITY_BYTES_2=0\n",
            encoding="utf-8",
        )
        completed = self.run_config("--check", "--host", "mox10", expected=1)
        self.assertIn("capacities must be positive byte counts", completed.stderr)

    def test_prompted_secrets_remain_shell_only_even_with_allexport(self) -> None:
        completed = subprocess.run(
            [
                "bash",
                "-c",
                r'''
source "$1"
set -a
prompt_secret PROMPTED_SECRET "Synthetic secret" <<<"prompt-only-value"
prompt_tailscale_auth_key <<<"tskey-auth-prompt-only-value"
[[ "$PROMPTED_SECRET" == prompt-only-value ]]
[[ "$TAILSCALE_AUTH_KEY" == tskey-auth-prompt-only-value ]]
bash -c '[[ -z "${PROMPTED_SECRET+x}" && -z "${TAILSCALE_AUTH_KEY+x}" ]]'
''',
                "bash",
                str(CONFIG),
            ],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertNotIn(
            "prompt-only-value",
            completed.stdout + completed.stderr,
        )

    def test_reloading_clears_prior_host_and_secret_layers(self) -> None:
        environment = os.environ.copy()
        environment.update(
            {
                "APP_HA_CONFIG_TEST_MODE": "1",
                "APP_HA_ENV_DIR": str(self.env_dir),
            }
        )
        completed = subprocess.run(
            [
                "bash",
                "-c",
                (
                    'source "$1"; '
                    "load_proxmox_config --host mox10 --require-secrets; "
                    '[[ "$MOX_HOSTNAME" == mox10 && -n "$IDRAC_PASSWORD" ]]; '
                    "load_proxmox_config --no-secrets; "
                    '[[ -z "${MOX_HOSTNAME+x}" && -z "${IDRAC_PASSWORD+x}" ]]'
                ),
                "bash",
                str(CONFIG),
            ],
            env=environment,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        self.assertEqual(
            completed.returncode,
            0,
            msg=f"stdout:\n{completed.stdout}\nstderr:\n{completed.stderr}",
        )
        self.assertNotIn("must-not-appear", completed.stdout + completed.stderr)

    def test_unknown_keys_and_unsafe_modes_fail_closed(self) -> None:
        with self.cluster.open("a", encoding="utf-8") as handle:
            handle.write("GUEST_EGRESS_VPI=typo\n")
        unknown = self.run_config("--check", "--no-secrets", expected=1)
        self.assertIn("unknown or misplaced key GUEST_EGRESS_VPI", unknown.stderr)

        text = self.cluster.read_text(encoding="utf-8")
        self.cluster.write_text(
            text.replace("GUEST_EGRESS_VPI=typo\n", ""), encoding="utf-8"
        )
        self.cluster.chmod(0o664)
        self.run_config("--check", "--no-secrets")
        self.cluster.chmod(0o666)
        unsafe = self.run_config("--check", "--no-secrets", expected=1)
        self.assertIn("must not be world writable", unsafe.stderr)

    def test_remote_interface_configuration_is_strictly_validated(self) -> None:
        with self.cluster.open("a", encoding="utf-8") as handle:
            handle.write("PROXMOX_PRIVATE_BRIDGE=vmbr0;unexpected\n")
        completed = self.run_config("--check", "--no-secrets", expected=1)
        self.assertIn("safe Linux interface name", completed.stderr)

    def test_secret_mode_and_symlinks_fail_closed(self) -> None:
        self.secrets.chmod(0o640)
        bad_secret = self.run_config("--check", "--require-secrets", expected=1)
        self.assertIn("must have mode 0600", bad_secret.stderr)
        self.secrets.chmod(0o600)

        self.mox.unlink()
        self.mox.symlink_to(self.cluster)
        symlink = self.run_config(
            "--check", "--host", "mox10", "--no-secrets", expected=1
        )
        self.assertIn("must not contain symlinks", symlink.stderr)

    # GNU stat answers -c and BSD/macOS stat answers -f. A fake stat of each
    # flavour on PATH runs both branches of the config.sh helpers on every OS.
    FAKE_STAT = r"""#!/bin/bash
case "$1" in
  -c) [[ "$FAKE_STAT_FLAVOR" == gnu ]] || { echo "stat: illegal option -- c" >&2; exit 1; } ;;
  -f) [[ "$FAKE_STAT_FLAVOR" == bsd ]] || { echo "stat: unsupported -f format" >&2; exit 1; } ;;
  *) exit 64 ;;
esac
format="$2"
target="${!#}"
if [[ "$target" == / ]]; then
  echo 755
  exit 0
fi
[[ "${FAKE_STAT_FAIL:-0}" != 1 ]] || exit 1
case "$format" in
  %a | %p) printf '%s\n' "$FAKE_STAT_MODE" ;;
  %u) printf '%s\n' "$FAKE_STAT_UID" ;;
  *) exit 64 ;;
esac
"""

    def fake_stat_environment(
        self, flavor: str, mode: str, **overrides: str
    ) -> dict[str, str]:
        fake_bin = Path(self.temporary.name) / "fake-stat-bin"
        fake_bin.mkdir(exist_ok=True)
        fake = fake_bin / "stat"
        fake.write_text(self.FAKE_STAT, encoding="utf-8")
        fake.chmod(0o755)
        environment = {
            "PATH": f"{fake_bin}{os.pathsep}{os.environ['PATH']}",
            "FAKE_STAT_FLAVOR": flavor,
            "FAKE_STAT_MODE": mode,
            "FAKE_STAT_UID": str(os.getuid()),
            "SUDO_UID": "",
        }
        environment.update(overrides)
        return environment

    def test_stat_helpers_accept_private_modes_in_both_flavours(self) -> None:
        for flavor, mode in (("gnu", "600"), ("bsd", "100600")):
            with self.subTest(flavor=flavor):
                self.run_config(
                    "--check",
                    "--require-secrets",
                    environment_updates=self.fake_stat_environment(flavor, mode),
                )

    def test_stat_helpers_reject_unusable_output_in_both_flavours(self) -> None:
        unusable = ("", "rw-------", "garbage0600", "1001+22", "0600 extra", "8")
        for flavor in ("gnu", "bsd"):
            for mode in unusable:
                with self.subTest(flavor=flavor, mode=mode):
                    completed = self.run_config(
                        "--check",
                        "--no-secrets",
                        expected=1,
                        environment_updates=self.fake_stat_environment(flavor, mode),
                    )
                    self.assertIn("Cannot read mode", completed.stderr)
            with self.subTest(flavor=flavor, mode="stat exits non-zero"):
                completed = self.run_config(
                    "--check",
                    "--no-secrets",
                    expected=1,
                    environment_updates=self.fake_stat_environment(
                        flavor, "600", FAKE_STAT_FAIL="1"
                    ),
                )
                self.assertIn("Cannot read mode", completed.stderr)

    def test_stat_helpers_enforce_owner_in_both_flavours(self) -> None:
        good = {"gnu": "600", "bsd": "100600"}
        for flavor, mode in good.items():
            with self.subTest(flavor=flavor, owner="another user"):
                completed = self.run_config(
                    "--check",
                    "--no-secrets",
                    expected=1,
                    environment_updates=self.fake_stat_environment(
                        flavor, mode, FAKE_STAT_UID=str(os.getuid() + 4242)
                    ),
                )
                self.assertIn("neither root nor the invoking user", completed.stderr)
            for owner in ("", "abc", "12 34", "1+1"):
                with self.subTest(flavor=flavor, owner=owner):
                    completed = self.run_config(
                        "--check",
                        "--no-secrets",
                        expected=1,
                        environment_updates=self.fake_stat_environment(
                            flavor, mode, FAKE_STAT_UID=owner
                        ),
                    )
                    self.assertIn("Cannot read owner", completed.stderr)

    def test_stat_helpers_enforce_exact_secret_mode_in_both_flavours(self) -> None:
        cases = (
            ("gnu", "640", "found 640"),
            ("gnu", "4600", "found 4600"),
            ("bsd", "100640", "found 640"),
            ("bsd", "104600", "found 4600"),
            # Low digits that need padding: %Mp%Lp would print 144 for 1044.
            ("bsd", "101044", "found 1044"),
        )
        for flavor, mode, expected in cases:
            with self.subTest(flavor=flavor, mode=mode):
                completed = self.run_config(
                    "--check",
                    "--require-secrets",
                    expected=1,
                    environment_updates=self.fake_stat_environment(flavor, mode),
                )
                self.assertIn("must have mode 0600", completed.stderr)
                self.assertIn(expected, completed.stderr)
        world_writable = self.run_config(
            "--check",
            "--no-secrets",
            expected=1,
            environment_updates=self.fake_stat_environment("bsd", "100007"),
        )
        self.assertIn("must not be world writable (mode 7)", world_writable.stderr)

    def test_native_stat_reports_special_and_short_modes_exactly(self) -> None:
        # No fake here: this is the platform's own stat. 044 is the shape a
        # wrong BSD format string gets wrong, and 4600 must not read as 600.
        for mode, expected in ((0o044, "found 44"), (0o4600, "found 4600")):
            with self.subTest(mode=oct(mode)):
                self.secrets.chmod(mode)
                completed = self.run_config(
                    "--check", "--require-secrets", expected=1
                )
                self.assertIn("must have mode 0600", completed.stderr)
                self.assertIn(expected, completed.stderr)
        self.secrets.chmod(0o600)
        self.run_config("--check", "--require-secrets")

    def test_known_hosts_trust_file_mode_is_enforced_in_both_flavours(self) -> None:
        known_hosts = Path(self.temporary.name) / "known_hosts"
        known_hosts.write_text(
            "mox2 ssh-ed25519 AAAAC3NzaSyntheticHostKey\n", encoding="utf-8"
        )
        known_hosts.chmod(0o600)
        cases = (
            ("gnu", "600", 0),
            ("gnu", "620", 1),
            ("gnu", "602", 1),
            ("gnu", "garbage0600", 1),
            ("bsd", "100600", 0),
            ("bsd", "100620", 1),
            ("bsd", "100602", 1),
            ("bsd", "garbage0600", 1),
        )
        for flavor, mode, expected in cases:
            with self.subTest(flavor=flavor, mode=mode):
                environment = os.environ.copy()
                environment.update(self.fake_stat_environment(flavor, mode))
                completed = subprocess.run(
                    [
                        "bash",
                        "-c",
                        'source "$1"\n'
                        'PROXMOX_SSH_KNOWN_HOSTS_FILE="$2"\n'
                        "_mox_known_hosts_file mox2\n",
                        "bash",
                        str(CONFIG),
                        str(known_hosts),
                    ],
                    env=environment,
                    text=True,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    check=False,
                )
                self.assertEqual(completed.returncode, expected, completed.stderr)
                if expected:
                    self.assertRegex(
                        completed.stderr,
                        "must not be group/world writable|Cannot inspect",
                    )
                else:
                    self.assertEqual(completed.stdout.strip(), str(known_hosts))

    def test_mox_ssh_uses_explicit_trust_and_quotes_remote_argv(self) -> None:
        known_hosts = Path(self.temporary.name) / "known_hosts"
        known_hosts.write_text(
            "mox2 ssh-ed25519 AAAAC3NzaSyntheticHostKey\n",
            encoding="utf-8",
        )
        known_hosts.chmod(0o600)
        sentinel = Path(self.temporary.name) / "argument-was-evaluated"
        malicious = f"$(touch {sentinel}); value with spaces"
        completed = subprocess.run(
            [
                "bash",
                "-c",
                r'''
source "$1"
MAX_MOX_HOSTS=2
PROXMOX_SSH_KNOWN_HOSTS_FILE="$2"
ssh() {
  printf 'ARG=<%s>\n' "$@"
}
mox_ssh mox2 printf '%s\n' "$3"
''',
                "bash",
                str(CONFIG),
                str(known_hosts),
                malicious,
            ],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertFalse(sentinel.exists())
        self.assertIn("-o", completed.stdout)
        self.assertIn(f"UserKnownHostsFile={known_hosts}", completed.stdout)
        self.assertIn("GlobalKnownHostsFile=none", completed.stdout)
        self.assertIn(r"\$\(touch", completed.stdout)
        self.assertIn(r"value\ with\ spaces", completed.stdout)


if __name__ == "__main__":
    unittest.main()
