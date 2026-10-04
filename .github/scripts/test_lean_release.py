#!/usr/bin/env python3
"""Offline tests of the fork's release invariants, not native build coverage."""

import io
from contextlib import redirect_stdout
import json
import os
import struct
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import lean_release as release
import lean_voice as voice
import lean_voice_cache as cache
from codex_package.layout import build_package_dir
from codex_package.targets import PACKAGE_VARIANTS, PackageInputs, TARGET_SPECS
from runtime import PLUGINS, required_library_paths

VERSION = "0.160.0-lean.1"
COMMIT = "a" * 40


class ReleaseRefTest(unittest.TestCase):
    def test_diagnostic_scope_selects_windows_without_allowing_publication(self) -> None:
        windows = release.voice_matrix("windows-voice", False)["include"]
        self.assertEqual([entry["target"] for entry in windows], ["x86_64-pc-windows-msvc"])
        for publish in (False, True):
            complete = release.voice_matrix("all", publish)["include"]
            self.assertEqual(
                {entry["target"] for entry in complete},
                {voice.voice_target(target) for target in release.TARGETS},
            )
        with self.assertRaisesRegex(ValueError, "all four complete packages"):
            release.voice_matrix("windows-voice", True)
        with self.assertRaisesRegex(ValueError, "Build scope"):
            release.voice_matrix("unknown", False)

    def test_only_matching_fork_tags_and_lean_dispatch_can_publish(self) -> None:
        for event, ref, publish, expected in (
            ("push", f"refs/tags/lean-v{VERSION}", "", True),
            ("workflow_dispatch", "refs/heads/lean", "false", False),
            ("workflow_dispatch", "refs/heads/lean", "true", True),
        ):
            with self.subTest(event=event, publish=publish):
                self.assertEqual(release.validate_ref(release.REPOSITORY, event, ref, VERSION, COMMIT, publish), expected)

    def test_rejects_wrong_repo_ref_event_version_and_publish_input(self) -> None:
        valid = (release.REPOSITORY, "workflow_dispatch", "refs/heads/lean", VERSION, COMMIT, "false")
        for index, invalid in (
            (0, "openai/codex"), (1, "pull_request"), (2, "refs/heads/main"),
            (2, f"refs/tags/lean-v{VERSION}"), (3, "0.160.0"),
            (3, "0.160.0-lean.01"), (3, "0.160.0-lean.1+other"),
            (3, "18446744073709551616.1.0-lean.1"),
            (4, "a" * 7), (5, "true\npublish=false"),
        ):
            args = list(valid)
            args[index] = invalid
            with self.subTest(index=index, invalid=invalid), self.assertRaises(ValueError):
                release.validate_ref(*args)
        with self.assertRaises(ValueError):
            release.validate_ref(release.REPOSITORY, "push", "refs/tags/lean-v0.160.0-lean.2", VERSION, COMMIT, "")


