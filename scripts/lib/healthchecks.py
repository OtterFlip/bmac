#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Healthchecks.io Management API client for BMAC's monitoring (workstation only).

The read-write API key comes from the HEALTHCHECKS_API_KEY environment
variable, or from the first line of stdin with --key-stdin; never from argv.
Nothing this tool prints contains the key, a check's UUID, or a ping URL:
checks are named by name only. The one place ping URLs are written is the
0600 file named by `sync --out`, which config rendering reads.

  healthchecks.py status --tag TAG
  healthchecks.py sync --desired FILE --tag TAG [--out FILE] [--dry-run]
                       [--delete-obsolete] [--max-checks N]
  healthchecks.py wait-ping --tag TAG --name NAME... --since ISO [--timeout S]
  healthchecks.py delete --tag TAG --name NAME...
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
from pathlib import Path
import re
import sys
import time
from typing import Any, Callable, Sequence
import urllib.error
import urllib.parse
import urllib.request


DEFAULT_API_URL = "https://healthchecks.io"
TIMEOUT_SECONDS = 20
RATE_LIMIT_WAITS = (5, 15, 30)
COMPARED_FIELDS = ("name", "slug", "desc", "timeout", "grace")
SECRET_FIELDS = ("uuid", "ping_url", "update_url", "pause_url", "resume_url")


class ApiError(Exception):
    """A failed API call, described without secrets."""


def parse_time(value: str | None) -> float | None:
    if not value:
        return None
    try:
        parsed = dt.datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=dt.timezone.utc)
    return parsed.timestamp()


def tag_set(value: Any) -> set[str]:
    return set(str(value or "").split())


class Client:
    def __init__(
        self,
        api_url: str,
        api_key: str,
        *,
        opener: Callable[..., Any] = urllib.request.urlopen,
        sleep: Callable[[float], None] = time.sleep,
    ) -> None:
        self.api_url = api_url.rstrip("/")
        self.api_key = api_key
        self.opener = opener
        self.sleep = sleep
        self.requests = 0

    def call(self, method: str, path: str, body: dict[str, Any] | None = None) -> tuple[int, Any]:
        data = json.dumps(body).encode("utf-8") if body is not None else None
        headers = {"X-Api-Key": self.api_key, "User-Agent": "bmac-healthchecks"}
        if data is not None:
            headers["Content-Type"] = "application/json"
        for wait in (*RATE_LIMIT_WAITS, None):
            request = urllib.request.Request(self.api_url + path, data=data, method=method, headers=headers)
            self.requests += 1
            try:
                with self.opener(request, timeout=TIMEOUT_SECONDS) as response:
                    status, text = response.status, response.read().decode("utf-8", "replace")
            except urllib.error.HTTPError as exc:
                status, text = exc.code, exc.read().decode("utf-8", "replace")
            except (urllib.error.URLError, OSError) as exc:
                reason = getattr(exc, "reason", exc)
                raise ApiError(f"{method} {path} failed: {reason}") from None
            if status == 429 and wait is not None:
                self.sleep(wait)
                continue
            break
        try:
            payload = json.loads(text) if text.strip() else None
        except ValueError:
            payload = None
        if status >= 400:
            message = payload.get("error") if isinstance(payload, dict) else None
            hint = {401: " (is HEALTHCHECKS_API_KEY a read-write project API key?)",
                    403: " (the API key is read-only or lacks access)"}.get(status, "")
            raise ApiError(f"{method} {path} returned HTTP {status}: {message or text.strip()[:200] or 'no body'}{hint}")
        return status, payload

    def checks(self, tag: str | None = None) -> list[dict[str, Any]]:
        path = "/api/v3/checks/" + (f"?tag={urllib.parse.quote(tag)}" if tag else "")
        _, payload = self.call("GET", path)
        if not isinstance(payload, dict) or not isinstance(payload.get("checks"), list):
            raise ApiError("GET /api/v3/checks/ returned an unexpected body")
        checks = payload["checks"]
        if tag:
            checks = [check for check in checks if tag in tag_set(check.get("tags"))]
        return checks


