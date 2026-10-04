import contextlib
import fcntl
import io
import os
import pwd
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch as mock_patch

import git_update


class GitUpdateTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        self.library = self.directory / "installed"
        self.library.mkdir()
        self.archive = self.directory / "app.asar"
        self.archive.write_bytes(b"original")
        self.personality = self.directory / "style.md"
        self.personality.write_text("Howaclawa")
        self.user = pwd.getpwuid(os.getuid()).pw_name
        self.repository = self.directory / "upstream"
        self.repository.mkdir()
        self.command("init", "-b", "lean")
        self.command("config", "user.name", "Fixture")
        self.command("config", "user.email", "fixture@example.invalid")
        self.sources = self.repository / git_update.SUBTREE
        self.sources.mkdir(parents=True)
        for name in git_update.PATCHER_SOURCES:
            (self.sources / name).write_text("# fixture\n")
        self.commit_runner("first")
        # Local Git fixtures test owned extraction and execution, not HTTPS compatibility.
        self.original_git = git_update.git
        close = mock_patch.object(git_update, "close_app")
        self.close_app = close.start()
        self.addCleanup(close.stop)

        def local_git(arguments, directory, environment):
            return self.original_git(
                arguments, directory, {**environment, "GIT_ALLOW_PROTOCOL": "file"}
            )

        for target, value in (
            ("REPOSITORY", self.repository.as_uri()),
            ("LIBRARY", self.library),
            ("git", local_git),
        ):
            patcher = mock_patch.object(git_update, target, value)
            patcher.start()
            self.addCleanup(patcher.stop)

    def command(self, *args):
        return subprocess.run(
            ["/usr/bin/git", *args],
            cwd=self.repository,
            env={
                "PATH": "/usr/bin:/bin",
                "HOME": str(self.directory),
                "GIT_CONFIG_NOSYSTEM": "1",
                "GIT_CONFIG_GLOBAL": "/dev/null",
            },
            capture_output=True,
            check=True,
        ).stdout

    def commit_runner(self, marker):
        # This fixture exercises choosing fresh Git code over installed code.
        (self.sources / "pacman_hook.py").write_text(
            "import argparse, os\nfrom pathlib import Path\n"
            "p=argparse.ArgumentParser()\n"
            "p.add_argument('--user')\np.add_argument('--personality-file')\np.add_argument('--asar')\n"
            "a=p.parse_args()\n"
            "assert 'PYTHONPATH' not in os.environ\n"
            f"Path(a.asar).write_text({marker!r} + ':' + Path(a.personality_file).read_text())\n"
        )
        self.command("add", ".")
        self.command("commit", "-m", marker)

    def invoke(self):
        return git_update.main(
            [
                "--user",
                self.user,
                "--personality-file",
                str(self.personality),
                "--asar",
                str(self.archive),
            ]
        )

    def test_latest_commit_runs_in_fresh_process_and_temporary_sources_are_removed(
        self,
    ):
        with (
            mock_patch.object(git_update.os, "geteuid", return_value=0),
            mock_patch.object(git_update, "trusted_directory"),
            mock_patch.object(git_update, "report") as report,
            mock_patch.dict(os.environ, {"PYTHONPATH": "/untrusted/user/code"}),
            contextlib.redirect_stdout(io.StringIO()) as output,
        ):
            self.assertEqual(self.invoke(), 0)
            self.assertEqual(self.archive.read_text(), "first:Howaclawa")
            self.commit_runner("second")
            self.assertEqual(self.invoke(), 0)
            self.assertEqual(self.archive.read_text(), "second:Howaclawa")
        report.assert_not_called()
        self.assertIn(
            self.command("rev-parse", "HEAD").decode().strip(), output.getvalue()
        )
        self.assertEqual(self.personality.read_text(), "Howaclawa")
        self.assertEqual(
            {path.name for path in self.library.iterdir()}, {".update.lock"}
        )

    def test_fetch_failure_warns_returns_success_and_never_runs_old_code(self):
        with (
            mock_patch.object(git_update.os, "geteuid", return_value=0),
            mock_patch.object(git_update, "trusted_directory"),
            mock_patch.object(
                git_update, "fetch_patcher", side_effect=RuntimeError("offline")
            ),
            mock_patch.object(git_update, "report") as report,
        ):
            self.assertEqual(self.invoke(), 0)
        self.assertEqual(self.archive.read_bytes(), b"original")
        self.assertIn("offline", report.call_args.args[1])
        self.assertTrue(report.call_args.kwargs["warning"])
        self.assertEqual(
            {path.name for path in self.library.iterdir()}, {".update.lock"}
        )

    def test_shutdown_failure_warns_and_prevents_fetch_or_archive_changes(self):
        self.close_app.side_effect = TimeoutError("app refused to close")
        with (
            mock_patch.object(git_update.os, "geteuid", return_value=0),
            mock_patch.object(git_update, "trusted_directory"),
            mock_patch.object(git_update, "fetch_patcher") as fetch,
            mock_patch.object(git_update, "report") as report,
        ):
            self.assertEqual(self.invoke(), 0)
        fetch.assert_not_called()
        self.assertEqual(self.archive.read_bytes(), b"original")
        self.assertIn("app refused to close", report.call_args.args[1])
        self.assertTrue(report.call_args.kwargs["warning"])

    def test_missing_and_symlinked_git_sources_are_rejected_before_execution(self):
        target = self.sources / "regions.py"
        for symlink in (False, True):
            with self.subTest(symlink=symlink):
                target.unlink()
                if symlink:
                    target.symlink_to("patch.py")
                self.command("add", ".")
                self.command("commit", "-m", "invalid source")
                with tempfile.TemporaryDirectory(dir=self.directory) as temporary:
                    with self.assertRaisesRegex(
                        ValueError, "missing or not a regular file"
                    ):
                        git_update.fetch_patcher(Path(temporary))
                if not symlink:
                    target.write_text("# fixture\n")
                self.assertEqual(self.archive.read_bytes(), b"original")

    def test_git_command_ignores_inherited_configuration_and_disallows_other_transports(
        self,
    ):
        # Inspect the real public-fetch boundary independently of local-file fixtures.
        with tempfile.TemporaryDirectory(dir=self.directory) as temporary:
            with (
                mock_patch.object(git_update, "git", self.original_git),
                mock_patch.object(git_update.subprocess, "run") as run,
            ):
                run.return_value = subprocess.CompletedProcess([], 1, b"", b"blocked")
                with self.assertRaisesRegex(RuntimeError, "blocked"):
                    git_update.fetch_patcher(Path(temporary))
                command = run.call_args.args[0]
                environment = run.call_args.kwargs["env"]
                self.assertEqual(environment["GIT_ALLOW_PROTOCOL"], "https")
                self.assertEqual(environment["GIT_CONFIG_GLOBAL"], "/dev/null")
                self.assertEqual(environment["GIT_CONFIG_NOSYSTEM"], "1")
                self.assertEqual(environment["GIT_TERMINAL_PROMPT"], "0")
                self.assertNotIn("SSH_AUTH_SOCK", environment)
                self.assertNotIn("core.sshCommand", command)
                self.assertIn("core.hooksPath=/dev/null", command)
                self.assertIn("credential.helper=", command)

    def test_busy_lock_warns_and_does_not_start_a_second_fetch(self):
        with (self.library / ".update.lock").open("a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            with (
                mock_patch.object(git_update.os, "geteuid", return_value=0),
                mock_patch.object(git_update, "trusted_directory"),
                mock_patch.object(git_update, "fetch_patcher") as fetch,
                mock_patch.object(git_update, "report") as report,
            ):
                self.assertEqual(self.invoke(), 0)
            fetch.assert_not_called()
            self.assertTrue(report.call_args.kwargs["warning"])
            self.assertEqual(self.archive.read_bytes(), b"original")

    def test_fetched_runner_failure_is_visible_and_cleans_up(self):
        (self.sources / "pacman_hook.py").write_text("raise SystemExit(7)\n")
        self.command("add", ".")
        self.command("commit", "-m", "broken runner")
        with (
            mock_patch.object(git_update.os, "geteuid", return_value=0),
            mock_patch.object(git_update, "trusted_directory"),
            mock_patch.object(git_update, "report") as report,
            contextlib.redirect_stdout(io.StringIO()),
        ):
            self.assertEqual(self.invoke(), 0)
        self.assertEqual(self.archive.read_bytes(), b"original")
        self.assertTrue(report.call_args.kwargs["warning"])
        self.assertIn("exit status 7", report.call_args.args[1])
        self.assertEqual(
            {path.name for path in self.library.iterdir()}, {".update.lock"}
        )


if __name__ == "__main__":
    unittest.main()
