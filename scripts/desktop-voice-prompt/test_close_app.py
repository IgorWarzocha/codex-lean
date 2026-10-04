import contextlib
import io
import os
import pwd
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch as mock_patch

import pacman_hook


class CloseAppTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.executable = Path(temporary.name) / "ChatGPT"
        # Restrict enumeration to owned fixture processes, but read real kernel
        # identity data and use real pidfds and signals throughout.
        self.processes = Path(temporary.name) / "proc"
        self.processes.mkdir()
        for name, value in (
            ("APP_EXECUTABLE", self.executable),
            ("PROCESS_DIRECTORY", self.processes),
        ):
            override = mock_patch.object(pacman_hook, name, value)
            override.start()
            self.addCleanup(override.stop)
        self.user = pwd.getpwuid(os.getuid()).pw_name

    def start(self, arguments):
        process = subprocess.Popen(arguments, stdout=subprocess.PIPE, text=True)
        (self.processes / str(process.pid)).symlink_to(Path("/proc") / str(process.pid))

        def stop():
            if process.poll() is None:
                process.kill()
            process.wait(timeout=5)
            process.stdout.close()

        self.addCleanup(stop)
        return process

    def test_deleted_app_binary_is_closed_but_other_programs_are_left_running(self):
        shutil.copyfile("/usr/bin/sleep", self.executable)
        self.executable.chmod(0o700)
        target = self.start([str(self.executable), "60"])
        unrelated = self.start(["/usr/bin/sleep", "60"])
        # Package upgrades unlink the executable used by the running desktop app.
        self.executable.unlink()
        with contextlib.redirect_stdout(io.StringIO()):
            pacman_hook.close_app(self.user)
        self.assertEqual(target.wait(timeout=5), -15)
        self.assertIsNone(unrelated.poll())

    def test_other_users_app_processes_are_not_signalled(self):
        shutil.copyfile("/usr/bin/sleep", self.executable)
        self.executable.chmod(0o700)
        target = self.start([str(self.executable), "60"])
        account = pwd.struct_passwd(
            ("other", "x", os.getuid() + 1234, 1234, "", "/home/other", "/bin/sh")
        )
        with mock_patch.object(pacman_hook.pwd, "getpwnam", return_value=account):
            pacman_hook.close_app("other")
        self.assertIsNone(target.poll())

    def test_hung_app_times_out_without_force_killing(self):
        shutil.copyfile("/usr/bin/python3", self.executable)
        self.executable.chmod(0o700)
        target = self.start(
            [
                str(self.executable),
                "-S",
                "-c",
                "import signal;signal.signal(signal.SIGTERM,signal.SIG_IGN);print('ready',flush=True);signal.pause()",
            ]
        )
        self.assertEqual(target.stdout.readline().strip(), "ready")
        with self.assertRaisesRegex(TimeoutError, "did not close gracefully"):
            pacman_hook.close_app(self.user, timeout=0.05)
        self.assertIsNone(target.poll())

    def test_orphaned_electron_child_is_not_treated_as_a_main_process(self):
        shutil.copyfile("/usr/bin/python3", self.executable)
        self.executable.chmod(0o700)
        target = self.start(
            [
                str(self.executable),
                "-S",
                "-c",
                "import signal;print('ready',flush=True);signal.pause()",
                "--type=renderer",
            ]
        )
        self.assertEqual(target.stdout.readline().strip(), "ready")
        with self.assertRaisesRegex(TimeoutError, "did not close gracefully"):
            pacman_hook.close_app(self.user, timeout=0.05)
        self.assertIsNone(target.poll())


if __name__ == "__main__":
    unittest.main()
