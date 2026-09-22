#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Validate registry routes and render a deterministic HAProxy generation."""

from __future__ import annotations

import argparse
import hashlib
import ipaddress
import json
from pathlib import Path
import re
import shutil
import sys
import tempfile
from typing import Any, Sequence


SCHEMA_VERSION = 1
ROUTE_KEYS = {
    "domain",
    "resource",
    "kind",
    "purpose",
    "ip",
    "http_port",
    "https_port",
}
DOMAIN_LABEL_RE = re.compile(r"^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$")
RESOURCE_RE = re.compile(
    r"^(?:prod(?P<production>[1-9][0-9]*)|"
    r"stage(?P<stage>[1-9][0-9]*)prod[1-9][0-9]*)$"
)
PURPOSE_RE = re.compile(r"^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$")
BACKEND_RE = re.compile(r"^[A-Za-z][A-Za-z0-9_.-]{0,127}$")
SAFE_ABSOLUTE_PATH_RE = re.compile(r"^/[A-Za-z0-9._/-]+$")
MAX_ROUTES = 4096
MAX_INPUT_BYTES = 16 * 1024 * 1024
DEFAULT_GENERATION_ROOT = Path("/etc/haproxy/app-ha-generations")


class RouteError(ValueError):
    """Raised when registry route input cannot be rendered safely."""


def _expect_plain_int(value: Any, description: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int):
        raise RouteError(f"{description} must be an integer")
    if not 1 <= value <= 65535:
        raise RouteError(f"{description} must be between 1 and 65535")
    return value


def validate_domain(value: Any) -> str:
    if not isinstance(value, str):
        raise RouteError("route domain must be a string")
    if value != value.strip() or value != value.lower() or value.endswith("."):
        raise RouteError(f"route domain is not normalized: {value!r}")
    if not value or value.startswith("*.") or "://" in value:
        raise RouteError(f"route domain must be an exact hostname: {value!r}")
    if "/" in value or ":" in value or "." not in value or len(value) > 253:
        raise RouteError(f"route domain is not a bare fully-qualified hostname: {value!r}")
    try:
        ascii_value = value.encode("idna").decode("ascii")
    except UnicodeError as exc:
        raise RouteError(f"route domain cannot be converted to IDNA: {value!r}") from exc
    if ascii_value != value:
        raise RouteError(f"route domain must already be normalized ASCII: {value!r}")
    if any(not DOMAIN_LABEL_RE.fullmatch(label) for label in value.split(".")):
        raise RouteError(f"route domain contains an invalid label: {value!r}")
    try:
        ipaddress.ip_address(value)
    except ValueError:
        return value
    raise RouteError(f"route domain may not be an IP address: {value!r}")


def validate_resource(value: Any) -> str:
    if not isinstance(value, str) or not RESOURCE_RE.fullmatch(value):
        raise RouteError(f"route resource name is invalid: {value!r}")
    return value


def backend_names(resource: str) -> tuple[str, str, str]:
    names = (
        f"app_ha_http_{resource}",
        f"app_ha_tls_{resource}",
        f"app_ha_server_{resource}",
    )
    for name in names:
        if not BACKEND_RE.fullmatch(name):
            raise RouteError(f"derived HAProxy backend identifier is invalid: {name!r}")
    return names


def validate_generation_root(value: Path) -> Path:
    text = str(value)
    if (
        not value.is_absolute()
        or ".." in value.parts
        or not SAFE_ABSOLUTE_PATH_RE.fullmatch(text)
    ):
        raise RouteError(f"HAProxy generation root is unsafe: {text!r}")
    return value


def _parse_ipv4(value: Any, description: str) -> ipaddress.IPv4Address:
    if not isinstance(value, str):
        raise RouteError(f"{description} must be a string")
    try:
        address = ipaddress.ip_address(value)
    except ValueError as exc:
        raise RouteError(f"{description} is invalid: {value!r}") from exc
    if not isinstance(address, ipaddress.IPv4Address) or str(address) != value:
        raise RouteError(f"{description} must be canonical IPv4: {value!r}")
    if address.is_unspecified or address.is_loopback or address.is_multicast:
        raise RouteError(f"{description} is not a routable backend address: {value!r}")
    return address


