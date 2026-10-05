import contextlib
import hashlib
import io
import json
import plistlib
import re
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

import app_instructions
import patch
from asar import Asar
from test_patch import fixture


HERE = Path(__file__).resolve().parent
CAPTURE = json.loads((HERE / "fixtures/linux-31730-instructions.json").read_text())
BOOTSTRAP = ".vite/build/bootstrap-captured.js"
WORKER = ".vite/build/worker.js"
MAIN = ".vite/build/main-captured.js"
INITIAL = "webview/assets/app-initial-captured.js"
PRELOAD = ".vite/build/preload.js"
PERSONALITY = Path("/fixture/codex/communication-preferences.md")


class AppInstructionsTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        self.source = self.directory / "original.asar"
        self.output = self.directory / "slim.asar"
        self.files = {
            "package.json": json.dumps(
                {
                    "name": "openai-codex-electron",
                    "productName": "Codex",
                    "version": "26.930.31730",
                }
            ).encode(),
            MAIN: (
                "const e=require('original');class Owner{"
                + CAPTURE["text"]
                + "async isNonGitWorkspace(){}};"
            ).encode(),
            PRELOAD: b'let e=require("electron");e.contextBridge.exposeInMainWorld(`electronBridge`,z);',
            INITIAL: (CAPTURE["rpc"] + CAPTURE["call"] + "var untouched=1;").encode(),
            BOOTSTRAP: CAPTURE["bootstrap"].encode(),
            WORKER: CAPTURE["worker"].encode(),
            "untouched": b"native transport and tools",
        }
        fixture(self.source, self.files)

    def invoke(self, *args):
        with (
            contextlib.redirect_stdout(io.StringIO()),
            contextlib.redirect_stderr(io.StringIO()),
        ):
            return patch.main(["--asar", str(self.source), *args])

    def assert_rejected_without_outputs(self, files):
        fixture(self.source, files)
        before = self.source.read_bytes()
        plist = self.directory / "Info.plist"
        output_plist = self.directory / "Info.slim.plist"
        plist.write_bytes(
            plistlib.dumps(
                {
                    "ElectronAsarIntegrity": {
                        "Resources/app.asar": {
                            "algorithm": "SHA256",
                            "hash": Asar(self.source).header_sha256,
                        }
                    }
                }
            )
        )
        self.assertEqual(
            self.invoke(
                "--slim-app-instructions",
                "--info-plist",
                str(plist),
                "--output-info-plist",
                str(output_plist),
                "--output",
                str(self.output),
            ),
            1,
        )
        self.assertFalse(self.output.exists())
        self.assertFalse(output_plist.exists())
        self.assertEqual(self.source.read_bytes(), before)

    def test_actual_composers_preserve_all_feature_combinations_overrides_git_and_heartbeats(
        self,
    ):
        transformed = app_instructions.slim_defaults(Asar(self.source))
        result = subprocess.run(
            ["node", str(HERE / "test_app_instructions.cjs")],
            input=json.dumps(
                {
                    "before": {
                        "bootstrap": CAPTURE["bootstrap"],
                        "worker": CAPTURE["worker"],
                    },
                    "after": {
                        "bootstrap": transformed[BOOTSTRAP].decode(),
                        "worker": transformed[WORKER].decode(),
                    },
                    "sections": [text for _, text in app_instructions.SECTIONS],
                }
            ),
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("2048 native composer comparisons passed", result.stdout)

    def test_opt_in_changes_only_default_literals_and_keeps_voice_personality_and_archive_contracts(
        self,
    ):
        original = Asar(self.source)
        version, layout = patch.verify_bundle(original)
        self.assertEqual(version, "26.930.31730")
        default = patch.replacements(original, layout, PERSONALITY)
        explicit_off = patch.replacements(
            original, layout, PERSONALITY, slim_app_instructions=False
        )
        self.assertEqual(default, explicit_off)
        baseline = self.directory / "append-only.asar"
        original.write(baseline, default)
        second = self.directory / "explicit-off.asar"
        original.write(second, explicit_off)
        self.assertEqual(baseline.read_bytes(), second.read_bytes())
        self.assertEqual(
            self.invoke(
                "--slim-app-instructions",
                "--personality-file",
                str(PERSONALITY),
                "--output",
                str(self.output),
            ),
            0,
        )
        result = Asar(self.output)
        for name in (MAIN, PRELOAD, INITIAL):
            # Entire text/voice personality transformations are byte-identical.
            self.assertEqual(result.read(name), default[name])
        for name, entry in original.entries.items():
            if name in (BOOTSTRAP, WORKER, MAIN, PRELOAD, INITIAL):
                continue
            self.assertEqual(result.entries[name], entry)
            if "offset" in entry:
                self.assertEqual(result.read(name), original.read(name))
        for name in (BOOTSTRAP, WORKER):
            expected = original.read(name).decode()
            for pattern, text in app_instructions.SECTIONS:
                matches = list(re.finditer(pattern, expected))
                self.assertEqual(len(matches), 1)
                match = matches[0]
                expected = (
                    expected[: match.start()]
                    + json.dumps(text)
                    + expected[match.end() :]
                )
            self.assertEqual(result.read(name), expected.encode())
        for name in (BOOTSTRAP, WORKER, MAIN, PRELOAD, INITIAL):
            data = result.read(name)
            integrity = result.entries[name]["integrity"]
            self.assertEqual(integrity["hash"], hashlib.sha256(data).hexdigest())
            size = integrity["blockSize"]
            self.assertEqual(
                integrity["blocks"],
                [
                    hashlib.sha256(data[i : i + size]).hexdigest()
                    for i in range(0, len(data), size)
                ],
            )

    def test_malformed_duplicate_missing_or_drifted_owners_reject_without_partial_outputs(
        self,
    ):
        mutations = {
            "missing": lambda files, owner: files.pop(owner),
            "duplicate": lambda files, owner: files.update({owner: files[owner] * 2}),
            "literal drift": lambda files, owner: files.update(
                {owner: files[owner].replace(b"Do not download", b"Allow download", 1)}
            ),
            "malformed literal": lambda files, owner: files.update(
                {
                    owner: files[owner].replace(
                        b"### Thread Coordination", b"### Thread Coordination'", 1
                    )
                }
            ),
            "composer drift": lambda files, owner: files.update(
                {owner: files[owner].replace(b"n&&t?", b"n||t?", 1)}
            ),
        }
        for owner in (BOOTSTRAP, WORKER):
            for fault, mutate in mutations.items():
                with self.subTest(owner=owner, fault=fault):
                    files = dict(self.files)
                    mutate(files, owner)
                    self.assert_rejected_without_outputs(files)

    def test_outer_bootstrap_composer_drift_rejects_without_partial_outputs(self):
        mutations = (
            (
                b"mR({instructionOverrides:n,sidebarSectionToolsEnabled:a",
                b"mR({instructionOverrides:null,sidebarSectionToolsEnabled:a",
            ),
            (b"heartbeatEnabled:r!=null&&_k(r)", b"heartbeatEnabled:!1"),
            (b"function vR(e){let t=e?.branchPrefix", b"function vR(e){let t=null"),
            (b"return e.replaceAll(oR,", b"return e.replaceAll(hR,"),
        )
        for original, changed in mutations:
            with self.subTest(original=original):
                files = dict(self.files)
                self.assertEqual(files[BOOTSTRAP].count(original), 1)
                files[BOOTSTRAP] = files[BOOTSTRAP].replace(original, changed)
                self.assert_rejected_without_outputs(files)

    def test_unrelated_chunk_renames_are_allowed_but_multiple_bootstrap_candidates_reject(
        self,
    ):
        files = dict(self.files)
        files[".vite/build/bootstrap-renamed.js"] = files.pop(BOOTSTRAP)
        fixture(self.source, files)
        self.assertEqual(self.invoke("--check", "--slim-app-instructions"), 0)
        self.assertFalse(self.output.exists())
        files[BOOTSTRAP] = files[".vite/build/bootstrap-renamed.js"]
        fixture(self.source, files)
        self.assertEqual(
            self.invoke("--slim-app-instructions", "--output", str(self.output)), 1
        )
        self.assertFalse(self.output.exists())

    def test_old_six_file_git_payload_still_runs_append_only_cli_and_hook(self):
        legacy = self.directory / "legacy-bootstrap-payload"
        legacy.mkdir()
        for name in (
            "asar.py",
            "regions.py",
            "patch.py",
            "runtime-main.js",
            "runtime-preload.js",
            "pacman_hook.py",
        ):
            shutil.copyfile(HERE / name, legacy / name)
        output = self.directory / "legacy.asar"
        cli = subprocess.run(
            [
                "python3",
                "-E",
                "-s",
                "-B",
                str(legacy / "patch.py"),
                "--asar",
                str(self.source),
                "--personality-file",
                str(PERSONALITY),
                "--output",
                str(output),
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(cli.returncode, 0, cli.stdout + cli.stderr)
        hook_archive = self.directory / "hook-fixture.asar"
        shutil.copyfile(self.source, hook_archive)
        # Invoke only the owned reapplication helper on a temporary archive, not
        # the root launcher, notification path, shutdown or installed app.
        runner = subprocess.run(
            [
                "python3",
                "-E",
                "-s",
                "-B",
                "-c",
                f"import sys;sys.path.insert(0,{str(legacy)!r});from pathlib import Path;from pacman_hook import reapply;reapply(Path({str(hook_archive)!r}),Path({str(PERSONALITY)!r}))",
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(runner.returncode, 0, runner.stdout + runner.stderr)
        self.assertEqual(hook_archive.read_bytes(), output.read_bytes())
        self.assertEqual(Asar(output).read(BOOTSTRAP), self.files[BOOTSTRAP])
        self.assertEqual(Asar(output).read(WORKER), self.files[WORKER])
        missing_optional = subprocess.run(
            [
                "python3",
                "-E",
                "-s",
                "-B",
                str(legacy / "patch.py"),
                "--asar",
                str(self.source),
                "--check",
                "--slim-app-instructions",
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(missing_optional.returncode, 1)
        self.assertIn("Cannot patch", missing_optional.stderr)


if __name__ == "__main__":
    unittest.main()
