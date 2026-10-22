#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Check this mox host's internet connection and fall back through another.

Every five seconds the service probes the next target in a rotation of many
independent HTTPS endpoints, through this host's public interface. A probe
that fails moves on to the following targets; only when every target fails
is the connection considered down for that check. After three such checks in
a row, the guest-egress routing table is pointed at the first other mox host
through which a probe reaches the internet; after five good checks of this
host's own connection, it is pointed back.

Probes are HTTPS only (no UDP or ICMP), connect to addresses resolved while
the connection was healthy (so a probe through another host doesn't depend
on this host's DNS), and spread across the list so no single target sees
more than an occasional request. A target that keeps failing while the
others answer is retired.

  egress_health.py validate-targets   probe the shipped list; keep the working ones
  egress_health.py run                the service loop
  egress_health.py status             print the current state as JSON
"""

from __future__ import annotations

import argparse
import concurrent.futures
import datetime as dt
import json
import os
from pathlib import Path
import socket
import ssl
import subprocess
import sys
import time
from typing import Any, Callable, Sequence
from urllib.parse import urlsplit


INSTALL_DIR = Path(__file__).resolve().parent
SETTINGS = Path("/etc/app-ha/guest-network.env")
TARGETS_SOURCE = INSTALL_DIR / "egress_probe_targets.txt"
TARGETS_STATE = Path("/var/lib/app-ha/egress-targets.json")
STATUS_FILE = Path("/run/app-ha/egress-health.json")
GUEST_NETWORK = INSTALL_DIR / "guest_network.sh"

CHECK_INTERVAL_SECONDS = 5
PROBE_TIMEOUT_SECONDS = 4
PARALLEL_PROBES = 4
FAILURES_BEFORE_FALLBACK = 3
SUCCESSES_BEFORE_RETURN = 5
STRIKES_BEFORE_RETIREMENT = 5
MIN_ACTIVE_TARGETS = 5
WARN_BELOW_TARGETS = 15
RESOLVE_INTERVAL_SECONDS = 3600
GUEST_EGRESS_TABLE = 200
PEER_TABLE_BASE = 200
PEER_MARK_BASE = 0x100
SO_MARK = getattr(socket, "SO_MARK", 36)
GATEWAY_IF = "bmacgw0"
GATEWAY_PEER = "bmacgw0br"
FALLBACK_NFT_TABLE = "app_ha_egress_fallback"


def utc_now() -> str:
    return dt.datetime.now(dt.timezone.utc).replace(microsecond=0).isoformat()


def log(message: str) -> None:
    print(f"egress_health: {message}", flush=True)


def load_settings(path: Path = SETTINGS) -> dict[str, str]:
    settings: dict[str, str] = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line or line.startswith("#"):
            continue
        key, _, value = line.partition("=")
        settings[key] = value
    for key in (
        "BMAC_MOX_INDEX",
        "BMAC_PRIVATE_BRIDGE",
        "BMAC_PUBLIC_IF",
        "BMAC_PUBLIC_IP",
        "BMAC_PUBLIC_GATEWAY",
        "BMAC_MOX_FIRST",
        "BMAC_MAX_MOX_HOSTS",
    ):
        if not settings.get(key):
            raise SystemExit(f"egress_health: {key} is missing from {path}")
    return settings


def peer_address(settings: dict[str, str], index: int) -> str:
    prefix, _, first = settings["BMAC_MOX_FIRST"].rpartition(".")
    return f"{prefix}.{int(first) + index - 1}"


def fallback_order(own: int, max_hosts: int) -> list[int]:
    """Other mox slots, starting just after this one and wrapping around."""

    return [((own - 1 + step) % max_hosts) + 1 for step in range(1, max_hosts)]


def read_target_list(path: Path = TARGETS_SOURCE) -> list[str]:
    targets = []
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        parts = urlsplit(line)
        if parts.scheme != "https" or not parts.hostname:
            raise SystemExit(f"egress_health: not an HTTPS target: {line}")
        targets.append(line)
    if len(set(targets)) != len(targets):
        raise SystemExit("egress_health: the target list contains duplicates")
    return targets


def resolve(url: str) -> list[str]:
    host = urlsplit(url).hostname or ""
    try:
        infos = socket.getaddrinfo(host, 443, socket.AF_INET, socket.SOCK_STREAM)
    except OSError:
        return []
    return sorted({info[4][0] for info in infos})


Probe = Callable[[dict[str, Any], "int | None"], bool]


def https_probe(
    target: dict[str, Any],
    mark: int | None,
    *,
    source_ip: str | None = None,
    timeout: float = PROBE_TIMEOUT_SECONDS,
) -> bool:
    """One HEAD request to a cached address; any HTTP status line is success."""

    parts = urlsplit(target["url"])
    host = parts.hostname or ""
    path = parts.path or "/"
    if parts.query:
        path += "?" + parts.query
    context = ssl.create_default_context()
    for address in target.get("addresses") or []:
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        try:
            sock.settimeout(timeout)
            if mark is not None:
                sock.setsockopt(socket.SOL_SOCKET, SO_MARK, mark)
            elif source_ip:
                sock.bind((source_ip, 0))
            sock.connect((address, 443))
            with context.wrap_socket(sock, server_hostname=host) as tls:
                request = (
                    f"HEAD {path} HTTP/1.1\r\nHost: {host}\r\n"
                    "User-Agent: bmac-egress-health\r\nConnection: close\r\n\r\n"
                )
                tls.sendall(request.encode("ascii"))
                head = tls.recv(64)
            if head.startswith(b"HTTP/1.") or head.startswith(b"HTTP/2"):
                return True
        except (OSError, ssl.SSLError):
            pass
        finally:
            sock.close()
    return False


class Rotation:
    """Spread probes across the active targets and retire persistent failures."""

    def __init__(self, targets: list[dict[str, Any]], probe: Probe) -> None:
        self.targets = targets
        self.probe = probe
        self.position = 0
        self.retired: list[dict[str, Any]] = []

    def active(self) -> list[dict[str, Any]]:
        return [target for target in self.targets if not target.get("retired")]

    def check(self, mark: int | None = None, *, account: bool = True) -> bool:
        """Probe the next target; on failure keep advancing until one answers.

        Returns False only when every active target failed. Strikes are only
        counted for this host's own connection (account=True), where a working
        target proves the failing one is at fault.
        """

        active = self.active()
        if not active:
            return False
        count = len(active)
        start = self.position % count
        order = [active[(start + step) % count] for step in range(count)]
        self.position = (start + 1) % count
        if self.probe(order[0], mark):
            if account:
                order[0]["strikes"] = 0
            return True
        failed = [order[0]]
        remaining = order[1:]
        while remaining:
            batch, remaining = remaining[:PARALLEL_PROBES], remaining[PARALLEL_PROBES:]
            with concurrent.futures.ThreadPoolExecutor(len(batch)) as pool:
                results = list(pool.map(lambda target: self.probe(target, mark), batch))
            for target, ok in zip(batch, results):
                if ok:
                    if account:
                        target["strikes"] = 0
                        for bad in failed:
                            self._strike(bad)
                    # Next check starts after the target that answered.
                    self.position = (active.index(target) + 1) % count
                    return True
                failed.append(target)
        return False

    def _strike(self, target: dict[str, Any]) -> None:
        target["strikes"] = int(target.get("strikes", 0)) + 1
        if (
            target["strikes"] >= STRIKES_BEFORE_RETIREMENT
            and len(self.active()) > MIN_ACTIVE_TARGETS
        ):
            target["retired"] = utc_now()
            self.retired.append(target)
            log(f"retired probe target {target['url']} after {target['strikes']} strikes")


class Health:
    """The own-connection/fallback state machine, independent of the network."""

    def __init__(
        self,
        rotation: Rotation,
        own_index: int,
        max_hosts: int,
        set_route: Callable[[int | None], None],
    ) -> None:
        self.rotation = rotation
        self.own_index = own_index
        self.max_hosts = max_hosts
        self.set_route = set_route
        self.own_ok = True
        self.failures = 0
        self.successes = 0
        self.fallback: int | None = None
        self.mode = "own"

    def tick(self) -> None:
        own = self.rotation.check(None)
        if own:
            self.failures = 0
            self.successes += 1
        else:
            self.successes = 0
            self.failures += 1
        if self.mode == "own":
            if self.failures >= FAILURES_BEFORE_FALLBACK:
                self.own_ok = False
                log("this host's internet connection is down; choosing a fallback host")
                self._choose_fallback()
            return
        # Falling back (or no host is reachable).
        if self.successes >= SUCCESSES_BEFORE_RETURN:
            log("this host's internet connection is back; returning guest egress to it")
            self.own_ok = True
            self.fallback = None
            self.mode = "own"
            self.set_route(None)
            return
        if self.fallback is not None and not self.rotation.check(
            PEER_MARK_BASE + self.fallback, account=False
        ):
            log(f"fallback host mox{self.fallback} no longer reaches the internet")
            self._choose_fallback()
        elif self.fallback is None:
            self._choose_fallback()

    def _choose_fallback(self) -> None:
        for index in fallback_order(self.own_index, self.max_hosts):
            if self.rotation.check(PEER_MARK_BASE + index, account=False):
                if self.fallback != index:
                    log(f"guest egress now falls back through mox{index}")
                self.fallback = index
                self.mode = "fallback"
                self.set_route(index)
                return
        if self.mode != "none":
            log("no other mox host reaches the internet; guest egress is unavailable")
        self.fallback = None
        self.mode = "none"
        self.set_route(None)


def run_command(*arguments: str) -> None:
    subprocess.run(arguments, check=True)


def set_guest_route(settings: dict[str, str], peer: int | None) -> None:
    table = str(GUEST_EGRESS_TABLE)
    if peer is None:
        run_command(
            "ip", "route", "replace", "default", "via", settings["BMAC_PUBLIC_GATEWAY"],
            "dev", settings["BMAC_PUBLIC_IF"], "table", table,
        )
        subprocess.run(
            ["nft", "delete", "table", "ip", FALLBACK_NFT_TABLE],
            check=False, stderr=subprocess.DEVNULL,
        )
        return
    run_command(
        "ip", "route", "replace", "default", "via", peer_address(settings, peer),
        "dev", settings["BMAC_PRIVATE_BRIDGE"], "table", table,
    )
    # The reply half of a falling-back flow returns from the other host
    # straight to the guest, so this host must not track (or reject) it.
    rules = (
        f"table ip {FALLBACK_NFT_TABLE} {{\n"
        "  chain prerouting {\n"
        "    type filter hook prerouting priority raw; policy accept;\n"
        f'    iifname "{GATEWAY_IF}" notrack\n'
        "  }\n"
        "}\n"
    )
    subprocess.run(["nft", "-f", "-"], input=rules, text=True, check=True)


def ensure_gateway() -> None:
    master = Path(f"/sys/class/net/{GATEWAY_PEER}/master")
    if Path(f"/sys/class/net/{GATEWAY_IF}").exists() and master.exists():
        return
    log("the guest gateway interface is missing or detached; recreating it")
    subprocess.run([str(GUEST_NETWORK), "ensure-gateway"], check=False)


def write_json(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.tmp")
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    os.replace(temporary, path)


def load_targets_state() -> dict[str, Any]:
    try:
        state = json.loads(TARGETS_STATE.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        raise SystemExit(
            f"egress_health: {TARGETS_STATE} is missing; run validate-targets first"
        )
    if not isinstance(state, dict) or not isinstance(state.get("targets"), list):
        raise SystemExit(f"egress_health: {TARGETS_STATE} is malformed")
    return state


def command_validate_targets(settings: dict[str, str]) -> int:
    urls = read_target_list()
    candidates = [{"url": url, "addresses": resolve(url), "strikes": 0} for url in urls]
    with concurrent.futures.ThreadPoolExecutor(8) as pool:
        results = list(
            pool.map(
                lambda target: https_probe(target, None, source_ip=settings["BMAC_PUBLIC_IP"]),
                candidates,
            )
        )
    working = [target for target, ok in zip(candidates, results) if ok]
    for target, ok in zip(candidates, results):
        if not ok:
            log(f"skipping unreachable probe target {target['url']}")
    write_json(
        TARGETS_STATE,
        {"validated_at": utc_now(), "resolved_at": utc_now(), "targets": working},
    )
    print(f"{len(working)} of {len(candidates)} probe targets are reachable")
    if len(working) < MIN_ACTIVE_TARGETS:
        print(f"ERROR: fewer than {MIN_ACTIVE_TARGETS} probe targets work", file=sys.stderr)
        return 1
    if len(working) < WARN_BELOW_TARGETS:
        print(f"WARNING: only {len(working)} probe targets work", file=sys.stderr)
    return 0


def command_run(settings: dict[str, str]) -> int:
    state = load_targets_state()
    public_ip = settings["BMAC_PUBLIC_IP"]

    def probe(target: dict[str, Any], mark: int | None) -> bool:
        return https_probe(target, mark, source_ip=public_ip)

    rotation = Rotation(state["targets"], probe)
    health = Health(
        rotation,
        int(settings["BMAC_MOX_INDEX"]),
        int(settings["BMAC_MAX_MOX_HOSTS"]),
        lambda peer: set_guest_route(settings, peer),
    )
    set_guest_route(settings, None)
    last_resolved = time.monotonic()
    while True:
        started = time.monotonic()
        ensure_gateway()
        health.tick()
        if health.own_ok and started - last_resolved >= RESOLVE_INTERVAL_SECONDS:
            for target in rotation.active():
                addresses = resolve(target["url"])
                if addresses:
                    target["addresses"] = addresses
            state["resolved_at"] = utc_now()
            last_resolved = started
        write_json(TARGETS_STATE, state)
        write_json(
            STATUS_FILE,
            {
                "updated_at": utc_now(),
                "own_connection_ok": health.own_ok,
                "mode": health.mode,
                "fallback_host": f"mox{health.fallback}" if health.fallback else None,
                "consecutive_failures": health.failures,
                "consecutive_successes": health.successes,
                "active_targets": len(rotation.active()),
                "retired_targets": [
                    target["url"] for target in state["targets"] if target.get("retired")
                ],
            },
        )
        time.sleep(max(0.0, CHECK_INTERVAL_SECONDS - (time.monotonic() - started)))


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("command", choices=("validate-targets", "run", "status"))
    args = parser.parse_args(argv)
    if args.command == "status":
        try:
            print(STATUS_FILE.read_text(encoding="utf-8"), end="")
        except OSError:
            print("{}")
            return 1
        return 0
    settings = load_settings()
    if args.command == "validate-targets":
        return command_validate_targets(settings)
    return command_run(settings)


if __name__ == "__main__":
    sys.exit(main())