def _parse_network(value: str) -> ipaddress.IPv4Network:
    try:
        network = ipaddress.ip_network(value, strict=True)
    except ValueError as exc:
        raise RouteError(f"backend network is invalid: {value!r}") from exc
    if not isinstance(network, ipaddress.IPv4Network):
        raise RouteError("backend network must be IPv4")
    return network


def _parse_range(
    start_value: str,
    end_value: str,
    network: ipaddress.IPv4Network,
    description: str,
) -> tuple[ipaddress.IPv4Address, ipaddress.IPv4Address]:
    start = _parse_ipv4(start_value, f"{description} start")
    end = _parse_ipv4(end_value, f"{description} end")
    if start not in network or end not in network or int(start) > int(end):
        raise RouteError(f"{description} must be an ordered subset of {network}")
    return start, end


def _validate_route_object(
    value: Any,
    *,
    network: ipaddress.IPv4Network,
    production_range: tuple[ipaddress.IPv4Address, ipaddress.IPv4Address],
    staging_range: tuple[ipaddress.IPv4Address, ipaddress.IPv4Address],
) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise RouteError("every route must be a JSON object")
    unknown = sorted(set(value) - ROUTE_KEYS)
    missing = sorted(ROUTE_KEYS - set(value))
    if unknown:
        raise RouteError(f"route has unknown fields: {', '.join(unknown)}")
    if missing:
        raise RouteError(f"route is missing fields: {', '.join(missing)}")

    domain = validate_domain(value["domain"])
    resource = validate_resource(value["resource"])
    backend_names(resource)

    kind = value["kind"]
    if kind not in {"production", "staging"}:
        raise RouteError(f"route kind is invalid: {kind!r}")
    resource_match = RESOURCE_RE.fullmatch(resource)
    assert resource_match is not None
    resource_kind = (
        "staging" if resource_match.group("stage") is not None else "production"
    )
    if kind != resource_kind:
        raise RouteError(
            f"route kind {kind!r} does not match resource {resource!r}"
        )
    purpose = value["purpose"]
    if (
        not isinstance(purpose, str)
        or len(purpose) > 63
        or not PURPOSE_RE.fullmatch(purpose)
    ):
        raise RouteError(f"route purpose is invalid: {purpose!r}")

    address = _parse_ipv4(value["ip"], "route backend IP")
    if address not in network:
        raise RouteError(f"route backend IP {address} is outside {network}")
    allowed_start, allowed_end = (
        production_range if kind == "production" else staging_range
    )
    if not int(allowed_start) <= int(address) <= int(allowed_end):
        raise RouteError(
            f"{kind} backend IP {address} is outside "
            f"{allowed_start}-{allowed_end}"
        )

    http_port = _expect_plain_int(value["http_port"], "route HTTP port")
    https_port = _expect_plain_int(value["https_port"], "route HTTPS port")
    return {
        "domain": domain,
        "resource": resource,
        "kind": kind,
        "purpose": purpose,
        "ip": str(address),
        "http_port": http_port,
        "https_port": https_port,
    }


def validate_routes(
    value: Any,
    *,
    backend_network: str,
    production_ip_start: str,
    production_ip_end: str,
    staging_ip_start: str,
    staging_ip_end: str,
) -> list[dict[str, Any]]:
    if not isinstance(value, list):
        raise RouteError("list-routes input must be a JSON array")
    if len(value) > MAX_ROUTES:
        raise RouteError(f"list-routes input exceeds {MAX_ROUTES} routes")

    network = _parse_network(backend_network)
    production_range = _parse_range(
        production_ip_start, production_ip_end, network, "production range"
    )
    staging_range = _parse_range(
        staging_ip_start, staging_ip_end, network, "staging range"
    )
    if not int(production_range[1]) < int(staging_range[0]):
        raise RouteError("production and staging backend ranges must not overlap")

    routes = [
        _validate_route_object(
            route,
            network=network,
            production_range=production_range,
            staging_range=staging_range,
        )
        for route in value
    ]
    routes.sort(key=lambda route: (route["domain"], route["resource"]))

    domains: dict[str, str] = {}
    resources: dict[str, tuple[str, int, int, str, str]] = {}
    addresses: dict[str, str] = {}
    for route in routes:
        domain = route["domain"]
        resource = route["resource"]
        if domain in domains:
            raise RouteError(
                f"duplicate route domain {domain!r} for "
                f"{domains[domain]!r} and {resource!r}"
            )
        domains[domain] = resource

        target = (
            route["ip"],
            route["http_port"],
            route["https_port"],
            route["kind"],
            route["purpose"],
        )
        prior_target = resources.get(resource)
        if prior_target is not None and prior_target != target:
            raise RouteError(f"resource {resource!r} has inconsistent backend targets")
        resources[resource] = target

        prior_resource = addresses.get(route["ip"])
        if prior_resource is not None and prior_resource != resource:
            raise RouteError(
                f"backend IP {route['ip']} is shared by "
                f"{prior_resource!r} and {resource!r}"
            )
        addresses[route["ip"]] = resource
    return routes


