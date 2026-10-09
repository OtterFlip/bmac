#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Unit tests for deterministic generic HAProxy route rendering."""

from __future__ import annotations

import hashlib
import importlib.util
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


# The config loader and the registry reject paths with symlinked components.
# macOS keeps TMPDIR under /var -> /private/var, so hand out resolved temp
# paths. A no-op where the temp directory is already a real path.
tempfile.tempdir = str(Path(tempfile.gettempdir()).resolve())

LIB_DIR = Path(__file__).resolve().parent
RENDERER_PATH = LIB_DIR / "haproxy_routes.py"
REGISTRY_PATH = LIB_DIR / "cluster_registry.py"
SYNC_PATH = LIB_DIR / "sync_haproxy_routes.sh"
HOST_SETUP_PATH = LIB_DIR.parent / "hosts" / "add_proxmox_host.sh"

SPEC = importlib.util.spec_from_file_location("haproxy_routes", RENDERER_PATH)
assert SPEC is not None and SPEC.loader is not None
haproxy_routes = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = haproxy_routes
SPEC.loader.exec_module(haproxy_routes)

NETWORK_ARGUMENTS = {
    "backend_network": "10.213.0.0/24",
    "production_ip_start": "10.213.0.31",
    "production_ip_end": "10.213.0.50",
    "staging_ip_start": "10.213.0.51",
    "staging_ip_end": "10.213.0.200",
}


def route(
    domain: str = "example.com",
    resource: str = "prod1",
    *,
    kind: str = "production",
    purpose: str = "example",
    ip: str = "10.213.0.31",
    http_port: int = 80,
    https_port: int = 443,
) -> dict:
    return {
        "domain": domain,
        "resource": resource,
        "kind": kind,
        "purpose": purpose,
        "ip": ip,
        "http_port": http_port,
        "https_port": https_port,
    }


def validate(values: object) -> list[dict]:
    return haproxy_routes.validate_routes(values, **NETWORK_ARGUMENTS)


def directory_contents(root: Path) -> dict[str, bytes]:
    return {
        str(path.relative_to(root)): path.read_bytes()
        for path in sorted(root.rglob("*"))
        if path.is_file()
    }