class ReleaseNotesTest(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        root_patch = patch.object(release, "REPO_ROOT", self.root)
        root_patch.start()
        self.addCleanup(root_patch.stop)
        self.source = self.root / "docs/release-notes" / f"{VERSION}.md"
        self.source.parent.mkdir(parents=True)
        self.curated = f"# Codex Lean {VERSION}\n\n## Changes\n\n- Parents resume when children finish.\n"
        self.source.write_text(self.curated, encoding="utf-8")

    def test_body_preserves_curated_changes_and_pins_doc_links_to_source_commit(self) -> None:
        body = release.render_release_notes(VERSION, COMMIT)
        self.assertTrue(body.startswith(self.curated))
        self.assertIn(f"/blob/{COMMIT}/docs/install.md", body)
        self.assertIn(f"/blob/{COMMIT}/docs/releases.md", body)
        self.assertIn(f"/commit/{COMMIT}", body)
        self.assertIn("glibc 2.28", body)
        self.assertIn("not developer-signed", body)
        self.assertIn("CI does not test live microphone", body)

    def test_new_version_cannot_fall_back_to_previous_notes_or_copy_its_title(self) -> None:
        next_version = "0.160.0-lean.2"
        with self.assertRaisesRegex(ValueError, "Missing curated release notes"):
            release.render_release_notes(next_version, COMMIT)
        self.source.with_name(f"{next_version}.md").write_text(self.curated, encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "title must match"):
            release.render_release_notes(next_version, COMMIT)

    def test_rejects_missing_empty_and_placeholder_changes(self) -> None:
        for section in ("", "## Changes\n", "## Changes\n\n- \n",
                        "## Changes\n\n## Features\n\n- Old features.\n",
                        "## Changes\n\n- TODO\n", "## Changes\n\n- TBD\n",
                        "## Changes\n\n- Placeholder\n"):
            with self.subTest(section=section):
                self.source.write_text(f"# Codex Lean {VERSION}\n\n{section}", encoding="utf-8")
                with self.assertRaises(ValueError):
                    release.render_release_notes(VERSION, COMMIT)

    def test_check_blocks_full_builds_without_notes_but_allows_voice_diagnosis(self) -> None:
        cargo = self.root / "codex-rs/Cargo.toml"
        cargo.parent.mkdir()
        cargo.write_text(f'[workspace.package]\nversion = "{VERSION}"\n', encoding="utf-8")
        env = {"GITHUB_REPOSITORY": release.REPOSITORY, "GITHUB_EVENT_NAME": "workflow_dispatch",
               "GITHUB_REF": "refs/heads/lean", "GITHUB_SHA": COMMIT, "INPUT_PUBLISH": "false",
               "INPUT_SCOPE": "all"}
        with patch.dict(os.environ, env), redirect_stdout(io.StringIO()) as output:
            release.check()
        self.assertIn(f"version={VERSION}\n", output.getvalue())
        self.source.unlink()
        for publish in ("false", "true"):
            with self.subTest(publish=publish), patch.dict(os.environ, {**env, "INPUT_PUBLISH": publish}):
                with redirect_stdout(io.StringIO()) as output:
                    with self.assertRaisesRegex(ValueError, "Missing curated release notes"):
                        release.check()
                self.assertEqual(output.getvalue(), "")
        with patch.dict(os.environ, {**env, "INPUT_SCOPE": "windows-voice"}):
            with redirect_stdout(io.StringIO()) as output:
                release.check()
        self.assertIn("scope=windows-voice\n", output.getvalue())

    def test_verify_requires_curated_notes_before_writing_outputs(self) -> None:
        self.source.unlink()
        with patch.dict(os.environ, {"RELEASE_VERSION": VERSION, "RELEASE_COMMIT": COMMIT}):
            with self.assertRaisesRegex(ValueError, "Missing curated release notes"):
                release.verify()
        self.assertFalse((self.root / "lean-release-notes.md").exists())
        self.assertFalse((self.root / "lean-dist/SHA256SUMS").exists())

    def test_preview_writes_the_same_body_without_assets(self) -> None:
        with patch.dict(os.environ, {"RELEASE_VERSION": VERSION, "RELEASE_COMMIT": COMMIT}):
            release.notes()
        self.assertEqual((self.root / "lean-release-notes.md").read_text(encoding="utf-8"),
                         release.render_release_notes(VERSION, COMMIT))


class ReleasePackageTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.packages = {}

    def make_package(self, target: str, voice_commit: str = COMMIT) -> tuple[Path, str]:
        spec = TARGET_SPECS[target]
        source = self.root / "source"
        source.mkdir(exist_ok=True)
        binary = source / "stub"
        binary.write_bytes(b"test package bytes, not a native executable")
        binary.chmod(0o755)
        package = self.root / target
        package.mkdir()
        inputs = PackageInputs(
            entrypoint_bin=binary, code_mode_host_bin=binary, rg_bin=binary,
            zsh_bin=None if spec.is_windows else binary,
            bwrap_bin=binary if spec.is_linux else None,
            codex_command_runner_bin=binary if spec.is_windows else None,
            codex_windows_sandbox_setup_bin=binary if spec.is_windows else None,
        )
        build_package_dir(package, VERSION, PACKAGE_VARIANTS["codex"], spec, inputs)
        native_target = voice.voice_target(target)
        runtime = self.root / f"runtime-{target}"
        runtime.mkdir()
        plugin_pattern = ("bin/gst{}.dll" if spec.is_windows else
                          "lib/gstreamer-1.0/libgst{}.so" if spec.is_linux else "plugins/libgst{}.dylib")
        core = ("bin/gstreamer-1.0-0.dll" if spec.is_windows else
                "lib/libgstreamer-1.0.so.0" if spec.is_linux else "lib/libgstreamer-1.0.0.dylib")
        plugins = [plugin_pattern.format(name) for name in PLUGINS]
        names = [*plugins, *required_library_paths(native_target), core]
        if spec.is_windows:
            names.append("bin/vcruntime140.dll")
        libraries = []
        for name in names:
            path = runtime / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"receipt fixture, not a native library")
            libraries.append({"path": name, "sha256": release.sha256(path)})
        (runtime / "runtime.json").write_text(json.dumps({
            "schemaVersion": 1, "developmentOnly": False, "distribution": "publicRelease",
            "target": native_target, "sourceCommit": voice_commit,
            "sourceManifestSha256": release.sha256(release.REPO_ROOT / "third_party/voice/sources.json"),
            "plugins": plugins, "libraries": libraries,
        }))
        output = self.root / f"assembled-{target}"
        voice.assemble(package, binary, native_target, COMMIT, output, runtime=runtime,
                       release_version=VERSION, voice_build_commit=voice_commit)
        proof = {
            "schemaVersion": cache.SCHEMA, "target": native_target, "sourceCommit": voice_commit,
            "compatibilityPolicy": cache.COMPATIBILITY_POLICY,
            "workspaceVersion": cache.workspace_version(cache.ROOT),
            "sourceFingerprint": cache.source_fingerprint(cache.ROOT, native_target),
            "runnerImage": {"ImageOS": "fixture", "ImageVersion": "1"},
            "toolVersions": cache.tool_identity(),
            "archiveSha256": "0" * 64, "helperSha256": release.sha256(binary),
            "runtimeSha256": cache.inventory(runtime),
        }
        proof["inputFingerprint"] = cache.input_fingerprint(proof["sourceFingerprint"], proof["runnerImage"], proof["toolVersions"])
        proof_path = output / "codex-resources/voice/provenance.json"
        proof_path.write_text(json.dumps(proof))
        manifest_path = output / "codex-resources/voice/manifest.json"
        manifest = json.loads(manifest_path.read_text())
        manifest["voiceInputFingerprint"] = proof["inputFingerprint"]
        manifest["sha256"]["codex-resources/voice/provenance.json"] = release.sha256(proof_path)
        manifest_path.write_text(json.dumps(manifest))
        self.packages[target] = output
        return output, release.sha256(binary) if spec.is_linux else ""

    def test_finalized_manifest_reports_real_capabilities_and_exact_bwrap(self) -> None:
        for target in release.TARGETS:
            with self.subTest(target=target):
                package, digest = self.make_package(target)
                release.finalize_package(package, VERSION, target, COMMIT, digest)
                metadata = json.loads((package / "codex-package.json").read_text())
                self.assertEqual(metadata["forkRelease"]["commit"], COMMIT)
                self.assertIs(metadata["forkRelease"]["voiceBundled"], True)
                self.assertIs(metadata["forkRelease"]["signed"], False)
                self.assertIs(metadata["forkRelease"]["notarized"], False)
                self.assertEqual(metadata["forkRelease"]["bwrapSha256"], digest or None)
                self.assertIn("input-verified native voice helper", (package / "README.txt").read_text())
                self.assertTrue((package / "LICENSE").is_file())

    def test_rejects_mutated_bwrap_missing_zsh_voice_and_wrong_version(self) -> None:
        target = release.TARGETS[0]
        package, digest = self.make_package(target)
        bwrap = package / "codex-resources/bwrap"
        original = bwrap.read_bytes()
        bwrap.write_bytes(original + b"post-hash mutation")
        with self.assertRaisesRegex(ValueError, "bwrap"):
            release.finalize_package(package, VERSION, target, COMMIT, digest)
        bwrap.write_bytes(original)
        zsh = package / "codex-resources/zsh/bin/zsh"
        zsh.unlink()
        with self.assertRaisesRegex(RuntimeError, "zsh"):
            release.finalize_package(package, VERSION, target, COMMIT, digest)
        zsh.write_bytes(original)
        zsh.chmod(0o755)
        resources = package / "codex-resources/voice"
        resources.rename(self.root / "missing-voice")
        with self.assertRaises(FileNotFoundError):
            release.finalize_package(package, VERSION, target, COMMIT, digest)
        (self.root / "missing-voice").rename(resources)
        with self.assertRaisesRegex(ValueError, "version"):
            release.finalize_package(package, "0.160.0-lean.2", target, COMMIT, digest)

    def test_rejects_mixed_voice_commits_and_changed_helper_bytes(self) -> None:
        target = release.TARGETS[0]
        package, digest = self.make_package(target)
        receipt_file = package / "codex-resources/voice/runtime.json"
        original = receipt_file.read_bytes()
        receipt = json.loads(original)
        receipt["sourceCommit"] = "b" * 40
        receipt_file.write_text(json.dumps(receipt))
        with self.assertRaisesRegex(ValueError, "commit"):
            release.finalize_package(package, VERSION, target, COMMIT, digest)
        receipt_file.write_bytes(original)
        helper = package / "codex-resources/voice/bin/codex-voice-host"
        helper.write_bytes(b"different helper build")
        with self.assertRaisesRegex(ValueError, "digest"):
            release.finalize_package(package, VERSION, target, COMMIT, digest)

    def test_reused_voice_keeps_original_stamp_and_requires_independent_input_proof(self) -> None:
        target = release.TARGETS[0]
        original_commit = "b" * 40
        package, digest = self.make_package(target, original_commit)
        release.finalize_package(package, VERSION, target, COMMIT, digest)
        metadata = json.loads((package / "codex-package.json").read_text())
        manifest = json.loads((package / "codex-resources/voice/manifest.json").read_text())
        self.assertEqual(metadata["forkRelease"]["commit"], COMMIT)
        self.assertEqual(metadata["forkRelease"]["voiceBuildCommit"], original_commit)
        self.assertEqual(manifest["buildCommit"], COMMIT)
        self.assertEqual(manifest["voiceBuildCommit"], original_commit)
        proof_path = package / "codex-resources/voice/provenance.json"
        proof = json.loads(proof_path.read_text())
        proof["sourceFingerprint"] = "0" * 64
        proof["inputFingerprint"] = cache.input_fingerprint(proof["sourceFingerprint"], proof["runnerImage"], proof["toolVersions"])
        proof_path.write_text(json.dumps(proof))
        manifest["voiceInputFingerprint"] = proof["inputFingerprint"]
        manifest["sha256"]["codex-resources/voice/provenance.json"] = release.sha256(proof_path)
        (package / "codex-resources/voice/manifest.json").write_text(json.dumps(manifest))
        with self.assertRaisesRegex(ValueError, "provenance"):
            release.finalize_package(package, VERSION, target, COMMIT, digest)

    def build_assets(self) -> Path:
        directory = self.root / "dist"
        directory.mkdir()
        for target in release.TARGETS:
            package, digest = self.make_package(target)
            release.finalize_package(package, VERSION, target, COMMIT, digest)
            archive = directory / release.archive_name(VERSION, target)
            release.write_archive(package, archive, force=False)
            self.write_checksum(archive)
        return directory

    @staticmethod
    def write_checksum(archive: Path) -> None:
        archive.with_name(archive.name + ".sha256").write_text(f"{release.sha256(archive)}  {archive.name}\n")

    def test_verify_writes_curated_body_only_after_asset_validation(self) -> None:
        directory = self.build_assets().rename(self.root / "lean-dist")
        source = self.root / "docs/release-notes" / f"{VERSION}.md"
        source.parent.mkdir(parents=True)
        source.write_text(f"# Codex Lean {VERSION}\n\n## Changes\n\n- Parents resume.\n", encoding="utf-8")
        env = {"RELEASE_VERSION": VERSION, "RELEASE_COMMIT": COMMIT}
        with patch.object(release, "REPO_ROOT", self.root), patch.dict(os.environ, env):
            archive = directory / release.archive_name(VERSION, release.TARGETS[0])
            original = archive.read_bytes()
            archive.write_bytes(original + b"corruption")
            with self.assertRaisesRegex(ValueError, "Checksum"):
                release.verify()
            self.assertFalse((self.root / "lean-release-notes.md").exists())
            self.assertFalse((directory / "SHA256SUMS").exists())
            archive.write_bytes(original)
            expected = release.render_release_notes(VERSION, COMMIT)
            release.verify()
        self.assertEqual((self.root / "lean-release-notes.md").read_text(encoding="utf-8"), expected)
        self.assertEqual(len((directory / "SHA256SUMS").read_text(encoding="utf-8").splitlines()), 4)

    def test_verify_requires_all_platforms_matching_checksum_and_same_commit(self) -> None:
        directory = self.build_assets()
        checksums = release.verify_assets(directory, VERSION, COMMIT)
        self.assertEqual(len(checksums.splitlines()), 4)
        archive = directory / release.archive_name(VERSION, release.TARGETS[0])
        original = archive.read_bytes()
        archive.write_bytes(original + b"corruption")
        with self.assertRaisesRegex(ValueError, "Checksum"):
            release.verify_assets(directory, VERSION, COMMIT)
        archive.write_bytes(original)
        with self.assertRaisesRegex(ValueError, "provenance"):
            release.verify_assets(directory, VERSION, "b" * 40)
        package = self.packages[release.TARGETS[0]]
        metadata_file = package / "codex-package.json"
        metadata = json.loads(metadata_file.read_text())
        metadata["forkRelease"]["voiceBundled"] = False
        metadata_file.write_text(json.dumps(metadata))
        release.write_archive(package, archive, force=True)
        self.write_checksum(archive)
        with self.assertRaisesRegex(ValueError, "provenance"):
            release.verify_assets(directory, VERSION, COMMIT)
        archive.unlink()
        with self.assertRaisesRegex(ValueError, "every platform"):
            release.verify_assets(directory, VERSION, COMMIT)

    def test_publisher_checks_voice_bytes_even_with_recomputed_archive_checksum(self) -> None:
        directory = self.build_assets()
        target = release.TARGETS[0]
        package = self.packages[target]
        helper = package / "codex-resources/voice/bin/codex-voice-host"
        helper.write_bytes(b"post-build corruption")
        archive = directory / release.archive_name(VERSION, target)
        release.write_archive(package, archive, force=True)
        self.write_checksum(archive)
        with self.assertRaisesRegex(ValueError, "digest"):
            release.verify_assets(directory, VERSION, COMMIT)