def routing_generation(routes: Sequence[dict[str, Any]]) -> str:
    routing_data = [
        {
            "domain": route["domain"],
            "resource": route["resource"],
            "ip": route["ip"],
            "http_port": route["http_port"],
            "https_port": route["https_port"],
        }
        for route in routes
    ]
    canonical = json.dumps(
        {
            "renderer_schema_version": SCHEMA_VERSION,
            "routes": routing_data,
        },
        sort_keys=True,
        separators=(",", ":"),
        ensure_ascii=True,
    ).encode("ascii")
    return hashlib.sha256(canonical).hexdigest()


def _resource_targets(
    routes: Sequence[dict[str, Any]],
) -> list[tuple[str, str, int, int]]:
    targets: dict[str, tuple[str, int, int]] = {}
    for route in routes:
        targets[route["resource"]] = (
            route["ip"],
            route["http_port"],
            route["https_port"],
        )
    return [
        (resource, *targets[resource])
        for resource in sorted(targets)
    ]


def render_files(
    routes: Sequence[dict[str, Any]],
    *,
    generation_root: Path = DEFAULT_GENERATION_ROOT,
) -> dict[str, str]:
    generation_root = validate_generation_root(generation_root)
    generation = routing_generation(routes)
    generation_dir = generation_root / generation
    http_map = "".join(
        f"{route['domain']} {backend_names(route['resource'])[0]}\n"
        for route in routes
    )
    tls_map = "".join(
        f"{route['domain']} {backend_names(route['resource'])[1]}\n"
        for route in routes
    )
    http_map_path = generation_dir / "maps" / "app-ha-http-host.map"
    tls_map_path = generation_dir / "maps" / "app-ha-tls-sni.map"

    lines = [
        "# Managed by app-ha HAProxy route synchronization. Do not edit.",
        f"# generation: {generation}",
        "global",
        "    log /dev/log local0",
        "    log /dev/log local1 notice",
        "    user haproxy",
        "    group haproxy",
        "    daemon",
        "",
        "defaults",
        "    log global",
        "    timeout connect 5s",
        "    timeout client  60s",
        "    timeout server  60s",
        "",
        "frontend app_ha_public_http",
        "    bind :80",
        "    mode http",
        "    option httplog",
        "    http-request set-header X-Forwarded-Proto http",
        (
            "    use_backend "
            f"%[req.hdr(host),lower,field(1,:),map({http_map_path},"
            "app_ha_reject_http)]"
        ),
        "    default_backend app_ha_reject_http",
        "",
        "frontend app_ha_public_tls",
        "    bind :443",
        "    mode tcp",
        "    option tcplog",
        "    tcp-request inspect-delay 5s",
        "    tcp-request content accept if { req.ssl_hello_type 1 }",
        (
            "    use_backend "
            f"%[req.ssl_sni,lower,map({tls_map_path},app_ha_reject_tls)]"
        ),
        "    default_backend app_ha_reject_tls",
        "",
        "backend app_ha_reject_http",
        "    mode http",
        "    http-request deny deny_status 404",
        "",
        "backend app_ha_reject_tls",
        "    mode tcp",
        "    server app_ha_reject 127.0.0.1:1 disabled",
    ]

    for resource, address, http_port, https_port in _resource_targets(routes):
        http_backend, tls_backend, server_name = backend_names(resource)
        lines.extend(
            [
                "",
                f"backend {http_backend}",
                "    mode http",
                "    option forwardfor",
                f"    server {server_name} {address}:{http_port} check",
                "",
                f"backend {tls_backend}",
                "    mode tcp",
                f"    server {server_name} {address}:{https_port} check",
            ]
        )
    lines.append("")
    return {
        "haproxy.cfg": "\n".join(lines),
        "maps/app-ha-http-host.map": http_map,
        "maps/app-ha-tls-sni.map": tls_map,
    }


