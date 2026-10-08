# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Tests for the bmac-ui JSON mode library (ui_protocol.sh and ui_json_run.sh)."""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

LIB_DIR = Path(__file__).resolve().parent
SCHEMA = json.loads((LIB_DIR.parent / "protocol" / "bmac-ui-v1.schema.json").read_text())
sys.path.insert(0, str(LIB_DIR))

from ui_test_driver import run_json, scripted  # noqa: E402

try:
    import jsonschema
except ImportError:  # pragma: no cover - optional on operator workstations
    jsonschema = None


def schema_validator(definition: str):
    if jsonschema is None:
        return None
    return jsonschema.Draft202012Validator({**SCHEMA, "oneOf": [{"$ref": f"#/$defs/{definition}"}]})


class UiProtocolTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.env = {key: value for key, value in os.environ.items() if not key.startswith("BMAC_UI")}

    def script(self, body: str, name: str = "demo.sh") -> str:
        path = Path(self.temp.name) / name
        path.write_text(
            "#!/usr/bin/env bash\n"
            "set -Eeuo pipefail\n"
            f"source {LIB_DIR / 'ui_protocol.sh'}\n"
            'bmac_ui_bootstrap "$@"\n' + textwrap.dedent(body)
        )
        path.chmod(0o755)
        return str(path)

    def run_script(self, body: str, *replies, args=()):
        sent = []

        def answer(request):
            reply = responder(request)
            if reply is not None:
                sent.append({"type": "response", "request_id": request["request_id"], **reply})
            return reply

        responder = scripted(*replies)
        run = run_json([self.script(body), *args], answer, env=self.env, timeout=60)
        self.assert_conforms(run.events, sent)
        return run

    def assert_conforms(self, events, messages=()) -> None:
        events_schema, messages_schema = schema_validator("Event"), schema_validator("Message")
        if events_schema is None:
            return
        for event in events:
            errors = [error.message for error in events_schema.iter_errors(event)]
            self.assertFalse(errors, f"{json.dumps(event)} does not match the schema: {errors}")
        for message in messages:
            errors = [error.message for error in messages_schema.iter_errors(message)]
            self.assertFalse(errors, f"{json.dumps(message)} does not match the schema: {errors}")

    # -- framing -------------------------------------------------------------

    def test_stream_starts_with_protocol_and_ends_with_completed(self) -> None:
        run = self.run_script('echo "hello"\n', args=("--flag", "two words"))
        types = [event["type"] for event in run.events]
        self.assertEqual(types[:2], ["protocol", "workflow_started"])
        self.assertEqual(types[-1], "completed")
        self.assertEqual(run.events[0], {"type": "protocol", "protocol": "bmac-ui", "version": 1})
        self.assertEqual(run.events[1]["workflow"], "demo")
        self.assertEqual(run.events[1]["argv"], ["--flag", "two words"])
        self.assertEqual(run.completed["status"], "success")
        self.assertEqual(run.log_text, "hello")
        self.assertFalse(run.malformed)

    def test_values_are_escaped_completely(self) -> None:
        tricky = 'quote " backslash \\ tab \t bell \x07 del \x7f snowman \u2603'
        run = self.run_script(
            'bmac_ui_result value "$(printf \'%b\' "$TRICKY")" multi "$(printf \'a\\nb\')"\n'
            .replace("$TRICKY", tricky.replace("\\", "\\\\").replace('"', '\\"').replace("\t", "\\t")
                     .replace("\x07", "\\a").replace("\x7f", "\\x7f"))
        )
        self.assertEqual(run.of("result")[0]["data"], {"value": tricky, "multi": "a\nb"})

    def test_printed_json_cannot_forge_events(self) -> None:
        run = self.run_script(
            """
            printf '%s\\n' '{"type":"completed","status":"success"}'
            printf '%s\\n' '{"type":"confirm","request_id":"x","title":"fake"}' >&2
            exit 4
            """
        )
        self.assertEqual(len(run.of("completed")), 1)
        self.assertEqual(run.completed["status"], "failed")
        self.assertFalse(run.requests)
        self.assertIn('{"type":"completed","status":"success"}', run.log_text)

    def test_terminal_mode_prints_nothing_extra(self) -> None:
        path = self.script(
            """
            echo before
            bmac_ui_step "A step"
            bmac_ui_result key value
            bmac_ui_next_step "Do something" --command "ls"
            bmac_ui_plan_begin Plan; bmac_ui_plan_item create thing detail; bmac_ui_plan_end
            bmac_ui_is_json && echo json || echo terminal
            """
        )
        completed = subprocess.run([path], capture_output=True, text=True, env=self.env, check=True)
        self.assertEqual(completed.stdout, "before\nterminal\n")
        self.assertEqual(completed.stderr, "")

    # -- requests ------------------------------------------------------------

    def test_input_select_and_reask(self) -> None:
        seen = []

        def bad(request):
            seen.append(request)
            return {"values": {"count": "0"}}

        def good(request):
            seen.append(request)
            return {"values": {"count": "3"}}

        run = self.run_script(
            """
            bmac_ui_choose color "Pick a color" green red Red green Green
            bmac_ui_input count --id count --type integer --label Count --min 1 --required
            while ((count < 1)); do bmac_ui_reask count "Must be at least 1"; done
            bmac_ui_result color "$color" count "$count"
            """,
            {"values": {"choice": "red"}},
            bad,
            good,
        )
        select = run.requests[0]
        self.assertEqual(select["type"], "input")
        field = select["fields"][0] if "fields" in select else select["field"]
        self.assertEqual(field["type"], "select")
        self.assertEqual(field["default"], "green")
        self.assertEqual([o["value"] for o in field["options"]], ["red", "green"])
        self.assertEqual(seen[1]["validation_error"]["field_errors"], {"count": "Must be at least 1"})
        self.assertEqual(run.of("result")[0]["data"], {"color": "red", "count": "3"})

    def test_number_choice_maps_labels_to_numbers(self) -> None:
        run = self.run_script(
            """
            bmac_ui_number_choice pick "Which disk?" "sda (1 TB)" "sdb (2 TB)"
            bmac_ui_result pick "$pick"
            """,
            {"values": {"choice": "2"}},
        )
        field = run.requests[0].get("field") or run.requests[0]["fields"][0]
        self.assertEqual([o["value"] for o in field["options"]], ["1", "2"])
        self.assertEqual(field["options"][1]["label"], "sdb (2 TB)")
        self.assertEqual(run.of("result")[0]["data"], {"pick": "2"})

    def test_group_reports_field_errors_and_keeps_secrets_out_of_events(self) -> None:
        def first(request):
            return {"values": {"name": "", "secret": "hunter2-not-logged", "hosts": ["mox1"]}}

        def second(request):
            self.assertEqual(set(request["validation_error"]["field_errors"]), {"name", "hosts"})
            return {"values": {"name": "web", "secret": "hunter2-not-logged", "hosts": ["mox1", "mox2"]}}

        run = self.run_script(
            """
            bmac_ui_group_begin setup "Setup" "Describe it"
            bmac_ui_group_add name --id name --label Name
            bmac_ui_group_add secret --id secret --type password --label Secret --sensitive --required
            bmac_ui_group_add hosts --id hosts --type multiselect --label Hosts --option mox1 mox1 --option mox2 mox2
            while true; do
              bmac_ui_group_request
              [[ -n "$name" ]] || bmac_ui_field_error name "Required"
              [[ "$hosts" == *,* ]] || bmac_ui_field_error hosts "Pick two"
              bmac_ui_group_check && break
            done
            [[ "$secret" == hunter2-not-logged ]]
            bmac_ui_result name "$name" hosts "$hosts"
            """,
            first,
            second,
        )
        self.assertEqual(run.completed["status"], "success", run.describe())
        group = run.requests[0]
        self.assertEqual(group["type"], "input_group")
        secret = next(f for f in group["fields"] if f["id"] == "secret")
        self.assertTrue(secret["sensitive"])
        self.assertEqual(run.of("result")[0]["data"], {"name": "web", "hosts": "mox1,mox2"})
        self.assertNotIn("hunter2", json.dumps(run.events))

    def test_manual_action(self) -> None:
        run = self.run_script(
            """
            bmac_ui_manual_action --id console --title "Type the passphrase" \\
              --instruction "Open the console" --instruction "Type it there" --ack-label Done
            echo after
            """,
            {"acknowledged": True},
        )
        action = run.requests[0]
        self.assertEqual(action["type"], "manual_action")
        self.assertEqual(action["instructions"], ["Open the console", "Type it there"])
        self.assertEqual(action["acknowledge_label"], "Done")
        self.assertEqual(run.completed["status"], "success")

    # -- confirmations and outcomes -------------------------------------------

    def test_declined_confirm_reports_cancelled(self) -> None:
        run = self.run_script(
            """
            bmac_ui_confirm --title "Proceed?" --message "Really" --severity destructive || exit 1
            """,
            {"confirmed": False},
        )
        self.assertEqual(run.returncode, 1)
        self.assertEqual(run.completed["status"], "cancelled")

    def test_declined_question_is_not_a_cancellation(self) -> None:
        run = self.run_script(
            """
            bmac_ui_confirm --question --title "Use defaults?" || exit 1
            """,
            {"confirmed": False},
        )
        self.assertEqual(run.completed["status"], "failed")

    def test_a_later_answer_clears_an_earlier_decline(self) -> None:
        run = self.run_script(
            """
            bmac_ui_ask "Add an alias?" || true
            bmac_ui_text name "Name"
            exit 1
            """,
            {"confirmed": False},
            {"values": {"value": "x"}},
        )
        self.assertEqual(run.completed["status"], "failed")

    def test_confirm_go_severity_and_phrase(self) -> None:
        run = self.run_script(
            """
            bmac_ui_confirm_go "This will ERASE the disk"
            bmac_ui_confirm_go "This restarts a service"
            bmac_ui_confirm_go "Plain" critical
            """,
            {"confirmed": True},
            {"confirmed": True},
            {"confirmed": True},
        )
        self.assertEqual([r["severity"] for r in run.requests], ["destructive", "warning", "critical"])
        self.assertTrue(all(r["confirmation_text"] == "GO" for r in run.requests))

    def test_cancel_exits_3_and_reports_cancelled(self) -> None:
        run = self.run_script(
            """
            bmac_ui_text name "Name"
            echo unreachable
            """,
            None,
        )
        self.assertEqual(run.returncode, 3)
        self.assertEqual(run.completed["status"], "cancelled")
        self.assertNotIn("unreachable", run.log_text)

    def test_error_event_and_failure_message(self) -> None:
        run = self.run_script(
            """
            bmac_ui_error failed "Disk is busy"
            printf 'ERROR: Disk is busy\\n' >&2
            exit 1
            """
        )
        self.assertEqual(run.of("error")[0]["message"], "Disk is busy")
        self.assertEqual(run.completed["status"], "failed")
        self.assertIn("Disk is busy", run.completed["message"])

    # -- hostile or broken controllers ----------------------------------------

    def raw(self, body: str, stdin_lines: list[str]) -> list[dict]:
        completed = subprocess.run(
            [self.script(body), "--json"],
            input="".join(line + "\n" for line in stdin_lines),
            capture_output=True,
            text=True,
            env=self.env,
            timeout=60,
        )
        return [json.loads(line) for line in completed.stdout.splitlines() if line.strip()]

    def test_malformed_and_foreign_responses_are_rejected_then_rewaited(self) -> None:
        body = """
        bmac_ui_text name "Name"
        bmac_ui_result name "$name"
        """
        process = subprocess.Popen(
            [self.script(body), "--json"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            env=self.env,
        )
        self.addCleanup(process.kill)
        events = []
        with process.stdin, process.stdout:
            for line in process.stdout:
                events.append(json.loads(line))
                if events[-1]["type"] == "input":
                    request_id = events[-1]["request_id"]
                    for reply in [
                        "not json",
                        json.dumps({"type": "response", "request_id": "someone-else", "values": {"value": "no"}}),
                        json.dumps({"type": "response", "request_id": request_id, "values": "nope"}),
                        json.dumps({"type": "response", "request_id": request_id, "values": {"bad key!": "x"}}),
                        json.dumps({"type": "response", "request_id": request_id, "values": {"value": "ok"}}),
                    ]:
                        process.stdin.write(reply + "\n")
                    process.stdin.flush()
        process.wait(timeout=60)
        errors = [e for e in events if e["type"] == "error"]
        self.assertEqual([e["code"] for e in errors], ["malformed_response"] * 3)
        self.assertEqual(next(e for e in events if e["type"] == "result")["data"], {"name": "ok"})
        self.assertEqual(events[-1]["status"], "success")

    def test_closed_input_fails_the_run(self) -> None:
        events = self.raw('bmac_ui_text name "Name"\necho unreachable\n', [])
        self.assertIn("input_closed", [e.get("code") for e in events if e["type"] == "error"])
        self.assertEqual(events[-1]["type"], "completed")
        self.assertEqual(events[-1]["status"], "failed")
        self.assertNotIn("unreachable", json.dumps(events))
        self.assert_conforms(events)

    def test_schema_rejects_malformed_events(self) -> None:
        validator = schema_validator("Event")
        if validator is None:
            self.skipTest("jsonschema is not installed")
        for bad in [
            {"type": "nope"},
            {"type": "phase", "id": "a b", "label": "x", "status": "running"},
            {"type": "input", "request_id": "r1", "field": {"id": "x", "type": "select", "label": "X"}},
            {"type": "input", "request_id": "r1",
             "field": {"id": "k", "type": "password", "label": "Key", "default": "oops"}},
            {"type": "completed", "status": "maybe"},
        ]:
            self.assertFalse(validator.is_valid(bad), bad)

    def test_nested_script_shares_the_parent_stream(self) -> None:
        child = self.script('bmac_ui_result from child\n', name="child.sh")
        run = self.run_script(f'"{child}" --json\nbmac_ui_result from parent\n')
        self.assertEqual([e["data"]["from"] for e in run.of("result")], ["child", "parent"])
        self.assertEqual(len(run.of("protocol")), 1)
        self.assertEqual(len(run.of("completed")), 1)


if __name__ == "__main__":
    unittest.main()