def public(check: dict[str, Any]) -> dict[str, Any]:
    """A check without anything that reveals its UUID."""

    return {
        "name": check.get("name"),
        "slug": check.get("slug"),
        "tags": check.get("tags"),
        "status": check.get("status"),
        "last_ping": check.get("last_ping"),
        "n_pings": check.get("n_pings"),
        "timeout": check.get("timeout"),
        "grace": check.get("grace"),
    }


def differs(existing: dict[str, Any], desired: dict[str, Any]) -> bool:
    if any(existing.get(field) != desired.get(field) for field in COMPARED_FIELDS):
        return True
    return tag_set(existing.get("tags")) != tag_set(desired.get("tags"))


def load_desired(path: Path, tag: str) -> list[dict[str, Any]]:
    value = json.loads(path.read_text(encoding="utf-8"))
    checks = value.get("checks") if isinstance(value, dict) else None
    if not isinstance(checks, list):
        raise ApiError(f"{path} has no checks list")
    names = set()
    for check in checks:
        for field in ("node", "name", "slug", "tags", "desc", "timeout", "grace"):
            if field not in check:
                raise ApiError(f"desired check is missing {field}")
        if tag not in tag_set(check["tags"]):
            raise ApiError(f"desired check {check['name']} lacks the cluster tag {tag}")
        if not re.fullmatch(r"[a-z0-9_-]{1,100}", check["slug"]):
            raise ApiError(f"desired check {check['name']} has an invalid slug")
        if check["name"] in names:
            raise ApiError(f"duplicate desired check {check['name']}")
        names.add(check["name"])
    return checks


def write_private_json(path: Path, value: Any) -> None:
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(descriptor, "w", encoding="utf-8") as handle:
        json.dump(value, handle, indent=2, sort_keys=True)
        handle.write("\n")


def command_sync(client: Client, args: argparse.Namespace) -> int:
    desired = load_desired(args.desired, args.tag)
    everything = client.checks()
    tagged = [check for check in everything if args.tag in tag_set(check.get("tags"))]
    by_name = {check.get("name"): check for check in tagged}
    used = len(everything)
    report: dict[str, Any] = {"dry_run": args.dry_run, "created": [], "updated": [], "unchanged": [],
                              "refused": [], "obsolete": [], "deleted": []}
    nodes: dict[str, Any] = {}
    for check in desired:
        existing = by_name.get(check["name"])
        if existing is None and used >= args.max_checks:
            report["refused"].append(check["name"])
            continue
        if existing is not None and not differs(existing, check):
            report["unchanged"].append(check["name"])
            result = existing
        else:
            report["created" if existing is None else "updated"].append(check["name"])
            if existing is None:
                used += 1
            if args.dry_run:
                continue
            body = {field: check[field] for field in ("name", "slug", "tags", "desc", "timeout", "grace")}
            body.update({"channels": "*", "unique": ["name"]})
            _, result = client.call("POST", "/api/v3/checks/", body)
            if not isinstance(result, dict) or not result.get("ping_url"):
                raise ApiError(f"creating {check['name']} returned no ping URL (is the key read-write?)")
        if result.get("ping_url"):
            nodes[check["node"]] = {"name": check["name"], "ping_url": result["ping_url"],
                                    "status": result.get("status"), "last_ping": result.get("last_ping")}
    wanted = {check["name"] for check in desired}
    for check in tagged:
        if check.get("name") in wanted:
            continue
        report["obsolete"].append(check.get("name"))
        if args.delete_obsolete and not args.dry_run:
            client.call("DELETE", f"/api/v3/checks/{urllib.parse.quote(str(check['uuid']))}")
            report["deleted"].append(check.get("name"))
            used -= 1
    report["budget"] = {"used": used, "max": args.max_checks}
    if args.out and not args.dry_run:
        write_private_json(args.out, {"nodes": nodes})
    print(json.dumps(report, indent=2))
    return 2 if report["refused"] else 0