def _sha256_text(value: str) -> str:
    return hashlib.sha256(value.encode("utf-8")).hexdigest()


def write_generation(
    output_dir: Path,
    routes: Sequence[dict[str, Any]],
    *,
    generation_root: Path = DEFAULT_GENERATION_ROOT,
) -> dict[str, Any]:
    files = render_files(routes, generation_root=generation_root)
    generation = routing_generation(routes)
    manifest = {
        "schema_version": SCHEMA_VERSION,
        "generation": generation,
        "route_count": len(routes),
        "resource_count": len({route["resource"] for route in routes}),
        "files": {
            name: _sha256_text(content)
            for name, content in sorted(files.items())
        },
    }

    output_dir = output_dir.resolve()
    output_dir.parent.mkdir(parents=True, exist_ok=True)
    temporary = Path(
        tempfile.mkdtemp(prefix=f".{output_dir.name}.", dir=output_dir.parent)
    )
    try:
        for relative, content in files.items():
            destination = temporary / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_text(content, encoding="utf-8", newline="\n")
            destination.chmod(0o644)
        (temporary / "manifest.json").write_text(
            json.dumps(manifest, sort_keys=True, indent=2, ensure_ascii=True) + "\n",
            encoding="utf-8",
            newline="\n",
        )
        (temporary / "manifest.json").chmod(0o644)
        if output_dir.exists():
            raise RouteError(f"output directory already exists: {output_dir}")
        temporary.rename(output_dir)
    except BaseException:
        shutil.rmtree(temporary, ignore_errors=True)
        raise
    return manifest


def _json_object_without_duplicate_keys(
    pairs: list[tuple[str, Any]],
) -> dict[str, Any]:
    value: dict[str, Any] = {}
    for key, nested in pairs:
        if key in value:
            raise RouteError(f"route input contains duplicate JSON key {key!r}")
        value[key] = nested
    return value


def read_routes(path_value: str) -> Any:
    try:
        if path_value == "-":
            text = sys.stdin.read(MAX_INPUT_BYTES + 1)
        else:
            path = Path(path_value)
            if path.stat().st_size > MAX_INPUT_BYTES:
                raise RouteError(f"route input exceeds {MAX_INPUT_BYTES} bytes")
            text = path.read_text(encoding="utf-8")
    except UnicodeError as exc:
        raise RouteError("route input must be valid UTF-8") from exc
    if len(text.encode("utf-8")) > MAX_INPUT_BYTES:
        raise RouteError(f"route input exceeds {MAX_INPUT_BYTES} bytes")
    try:
        return json.loads(text, object_pairs_hook=_json_object_without_duplicate_keys)
    except json.JSONDecodeError as exc:
        raise RouteError(f"route input is not valid JSON: {exc}") from exc


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description=(
            "Render deterministic exact Host and TLS-SNI HAProxy routes from "
            "cluster_registry.py list-routes JSON."
        )
    )
    parser.add_argument(
        "--routes-json",
        default="-",
        help="list-routes JSON file, or - for stdin (default: -)",
    )
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--backend-network", required=True)
    parser.add_argument("--production-ip-start", required=True)
    parser.add_argument("--production-ip-end", required=True)
    parser.add_argument("--staging-ip-start", required=True)
    parser.add_argument("--staging-ip-end", required=True)
    parser.add_argument(
        "--generation-root",
        type=Path,
        default=DEFAULT_GENERATION_ROOT,
        help=(
            "absolute HAProxy generation root embedded in haproxy.cfg "
            f"(default: {DEFAULT_GENERATION_ROOT})"
        ),
    )
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        validate_generation_root(args.generation_root)
        routes = validate_routes(
            read_routes(args.routes_json),
            backend_network=args.backend_network,
            production_ip_start=args.production_ip_start,
            production_ip_end=args.production_ip_end,
            staging_ip_start=args.staging_ip_start,
            staging_ip_end=args.staging_ip_end,
        )
        manifest = write_generation(
            args.output_dir,
            routes,
            generation_root=args.generation_root,
        )
    except (OSError, RouteError) as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 1
    print(json.dumps(manifest, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