class VoiceFrameTest(unittest.TestCase):
    def test_decodes_bounded_runtime_reply_and_rejects_truncation_or_oversize(self) -> None:
        payload = b'{"type":"runtimeReady"}'
        frame = struct.pack(">I", len(payload)) + payload
        self.assertEqual(voice.read_frame(io.BytesIO(frame)), {"type": "runtimeReady"})
        for invalid in (b"", frame[:-1], struct.pack(">I", 0), struct.pack(">I", 128 * 1024 + 1)):
            with self.subTest(invalid=invalid), self.assertRaises(ValueError):
                voice.read_frame(io.BytesIO(invalid))


class ReleaseTagTest(unittest.TestCase):
    def test_existing_tag_must_resolve_to_the_tested_commit(self) -> None:
        ref = [{"ref": f"refs/tags/lean-v{VERSION}"}]
        env = {"GH_REPO": release.REPOSITORY, "RELEASE_TAG": f"lean-v{VERSION}",
               "RELEASE_VERSION": VERSION, "RELEASE_COMMIT": COMMIT}
        for resolved in (COMMIT, "b" * 40):
            with self.subTest(resolved=resolved), patch.dict(os.environ, env):
                responses = [subprocess.CompletedProcess([], 0, json.dumps(ref)),
                             subprocess.CompletedProcess([], 0, resolved + "\n")]
                with patch.object(release.subprocess, "run", side_effect=responses) as run:
                    if resolved == COMMIT:
                        release.tag()
                    else:
                        with self.assertRaisesRegex(ValueError, "different commit"):
                            release.tag()
                    self.assertFalse(any("POST" in call.args[0] for call in run.call_args_list))

    def test_new_tag_uses_exact_tested_sha_not_branch_head(self) -> None:
        env = {"GH_REPO": release.REPOSITORY, "RELEASE_TAG": f"lean-v{VERSION}",
               "RELEASE_VERSION": VERSION, "RELEASE_COMMIT": COMMIT}
        with patch.dict(os.environ, env), patch.object(release.subprocess, "run") as run:
            # A matching-prefix tag is not the exact requested tag.
            run.return_value = subprocess.CompletedProcess([], 0, json.dumps([
                {"ref": f"refs/tags/lean-v{VERSION}0"},
            ]))
            release.tag()
            creation = run.call_args.args[0]
            self.assertIn("POST", creation)
            self.assertIn(f"sha={COMMIT}", creation)
            self.assertIn(f"ref=refs/tags/lean-v{VERSION}", creation)


if __name__ == "__main__":
    unittest.main()
