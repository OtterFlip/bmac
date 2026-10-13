# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""The Postmark template tool, against a fake Postmark API. Nothing here
contacts Postmark."""

from __future__ import annotations

import importlib.util
import io
import json
import os
import shutil
import sys
import tempfile
import unittest
import urllib.error
from unittest import mock
from pathlib import Path
from typing import Any
from unittest import mock

V3_ROOT = Path(__file__).resolve().parent.parent

_spec = importlib.util.spec_from_file_location("v3_postmark_templates", V3_ROOT / "dev" / "postmark_templates.py")
assert _spec and _spec.loader
tool = importlib.util.module_from_spec(_spec)
sys.modules[_spec.name] = tool
_spec.loader.exec_module(tool)

TOKEN = "fake-server-token-value"
OTHER_TOKEN = "fake-other-token-value"


class Response(io.BytesIO):
    def __enter__(self) -> "Response":
        return self

    def __exit__(self, *_: Any) -> None:
        self.close()


class FakePostmark:
    def __init__(self, name: str = "bmac_lab") -> None:
        self.name = name
        self.templates: dict[str, dict[str, Any]] = {}
        self.calls: list[tuple[str, str]] = []
        self.emails: list[dict[str, Any]] = []
        self.suggest: dict[str, Any] | None = None

    def add(self, body: dict[str, Any]) -> None:
        self.templates[body["Alias"]] = {
            "Subject": None,
            "LayoutTemplate": None,
            **body,
            "TemplateId": len(self.templates) + 1,
        }

    def error(self, request: Any, status: int, code: int, message: str) -> urllib.error.HTTPError:
        body = io.BytesIO(json.dumps({"ErrorCode": code, "Message": message}).encode())
        return urllib.error.HTTPError(request.full_url, status, message, {}, body)

    def __call__(self, request: Any, timeout: float) -> Response:
        assert timeout == 15
        if request.get_header("X-postmark-server-token") != TOKEN:
            raise self.error(request, 401, 10, "bad token")
        path = request.full_url.removeprefix(tool.API)
        method = request.get_method()
        self.calls.append((method, path))
        body = json.loads(request.data) if request.data else None
        if path == "/email/withTemplate":
            self.emails.append(body)
        if path == "/server":
            answer: Any = {"ID": 1, "Name": self.name, "DeliveryType": "Sandbox", "ApiTokens": [TOKEN, OTHER_TOKEN]}
        elif path.startswith("/templates?"):
            offset = int(path.split("offset=")[1].split("&")[0])
            items = list(self.templates.values())
            page = [
                {"Alias": t["Alias"], "TemplateType": t["TemplateType"], "LayoutTemplate": t["LayoutTemplate"]}
                for t in items[offset : offset + 100]
            ]
            answer = {"TotalCount": len(items), "Templates": page}
        elif path == "/templates/validate":
            answer = self.validate(body)
        elif path == "/templates" and method == "POST":
            self.add(body)
            answer = {"Alias": body["Alias"], "TemplateId": 9, "Active": True}
        elif path.startswith("/templates/"):
            alias = path.removeprefix("/templates/")
            if alias not in self.templates:
                raise self.error(request, 422, 1101, "template not found")
            if method == "GET":
                answer = self.templates[alias]
            elif method == "PUT":
                self.templates[alias].update(body)
                answer = {"Alias": alias, "TemplateId": 1, "Active": True}
            else:
                del self.templates[alias]
                answer = {"ErrorCode": 0, "Message": "deleted"}
        elif path == "/email/withTemplate":
            answer = {"ErrorCode": 0, "Message": "OK", "MessageID": "m-1", "To": body["To"]}
        else:
            raise AssertionError(f"unexpected {method} {path}")
        return Response(json.dumps(answer).encode())

    def validate(self, body: dict[str, Any]) -> dict[str, Any]:
        model = body["TestRenderModel"]
        if self.suggest is not None:
            model = self.suggest
        return {
            "AllContentIsValid": True,
            "HtmlBody": {"ContentIsValid": True, "ValidationErrors": []},
            "TextBody": {"ContentIsValid": True, "ValidationErrors": []},
            "Subject": {"ContentIsValid": True, "ValidationErrors": []},
            "SuggestedTemplateModel": model,
        }


class ToolTest(unittest.TestCase):
    def setUp(self) -> None:
        scratch = tempfile.TemporaryDirectory(prefix="bmac-postmark-")
        self.addCleanup(scratch.cleanup)
        self.root = Path(scratch.name) / "templates"
        shutil.copytree(tool.TEMPLATES, self.root)
        self.api = FakePostmark()
        self.lines: list[str] = []
        env = mock.patch.dict(os.environ, {"POSTMARK_SERVER_TOKEN": TOKEN}, clear=False)
        env.start()
        self.addCleanup(env.stop)
        os.environ.pop("POSTMARK_SERVER_NAME", None)

    def run_tool(self, *argv: str) -> int:
        self.lines = []
        return tool.main(list(argv), opener=self.api, root=self.root, out=self.lines.append)

    def output(self) -> str:
        return "\n".join(self.lines)

    def changes(self) -> list[tuple[str, str]]:
        return [call for call in self.api.calls if call[0] in ("POST", "PUT", "DELETE") and call[1] != "/templates/validate"]

    def manifest(self) -> dict[str, Any]:
        return json.loads((self.root / "manifest.json").read_text())

    def write_manifest(self, manifest: dict[str, Any]) -> None:
        (self.root / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")

    def test_the_repository_manifest_has_no_problems(self) -> None:
        self.assertEqual(tool.problems(tool.load()), [])

    def test_the_license_header_is_not_sent(self) -> None:
        for template in tool.load(self.root):
            for text in (template.subject, template.text, template.html):
                self.assertNotIn("SPDX", text)
                self.assertNotIn("Copyright", text)

    def test_sync_plans_without_apply_and_creates_the_layout_first(self) -> None:
        self.assertEqual(self.run_tool("sync"), 0, self.output())
        self.assertIn("create bmac-layout-v1", self.output())
        self.assertIn("Plan only", self.output())
        self.assertEqual(self.changes(), [])

        self.assertEqual(self.run_tool("sync", "--apply", "--server", "bmac_lab"), 0, self.output())
        created = [path for method, path in self.changes() if method == "POST"]
        self.assertEqual(len(created), 7)
        self.assertEqual(list(self.api.templates)[0], "bmac-layout-v1")
        self.assertEqual(self.api.templates["bmac-test-v1"]["LayoutTemplate"], "bmac-layout-v1")

        self.api.calls.clear()
        self.assertEqual(self.run_tool("sync", "--apply", "--server", "bmac_lab"), 0, self.output())
        self.assertIn("Nothing to change.", self.output())
        self.assertEqual(self.changes(), [])
        self.assertEqual(self.run_tool("diff"), 0, self.output())

    def test_apply_needs_the_matching_server_name(self) -> None:
        self.assertEqual(self.run_tool("sync", "--apply"), 1)
        self.assertIn("--server NAME", self.output())
        self.assertEqual(self.run_tool("sync", "--apply", "--server", "production"), 1)
        self.assertIn("not 'production'", self.output())
        self.assertEqual(self.changes(), [])
        with mock.patch.dict(os.environ, {"POSTMARK_SERVER_NAME": "bmac_lab"}):
            self.assertEqual(self.run_tool("sync", "--apply"), 0, self.output())

    def test_the_server_tokens_are_never_printed(self) -> None:
        for argv in (["list"], ["diff"], ["validate"], ["sync"], ["sync", "--apply", "--server", "bmac_lab"]):
            self.run_tool(*argv)
            self.assertNotIn(TOKEN, self.output())
            self.assertNotIn(OTHER_TOKEN, self.output())
        os.environ["POSTMARK_SERVER_TOKEN"] = "wrong-token-value"
        self.assertEqual(self.run_tool("list"), 1)
        self.assertIn("refused the server token", self.output())
        self.assertNotIn("wrong-token-value", self.output())

    def test_drafts_are_updated_and_released_templates_are_never_overwritten(self) -> None:
        self.run_tool("sync", "--apply", "--server", "bmac_lab")
        self.api.templates["bmac-test-v1"]["HtmlBody"] = "edited in the web UI"
        self.assertEqual(self.run_tool("diff"), 1)
        self.assertIn("bmac-test-v1: differs in HtmlBody", self.output())
        self.api.calls.clear()
        self.assertEqual(self.run_tool("sync", "--apply", "--server", "bmac_lab"), 0, self.output())
        self.assertEqual(self.changes(), [("PUT", "/templates/bmac-test-v1")])

        self.assertEqual(self.run_tool("release", "bmac-test-v1"), 0, self.output())
        self.api.templates["bmac-test-v1"]["HtmlBody"] = "edited again"
        self.api.calls.clear()
        self.assertEqual(self.run_tool("sync", "--apply", "--server", "bmac_lab"), 1)
        self.assertIn("DRIFT bmac-test-v1 is released", self.output())
        self.assertEqual(self.changes(), [])

    def test_foreign_templates_are_listed_and_left_alone(self) -> None:
        self.api.add({"Alias": "welcome", "Name": "Welcome", "TemplateType": "Standard", "HtmlBody": "x", "TextBody": "x"})
        self.api.add({"Alias": "bmac-alert-opened-v0", "Name": "old", "TemplateType": "Standard", "HtmlBody": "x", "TextBody": "x"})
        self.assertEqual(self.run_tool("list"), 0)
        self.assertIn("welcome  Standard  foreign", self.output())
        self.assertIn("bmac-alert-opened-v0  Standard  bmac, not in this manifest", self.output())
        self.assertIn("2 of 100 templates", self.output())
        self.run_tool("sync", "--apply", "--server", "bmac_lab")
        self.assertIn("welcome", self.api.templates)
        self.assertEqual(self.api.templates["welcome"]["HtmlBody"], "x")

        self.assertEqual(self.run_tool("retire", "welcome", "--apply", "--server", "bmac_lab"), 1)
        self.assertIn("not BMAC's", self.output())
        self.assertEqual(self.run_tool("retire", "bmac-test-v1", "--apply", "--server", "bmac_lab"), 1)
        self.assertIn("still in use", self.output())
        self.assertEqual(self.run_tool("retire", "bmac-alert-opened-v0", "--server", "bmac_lab"), 1)
        self.assertIn("add --apply", self.output())
        self.assertEqual(self.run_tool("retire", "bmac-alert-opened-v0", "--apply", "--server", "bmac_lab"), 0)
        self.assertNotIn("bmac-alert-opened-v0", self.api.templates)
        self.assertIn("welcome", self.api.templates)

    def test_a_layout_in_use_is_not_retired(self) -> None:
        self.api.add({"Alias": "bmac-layout-v0", "Name": "old", "TemplateType": "Layout", "HtmlBody": "x", "TextBody": "x"})
        self.api.add({"Alias": "bmac-x-v0", "Name": "old", "TemplateType": "Standard", "HtmlBody": "x", "TextBody": "x", "LayoutTemplate": "bmac-layout-v0"})
        self.assertEqual(self.run_tool("retire", "bmac-layout-v0", "--apply", "--server", "bmac_lab"), 1)
        self.assertIn("layout of bmac-x-v0", self.output())

    def test_sync_refuses_to_exceed_the_template_limit(self) -> None:
        for n in range(95):
            self.api.add({"Alias": f"other-{n}", "Name": f"o{n}", "TemplateType": "Standard", "HtmlBody": "x", "TextBody": "x"})
        self.assertEqual(self.run_tool("sync", "--apply", "--server", "bmac_lab"), 1)
        self.assertIn("exceed Postmark's 100", self.output())
        self.assertEqual(self.changes(), [])

    def test_validation_must_suggest_exactly_the_manifest_fields(self) -> None:
        self.api.suggest = {"requested_by": "x"}
        self.assertEqual(self.run_tool("validate"), 1)
        self.assertIn("suggested model", self.output())
        self.assertEqual(self.run_tool("sync", "--apply", "--server", "bmac_lab"), 1)
        self.assertEqual(self.changes(), [])

    def test_send_renders_the_test_model_with_the_layout_fields(self) -> None:
        self.run_tool("sync", "--apply", "--server", "bmac_lab")
        self.assertEqual(self.run_tool("send", "bmac-test-v1", "--to", "a@example.com", "--from", "b@example.com", "--server", "bmac_lab"), 1)
        self.assertEqual(
            self.run_tool("send", "bmac-test-v1", "--to", "a@example.com", "--from", "b@example.com", "--apply", "--server", "bmac_lab"),
            0,
            self.output(),
        )
        self.assertIn("MessageID m-1", self.output())

    def test_send_takes_the_sender_from_the_environment(self) -> None:
        self.run_tool("sync", "--apply", "--server", "bmac_lab")
        send = ("send", "bmac-test-v1", "--to", "a@example.com", "--apply", "--server", "bmac_lab")
        with mock.patch.dict(os.environ, {"POSTMARK_SENDER": ""}):
            self.assertEqual(self.run_tool(*send), 1)
        self.assertIn("needs --from or POSTMARK_SENDER", self.output())
        with mock.patch.dict(os.environ, {"POSTMARK_SENDER": "lab@example.com"}):
            self.assertEqual(self.run_tool(*send), 0, self.output())
        self.assertEqual([e["From"] for e in self.api.emails], ["lab@example.com"])

    def test_a_released_template_whose_files_change_is_a_problem(self) -> None:
        self.assertEqual(self.run_tool("release", "bmac-test-v1"), 0, self.output())
        self.assertEqual(tool.problems(tool.load(self.root)), [])
        body = self.root / "bmac-test-v1" / "body.txt"
        body.write_text(body.read_text() + "one more line\n")
        self.assertIn(
            "bmac-test-v1: its files changed after release; make the next version instead",
            tool.problems(tool.load(self.root)),
        )
        self.assertEqual(self.run_tool("list"), 1)
        self.assertEqual(self.api.calls, [])

    def test_raw_tags_unused_fields_and_unknown_names_are_problems(self) -> None:
        html = self.root / "bmac-test-v1" / "body.html"
        html.write_text(html.read_text().replace("{{ requested_by }}", "{{{ requested_by }}} {{ who }}"))
        found = tool.problems(tool.load(self.root))
        self.assertIn("bmac-test-v1: raw {{{ requested_by }}} is not allowed; use {{ requested_by }}", found)
        self.assertIn("bmac-test-v1: uses who, which is not in the manifest's fields", found)

        manifest = self.manifest()
        manifest["templates"][4]["fields"]["extra"] = "string"
        manifest["templates"][4]["test_model"]["extra"] = "x"
        self.write_manifest(manifest)
        self.assertIn("bmac-test-v1: field extra is never used", tool.problems(tool.load(self.root)))

    def test_the_layout_needs_its_content_placeholder_once(self) -> None:
        text = self.root / "bmac-layout-v1" / "body.txt"
        text.write_text(text.read_text().replace("{{{ @content }}}", "{{{ @content }}}\n{{{ @content }}}"))
        self.assertIn(
            "bmac-layout-v1: body.txt needs {{{ @content }}} exactly once",
            tool.problems(tool.load(self.root)),
        )

    def test_test_models_hold_nothing_secret_shaped(self) -> None:
        manifest = self.manifest()
        manifest["templates"][4]["test_model"]["requested_by"] = "0a1b2c3d-1111-2222-3333-444455556666"
        self.write_manifest(manifest)
        found = tool.problems(tool.load(self.root))
        self.assertTrue(any("secret-shaped" in line for line in found), found)

    def test_a_test_model_must_match_the_fields(self) -> None:
        manifest = self.manifest()
        manifest["templates"][3]["test_model"]["alerts"] = [{"severity": "x"}]
        self.write_manifest(manifest)
        self.assertIn(
            "bmac-digest-v1: the test model's fields differ from the manifest's fields",
            tool.problems(tool.load(self.root)),
        )


if __name__ == "__main__":
    unittest.main()
