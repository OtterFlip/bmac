# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Running a bmacd job through the API and answering its requests, for scenarios."""

from __future__ import annotations

import time
import uuid
from typing import Callable

from .api import Api

TERMINAL = {"COMPLETED", "FAILED", "CANCELLED"}


def answer(form: dict) -> dict:
    """The answer an operator who accepts every default would give."""
    kind = form["kind"]
    if kind == "confirm":
        return {"kind": "confirm", "confirmed": True}
    if kind == "manual_action":
        return {"kind": "acknowledge"}
    fields = form.get("fields") or [form["field"]]
    values = {}
    for field in fields:
        value = field.get("default")
        if value is None and field.get("options"):
            value = field["options"][0]["value"]
        if value is None:
            raise AssertionError(f"no default for {field['id']} in {form.get('title')}")
        values[field["id"]] = value
    return {"kind": "values", "values": values}


def instruction_texts(form: dict) -> list[str]:
    return [i if isinstance(i, str) else i["text"] for i in form.get("instructions", [])]


def run_job(
    api: Api,
    operation: str,
    parameters: dict,
    timeout: float = 300,
    answerer: Callable[[dict], dict] = answer,
) -> tuple[dict, list[dict]]:
    """Starts operation and answers each request with answerer until the job ends."""
    started = api.request("POST", "/jobs", {
        "operation": operation,
        "parameters": parameters,
        "idempotency_key": str(uuid.uuid4()),
        "dry_run": False,
    })
    job, seen, answered = started["id"], [], set()
    deadline = time.monotonic() + timeout
    while True:
        after = seen[-1]["seq"] + 1 if seen else 1
        view = api.request("GET", f"/jobs/{job}?from_seq={after}")
        seen.extend(view["events"])
        summary = view["job"]
        if summary["state"] in TERMINAL:
            return summary, seen
        request = summary.get("pending_request")
        if request and request not in answered:
            form = next(e for e in reversed(seen) if e["type"] == "request" and e["request_id"] == request)
            api.request("POST", f"/jobs/{job}/requests/{request}", answerer(form))
            answered.add(request)
        if time.monotonic() > deadline:
            raise AssertionError(f"{operation} timed out; the job is {summary['state']}")
        time.sleep(0.5)


def output_lines(events: list[dict]) -> list[str]:
    return [e["text"].rstrip() for e in events if e["type"] in ("output", "log") and "text" in e]
