import contextlib
import io
import json
import os
import pwd
import shlex
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch as mock_patch

import install_hook
import pacman_hook
import regions
from asar import Asar, UnsupportedBundle
from test_patch import (
    APP_INSTRUCTION_FILES,
    CALL,
    INITIAL,
    MAIN,
    PRELOAD,
    RPC,
    TEXT,
    fixture,
)


class HookTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        self.archive = self.directory / "app.asar"
        self.personality = self.directory / "clawa.md"
        self.personality.write_text("You are Howaclawa.\n")
        self.user = pwd.getpwuid(os.getuid()).pw_name
        self.files = {
            **APP_INSTRUCTION_FILES,
            "package.json": json.dumps(
                {
                    "name": "openai-codex-electron",
                    "productName": "Codex",
                    "version": "fixture",
                }
            ).encode(),
            MAIN: (
                "const e=require('original');class Owner{"
                + TEXT
                + "async isNonGitWorkspace(){}};"
            ).encode(),
            PRELOAD: b'let e=require("electron");e.contextBridge.exposeInMainWorld(`electronBridge`,z);',
            INITIAL: (RPC + CALL + "var untouched=1;").encode(),
            "untouched": b"native tools and transport",
        }
        fixture(self.archive, self.files)
        fingerprints = {
            kind: {regions.CodeRegion(kind, source).fingerprint}
            for kind, source in (("text", TEXT), ("rpc", RPC), ("call", CALL))
        }
        trust = mock_patch.object(regions, "FINGERPRINTS", fingerprints)
        self.addCleanup(trust.stop)
        trust.start()

    def invoke(self):
        return pacman_hook.main(
            [
                "--user",
                self.user,
                "--personality-file",
                str(self.personality),
                "--asar",
                str(self.archive),
            ]
        )

    def test_success_replaces_only_archive_atomically_preserves_mode_and_keeps_no_backup(
        self,
    ):
        self.archive.chmod(0o640)
        before = self.archive.stat()
        unpacked = self.directory / "app.asar.unpacked"
        unpacked.mkdir()
        (unpacked / "native.node").write_bytes(b"keep")
        with mock_patch.object(pacman_hook, "report") as report:
            self.assertEqual(self.invoke(), 0)
        report.assert_called_once()
        self.assertFalse(report.call_args.kwargs["warning"])
        result = Asar(self.archive)
        self.assertIn(b"codex-user-personality-v2", result.read(MAIN))
        self.assertEqual(result.read("untouched"), self.files["untouched"])
        self.assertEqual((unpacked / "native.node").read_bytes(), b"keep")
        after = self.archive.stat()
        self.assertNotEqual(before.st_ino, after.st_ino)
        self.assertEqual(after.st_mode, before.st_mode)
        self.assertEqual((after.st_uid, after.st_gid), (before.st_uid, before.st_gid))
        self.assertEqual(
            {path.name for path in self.directory.iterdir()},
            {"app.asar", "clawa.md", "app.asar.unpacked"},
        )

    def test_incompatible_update_stays_pristine_warns_and_returns_success(self):
        self.files[MAIN] = self.files[MAIN].replace(
            b"baseInstructions:e", b"baseInstructions:changed"
        )
        fixture(self.archive, self.files)
        before = self.archive.read_bytes()
        with mock_patch.object(pacman_hook, "report") as report:
            self.assertEqual(self.invoke(), 0)
        self.assertEqual(self.archive.read_bytes(), before)
        self.assertIn("Incompatible app", report.call_args.args[1])
        self.assertTrue(report.call_args.kwargs["warning"])
        self.assertEqual(
            {path.name for path in self.directory.iterdir()}, {"app.asar", "clawa.md"}
        )

    def test_commit_failure_removes_staging_leaves_original_and_does_not_fail_update(
        self,
    ):
        before = self.archive.read_bytes()
        with (
            mock_patch.object(
                pacman_hook.os, "replace", side_effect=PermissionError("denied")
            ),
            mock_patch.object(pacman_hook, "report") as report,
        ):
            self.assertEqual(self.invoke(), 0)
        self.assertEqual(self.archive.read_bytes(), before)
        self.assertIn("denied", report.call_args.args[1])
        self.assertTrue(report.call_args.kwargs["warning"])
        self.assertEqual(
            {path.name for path in self.directory.iterdir()}, {"app.asar", "clawa.md"}
        )

    def test_source_changed_during_preparation_is_not_overwritten(self):
        replacement = self.directory / "new-update.asar"
        replacement.write_bytes(self.archive.read_bytes())
        expected = replacement.read_bytes()
        replacement_inode = replacement.stat().st_ino
        original_replacements = pacman_hook.replacements

        def concurrent_update(*args):
            result = original_replacements(*args)
            os.replace(replacement, self.archive)
            return result

        with mock_patch.object(
            pacman_hook, "replacements", side_effect=concurrent_update
        ):
            with self.assertRaisesRegex(UnsupportedBundle, "changed during patching"):
                pacman_hook.reapply(self.archive, self.personality)
        self.assertEqual(self.archive.read_bytes(), expected)
        self.assertEqual(self.archive.stat().st_ino, replacement_inode)
        self.assertNotIn(b"codex-user-personality-v2", Asar(self.archive).read(MAIN))
        self.assertFalse(
            any(
                path.name.startswith(".codex-personality-")
                for path in self.directory.iterdir()
            )
        )

    def test_missing_personality_and_symlink_archive_are_visible_noops(self):
        before = self.archive.read_bytes()
        self.personality.unlink()
        with mock_patch.object(pacman_hook, "report") as report:
            self.assertEqual(self.invoke(), 0)
        self.assertIn("missing or not regular", report.call_args.args[1])
        self.assertEqual(self.archive.read_bytes(), before)
        link = self.directory / "linked.asar"
        link.symlink_to(self.archive)
        with self.assertRaisesRegex(ValueError, "not a symlink"):
            pacman_hook.reapply(link, self.personality)

    def test_pi_prompt_alias_is_rejected_for_desktop_user_even_when_hook_runs_as_root(
        self,
    ):
        home = self.directory / "home"
        prompt = home / ".pi/agent/REALTIME-SYSTEM-PROMPT.md"
        prompt.parent.mkdir(parents=True)
        prompt.write_text("private")
        alias = self.directory / "alias.md"
        alias.symlink_to(prompt)
        account = pwd.struct_passwd(("test", "x", 1234, 1234, "", str(home), "/bin/sh"))
        with mock_patch.object(pacman_hook.pwd, "getpwnam", return_value=account):
            with self.assertRaisesRegex(ValueError, "Pi-owned"):
                pacman_hook.validate_personality("test", alias)

    def test_log_and_notification_failure_still_leave_printed_diagnostics(self):
        account = pwd.struct_passwd(
            ("test", "x", 1234, 1234, "", "/home/test", "/bin/sh")
        )
        output = io.StringIO()
        with (
            mock_patch.object(pacman_hook.pwd, "getpwnam", return_value=account),
            mock_patch.object(Path, "is_socket", return_value=True),
            mock_patch.object(
                pacman_hook.subprocess,
                "run",
                side_effect=subprocess.TimeoutExpired("notify", 5),
            ),
            contextlib.redirect_stderr(output),
        ):
            pacman_hook.report(
                "test", "Incompatible app, update completed", warning=True
            )
        self.assertIn("Incompatible app, update completed", output.getvalue())
        self.assertIn("journal logging unavailable", output.getvalue())
        self.assertIn("desktop notification unavailable", output.getvalue())

    def test_user_owned_or_writable_or_symlinked_parent_is_rejected(self):
        directory = Path("/opt/codex-desktop-personality")
        for mode, uid in (
            (stat.S_IFDIR | 0o755, 1234),
            (stat.S_IFDIR | 0o775, 0),
            (stat.S_IFLNK | 0o777, 0),
        ):
            with self.subTest(mode=mode, uid=uid):
                metadata = [
                    os.stat_result((stat.S_IFDIR | 0o755, 0, 0, 1, 0, 0, 0, 0, 0, 0)),
                    os.stat_result((mode, 0, 0, 1, uid, 0, 0, 0, 0, 0)),
                ]
                with (
                    mock_patch.object(Path, "exists", return_value=True),
                    mock_patch.object(Path, "lstat", side_effect=metadata),
                    mock_patch.object(Path, "mkdir") as mkdir,
                ):
                    with self.assertRaisesRegex(ValueError, "protected: /opt$"):
                        install_hook.trusted_directory(directory)
                mkdir.assert_not_called()

    def test_installed_hook_quotes_arguments_and_publishes_without_backup(self):
        personality = Path("/home/test/style with 'quotes'.md")
        document = install_hook.hook_text("test", personality)
        command = next(
            line.removeprefix("Exec = ")
            for line in document.splitlines()
            if line.startswith("Exec = ")
        )
        self.assertEqual(
            shlex.split(command),
            [str(install_hook.LIBRARY / "pacman-hook.sh"), "test", str(personality)],
        )
        self.assertIn("When = PostTransaction", document)
        self.assertNotIn("AbortOnFail", document)
        self.assertNotIn("Operation = Remove", document)
        target = self.directory / "registered.hook"
        install_hook.publish(target, b"first", 0o644)
        install_hook.publish(target, document.encode(), 0o644)
        self.assertEqual(target.read_text(), document)
        self.assertEqual(target.stat().st_mode & 0o777, 0o644)
        self.assertFalse(
            any(path.name.startswith("tmp") for path in self.directory.iterdir())
        )


if __name__ == "__main__":
    unittest.main()
