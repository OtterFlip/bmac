# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path


SCRIPTS_DIR = Path(__file__).resolve().parent.parent
REPO_ROOT = SCRIPTS_DIR.parent
SETTINGS = Path(".config") / "com.btvcorp.bmac.dashboard"


class ConfigLocationTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.home = self.root / "home"
        (self.home / SETTINGS).mkdir(parents=True)

    def tree(self, installed: bool) -> Path:
        tree = self.root / ("installed" if installed else "checkout")
        (tree / "scripts").mkdir(parents=True)
        shutil.copytree(SCRIPTS_DIR / "lib", tree / "scripts" / "lib")
        if installed:
            (tree / "bmac-installed").write_text("", encoding="utf-8")
        return tree

    def resolve(self, tree: Path, body: str = "") -> subprocess.CompletedProcess[str]:
        env = {
            key: value
            for key, value in os.environ.items()
            if key not in {"APP_HA_CONFIG_TEST_MODE", "APP_HA_CONFIG_DIR", "XDG_CONFIG_HOME"}
        }
        env["HOME"] = str(self.home)
        script = (
            f'source "{tree}/scripts/lib/config.sh" || exit 1\n'
            'printf "%s|%s|%s|%s\\n" "$PROXMOX_INSTALLED_LAYOUT" "$PROXMOX_CONFIG_DIR" '
            '"$PROXMOX_ARTIFACTS_DIR" "$PROXMOX_USER_ROOT"\n' + body
        )
        return subprocess.run(["bash", "-c", script], env=env, capture_output=True, text=True, check=False)

    def pointer(self, text: str, mode: int = 0o600) -> Path:
        path = self.home / SETTINGS / "config-location"
        path.write_text(text, encoding="utf-8")
        path.chmod(mode)
        return path

    def test_checkout_uses_its_config_directory(self) -> None:
        tree = self.tree(installed=False)
        self.pointer("/elsewhere\n")
        completed = self.resolve(tree)
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(
            completed.stdout.strip(),
            f"0|{tree}/config|{tree}/config/artifacts|{tree}",
        )

    def test_installed_tree_defaults_to_the_dashboard_settings_directory(self) -> None:
        tree = self.tree(installed=True)
        completed = self.resolve(tree)
        self.assertEqual(completed.returncode, 0, completed.stderr)
        config = self.home / SETTINGS / "config"
        self.assertEqual(completed.stdout.strip(), f"1|{config}|{config}/artifacts|{config}")

    def test_installed_tree_follows_the_config_location_file(self) -> None:
        tree = self.tree(installed=True)
        self.pointer("/srv/bmac config\n")
        completed = self.resolve(tree)
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(
            completed.stdout.strip(),
            "1|/srv/bmac config|/srv/bmac config/artifacts|/srv/bmac config",
        )

    def test_unsafe_or_malformed_config_location_files_are_refused(self) -> None:
        tree = self.tree(installed=True)
        for text, mode, message in (
            ("/srv/bmac\n", 0o666, "must not be writable by other users"),
            ("relative/path\n", 0o600, "must hold one absolute directory path"),
            ("", 0o600, "could not be read"),
        ):
            with self.subTest(text=text, mode=oct(mode)):
                self.pointer(text, mode)
                completed = self.resolve(tree, "echo REACHED\n")
                self.assertNotEqual(completed.returncode, 0)
                self.assertNotIn("REACHED", completed.stdout)
                self.assertIn(message, completed.stderr)
        path = self.home / SETTINGS / "config-location"
        path.unlink()
        path.symlink_to(self.root / "target")
        completed = self.resolve(tree)
        self.assertIn("must be a regular file owned by you", completed.stderr)

    def test_legacy_artifacts_directories_must_be_moved_first(self) -> None:
        tree = self.tree(installed=False)
        legacy = tree / "scripts" / "user_callable" / "hosts" / "artifacts"
        legacy.mkdir(parents=True)
        (tree / "scripts" / "user_callable" / "guests" / "prod" / "artifacts").mkdir(parents=True)
        body = 'printf "[%s]\\n" "$(config_legacy_artifacts_problem hosts prod staging)"\n'
        completed = self.resolve(tree, body)
        self.assertIn("[]", completed.stdout)
        (legacy / "mox1").mkdir()
        completed = self.resolve(tree, body)
        self.assertIn(f"Artifacts now live in {tree}/config/artifacts/hosts.", completed.stdout)
        self.assertIn(f"mv '{legacy}' '{tree}/config/artifacts/hosts'", completed.stdout)

    def test_git_ignore_is_required_only_inside_a_work_tree(self) -> None:
        tree = self.tree(installed=True)
        outside = self.root / "outside" / "secrets.env"
        completed = self.resolve(tree, f'config_require_git_ignored "{outside}" Secret && echo OK\n')
        self.assertIn("OK", completed.stdout, completed.stderr)
        repo = self.root / "repo"
        subprocess.run(["git", "init", "-q", str(repo)], check=True)
        completed = self.resolve(tree, f'config_require_git_ignored "{repo}/secrets.env" Secret || echo REFUSED\n')
        self.assertIn("REFUSED", completed.stdout)
        self.assertIn("Secret is not ignored by Git", completed.stderr)
        (repo / ".gitignore").write_text("secrets.env\n", encoding="utf-8")
        completed = self.resolve(tree, f'config_require_git_ignored "{repo}/secrets.env" Secret && echo OK\n')
        self.assertIn("OK", completed.stdout, completed.stderr)

    def test_checkout_ignores_config_artifacts(self) -> None:
        completed = subprocess.run(
            ["git", "-C", str(REPO_ROOT), "check-ignore", "-q", "config/artifacts/hosts/mox1/luks-headers/x.bin"],
            check=False,
        )
        self.assertEqual(completed.returncode, 0)


if __name__ == "__main__":
    unittest.main()
