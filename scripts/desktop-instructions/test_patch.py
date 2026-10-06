import contextlib
import hashlib
import io
import json
import os
import plistlib
import struct
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch as mock_patch

import patch
import regions
from asar import Asar, UnsupportedBundle


MAIN = ".vite/build/main-test.js"
PRELOAD = ".vite/build/preload.js"
INITIAL = "webview/assets/app-initial-test.js"
TEXT = (
    "async getProjectAwareDeveloperInstructions(e){return native({baseInstructions:e})}"
)
RPC = (
    "async function nativeStart({codexResponseHandoffPrefix:e=``,prompt:s,manager:i,transport:c})"
    "{let m={...s==null?{}:{prompt:s},transport:c};await i.sendRequest(`thread/realtime/start`,m)}"
)
CALL = (
    "async function nativeCall({codexSessionId:e,conversationId:t,initialItems:n,offerSdp:r,prompt:i,realtimeSessionOverrides:o})"
    "{return o?{instructions:i}:{instructions:i}}"
)
CAPTURE = json.loads(
    (patch.HERE / "fixtures/linux-31730-instructions.json").read_text()
)
APP_INSTRUCTION_FILES = {
    ".vite/build/bootstrap-captured.js": CAPTURE["bootstrap"].encode(),
    ".vite/build/worker.js": CAPTURE["worker"].encode(),
}


def fixture(path: Path, files: dict[str, bytes]) -> None:
    header = {"files": {}}
    payload = b""
    for name, content in files.items():
        directory = header
        parts = name.split("/")
        for part in parts[:-1]:
            directory = directory["files"].setdefault(part, {"files": {}})
        directory["files"][parts[-1]] = {
            "offset": str(len(payload)),
            "size": len(content),
            "integrity": {
                "algorithm": "SHA256",
                "hash": hashlib.sha256(content).hexdigest(),
                "blockSize": 8,
                "blocks": [
                    hashlib.sha256(content[start : start + 8]).hexdigest()
                    for start in range(0, len(content), 8)
                ],
            },
        }
        payload += content
    header["files"]["alias"] = {"link": "untouched"}
    header["files"]["native.node"] = {"size": 99, "unpacked": True, "executable": True}
    header["files"]["deduplicated"] = dict(header["files"]["untouched"])
    write_archive(path, header, payload)


def write_archive(path, header, payload):
    encoded = json.dumps(header).encode()
    padded = encoded + b"\0" * (-len(encoded) % 4)
    path.write_bytes(
        struct.pack("<4I", 4, 8 + len(padded), 4 + len(padded), len(encoded))
        + padded
        + payload
    )


class PatcherTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        self.source = self.directory / "original.asar"
        self.output = self.directory / "patched.asar"
        self.files = {
            **APP_INSTRUCTION_FILES,
            "package.json": json.dumps(
                {
                    "name": "openai-codex-electron",
                    "productName": "Codex",
                    "version": "unrelated-new-build",
                }
            ).encode(),
            MAIN: (
                "const e=require('original');class Owner{"
                + TEXT
                + "async isNonGitWorkspace(){}};"
            ).encode(),
            PRELOAD: b'let e=require("electron");e.contextBridge.exposeInMainWorld(`electronBridge`,z);',
            INITIAL: (RPC + CALL + "var untouched=1;").encode(),
            "untouched": b"native transport and tools remain byte-identical",
        }
        fixture(self.source, self.files)

    def run_cli(self, *arguments):
        with (
            contextlib.redirect_stdout(io.StringIO()),
            contextlib.redirect_stderr(io.StringIO()),
        ):
            return patch.main(["--asar", str(self.source), *arguments])

    def trust_fixture(self):
        # Synthetic archives exercise our decisions, not external compatibility.
        fingerprints = {
            kind: {regions.CodeRegion(kind, source).fingerprint}
            for kind, source in (("text", TEXT), ("rpc", RPC), ("call", CALL))
        }
        return mock_patch.object(regions, "FINGERPRINTS", fingerprints)

    def test_archive_round_trip_preserves_original_entries_and_rehashes_changed_blocks(
        self,
    ):
        original = Asar(self.source)
        content = b"replacement with multiple integrity blocks"
        original.write(self.output, {MAIN: content})
        result = Asar(self.output)
        self.assertEqual(result.read(MAIN), content)
        for name, entry in original.entries.items():
            if name == MAIN:
                continue
            self.assertEqual(result.entries[name], entry)
            if "offset" in entry:
                self.assertEqual(result.read(name), original.read(name))
        integrity = result.entries[MAIN]["integrity"]
        self.assertEqual(integrity["hash"], hashlib.sha256(content).hexdigest())
        self.assertEqual(
            integrity["blocks"],
            [
                hashlib.sha256(content[i : i + 8]).hexdigest()
                for i in range(0, len(content), 8)
            ],
        )

    def test_patch_reads_no_prompt_and_leaves_source_and_native_renderer_code_intact(
        self,
    ):
        before = self.source.read_bytes()
        prompt = self.directory / 'not-yet-created-"${literal}`.md'
        with self.trust_fixture():
            self.assertEqual(
                self.run_cli(
                    "--personality-file", str(prompt), "--output", str(self.output)
                ),
                0,
            )
        self.assertEqual(self.source.read_bytes(), before)
        result = Asar(self.output)
        self.assertEqual(
            result.read(INITIAL),
            self.files[INITIAL]
            .replace(
                RPC.encode(),
                regions.append_voice(regions.CodeRegion("rpc", RPC)).encode(),
            )
            .replace(
                CALL.encode(),
                regions.append_voice(regions.CodeRegion("call", CALL)).encode(),
            ),
        )
        expected_main = self.files[MAIN].replace(
            TEXT.encode(),
            regions.append_text(regions.CodeRegion("text", TEXT)).encode(),
        )
        self.assertTrue(result.read(MAIN).endswith(expected_main))
        self.assertTrue(result.read(PRELOAD).endswith(self.files[PRELOAD]))
        self.assertIn(json.dumps(str(prompt)).encode(), result.read(MAIN))
        self.assertEqual(result.read("untouched"), self.files["untouched"])

    def test_check_is_read_only(self):
        before = self.source.read_bytes()
        with self.trust_fixture():
            self.assertEqual(self.run_cli("--check"), 0)
        self.assertEqual(self.source.read_bytes(), before)
        self.assertFalse(self.output.exists())

    def test_rejects_unknown_identity_region_code_and_ambiguous_owners_before_output(
        self,
    ):
        for fault in ("identity", "text", "rpc", "call", "ambiguous"):
            with self.subTest(fault=fault):
                files = dict(self.files)
                if fault == "identity":
                    files["package.json"] = files["package.json"].replace(
                        b"openai-codex-electron", b"another-app"
                    )
                elif fault == "ambiguous":
                    files[INITIAL] *= 2
                else:
                    owner = MAIN if fault == "text" else INITIAL
                    original = {"text": TEXT, "rpc": RPC, "call": CALL}[fault]
                    files[owner] = files[owner].replace(
                        original.encode(), (original[:-1] + ";changed()}").encode()
                    )
                fixture(self.source, files)
                with self.trust_fixture():
                    self.assertEqual(self.run_cli("--check"), 1)
                self.assertFalse(self.output.exists())

    def test_unrelated_code_version_and_chunk_filename_changes_are_accepted(self):
        files = dict(self.files)
        files["package.json"] = files["package.json"].replace(
            b"unrelated-new-build", b"yet-another-build"
        )
        files["unrelated.js"] = b"arbitrary unrelated update"
        files[".vite/build/main-renamed.js"] = files.pop(MAIN) + b"unrelated();"
        files["webview/assets/app-initial-renamed.js"] = (
            files.pop(INITIAL) + b"unrelated();"
        )
        fixture(self.source, files)
        with self.trust_fixture():
            self.assertEqual(self.run_cli("--output", str(self.output)), 0)
        self.assertEqual(Asar(self.output).read("unrelated.js"), files["unrelated.js"])

    def test_rejects_in_place_existing_output_and_symlink_without_clobber(self):
        with self.trust_fixture():
            self.assertEqual(
                self.run_cli(
                    "--personality-file", "/tmp/custom.md", "--output", str(self.source)
                ),
                1,
            )
            self.output.write_bytes(b"keep me")
            self.assertEqual(
                self.run_cli(
                    "--personality-file", "/tmp/custom.md", "--output", str(self.output)
                ),
                1,
            )
            self.assertEqual(self.output.read_bytes(), b"keep me")
            self.output.unlink()
            self.output.symlink_to(self.directory / "absent")
            self.assertEqual(
                self.run_cli(
                    "--personality-file", "/tmp/custom.md", "--output", str(self.output)
                ),
                1,
            )
            self.assertTrue(self.output.is_symlink())
            self.assertFalse((self.directory / "absent").exists())
        self.assertEqual(
            sorted(p.name for p in self.directory.iterdir()),
            ["original.asar", "patched.asar"],
        )

    def test_protected_prompt_and_alias_and_relative_paths_are_rejected(self):
        protected = Path.home() / ".pi/agent/REALTIME-SYSTEM-PROMPT.md"
        alias = self.directory / "alias.md"
        alias.symlink_to(protected)
        for prompt in (protected, protected.resolve(), alias, Path("relative.md")):
            with self.subTest(prompt=prompt), self.assertRaises(ValueError):
                patch.validate_prompt_path(prompt)

    def test_mac_integrity_metadata_matches_new_header_and_preserves_other_fields(self):
        source = self.directory / "Info.plist"
        output = self.directory / "patched.Info.plist"
        document = {
            "CFBundleName": "native app",
            "NativeKey": [1, 2],
            "ElectronAsarIntegrity": {
                "Resources/app.asar": {
                    "algorithm": "SHA256",
                    "hash": Asar(self.source).header_sha256,
                },
                "unrelated": {"algorithm": "SHA256", "hash": "unchanged"},
            },
        }
        original = plistlib.dumps(document, fmt=plistlib.FMT_BINARY)
        source.write_bytes(original)
        with self.trust_fixture():
            self.assertEqual(
                self.run_cli(
                    "--info-plist",
                    str(source),
                    "--output-info-plist",
                    str(output),
                    "--output",
                    str(self.output),
                ),
                0,
            )
        document["ElectronAsarIntegrity"]["Resources/app.asar"]["hash"] = Asar(
            self.output
        ).header_sha256
        self.assertEqual(plistlib.loads(output.read_bytes()), document)
        self.assertTrue(output.read_bytes().startswith(b"bplist00"))
        self.assertEqual(source.read_bytes(), original)

    def test_mismatched_mac_integrity_rejects_before_outputs_and_failed_second_output_rolls_back(
        self,
    ):
        source = self.directory / "Info.plist"
        output = self.directory / "patched.Info.plist"
        document = {
            "ElectronAsarIntegrity": {
                "Resources/app.asar": {"algorithm": "SHA256", "hash": "wrong"}
            }
        }
        source.write_bytes(plistlib.dumps(document))
        with self.trust_fixture():
            self.assertEqual(
                self.run_cli(
                    "--info-plist",
                    str(source),
                    "--output-info-plist",
                    str(output),
                    "--output",
                    str(self.output),
                ),
                1,
            )
        self.assertFalse(self.output.exists())
        self.assertFalse(output.exists())
        document["ElectronAsarIntegrity"]["Resources/app.asar"]["hash"] = Asar(
            self.source
        ).header_sha256
        source.write_bytes(plistlib.dumps(document))
        impossible = self.directory / "missing-parent" / "Info.plist"
        with self.trust_fixture():
            self.assertEqual(
                self.run_cli(
                    "--info-plist",
                    str(source),
                    "--output-info-plist",
                    str(impossible),
                    "--output",
                    str(self.output),
                ),
                1,
            )
        self.assertFalse(self.output.exists())

    def test_rejects_truncation_out_of_bounds_and_partial_overlap(self):
        self.source.write_bytes(b"bad")
        with self.assertRaises(UnsupportedBundle):
            Asar(self.source)
        for entry in ({"size": 2, "offset": "1000"}, {"size": 3, "offset": "1"}):
            write_archive(
                self.source,
                {"files": {"a": {"size": 3, "offset": "0"}, "b": entry}},
                b"abcd",
            )
            with self.assertRaises(UnsupportedBundle):
                Asar(self.source)

    def test_input_replacement_after_parsing_is_rejected_before_publication(self):
        original = Asar(self.source)
        alternate = self.directory / "alternate.asar"
        fixture(alternate, self.files)
        alternate.replace(self.source)
        with self.assertRaisesRegex(UnsupportedBundle, "changed during patching"):
            original.write(self.output, {MAIN: b"replacement"})
        self.assertFalse(self.output.exists())

    def test_runtime_loader_with_real_files_and_production_transformations(self):
        transformed = {
            "text": regions.append_text(regions.CodeRegion("text", TEXT)),
            "rpc": regions.append_voice(regions.CodeRegion("rpc", RPC)),
            "call": regions.append_voice(regions.CodeRegion("call", CALL)),
        }
        subprocess.run(
            ["node", "--test", str(patch.HERE / "test_runtime.cjs")],
            check=True,
            env={**os.environ, "CODEX_TRANSFORMED_REGIONS": json.dumps(transformed)},
        )


if __name__ == "__main__":
    unittest.main()