class RouteRendererTest(unittest.TestCase):
    def test_consumes_cluster_registry_list_routes_contract(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            state = root / "registry"

            def registry(*arguments: str) -> subprocess.CompletedProcess[str]:
                completed = subprocess.run(
                    [
                        sys.executable,
                        str(REGISTRY_PATH),
                        "--state-dir",
                        str(state),
                        *arguments,
                    ],
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
                return completed

            registry(
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
            registry(
                "allocate-prod",
                "--vmid",
                "100",
                "--purpose",
                "docs",
                "--primary-domain",
                "docs.example.com",
                "--alias",
                "www.docs.example.com",
                "--placement",
                "mox1",
                "--initial-node",
                "mox1",
            )
            registry(
                "update",
                "prod1",
                "--state",
                "provisioning",
                "--owner-node",
                "mox1",
            )
            registry(
                "update",
                "prod1",
                "--state",
                "stopped",
                "--routes-enabled",
            )
            listed = json.loads(registry("list-routes").stdout)
            validated = validate(listed)
            files = haproxy_routes.render_files(validated)
            self.assertEqual(
                files["maps/app-ha-http-host.map"],
                "docs.example.com app_ha_http_prod1\n"
                "www.docs.example.com app_ha_http_prod1\n",
            )

    def test_empty_registry_renders_reject_only_generation(self) -> None:
        routes = validate([])
        files = haproxy_routes.render_files(routes)
        self.assertEqual(files["maps/app-ha-http-host.map"], "")
        self.assertEqual(files["maps/app-ha-tls-sni.map"], "")
        config = files["haproxy.cfg"]
        self.assertIn("default_backend app_ha_reject_http", config)
        self.assertIn("default_backend app_ha_reject_tls", config)
        self.assertIn("http-request deny deny_status 404", config)
        self.assertIn("127.0.0.1:1 disabled", config)
        self.assertNotIn("10.213.0.3 ", config)

    def test_exact_maps_and_one_backend_pair_per_resource(self) -> None:
        routes = validate(
            [
                route("www.example.com"),
                route("example.com"),
                route(
                    "preview.example.net",
                    "stage1prod1",
                    kind="staging",
                    ip="10.213.0.51",
                    http_port=8080,
                    https_port=8443,
                ),
            ]
        )
        files = haproxy_routes.render_files(routes)
        self.assertEqual(
            files["maps/app-ha-http-host.map"],
            "example.com app_ha_http_prod1\n"
            "preview.example.net app_ha_http_stage1prod1\n"
            "www.example.com app_ha_http_prod1\n",
        )
        self.assertEqual(
            files["maps/app-ha-tls-sni.map"],
            "example.com app_ha_tls_prod1\n"
            "preview.example.net app_ha_tls_stage1prod1\n"
            "www.example.com app_ha_tls_prod1\n",
        )
        config = files["haproxy.cfg"]
        self.assertEqual(config.count("\nbackend app_ha_http_prod1\n"), 1)
        self.assertEqual(config.count("\nbackend app_ha_tls_prod1\n"), 1)
        self.assertEqual(config.count("\nbackend app_ha_http_stage1prod1\n"), 1)
        self.assertEqual(config.count("\nbackend app_ha_tls_stage1prod1\n"), 1)
        self.assertIn("10.213.0.31:80 check", config)
        self.assertIn("10.213.0.31:443 check", config)
        self.assertIn("10.213.0.51:8080 check", config)
        self.assertIn("10.213.0.51:8443 check", config)
        self.assertIn("req.hdr(host),lower,field(1,:),map(", config)
        self.assertIn("req.ssl_sni,lower,map(", config)

    def test_render_is_byte_deterministic_regardless_of_input_order(self) -> None:
        first_input = [
            route("www.example.com"),
            route(
                "preview.example.com",
                "stage1prod1",
                kind="staging",
                ip="10.213.0.51",
            ),
            route("example.com"),
        ]
        second_input = list(reversed(first_input))
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            first_output = root / "first"
            second_output = root / "second"
            first_manifest = haproxy_routes.write_generation(
                first_output, validate(first_input)
            )
            second_manifest = haproxy_routes.write_generation(
                second_output, validate(second_input)
            )
            self.assertEqual(first_manifest, second_manifest)
            self.assertEqual(
                directory_contents(first_output),
                directory_contents(second_output),
            )
            generation = first_manifest["generation"]
            config = (first_output / "haproxy.cfg").read_text(encoding="utf-8")
            self.assertIn(
                f"/etc/haproxy/app-ha-generations/{generation}/maps/"
                "app-ha-http-host.map",
                config,
            )

    def test_manifest_hashes_every_rendered_file(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "generation"
            manifest = haproxy_routes.write_generation(
                output, validate([route()])
            )
            stored = json.loads(
                (output / "manifest.json").read_text(encoding="utf-8")
            )
            self.assertEqual(stored, manifest)
            for relative, expected in manifest["files"].items():
                actual = hashlib.sha256((output / relative).read_bytes()).hexdigest()
                self.assertEqual(actual, expected)

    def test_generation_only_changes_for_routing_semantics(self) -> None:
        first = validate([route(purpose="first-purpose")])
        second = validate([route(purpose="another-purpose")])
        changed = validate([route(http_port=8080)])
        self.assertEqual(
            haproxy_routes.routing_generation(first),
            haproxy_routes.routing_generation(second),
        )
        self.assertNotEqual(
            haproxy_routes.routing_generation(first),
            haproxy_routes.routing_generation(changed),
        )

    def test_rejects_invalid_domains_resources_and_ports(self) -> None:
        invalid_routes = [
            route("*.example.com"),
            route("Example.com"),
            route("example.com."),
            route("https://example.com"),
            route("not-qualified"),
            route("127.0.0.1"),
            route(resource="other"),
            route(resource="prod" + "1" * 200),
            route(resource="stage1prod1", kind="production"),
            route(resource="prod1", kind="staging", ip="10.213.0.51"),
            route(http_port=0),
            route(https_port=65536),
            route(http_port=True),
        ]
        for invalid in invalid_routes:
            with self.subTest(route=invalid):
                with self.assertRaises(haproxy_routes.RouteError):
                    validate([invalid])

    def test_rejects_invalid_or_reserved_backend_addresses(self) -> None:
        invalid_routes = [
            route(ip="10.213.0.3"),
            route(ip="10.213.1.31"),
            route(ip="2001:db8::31"),
            route(ip="010.213.0.31"),
            route(ip="10.213.0.51"),
            route(
                resource="stage1prod1",
                kind="staging",
                ip="10.213.0.31",
            ),
        ]
        for invalid in invalid_routes:
            with self.subTest(route=invalid):
                with self.assertRaises(haproxy_routes.RouteError):
                    validate([invalid])

    def test_rejects_duplicate_or_inconsistent_registry_routes(self) -> None:
        cases = [
            [route(), route()],
            [route(), route("alias.example.com", ip="10.213.0.32")],
            [route(), route("alias.example.com", purpose="another")],
            [route(), route("other.example.com", "prod2")],
        ]
        for values in cases:
            with self.subTest(routes=values):
                with self.assertRaises(haproxy_routes.RouteError):
                    validate(values)

    def test_rejects_schema_drift(self) -> None:
        extra = route()
        extra["unexpected"] = "value"
        missing = route()
        del missing["purpose"]
        for value in ({"routes": []}, [extra], [missing]):
            with self.subTest(value=value):
                with self.assertRaises(haproxy_routes.RouteError):
                    validate(value)

    def test_rejects_duplicate_json_keys_and_unsafe_generation_path(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "routes.json"
            path.write_text(
                '[{"domain":"example.com","domain":"other.example.com"}]',
                encoding="utf-8",
            )
            with self.assertRaises(haproxy_routes.RouteError):
                haproxy_routes.read_routes(str(path))
        with self.assertRaises(haproxy_routes.RouteError):
            haproxy_routes.render_files(
                validate([route()]),
                generation_root=Path("/etc/haproxy/unsafe path"),
            )

    def test_cli_failure_leaves_no_output_generation(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            input_path = root / "routes.json"
            output_path = root / "output"
            input_path.write_text(
                json.dumps([route("*.example.com")]), encoding="utf-8"
            )
            completed = subprocess.run(
                [
                    sys.executable,
                    str(RENDERER_PATH),
                    "--routes-json",
                    str(input_path),
                    "--output-dir",
                    str(output_path),
                    "--backend-network",
                    NETWORK_ARGUMENTS["backend_network"],
                    "--production-ip-start",
                    NETWORK_ARGUMENTS["production_ip_start"],
                    "--production-ip-end",
                    NETWORK_ARGUMENTS["production_ip_end"],
                    "--staging-ip-start",
                    NETWORK_ARGUMENTS["staging_ip_start"],
                    "--staging-ip-end",
                    NETWORK_ARGUMENTS["staging_ip_end"],
                ],
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )
            self.assertEqual(completed.returncode, 1)
            self.assertIn("exact hostname", completed.stderr)
            self.assertFalse(output_path.exists())

    @unittest.skipUnless(shutil.which("haproxy"), "haproxy is not installed")
    def test_rendered_empty_and_populated_configs_pass_haproxy_check(self) -> None:
        for routes in (
            validate([]),
            validate(
                [
                    route(),
                    route(
                        "preview.example.com",
                        "stage1prod1",
                        kind="staging",
                        ip="10.213.0.51",
                    ),
                ]
            ),
        ):
            with self.subTest(route_count=len(routes)):
                with tempfile.TemporaryDirectory() as temporary:
                    root = Path(temporary)
                    generation_root = root / "etc" / "haproxy" / "generations"
                    output = generation_root / haproxy_routes.routing_generation(routes)
                    haproxy_routes.write_generation(
                        output,
                        routes,
                        generation_root=generation_root,
                    )
                    completed = subprocess.run(
                        ["haproxy", "-c", "-f", str(output / "haproxy.cfg")],
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


PRODUCTION = route("example.com")
PRODUCTION_ALIAS = route("www.example.com")
STAGE_ONE = route(
    "stage1prod1.example.com", "stage1prod1", kind="staging", ip="10.213.0.51"
)
STAGE_TWO = route(
    "stage2prod1.example.com", "stage2prod1", kind="staging", ip="10.213.0.52"
)


def write_rendered(root: Path, routes: list[dict]) -> tuple[str, Path]:
    validated = validate(routes)
    generation = haproxy_routes.routing_generation(validated)
    output = root / generation
    if not output.exists():
        haproxy_routes.write_generation(output, validated)
    return generation, output


def archive_of(directory: Path, archive: Path) -> Path:
    subprocess.run(
        ["tar", "-C", str(directory), "-cf", str(archive), "."], check=True
    )
    return archive


class CompatibilityTest(unittest.TestCase):
    def compare(self, previous: list[dict], candidate: list[dict]) -> dict:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            _, previous_dir = write_rendered(root, previous)
            _, candidate_dir = write_rendered(root, candidate)
            _, previous_routes = haproxy_routes.load_rendered_generation(
                haproxy_routes.read_generation_dir(previous_dir)
            )
            _, candidate_routes = haproxy_routes.load_rendered_generation(
                haproxy_routes.read_generation_dir(candidate_dir)
            )
            return haproxy_routes.compare_generations(
                previous_routes, candidate_routes
            )

    def test_adding_and_removing_staging_routes_is_compatible(self) -> None:
        added = self.compare([PRODUCTION], [PRODUCTION, STAGE_ONE])
        self.assertTrue(added["compatible"], added)
        self.assertEqual(added["added_domains"], ["stage1prod1.example.com"])
        removed = self.compare([PRODUCTION, STAGE_ONE], [PRODUCTION])
        self.assertTrue(removed["compatible"], removed)
        self.assertEqual(removed["removed_domains"], ["stage1prod1.example.com"])

    def test_consecutive_and_lagging_staging_changes_are_compatible(self) -> None:
        chain = [
            [PRODUCTION],
            [PRODUCTION, STAGE_ONE],
            [PRODUCTION, STAGE_ONE, STAGE_TWO],
            [PRODUCTION, STAGE_TWO],
            [PRODUCTION],
        ]
        for previous in range(len(chain)):
            for candidate in range(previous + 1, len(chain)):
                with self.subTest(previous=previous, candidate=candidate):
                    result = self.compare(chain[previous], chain[candidate])
                    self.assertTrue(result["compatible"], result)

    def test_production_alias_changes_are_compatible(self) -> None:
        self.assertTrue(
            self.compare([PRODUCTION], [PRODUCTION, PRODUCTION_ALIAS])["compatible"]
        )
        self.assertTrue(
            self.compare([PRODUCTION, PRODUCTION_ALIAS], [PRODUCTION])["compatible"]
        )

    def test_retargeted_routes_are_incompatible(self) -> None:
        cases = {
            "backend port": [route(http_port=8080)],
            "backend IP": [route(ip="10.213.0.32")],
            "domain owner": [route("example.com", "prod2", ip="10.213.0.32")],
        }
        for description, candidate in cases.items():
            with self.subTest(change=description):
                result = self.compare([PRODUCTION], candidate)
                self.assertFalse(result["compatible"], result)
                self.assertTrue(result["reasons"])

    def test_backend_ip_reuse_within_one_transition_is_incompatible(self) -> None:
        reused = route(
            "stage2prod1.example.com", "stage2prod1", kind="staging", ip="10.213.0.51"
        )
        result = self.compare([PRODUCTION, STAGE_ONE], [PRODUCTION, reused])
        self.assertFalse(result["compatible"])
        self.assertIn(
            "backend IP 10.213.0.51 moves from stage1prod1 to stage2prod1",
            result["reasons"],
        )

    def test_shared_configuration_drift_is_not_provably_equivalent(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            _, output = write_rendered(Path(temporary), [PRODUCTION])
            files = haproxy_routes.read_generation_dir(output)
            haproxy_routes.load_rendered_generation(files)
            drifts = {
                "global timeout": (
                    "haproxy.cfg",
                    "timeout client  60s",
                    "timeout client  600s",
                ),
                "extra server": (
                    "haproxy.cfg",
                    "10.213.0.31:80 check",
                    "10.213.0.31:80 check\n    server extra 10.213.0.40:80 check",
                ),
                "hand-edited map": (
                    "maps/app-ha-http-host.map",
                    "example.com",
                    "other.example.com",
                ),
            }
            for description, (name, old, new) in drifts.items():
                with self.subTest(drift=description):
                    changed = dict(files)
                    changed[name] = changed[name].replace(old, new, 1)
                    with self.assertRaises(haproxy_routes.RouteError):
                        haproxy_routes.load_rendered_generation(changed)

    def run_compare(
        self, previous_archive: Path, candidate_dir: Path
    ) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [
                sys.executable,
                str(RENDERER_PATH),
                "compare",
                "--previous-archive",
                str(previous_archive),
                "--candidate-dir",
                str(candidate_dir),
            ],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )

    def test_compare_cli_reads_live_archive_and_reports_policy(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            previous, previous_dir = write_rendered(root, [PRODUCTION])
            candidate, candidate_dir = write_rendered(root, [PRODUCTION, STAGE_ONE])
            _, retargeted_dir = write_rendered(root, [route(ip="10.213.0.32")])
            archive = archive_of(previous_dir, root / "previous.tar")

            compatible = self.run_compare(archive, candidate_dir)
            self.assertEqual(compatible.returncode, 0, compatible.stderr)
            result = json.loads(compatible.stdout)
            self.assertEqual(result["previous_generation"], previous)
            self.assertEqual(result["candidate_generation"], candidate)

            incompatible = self.run_compare(archive, retargeted_dir)
            self.assertEqual(incompatible.returncode, 3, incompatible.stderr)
            self.assertFalse(json.loads(incompatible.stdout)["compatible"])

    def test_compare_cli_rejects_unsafe_archives_and_candidates(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            _, previous_dir = write_rendered(root, [PRODUCTION])
            _, candidate_dir = write_rendered(root, [PRODUCTION, STAGE_ONE])
            unsafe = root / "unsafe"
            shutil.copytree(previous_dir, unsafe)
            (unsafe / "maps" / "app-ha-tls-sni.map").unlink()
            (unsafe / "maps" / "app-ha-tls-sni.map").symlink_to("/etc/passwd")
            archive = archive_of(unsafe, root / "unsafe.tar")
            refused = self.run_compare(archive, candidate_dir)
            self.assertEqual(refused.returncode, 3)
            self.assertIn("not provably equivalent", refused.stdout)

            broken = root / "broken-candidate"
            shutil.copytree(candidate_dir, broken)
            (broken / "haproxy.cfg").write_text("global\n", encoding="utf-8")
            invalid = self.run_compare(
                archive_of(previous_dir, root / "previous.tar"), broken
            )
            self.assertEqual(invalid.returncode, 1)
            self.assertIn("candidate generation", invalid.stderr)


class ShellIntegrationTest(unittest.TestCase):
    def run_sourced(
        self, body: str, *, expected: int = 0
    ) -> subprocess.CompletedProcess[str]:
        completed = subprocess.run(
            [
                "bash",
                "-c",
                f'source "$1"\n{body}',
                "bash",
                str(SYNC_PATH),
            ],
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
        return completed

    def test_shell_scripts_parse_and_sync_help_needs_no_secrets(self) -> None:
        for script in (SYNC_PATH, HOST_SETUP_PATH):
            completed = subprocess.run(
                ["bash", "-n", str(script)],
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )
            self.assertEqual(completed.returncode, 0, msg=completed.stderr)
        help_result = subprocess.run(
            ["bash", str(SYNC_PATH), "--help"],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        self.assertEqual(help_result.returncode, 0, msg=help_result.stderr)
        self.assertIn("without reading secrets.env", help_result.stdout)
        self.assertIn("--lock-timeout SECONDS", help_result.stdout)
        invalid_timeout = subprocess.run(
            ["bash", str(SYNC_PATH), "--lock-timeout", "0"],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        self.assertNotEqual(invalid_timeout.returncode, 0)
        self.assertIn("integer from 1 through 600", invalid_timeout.stderr)

    def test_sync_transaction_contains_required_safety_phases(self) -> None:
        text = SYNC_PATH.read_text(encoding="utf-8")
        coordinator = text[text.index("coordinator_transaction()") :]
        self.assertLess(
            coordinator.index("assert_coordinator_online_and_quorate"),
            coordinator.index("registry_cmd list-routes"),
        )
        self.assertLess(
            coordinator.index("render_routes"),
            coordinator.index("rollout_generation "),
        )
        rollout = text[
            text.index("rollout_generation() {") : text.index("coordinator_transaction()")
        ]
        self.assertLess(
            rollout.index("stage_node "),
            rollout.index("select_compatible_predecessor "),
        )
        self.assertLess(
            rollout.index("select_compatible_predecessor "),
            rollout.index("disable_ingress_node "),
        )
        self.assertLess(
            rollout.index("disable_ingress_node "),
            rollout.index("publish_desired_generation"),
        )
        self.assertLess(
            rollout.index("publish_desired_generation"),
            rollout.index("install_node "),
        )
        self.assertLess(
            rollout.index("install_node "),
            rollout.index("finalize_rollout "),
        )
        self.assertIn("haproxy -c -f", text)
        self.assertIn("systemctl reload haproxy", text)
        self.assertIn("load_proxmox_config --no-secrets", text)
        self.assertIn("list-routes", text)
        self.assertIn("ingress-begin", text)
        self.assertIn("ingress-mark", text)
        self.assertIn("registry_cmd --lock-timeout 0", text)
        self.assertIn("app-ha-active-generation", text)
        self.assertIn("MAX_RETAINED_GENERATIONS=4", text)
        self.assertIn('flock -w "$LOCK_WAIT_SECONDS"', text)
        self.assertIn('--lock-timeout "$LOCK_WAIT_SECONDS"', text)
        self.assertIn("coordinator ${coordinator} owns periodic reconciliation", text)
        self.assertIn("StrictHostKeyChecking=yes", (LIB_DIR / "config.sh").read_text())
        self.assertNotIn("10.213.0.3 ", text)

    def test_current_non_coordinator_reactivates_ingress_without_delegating(self) -> None:
        text = SYNC_PATH.read_text(encoding="utf-8")
        local = text[text.index("local_reconcile()") : text.index("main()")]
        early_return = local[
            local.index('[[ "$node" != "$coordinator" ]]')
            : local.index("if ! (")
        ]
        self.assertIn("enable_ingress_node", early_return)
        self.assertLess(
            early_return.index("enable_ingress_node"),
            early_return.index("return 0"),
        )
        enable = text[text.index("enable_ingress_node()") : text.index("install_node()")]
        self.assertIn(
            "systemctl start --no-block app-ha-haproxy-ingress.service",
            enable,
        )
        self.assertNotIn(
            "systemctl is-active --quiet app-ha-haproxy-route-sync.service",
            enable,
        )

    def test_quorum_loss_immediately_before_desired_commit_fails_closed(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            trace = root / "trace"
            completed = self.run_sourced(
                f"""
MAX_MOX_HOSTS=2
PROXMOX_CLUSTER_NAME=app-ha-test
CLUSTER_STATE_DIR={root!s}
WORK_DIR={root!s}
assert_coordinator_online_and_quorate() {{
  printf 'quorum-check\\n' >>{trace!s}
  return 1
}}
registry_cmd() {{
  printf 'registry-write\\n' >>{trace!s}
}}
publish_desired_generation mox1 "{'a' * 64}" "{'b' * 64}" 1 \\
  "{root!s}/ingress-plan.json"
""",
                expected=1,
            )
            self.assertEqual(trace.read_text(encoding="utf-8"), "quorum-check\n")
            self.assertNotIn("registry-write", completed.stdout + completed.stderr)

    def test_offline_return_with_stale_generation_disables_local_ingress(
        self,
    ) -> None:
        generation = "a" * 64
        old_generation = "b" * 64
        completed = self.run_sourced(
            f"""
hostname() {{ printf 'mox2\\n'; }}
local_vmid() {{ printf '9112\\n'; }}
assert_local_node_online_and_quorate() {{ return 0; }}
registry_cmd() {{
  cat <<'JSON'
{{
  "desired": {{"generation": "{generation}"}},
  "nodes": [{{
    "node": "mox2",
    "desired_generation": "{generation}",
    "applied_generation": "{old_generation}",
    "state": "applied"
  }}]
}}
JSON
}}
disable_ingress_node() {{ printf 'disabled:%s:%s\\n' "$1" "$2"; }}
node_generation_is_current() {{
  printf 'stale generation should not be accepted\\n' >&2
  return 9
}}
if assert_local_current; then
  exit 8
fi
""",
        )
        self.assertEqual(completed.stdout, "disabled:mox2:9112\n")

    def test_host_setup_uses_local_fixed_ingress_and_generic_names(self) -> None:
        text = HOST_SETUP_PATH.read_text(encoding="utf-8")
        self.assertIn("dnat ip to $lxc_ip", text)
        self.assertIn('private_gateway="$3"', text)
        self.assertIn("app_ha_haproxy_ingress", text)
        self.assertIn("app-ha-haproxy-ingress.service", text)
        self.assertIn("app-ha-haproxy-route-sync.service", text)
        self.assertIn("app-ha-haproxy-route-sync.timer", text)
        self.assertIn("--assert-local-current", text)
        self.assertIn("install_shared_orchestration_tools", text)
        self.assertIn("sync_haproxy_routes", text)
        self.assertNotIn("10.213.0.3 ", text)


# Node-facing operations are replaced with a small simulated cluster. The
# renderer comparison, tar transfer, classification, publication, and commit
# ordering are the real sync implementation.
ROLLOUT_HARNESS = r"""
MAX_MOX_HOSTS=3
PROXMOX_PRIVATE_BRIDGE=vmbr-private
STATE=@STATE@
WORK_DIR=@WORK@
COORDINATOR_NODES_JSON='[]'
nodes=(mox1 mox2)
vmids=(9111 9112)
lxc_ips=(10.213.0.21/24 10.213.0.22/24)
lxc_gateways=(10.213.0.11 10.213.0.12)
needs_commit=(false false)
compatible=(false false)
previous_generations=("" "")
failures=()
trace() { printf '%s\n' "$*" >>"$STATE/trace"; }
live() { cat "$STATE/live-$1"; }
assert_coordinator_online_and_quorate() { [[ ! -e "$STATE/no-quorum" ]]; }
verify_target_membership() { :; }
node_is_online() { return 0; }
node_generation_is_current() { [[ "$(live "$1")" == "$3" ]]; }
stage_node() {
  [[ ! -e "$STATE/fail-stage-$1" ]] || return 1
  trace "stage $1"
}
prune_node_generations() { :; }
node_ingress_facts() { printf '%s %s\n' "$(live "$1")" "$(live "$1")"; }
fetch_node_generation() { tar -C "$STATE/generations/$3" -cf "$4" .; }
disable_ingress_node() { trace "disable $1"; }
enable_ingress_node() { trace "enable $1 ${3:0:8}"; }
install_node() {
  if [[ -e "$STATE/fail-install-$1" ]]; then
    cp "$STATE/fail-install-$1" "$STATE/live-$1"
    trace "install-failed $1"
    return 1
  fi
  printf '%s\n' "$4" >"$STATE/live-$1"
  trace "install $1"
}
mark_ingress_state() { trace "mark $1 $3"; }
registry_cmd() {
  local -a args=("$@")
  local index generation="" transition=null
  case "$*" in
    ingress-status) cat "$STATE/status.json" ;;
    *ingress-begin*)
      for ((index = 0; index < ${#args[@]}; index += 1)); do
        [[ "${args[index]}" != --generation ]] ||
          generation="${args[index + 1]}"
        if [[ "${args[index]}" == --compatible ]]; then
          transition='{}'
          trace "allow ${args[index + 1]:0:13}"
        fi
      done
      trace "begin ${generation:0:8}"
      printf '{"desired":{"generation":"%s"},"nodes":[],"transition":%s}\n' \
        "$generation" "$transition"
      ;;
    ingress-finalize*) trace "finalize" ;;
    *) return 1 ;;
  esac
}
rollout_generation mox1 "@CANDIDATE@" tx-test "$WORK_DIR/bundle.tar" \
  "@DIGEST@" 1
printf '%s\n' "${failures[@]}" >"$STATE/failures"
"""


class RolloutShellTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.state = self.root / "state"
        self.generations = self.state / "generations"
        self.work = self.root / "work"
        self.generations.mkdir(parents=True)
        self.work.mkdir()

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def prepare(
        self,
        previous: list[dict],
        candidate: list[dict],
        *,
        live: dict[str, str] | None = None,
        desired: str | None = None,
        allowances: tuple[tuple[str, str], ...] = (),
    ) -> tuple[str, str]:
        previous_generation, _ = write_rendered(self.generations, previous)
        candidate_generation, candidate_dir = write_rendered(
            self.generations, candidate
        )
        shutil.copytree(candidate_dir, self.work / "generation")
        desired = desired or previous_generation
        live = live or {"mox1": previous_generation, "mox2": previous_generation}
        for node, generation in live.items():
            (self.state / f"live-{node}").write_text(generation + "\n")
        status = {
            "desired": {"generation": desired},
            "nodes": [
                {
                    "node": node,
                    "desired_generation": desired,
                    "applied_generation": generation,
                    "state": "applied" if generation == desired else "staged",
                }
                for node, generation in live.items()
            ],
            "active_allowances": [
                {"node": node, "generation": generation}
                for node, generation in allowances
            ],
        }
        (self.state / "status.json").write_text(json.dumps(status))
        return previous_generation, candidate_generation

    def rollout(self, candidate: str, *, expected: int = 0) -> list[str]:
        script = (
            ROLLOUT_HARNESS.replace("@STATE@", str(self.state))
            .replace("@WORK@", str(self.work))
            .replace("@CANDIDATE@", candidate)
            .replace("@DIGEST@", "d" * 64)
        )
        completed = subprocess.run(
            ["bash", "-c", f'source "$1"\n{script}', "bash", str(SYNC_PATH)],
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
        trace = self.state / "trace"
        return trace.read_text().splitlines() if trace.exists() else []

    def failures(self) -> str:
        return (self.state / "failures").read_text()

    def test_adding_staging_route_keeps_production_ingress_enabled(self) -> None:
        previous, candidate = self.prepare([PRODUCTION], [PRODUCTION, STAGE_ONE])
        trace = self.rollout(candidate)
        self.assertFalse([line for line in trace if line.startswith("disable")])
        self.assertIn(f"allow mox1={previous[:8]}", trace)
        self.assertIn(f"allow mox2={previous[:8]}", trace)
        self.assertLess(trace.index(f"begin {candidate[:8]}"), trace.index("install mox1"))
        self.assertLess(trace.index("install mox2"), trace.index("finalize"))
        self.assertIn(f"enable mox2 {candidate[:8]}", trace)
        self.assertEqual(self.failures().strip(), "")

    def test_removing_staging_route_keeps_production_ingress_enabled(self) -> None:
        _, candidate = self.prepare([PRODUCTION, STAGE_ONE], [PRODUCTION])
        trace = self.rollout(candidate)
        self.assertFalse([line for line in trace if line.startswith("disable")])
        self.assertIn("finalize", trace)

    def test_consecutive_changes_while_a_node_lags_stay_enabled(self) -> None:
        older, _ = write_rendered(self.generations, [PRODUCTION])
        middle, candidate = self.prepare(
            [PRODUCTION, STAGE_ONE],
            [PRODUCTION, STAGE_ONE, STAGE_TWO],
            live={"mox1": "", "mox2": older},
            allowances=(("mox2", older),),
        )
        (self.state / "live-mox1").write_text(middle + "\n")
        status = json.loads((self.state / "status.json").read_text())
        status["nodes"] = [
            {"node": "mox1", "desired_generation": middle,
             "applied_generation": middle, "state": "applied"},
            {"node": "mox2", "desired_generation": middle,
             "applied_generation": older, "state": "staged"},
        ]
        (self.state / "status.json").write_text(json.dumps(status))
        trace = self.rollout(candidate)
        self.assertFalse([line for line in trace if line.startswith("disable")])
        self.assertIn(f"allow mox1={middle[:8]}", trace)
        self.assertIn(f"allow mox2={older[:8]}", trace)

    def test_incompatible_production_change_fails_closed_before_publish(self) -> None:
        _, candidate = self.prepare([PRODUCTION], [route(ip="10.213.0.32")])
        trace = self.rollout(candidate)
        begin = trace.index(f"begin {candidate[:8]}")
        self.assertLess(trace.index("disable mox1"), begin)
        self.assertLess(trace.index("disable mox2"), begin)
        self.assertFalse([line for line in trace if line.startswith("allow")])
        self.assertNotIn("finalize", trace)

    def test_unauthorized_live_generation_fails_closed(self) -> None:
        _, candidate = self.prepare(
            [PRODUCTION], [PRODUCTION, STAGE_ONE], desired="e" * 64
        )
        trace = self.rollout(candidate)
        self.assertIn("disable mox1", trace)
        self.assertIn("disable mox2", trace)

    def test_staging_failure_changes_nothing(self) -> None:
        _, candidate = self.prepare([PRODUCTION], [PRODUCTION, STAGE_ONE])
        (self.state / "fail-stage-mox2").touch()
        trace = self.rollout(candidate)
        self.assertEqual(trace, ["stage mox1"])
        self.assertIn("mox2: staging failed", self.failures())

    def test_reload_failure_with_restored_previous_keeps_node_serving(self) -> None:
        previous, candidate = self.prepare([PRODUCTION], [PRODUCTION, STAGE_ONE])
        (self.state / "fail-install-mox2").write_text(previous + "\n")
        trace = self.rollout(candidate)
        self.assertNotIn("disable mox2", trace)
        self.assertNotIn("mark mox2 reject-only", trace)
        self.assertIn(f"enable mox1 {candidate[:8]}", trace)
        self.assertNotIn("finalize", trace)
        self.assertIn("remains live within its rollout window", self.failures())

    def test_reload_failure_without_provable_previous_fails_closed(self) -> None:
        _, candidate = self.prepare([PRODUCTION], [PRODUCTION, STAGE_ONE])
        (self.state / "fail-install-mox2").write_text("broken\n")
        trace = self.rollout(candidate)
        self.assertIn("disable mox2", trace)
        self.assertIn("mark mox2 reject-only", trace)
        self.assertNotIn("finalize", trace)

    def test_quorum_loss_before_publish_leaves_desired_and_ingress_unchanged(
        self,
    ) -> None:
        _, candidate = self.prepare([PRODUCTION], [PRODUCTION, STAGE_ONE])
        (self.state / "no-quorum").touch()
        trace = self.rollout(candidate, expected=1)
        self.assertEqual(trace, ["stage mox1", "stage mox2"])

    def test_current_generation_is_not_reloaded(self) -> None:
        _, candidate = self.prepare([PRODUCTION], [PRODUCTION])
        trace = self.rollout(candidate)
        self.assertFalse(
            [line for line in trace if line.split()[0] in {"stage", "install", "disable"}]
        )
        self.assertIn(f"enable mox1 {candidate[:8]}", trace)


LOCAL_HARNESS = r"""
MAX_MOX_HOSTS=2
STATE=@STATE@
LOCAL_INGRESS_MARKER="$STATE/marker"
LOCAL_CHECK_RETRY_SECONDS=0
hostname() { printf 'mox2\n'; }
local_vmid() { printf '9112\n'; }
assert_local_node_online_and_quorate() { [[ ! -e "$STATE/no-quorum" ]]; }
online_nodes_json() {
  printf '[{"node":"mox1","status":"online"},{"node":"mox2","status":"online"}]'
}
registry_cmd() { cat "$STATE/status.json"; }
local_active_generation() { cat "$STATE/live"; }
node_generation_is_current() {
  local remaining
  printf 'x' >>"$STATE/checks"
  remaining="$(< "$STATE/inconsistent")"
  if ((remaining > 0)); then
    printf '%s' "$((remaining - 1))" >"$STATE/inconsistent"
    return 1
  fi
  [[ "$3" == "$(< "$STATE/live")" ]]
}
disable_ingress_node() { printf 'disabled:%s\n' "$1"; }
enable_ingress_node() { printf 'enabled:%s:%s\n' "$1" "${3:0:8}"; }
delegate_to_coordinator() { printf 'delegated\n'; }
@BODY@
"""


class LocalReconcileShellTest(unittest.TestCase):
    PREVIOUS = "1" * 64
    DESIRED = "2" * 64

    def run_local(
        self,
        body: str,
        *,
        allowances: tuple[tuple[str, str], ...] = (),
        no_quorum: bool = False,
        expected: int = 0,
        marker: str | None = PREVIOUS,
        live: str = PREVIOUS,
        inconsistent: int = 0,
    ) -> str:
        with tempfile.TemporaryDirectory() as temporary:
            state = Path(temporary)
            if marker is not None:
                (state / "marker").write_text(marker + "\n")
            (state / "live").write_text(live)
            (state / "inconsistent").write_text(str(inconsistent))
            (state / "checks").write_text("")
            if no_quorum:
                (state / "no-quorum").touch()
            (state / "status.json").write_text(
                json.dumps(
                    {
                        "desired": {"generation": self.DESIRED},
                        "nodes": [
                            {
                                "node": "mox2",
                                "desired_generation": self.DESIRED,
                                "applied_generation": self.PREVIOUS,
                                "state": "staged",
                            }
                        ],
                        "active_allowances": [
                            {"node": node, "generation": generation}
                            for node, generation in allowances
                        ],
                    }
                )
            )
            script = LOCAL_HARNESS.replace("@STATE@", str(state)).replace(
                "@BODY@", body
            )
            completed = subprocess.run(
                ["bash", "-c", f'source "$1"\n{script}', "bash", str(SYNC_PATH)],
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
            return completed.stdout

    def test_reconcile_during_rollout_keeps_authorized_previous_enabled(self) -> None:
        output = self.run_local(
            "local_reconcile", allowances=(("mox2", self.PREVIOUS),)
        )
        self.assertNotIn("disabled", output)
        self.assertNotIn("delegated", output)
        self.assertIn(f"enabled:mox2:{self.PREVIOUS[:8]}", output)

    def test_allowance_for_another_node_does_not_authorize(self) -> None:
        output = self.run_local(
            "assert_local_current || printf 'refused\\n'",
            allowances=(("mox1", self.PREVIOUS),),
        )
        self.assertEqual(output, "disabled:mox2\nrefused\n")

    def test_expired_allowance_fails_closed(self) -> None:
        output = self.run_local("assert_local_current || printf 'refused\\n'")
        self.assertEqual(output, "disabled:mox2\nrefused\n")

    def test_quorum_loss_fails_closed_despite_allowance(self) -> None:
        output = self.run_local(
            "assert_local_current 2>/dev/null || printf 'refused\\n'",
            allowances=(("mox2", self.PREVIOUS),),
            no_quorum=True,
        )
        self.assertEqual(output, "disabled:mox2\nrefused\n")

    def test_reloaded_generation_ahead_of_marker_stays_enabled(self) -> None:
        output = self.run_local(
            "local_reconcile",
            allowances=(("mox2", self.PREVIOUS),),
            live=self.DESIRED,
        )
        self.assertNotIn("disabled", output)
        self.assertNotIn("delegated", output)
        self.assertIn(f"enabled:mox2:{self.DESIRED[:8]}", output)

    def test_mid_commit_snapshot_is_retried_without_disabling(self) -> None:
        output = self.run_local(
            "assert_local_current && printf 'current:%s\\n' "
            '"${LOCAL_SERVING_GENERATION:0:8}"; '
            'printf "checks:%s\\n" "$(< "$STATE/checks")"',
            allowances=(("mox2", self.PREVIOUS),),
            inconsistent=2,
        )
        self.assertEqual(output, f"current:{self.PREVIOUS[:8]}\nchecks:xxx\n")

    def test_persistent_inconsistency_fails_closed_after_retries(self) -> None:
        output = self.run_local(
            "assert_local_current || printf 'refused\\n'; "
            'printf "checks:%s\\n" "$(< "$STATE/checks")"',
            allowances=(("mox2", self.PREVIOUS),),
            inconsistent=99,
        )
        self.assertEqual(output, "disabled:mox2\nrefused\nchecks:xxxx\n")

    def test_missing_marker_fails_closed(self) -> None:
        output = self.run_local(
            "assert_local_current || printf 'refused\\n'",
            allowances=(("mox2", self.PREVIOUS),),
            marker=None,
        )
        self.assertEqual(output, "disabled:mox2\nrefused\n")


if __name__ == "__main__":
    unittest.main()