def command_status(client: Client, args: argparse.Namespace) -> int:
    everything = client.checks()
    tagged = [public(check) for check in everything if args.tag in tag_set(check.get("tags"))]
    print(json.dumps({"project_checks": len(everything), "checks": sorted(tagged, key=lambda c: c["name"] or "")},
                     indent=2))
    return 0


def command_wait_ping(client: Client, args: argparse.Namespace) -> int:
    since = parse_time(args.since)
    if since is None:
        raise ApiError("--since must be an ISO 8601 time")
    deadline = time.monotonic() + args.timeout
    while True:
        by_name = {check.get("name"): check for check in client.checks(args.tag)}
        results = {}
        for name in args.name:
            check = by_name.get(name)
            last = parse_time(check.get("last_ping")) if check else None
            results[name] = {
                "status": check.get("status") if check else "missing",
                "last_ping": check.get("last_ping") if check else None,
                "ok": bool(check) and check.get("status") != "new" and last is not None and last >= since,
            }
        if all(result["ok"] for result in results.values()) or time.monotonic() >= deadline:
            break
        time.sleep(args.interval)
    print(json.dumps(results, indent=2))
    return 0 if all(result["ok"] for result in results.values()) else 1


def command_delete(client: Client, args: argparse.Namespace) -> int:
    by_name = {check.get("name"): check for check in client.checks(args.tag)}
    deleted, missing = [], []
    for name in args.name:
        check = by_name.get(name)
        if check is None:
            missing.append(name)
            continue
        client.call("DELETE", f"/api/v3/checks/{urllib.parse.quote(str(check['uuid']))}")
        deleted.append(name)
    print(json.dumps({"deleted": deleted, "missing": missing}, indent=2))
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--api-url", default=os.environ.get("HEALTHCHECKS_API_URL") or DEFAULT_API_URL)
    parser.add_argument("--key-stdin", action="store_true", help="read the API key from stdin")
    sub = parser.add_subparsers(dest="command", required=True)
    status = sub.add_parser("status")
    status.add_argument("--tag", required=True)
    status.set_defaults(handler=command_status)
    sync = sub.add_parser("sync")
    sync.add_argument("--desired", type=Path, required=True)
    sync.add_argument("--tag", required=True)
    sync.add_argument("--out", type=Path)
    sync.add_argument("--dry-run", action="store_true")
    sync.add_argument("--delete-obsolete", action="store_true")
    sync.add_argument("--max-checks", type=int, default=20)
    sync.set_defaults(handler=command_sync)
    wait = sub.add_parser("wait-ping")
    wait.add_argument("--tag", required=True)
    wait.add_argument("--name", action="append", required=True)
    wait.add_argument("--since", required=True)
    wait.add_argument("--timeout", type=float, default=240)
    wait.add_argument("--interval", type=float, default=10)
    wait.set_defaults(handler=command_wait_ping)
    delete = sub.add_parser("delete")
    delete.add_argument("--tag", required=True)
    delete.add_argument("--name", action="append", required=True)
    delete.set_defaults(handler=command_delete)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    key = sys.stdin.readline().strip() if args.key_stdin else os.environ.get("HEALTHCHECKS_API_KEY", "")
    if not key:
        print("ERROR: HEALTHCHECKS_API_KEY is not set (it belongs in config/secrets.env)", file=sys.stderr)
        return 2
    parts = urllib.parse.urlsplit(args.api_url)
    local = parts.scheme == "http" and parts.hostname in {"127.0.0.1", "localhost"}
    if not (parts.scheme == "https" or local) or not parts.hostname:
        print("ERROR: HEALTHCHECKS_API_URL must be an https URL", file=sys.stderr)
        return 2
    client = Client(args.api_url, key)
    try:
        return args.handler(client, args)
    except ApiError as exc:
        message = str(exc).replace(key, "<redacted>")
        print(f"ERROR: {message}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
