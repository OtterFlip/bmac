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
HOST_SETUP_PATH = LIB_DIR.parent / "hosts" / "setup_proxmox_host.sh"

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
            coordinator.index("stage_node "),
            coordinator.index("publish_desired_generation"),
        )
        self.assertLess(
            coordinator.index("publish_desired_generation"),
            coordinator.index("install_node "),
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


if __name__ == "__main__":
    unittest.main()
