#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Render the cluster's Cloudflare Tunnel configuration from the VM slots.

Every mox host runs a cloudflared replica of one tunnel with this same file.
It maps every FQDN of every registered production guest, and the FQDNs of all
of that guest's staging slots, to the slot's fixed private address. Staging
guests are never listed individually: their slots exist in advance, so
creating or removing a staging guest never changes this file.
"""

from __future__ import annotations

import argparse
import hashlib
import ipaddress
import json
import os
from pathlib import Path
import re
import sys
import tempfile
from typing import Any, Sequence


DOMAIN_LABEL_RE = re.compile(r"^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$")
PRODUCTION_RE = re.compile(r"^prod(?P<number>[1-9][0-9]*)$")
TUNNEL_ID_RE = re.compile(r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$")
ACCOUNT_ID_RE = re.compile(r"^[0-9a-f]{32}$")
MAX_PROD_VMS = 10
MAX_STAGING_VMS_PER_PROD = 9
DEFAULT_CREDENTIALS_FILE = "/etc/cloudflared/bmac-tunnel.json"
DEFAULT_METRICS = "127.0.0.1:20241"
GENERATION_PREFIX = "# bmac-tunnel-generation: "


class TunnelConfigError(ValueError):
    """Raised when slot settings or registry input cannot be rendered safely."""


class Slots:
    """The fixed VM address slots derived from cluster.conf."""

    def __init__(self, network: str, start: str, max_prod: int, max_staging: int) -> None:
        try:
            self.network = ipaddress.IPv4Network(network, strict=True)
            self.start = ipaddress.IPv4Address(start)
        except ValueError as exc:
            raise TunnelConfigError(f"invalid slot network or start: {exc}") from exc
        if isinstance(max_prod, bool) or not 1 <= max_prod <= MAX_PROD_VMS:
            raise TunnelConfigError(f"MAX_PROD_VMS must be 1-{MAX_PROD_VMS}")
        if isinstance(max_staging, bool) or not 0 <= max_staging <= MAX_STAGING_VMS_PER_PROD:
            raise TunnelConfigError(f"MAX_STAGING_VMS_PER_PROD must be 0-{MAX_STAGING_VMS_PER_PROD}")
        if (int(self.start) - int(self.network.network_address)) % 10 != 0:
            raise TunnelConfigError("VM_SLOT_START's last octet must end in 0")
        self.max_prod = max_prod
        self.max_staging = max_staging
        last = self.start + 10 * max_prod - 1
        hosts = list(self.network.hosts())
        if self.start not in self.network or last not in self.network or last > hosts[-1]:
            raise TunnelConfigError(f"VM slots {self.start}-{last} must be usable addresses in {self.network}")

    @property
    def first(self) -> ipaddress.IPv4Address:
        return self.start

    @property
    def last(self) -> ipaddress.IPv4Address:
        return self.start + 10 * self.max_prod - 1

    def production(self, number: int) -> ipaddress.IPv4Address:
        if not 1 <= number <= self.max_prod:
            raise TunnelConfigError(f"prod{number} is outside MAX_PROD_VMS={self.max_prod}")
        return self.start + 10 * (number - 1)

    def staging(self, production: int, stage: int) -> ipaddress.IPv4Address:
        if not 1 <= stage <= self.max_staging:
            raise TunnelConfigError(
                f"stage{stage}prod{production} is outside MAX_STAGING_VMS_PER_PROD={self.max_staging}"
            )
        return self.production(production) + stage


def validate_fqdn(value: Any, description: str) -> str:
    if not isinstance(value, str):
        raise TunnelConfigError(f"{description} must be a string")
    if value != value.strip().lower() or value.endswith(".") or "." not in value or len(value) > 253:
        raise TunnelConfigError(f"{description} is not a normalized FQDN: {value!r}")
    labels = value.split(".")
    if not all(DOMAIN_LABEL_RE.fullmatch(label) for label in labels):
        raise TunnelConfigError(f"{description} contains an invalid label: {value!r}")
    try:
        ipaddress.ip_address(value)
    except ValueError:
        return value
    raise TunnelConfigError(f"{description} may not be an IP address: {value!r}")


def production_entries(productions: Any, slots: Slots) -> list[dict[str, Any]]:
    """Validate registry production rows and return them sorted by number."""

    if not isinstance(productions, list):
        raise TunnelConfigError("production input must be a JSON list")
    entries = []
    seen_numbers: set[int] = set()
    for row in productions:
        if not isinstance(row, dict):
            raise TunnelConfigError("each production entry must be an object")
        name = row.get("name")
        match = PRODUCTION_RE.fullmatch(name) if isinstance(name, str) else None
        if match is None:
            raise TunnelConfigError(f"invalid production name: {name!r}")
        number = int(match.group("number"))
        if number in seen_numbers:
            raise TunnelConfigError(f"duplicate production entry: {name}")
        seen_numbers.add(number)
        expected_ip = slots.production(number)
        if row.get("ip") != str(expected_ip):
            raise TunnelConfigError(f"{name} address {row.get('ip')!r} is not its slot {expected_ip}")
        primary = validate_fqdn(row.get("primary"), f"{name} primary domain")
        aliases = row.get("aliases", [])
        if not isinstance(aliases, list):
            raise TunnelConfigError(f"{name} aliases must be a list")
        aliases = [validate_fqdn(alias, f"{name} alias") for alias in aliases]
        entries.append({"name": name, "number": number, "primary": primary, "aliases": aliases})
    return sorted(entries, key=lambda entry: entry["number"])


def routes(productions: Any, slots: Slots) -> list[dict[str, str]]:
    """Return every FQDN the tunnel serves with its slot address and owner."""

    result = []
    owners: dict[str, str] = {}
    for entry in production_entries(productions, slots):
        name, number, primary = entry["name"], entry["number"], entry["primary"]
        names = [(primary, name, slots.production(number))]
        names += [(alias, name, slots.production(number)) for alias in entry["aliases"]]
        for stage in range(1, slots.max_staging + 1):
            names.append((f"stage{stage}{name}.{primary}", f"stage{stage}{name}", slots.staging(number, stage)))
        for fqdn, resource, address in names:
            if fqdn in owners:
                raise TunnelConfigError(f"{fqdn} is claimed by both {owners[fqdn]} and {resource}")
            owners[fqdn] = resource
            result.append({"fqdn": fqdn, "resource": resource, "ip": str(address)})
    return result


def _quote(value: str) -> str:
    # JSON strings are valid YAML double-quoted scalars.
    return json.dumps(value)


def render(
    productions: Any,
    slots: Slots,
    *,
    tunnel_id: str,
    credentials_file: str = DEFAULT_CREDENTIALS_FILE,
    metrics: str = DEFAULT_METRICS,
) -> str:
    """Render the deterministic cloudflared config.yml text."""

    if not isinstance(tunnel_id, str) or not TUNNEL_ID_RE.fullmatch(tunnel_id):
        raise TunnelConfigError("CLOUDFLARE_TUNNEL_ID must be a lowercase tunnel UUID")
    body = [
        f"tunnel: {_quote(tunnel_id)}",
        f"credentials-file: {_quote(credentials_file)}",
        # QUIC runs over UDP, which FiberState's DDoS filter throttles.
        'protocol: "http2"',
        "no-autoupdate: true",
        f"metrics: {_quote(metrics)}",
        "ingress:",
    ]
    for route in routes(productions, slots):
        body += [
            f"  - hostname: {_quote(route['fqdn'])}",
            f"    service: {_quote('https://' + route['ip'] + ':443')}",
            "    originRequest:",
            f"      originServerName: {_quote(route['fqdn'])}",
        ]
    body.append('  - service: "http_status:404"')
    text = "\n".join(body) + "\n"
    generation = hashlib.sha256(text.encode("utf-8")).hexdigest()
    return f"{GENERATION_PREFIX}{generation}\n{text}"


def generation_of(text: str) -> str | None:
    first = text.split("\n", 1)[0]
    if first.startswith(GENERATION_PREFIX):
        value = first[len(GENERATION_PREFIX):]
        if re.fullmatch(r"[0-9a-f]{64}", value):
            return value
    return None


def credentials(account_id: str, tunnel_id: str, secret: str) -> str:
    """Return the credentials file cloudflared expects for a local tunnel."""

    if not isinstance(account_id, str) or not ACCOUNT_ID_RE.fullmatch(account_id):
        raise TunnelConfigError("CLOUDFLARE_ACCOUNT_ID must be a 32-character lowercase hex ID")
    if not isinstance(tunnel_id, str) or not TUNNEL_ID_RE.fullmatch(tunnel_id):
        raise TunnelConfigError("CLOUDFLARE_TUNNEL_ID must be a lowercase tunnel UUID")
    if not isinstance(secret, str) or not re.fullmatch(r"[A-Za-z0-9+/=]{32,}", secret):
        raise TunnelConfigError("CLOUDFLARE_TUNNEL_SECRET must be the base64 TunnelSecret value")
    return json.dumps({"AccountTag": account_id, "TunnelSecret": secret, "TunnelID": tunnel_id}) + "\n"


def _write_atomic(path: Path, text: str, mode: int) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(dir=path.parent, prefix=f".{path.name}.")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            handle.write(text)
            handle.flush()
            os.fsync(handle.fileno())
        os.chmod(temporary, mode)
        os.replace(temporary, path)
    except BaseException:
        Path(temporary).unlink(missing_ok=True)
        raise


def _slots_from_args(args: argparse.Namespace) -> Slots:
    return Slots(args.network, args.slot_start, args.max_prod, args.max_staging)


def _load_productions(path: str) -> Any:
    text = sys.stdin.read() if path == "-" else Path(path).read_text(encoding="utf-8")
    try:
        return json.loads(text)
    except json.JSONDecodeError as exc:
        raise TunnelConfigError(f"production input is not JSON: {exc}") from exc


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    def slot_arguments(command: argparse.ArgumentParser) -> None:
        command.add_argument("--network", required=True)
        command.add_argument("--slot-start", required=True)
        command.add_argument("--max-prod", type=int, required=True)
        command.add_argument("--max-staging", type=int, required=True)

    render_cmd = sub.add_parser("render", help="render config.yml from production JSON")
    slot_arguments(render_cmd)
    render_cmd.add_argument("--productions", required=True, help="JSON file, or - for stdin")
    render_cmd.add_argument("--tunnel-id", required=True)
    render_cmd.add_argument("--credentials-file", default=DEFAULT_CREDENTIALS_FILE)
    render_cmd.add_argument("--output", help="write atomically here instead of stdout")

    routes_cmd = sub.add_parser("routes", help="print every FQDN, owner and address as JSON")
    slot_arguments(routes_cmd)
    routes_cmd.add_argument("--productions", required=True, help="JSON file, or - for stdin")

    slots_cmd = sub.add_parser("slots", help="print every slot as JSON")
    slot_arguments(slots_cmd)

    creds_cmd = sub.add_parser("credentials", help="write the tunnel credentials file")
    creds_cmd.add_argument("--account-id", required=True)
    creds_cmd.add_argument("--tunnel-id", required=True)
    creds_cmd.add_argument("--secret-env", default="CLOUDFLARE_TUNNEL_SECRET",
                           help="environment variable holding the secret")
    creds_cmd.add_argument("--output", required=True)

    generation_cmd = sub.add_parser("generation", help="print an installed file's generation")
    generation_cmd.add_argument("path")

    args = parser.parse_args(argv)
    try:
        if args.command == "render":
            text = render(
                _load_productions(args.productions),
                _slots_from_args(args),
                tunnel_id=args.tunnel_id,
                credentials_file=args.credentials_file,
            )
            if args.output:
                _write_atomic(Path(args.output), text, 0o644)
                print(generation_of(text))
            else:
                sys.stdout.write(text)
        elif args.command == "routes":
            print(json.dumps(routes(_load_productions(args.productions), _slots_from_args(args)), indent=2))
        elif args.command == "slots":
            slots = _slots_from_args(args)
            rows = []
            for number in range(1, slots.max_prod + 1):
                rows.append({"resource": f"prod{number}", "ip": str(slots.production(number))})
                for stage in range(1, slots.max_staging + 1):
                    rows.append({"resource": f"stage{stage}prod{number}", "ip": str(slots.staging(number, stage))})
            print(json.dumps(rows, indent=2))
        elif args.command == "credentials":
            text = credentials(args.account_id, args.tunnel_id, os.environ.get(args.secret_env, ""))
            _write_atomic(Path(args.output), text, 0o640)
        elif args.command == "generation":
            path = Path(args.path)
            generation = generation_of(path.read_text(encoding="utf-8")) if path.is_file() else None
            if generation is None:
                return 1
            print(generation)
    except TunnelConfigError as exc:
        print(f"tunnel_config: {exc}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
