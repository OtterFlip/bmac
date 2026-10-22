#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Structural tests for the example todo app's deploy script and unit."""

from __future__ import annotations

import json
from pathlib import Path
import re
import subprocess
import unittest


REPO_ROOT = Path(__file__).resolve().parents[2]
PROD_DIR = REPO_ROOT / "scripts" / "user_callable" / "guests" / "prod"
DEPLOY = PROD_DIR / "deploy_hello_app_to_prod.sh"
APP_DIR = PROD_DIR / "hello_todo"
UNIT = APP_DIR / "hello-todo.service"


def remote_installer() -> str:
    text = DEPLOY.read_text()
    match = re.search(r"write_remote_installer\(\) \{\n  cat >[^\n]*<<'EOF2'\n(.*?)\nEOF2\n", text, re.S)
    assert match, "remote installer heredoc not found"
    return match.group(1)


class DeployHelloAppTest(unittest.TestCase):
    def test_deploy_script_and_remote_installer_parse(self) -> None:
        subprocess.run(["bash", "-n", str(DEPLOY)], check=True)
        subprocess.run(["bash", "-n"], input=remote_installer(), text=True, check=True)

    def test_builds_a_locked_static_binary_without_nginx(self) -> None:
        text = DEPLOY.read_text()
        self.assertIn("BUILD_TARGET=x86_64-unknown-linux-musl", text)
        self.assertIn("cargo build --release --locked", text)
        # The script only mentions NGINX to tell people to remove the old
        # example; the VM install itself needs no web server or apt packages.
        installer = remote_installer().lower()
        for word in ("nginx", "healthz", "apt-get"):
            self.assertNotIn(word, installer)
        self.assertTrue((APP_DIR / "Cargo.lock").is_file(), "--locked needs a committed Cargo.lock")

    def test_installer_matches_the_unit_paths(self) -> None:
        installer = remote_installer()
        unit = UNIT.read_text()
        for path in ("/usr/local/bin/hello-todo", "/etc/hello-todo/cert.pem", "/etc/hello-todo/key.pem"):
            self.assertIn(path, installer)
            self.assertIn(path, unit)
        self.assertIn("systemctl enable", installer)
        self.assertIn("WantedBy=multi-user.target", unit)
        self.assertIn("--listen [::]:443", unit)

    def test_workflow_is_linux_only(self) -> None:
        workflows = json.loads((REPO_ROOT / "dashboard" / "engine" / "workflows.json").read_text())

        def find(node):
            if isinstance(node, dict):
                if node.get("id") == "deploy_hello_app_to_prod":
                    return node
                node = list(node.values())
            if isinstance(node, list):
                for child in node:
                    found = find(child)
                    if found:
                        return found
            return None

        entry = find(workflows)
        self.assertIsNotNone(entry)
        self.assertEqual(entry["platforms"], ["linux"])


if __name__ == "__main__":
    unittest.main()
