# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Drive a script's JSON mode the way the dashboard does, for tests.

run_json starts SCRIPT --json, answers each request with the callback, and
returns every event the script emitted. The callback receives a request event
and returns the response fields (values, confirmed, acknowledged, or
cancelled); returning None sends a cancel message instead.
"""

from __future__ import annotations

import json
import os
import signal
import subprocess
import threading
from dataclasses import dataclass, field
from typing import Callable, Optional

REQUEST_TYPES = {"input", "input_group", "confirm", "manual_action"}


@dataclass
class JsonRun:
    returncode: int
    events: list[dict]
    stderr: str
    malformed: list[str] = field(default_factory=list)

    def of(self, kind: str) -> list[dict]:
        return [event for event in self.events if event["type"] == kind]

    @property
    def requests(self) -> list[dict]:
        return [event for event in self.events if event["type"] in REQUEST_TYPES]

    @property
    def completed(self) -> dict:
        done = self.of("completed")
        return done[-1] if done else {}

    @property
    def log_text(self) -> str:
        return "\n".join(event["text"] for event in self.of("log"))

    def describe(self) -> str:
        return "\n".join(json.dumps(event) for event in self.events) + "\nstderr:\n" + self.stderr


def _kill_group(process: subprocess.Popen) -> None:
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass


def run_json(
    argv: list[str],
    answer: Callable[[dict], Optional[dict]],
    env: Optional[dict] = None,
    timeout: float = 120,
) -> JsonRun:
    process = subprocess.Popen(
        [argv[0], "--json", *argv[1:]],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
        text=True,
        bufsize=1,
        start_new_session=True,
    )
    events: list[dict] = []
    malformed: list[str] = []
    assert process.stdout is not None and process.stdin is not None
    watchdog = threading.Timer(timeout, lambda: _kill_group(process))
    watchdog.start()
    try:
        for line in process.stdout:
            line = line.strip()
            if not line:
                continue
            try:
                event = json.loads(line)
            except ValueError:
                malformed.append(line)
                continue
            events.append(event)
            if event.get("type") == "error" and event.get("code") == "malformed_response":
                _kill_group(process)
                raise AssertionError(f"the script rejected a response: {event.get('message')}")
            if event.get("type") == "validation_error":
                request = next(e for e in reversed(events) if e.get("request_id") == event["request_id"]
                               and e["type"] in REQUEST_TYPES)
                reply = answer({**request, "validation_error": event})
            elif event.get("type") in REQUEST_TYPES:
                reply = answer(event)
            else:
                continue
            if reply is None:
                message = {"type": "cancel"}
            else:
                message = {"type": "response", "request_id": event["request_id"], **reply}
            process.stdin.write(json.dumps(message) + "\n")
            process.stdin.flush()
        process.stdin.close()
        returncode = process.wait(timeout=timeout)
    finally:
        watchdog.cancel()
        _kill_group(process)
        process.wait()
    stderr = process.stderr.read() if process.stderr else ""
    for stream in (process.stdin, process.stdout, process.stderr):
        if stream and not stream.closed:
            try:
                stream.close()
            except BrokenPipeError:
                pass
    return JsonRun(returncode, events, stderr, malformed)


def scripted(*replies: Optional[dict]) -> Callable[[dict], Optional[dict]]:
    """Answer requests in order with the given replies; fail when they run out."""
    queue = list(replies)

    def answer(request: dict) -> Optional[dict]:
        if not queue:
            raise AssertionError(f"unexpected request: {json.dumps(request)}")
        reply = queue.pop(0)
        return reply(request) if callable(reply) else reply

    return answer
