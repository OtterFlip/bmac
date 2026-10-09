#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Workstation half of the bmac-ui v1 JSON protocol.

lib/ui_json_run.sh starts two `filter` processes for a script running in JSON
mode: one for its stdout and one for its stderr. Each turns every line it
reads into exactly one NDJSON event on the runner's stdout. Lines that
lib/ui_protocol.sh wrote as protocol events carry a per-run marker and are
validated, normalized, and passed through; every other line becomes a `log`
event. Nothing else is ever written to the runner's stdout.

`response` parses one NDJSON line that the controlling program wrote to the
script's protocol input and prints a NUL-delimited answer for Bash.
"""

import argparse
import json
import os
import re
import signal
import sys

MARKER_PREFIX = b"\x1ebmac-ui:"


def json_mode() -> bool:
    """True when this process runs under a script that is in JSON mode."""

    return (
        os.environ.get("BMAC_UI_JSON") == "1"
        and os.environ.get("BMAC_UI_EVENT_FD", "").isdigit()
        and bool(re.fullmatch(r"[A-Za-z0-9]{8,64}", os.environ.get("BMAC_UI_TOKEN", "")))
    )


def emit_event(event: dict) -> None:
    """Emit one protocol event from a Python helper; a no-op outside JSON mode."""

    if not json_mode():
        return
    data = (
        MARKER_PREFIX
        + os.environ["BMAC_UI_TOKEN"].encode("ascii")
        + b" "
        + json.dumps(event, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
        + b"\n"
    )
    fd = int(os.environ["BMAC_UI_EVENT_FD"])
    while data:
        written = os.write(fd, data)
        data = data[written:]


def emit_next_step(text: str, command: str = "", workflow: str = "", args=None) -> None:
    event = {"type": "next_step", "text": text}
    if command:
        event["command"] = command
    if workflow:
        event["workflow"] = workflow
        event["args"] = dict(args or {})
    emit_event(event)
MAX_TEXT = 32768
ANSI_RE = re.compile(
    r"\x1b\[[0-?]*[ -/]*[@-~]"
    r"|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)"
    r"|\x1b[@-Z\\-_]"
)
ERROR_RE = re.compile(r"^\s*(?:ERROR|FATAL|FAIL(?:ED)?)\b[:!]?")
WARNING_RE = re.compile(r"^\s*(?:WARNING|WARN|ATTENTION|CAUTION)\b[:!]?")
OTHER_CONTROL_RE = re.compile(r"[\x00-\x08\x0b-\x1f\x7f]")


def clean_text(raw: bytes) -> str:
    text = raw.decode("utf-8", errors="replace")
    if "\r" in text:
        # A carriage return redraws the line in a terminal; keep what a
        # terminal would finally show.
        parts = [part for part in text.split("\r") if part]
        text = parts[-1] if parts else ""
    text = ANSI_RE.sub("", text)
    text = OTHER_CONTROL_RE.sub("", text.replace("\t", "    "))
    if len(text) > MAX_TEXT:
        text = text[:MAX_TEXT] + " ...(truncated)"
    return text


def classify(text: str, stream: str) -> str:
    if ERROR_RE.match(text):
        return "error"
    if WARNING_RE.match(text):
        return "warning"
    return "info"


def write_event(out, event: dict) -> None:
    out.write(
        json.dumps(event, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
        + b"\n"
    )
    out.flush()


def protocol_event(payload: bytes):
    text = payload.decode("utf-8", errors="replace")
    try:
        event = json.loads(text)
    except ValueError as exc:
        return None, f"malformed protocol event from script: {exc}"
    if not isinstance(event, dict) or not isinstance(event.get("type"), str):
        return None, "protocol event from script is not an object with a type"
    return event, None


def command_filter(args: argparse.Namespace) -> int:
    # Cancellation signals the whole process group. A filter must outlive the
    # script so its final output and the `completed` event still get through;
    # it ends when the script's side of its pipe closes.
    for name in ("SIGINT", "SIGTERM", "SIGHUP"):
        signal.signal(getattr(signal, name), signal.SIG_IGN)
    marker = MARKER_PREFIX + args.token.encode("ascii") + b" "
    out = sys.stdout.buffer
    last_error = None
    for raw in sys.stdin.buffer:
        if raw.endswith(b"\n"):
            raw = raw[:-1]
        if raw.startswith(marker):
            event, problem = protocol_event(raw[len(marker):])
            if event is None:
                write_event(
                    out,
                    {"type": "log", "stream": args.stream, "level": "error",
                     "source": "bmac-ui", "text": problem},
                )
            else:
                write_event(out, event)
            continue
        if raw.startswith(MARKER_PREFIX):
            # A marker without this run's token did not come from this run's
            # protocol library (for example, output relayed from elsewhere).
            raw = raw[len(MARKER_PREFIX):]
        text = clean_text(raw)
        level = classify(text, args.stream)
        if args.stream == "stderr" and level == "error":
            last_error = re.sub(r"^\s*(?:ERROR|FATAL)\s*:\s*", "", text).strip()
        write_event(
            out, {"type": "log", "stream": args.stream, "level": level, "text": text}
        )
    if args.last_error_file and last_error:
        with open(args.last_error_file, "w", encoding="utf-8") as handle:
            handle.write(last_error[:2000])
    return 0


def scalar(value) -> str:
    if value is True:
        return "true"
    if value is False:
        return "false"
    if value is None:
        return ""
    if isinstance(value, (int, float)):
        return repr(value) if isinstance(value, float) else str(value)
    if isinstance(value, str):
        return value
    raise ValueError("values must be strings, numbers, booleans, null, or lists of those")


def emit(*parts: str) -> None:
    out = sys.stdout.buffer
    for part in parts:
        if "\0" in part:
            raise ValueError("values may not contain NUL")
        out.write(part.encode("utf-8") + b"\0")
    out.flush()


def command_response(args: argparse.Namespace) -> int:
    line = sys.stdin.buffer.read().decode("utf-8", errors="replace").strip()
    try:
        message = json.loads(line)
    except ValueError as exc:
        emit("malformed", f"response is not valid JSON: {exc}")
        return 0
    if not isinstance(message, dict):
        emit("malformed", "response is not a JSON object")
        return 0
    kind = message.get("type")
    if kind == "cancel":
        emit("cancelled")
        return 0
    if kind != "response":
        emit("malformed", f"unexpected message type {kind!r}")
        return 0
    if message.get("request_id") != args.request_id:
        emit("ignore")
        return 0
    if message.get("cancelled") is True:
        emit("cancelled")
        return 0
    try:
        if args.kind == "confirm":
            if not isinstance(message.get("confirmed"), bool):
                emit("malformed", "a confirmation response needs a boolean 'confirmed'")
            else:
                emit("confirmed" if message["confirmed"] else "declined")
            return 0
        if args.kind == "ack":
            if message.get("acknowledged") is not True:
                emit("malformed", "a manual action response needs 'acknowledged': true")
            else:
                emit("acknowledged")
            return 0
        values = message.get("values")
        if not isinstance(values, dict):
            emit("malformed", "an input response needs a 'values' object")
            return 0
        parts = ["ok"]
        for key, value in values.items():
            if not isinstance(key, str) or not re.fullmatch(r"[A-Za-z0-9_.-]+", key):
                emit("malformed", f"unsafe field id {key!r}")
                return 0
            if isinstance(value, list):
                rendered = ",".join(scalar(item) for item in value)
            else:
                rendered = scalar(value)
            parts.extend([key, rendered])
        emit(*parts)
    except ValueError as exc:
        emit("malformed", str(exc))
    return 0


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="command", required=True)
    filt = sub.add_parser("filter", help="wrap script output as NDJSON events")
    filt.add_argument("--token", required=True)
    filt.add_argument("--stream", choices=("stdout", "stderr"), required=True)
    filt.add_argument("--last-error-file")
    filt.set_defaults(handler=command_filter)
    resp = sub.add_parser("response", help="parse one response line from stdin")
    resp.add_argument("--request-id", required=True)
    resp.add_argument("--kind", choices=("values", "confirm", "ack"), required=True)
    resp.set_defaults(handler=command_response)
    args = parser.parse_args(argv)
    if args.command == "filter" and not re.fullmatch(r"[A-Za-z0-9]{8,64}", args.token):
        parser.error("--token must be 8-64 alphanumeric characters")
    try:
        return args.handler(args)
    except BrokenPipeError:
        # The reader went away; there is nobody left to report to.
        os.dup2(os.open(os.devnull, os.O_WRONLY), sys.stdout.fileno())
        return 0


if __name__ == "__main__":
    raise SystemExit(main())
