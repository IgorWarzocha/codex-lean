"""Offline compatibility, immutable artifact integrity and original provenance."""

import io
import json
import os
import shutil
import tarfile
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from unittest.mock import patch

import lean_voice_cache as cache
import lean_voice as voice
from runtime import PLUGINS, digest, required_library_paths

TARGET = "x86_64-unknown-linux-gnu"
COMMIT = "a" * 40
IMAGE = {"ImageOS": "ubuntu24", "ImageVersion": "20261001.1"}
TOOLS = {"python": "3.12.9"}


def make_checkout(root: Path) -> None:
    def write(name, content):
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8")

    for name in cache.LEGACY_FILES:
        write(name, "pinned input\n")
    for name in cache.TREES:
        write(name + "/input", "recipe\n")
    write(".github/workflows/lean-release.yml", '''jobs:
  voice:
    env:
      NATIVE_FLAGS: pinned
    steps:
      - name: Calculate exact native inputs from this checkout and runner image
        run: python validator.py
      - name: Build same-commit native helper and private audio runtime
        if: steps.voice-cache.outputs.cache-hit != 'true'
        run: bazel build -c opt native
      - name: Stage and seal the matching runtime
        if: steps.voice-cache.outputs.cache-hit != 'true'
        env:
          EXPECTED_VOICE_INPUT_FINGERPRINT: cached
        run: |
          strip --strip-debug helper
          python .github/scripts/lean_voice.py seal --staged output
  build:
    steps: []
''')
    write("codex-rs/Cargo.toml", '''[workspace]
members = ["voice-host", "shared", "cli"]
[workspace.package]
version = "1.0.0-lean.1"
[workspace.dependencies]
protocol = { path = "shared" }
''')
    write("codex-rs/voice-host/Cargo.toml", '''[package]
name = "voice"
version.workspace = true
[dependencies]
protocol = { workspace = true }
[target.'cfg(windows)'.build-dependencies]
windows = { path = "../windows" }
''')
    for directory, name in (("shared", "protocol"), ("windows", "windows"), ("cli", "cli")):
        write(f"codex-rs/{directory}/Cargo.toml", f'[package]\nname = "{name}"\nversion.workspace = true\n')
    write("codex-rs/Cargo.lock", '''version = 4
[[package]]
name = "voice"
version = "1.0.0-lean.1"
dependencies = ["protocol 1.0.0-lean.1", "windows"]
[[package]]
name = "protocol"
version = "1.0.0-lean.1"
[[package]]
name = "windows"
version = "1.0.0-lean.1"
[[package]]
name = "cli"
version = "1.0.0-lean.1"
[[package]]
name = "external"
version = "2.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "pinned"
''')
    write("codex-rs/shared/src/lib.rs", "shared protocol\n")
    write("codex-rs/voice-host/src/main.rs", "voice helper\n")


def bump_release(root: Path, before="1.0.0-lean.1", after="1.0.0-lean.2") -> None:
    for name in ("Cargo.toml", "Cargo.lock"):
        path = root / "codex-rs" / name
        path.write_text(path.read_text().replace(before, after))


class FingerprintTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        make_checkout(self.root)
        self.original = cache.source_fingerprint(self.root, TARGET)

    def write(self, name, content):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8")

    def test_cli_skills_and_build_outputs_do_not_change_native_identity(self):
        for name in ("codex-rs/cli/src/main.rs", "codex-rs/skills/src/lib.rs",
                     "codex-rs/voice-host/target/native.o", "third_party/voice/__pycache__/runtime.pyc"):
            self.write(name, "unrelated edit")
        self.assertEqual(cache.source_fingerprint(self.root, TARGET), self.original)

    def test_protocol_transitive_platform_build_deps_and_tool_pins_invalidate(self):
        for name in ("codex-rs/shared/src/lib.rs", "codex-rs/windows/build.rs",
                     "MODULE.bazel.lock",
                     "third_party/voice/input", "patches/input", "bazel/input",
                     ".github/actions/setup-msvc-env/input"):
            path = self.root / name
            previous = path.read_bytes() if path.exists() else None
            with self.subTest(name=name):
                self.write(name, (previous.decode() if previous else "") + "\n# meaningful change\n")
                self.assertNotEqual(cache.source_fingerprint(self.root, TARGET), self.original)
                if previous is None:
                    path.unlink()
                else:
                    path.write_bytes(previous)

    def test_cache_validator_changes_are_not_native_build_inputs(self):
        for name in (".github/scripts/lean_voice.py", ".github/scripts/lean_voice_cache.py"):
            self.write(name, "changed validation and cache policy")
        self.assertEqual(cache.source_fingerprint(self.root, TARGET), self.original)
        path = self.root / ".github/workflows/lean-release.yml"
        path.write_text(path.read_text().replace("python validator.py", "python other-validator.py")
                        .replace("steps.voice-cache.outputs.cache-hit != 'true'",
                                 "steps.voice-cache.outputs.cache-hit != 'true' && steps.voice-legacy-cache.outputs.cache-hit != 'true'"))
        self.assertEqual(cache.source_fingerprint(self.root, TARGET), self.original)

    def test_native_workflow_commands_and_environment_invalidate(self):
        path = self.root / ".github/workflows/lean-release.yml"
        previous = path.read_text()
        for before, after in (("bazel build -c opt", "bazel build -c fastbuild"),
                              ("strip --strip-debug", "strip --strip-all"),
                              ("NATIVE_FLAGS: pinned", "NATIVE_FLAGS: changed"),
                              ("if: steps.voice-cache", "if: runner.os == 'Windows' && steps.voice-cache"),
                              ("EXPECTED_VOICE_INPUT_FINGERPRINT: cached",
                               "EXPECTED_VOICE_INPUT_FINGERPRINT: cached\n          NATIVE_STRIP_FLAGS: changed"),
                              ("seal --staged output", "seal --staged output\n          strip --strip-all helper")):
            with self.subTest(before=before):
                path.write_text(previous.replace(before, after))
                self.assertNotEqual(cache.source_fingerprint(self.root, TARGET), self.original)
                path.write_text(previous)

    def test_new_transitive_dependency_is_meaningful(self):
        path = self.root / "codex-rs/shared/Cargo.toml"
        original = path.read_text()
        self.write("codex-rs/leaf/Cargo.toml", '[package]\nname = "leaf"\n')
        path.write_text(original + '[dependencies]\nleaf = { path = "../leaf" }\n')
        before = cache.source_fingerprint(self.root, TARGET)
        self.write("codex-rs/leaf/src/lib.rs", "new dependency implementation")
        self.assertNotEqual(cache.source_fingerprint(self.root, TARGET), before)
        path.write_text(original)

    def test_workspace_release_bump_keeps_same_reusable_key(self):
        old_exact_input = cache.source_fingerprint(self.root, TARGET, legacy=True)
        bump_release(self.root)
        self.assertNotEqual(cache.source_fingerprint(self.root, TARGET, legacy=True), old_exact_input)
        self.assertEqual(cache.source_fingerprint(self.root, TARGET), self.original)
        self.assertEqual(cache.input_fingerprint(cache.source_fingerprint(self.root, TARGET), IMAGE, TOOLS),
                         cache.input_fingerprint(self.original, IMAGE, TOOLS))

    def test_dependencies_features_and_independent_versions_still_invalidate(self):
        mutations = {
            "codex-rs/cli/Cargo.toml": ('name = "cli"', 'name = "changed-cli"'),
            "codex-rs/voice-host/Cargo.toml": ('workspace = true }', 'workspace = true, features = ["new"] }'),
            "codex-rs/shared/Cargo.toml": ('version.workspace = true', 'version = "1.0.0-lean.1"'),
            "codex-rs/Cargo.toml": ('path = "shared"', 'path = "shared", features = ["new"]'),
            "codex-rs/Cargo.lock": ('version = "2.0.0"', 'version = "2.0.1"'),
        }
        for name, (before, after) in mutations.items():
            path = self.root / name
            previous = path.read_text()
            with self.subTest(name=name):
                self.assertIn(before, previous)
                path.write_text(previous.replace(before, after))
                self.assertNotEqual(cache.source_fingerprint(self.root, TARGET), self.original)
                path.write_text(previous)

    def test_registry_package_matching_local_name_and_version_is_not_normalized(self):
        path = self.root / "codex-rs/Cargo.lock"
        path.write_text(path.read_text() + '''
[[package]]
name = "protocol"
version = "1.0.0-lean.1"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "registry-pin"
''')
        before = cache.source_fingerprint(self.root, TARGET)
        path.write_text(path.read_text().replace('checksum = "registry-pin"', 'checksum = "changed-pin"'))
        self.assertNotEqual(cache.source_fingerprint(self.root, TARGET), before)
        previous = path.read_text()
        # Even a version that happens to equal the workspace version is external input.
        path.write_text(previous.replace('version = "1.0.0-lean.1"\nsource =',
                                        'version = "1.0.0-lean.2"\nsource ='))
        self.assertNotEqual(cache.source_fingerprint(self.root, TARGET), before)

    def test_unsynchronized_lock_and_package_version_consumers_fail_closed(self):
        workspace = self.root / "codex-rs/Cargo.toml"
        original = workspace.read_text()
        workspace.write_text(original.replace("1.0.0-lean.1", "1.0.0-lean.2"))
        with self.assertRaisesRegex(ValueError, "synchronized"):
            cache.source_fingerprint(self.root, TARGET)
        workspace.write_text(original)
        for name in ("codex-rs/voice-host/src/main.rs", "codex-rs/shared/build.rs"):
            with self.subTest(name=name):
                self.write(name, 'const VERSION: &str = env!("CARGO_PKG_VERSION_MAJOR");')
                with self.assertRaisesRegex(ValueError, "consumes CARGO_PKG_VERSION"):
                    cache.source_fingerprint(self.root, TARGET)
                (self.root / name).unlink()

    def test_platform_and_native_runner_image_change_the_exact_key(self):
        self.assertNotEqual(cache.source_fingerprint(self.root, "aarch64-apple-darwin"), self.original)
        original = cache.input_fingerprint(self.original, IMAGE, TOOLS)
        for field in IMAGE:
            image = {**IMAGE, field: IMAGE[field] + "-changed"}
            self.assertNotEqual(cache.input_fingerprint(self.original, image, TOOLS), original)
        self.assertNotEqual(cache.input_fingerprint(self.original, IMAGE, {"python": "3.12.10"}), original)
        with patch.dict(os.environ, {"ImageOS": "", "ImageVersion": ""}):
            with self.assertRaisesRegex(ValueError, "required"):
                cache.image_identity()

    def test_record_order_is_portable_between_windows_and_linux_checkouts(self):
        # PureWindowsPath orders paths case-insensitively. POSIX paths do not.
        # Include BUILD.bazel and bazel so native and publisher hashes cannot diverge.
        workspace = self.root / "codex-rs"
        config = cache.tomllib.loads((workspace / "Cargo.toml").read_text())
        manifests = cache.workspace_manifests(workspace, config) | {
            crate / "Cargo.toml" for crate in cache.local_crates(self.root)}
        cargo = cache.cargo_inputs(workspace, config, manifests)
        workflow = self.root / ".github/workflows/lean-release.yml"
        cargo[workflow] = cache.voice_build_recipe(workflow)
        records = [[path.relative_to(self.root).as_posix(), cargo[path] if path in cargo else digest(path)]
                   for path in self.root.rglob("*") if path.is_file() and path.relative_to(self.root).as_posix()
                   not in {".github/scripts/lean_voice.py", ".github/scripts/lean_voice_cache.py"}]
        records.sort(key=lambda record: record[0])
        expected = cache.json_digest({"schemaVersion": cache.SCHEMA,
                                      "compatibilityPolicy": cache.COMPATIBILITY_POLICY,
                                      "target": TARGET, "inputs": records})
        self.assertEqual(cache.source_fingerprint(self.root, TARGET), expected)

    def test_checkout_proof_rejects_self_attested_input_changes(self):
        proof = {"schemaVersion": cache.SCHEMA, "sourceCommit": COMMIT, "target": TARGET,
                 "compatibilityPolicy": cache.COMPATIBILITY_POLICY, "workspaceVersion": "1.0.0-lean.1",
                 "sourceFingerprint": self.original, "runnerImage": IMAGE,
                 "toolVersions": TOOLS,
                 "inputFingerprint": cache.input_fingerprint(self.original, IMAGE, TOOLS),
                 "archiveSha256": "0" * 64, "helperSha256": "1" * 64}
        self.assertEqual(cache.validate_provenance(proof, TARGET, root=self.root), COMMIT)
        for field, value in (("schemaVersion", 1), ("compatibilityPolicy", "ignore-all-versions"),
                             ("workspaceVersion", None), ("inputFingerprint", "3" * 64)):
            with self.subTest(field=field), self.assertRaisesRegex(ValueError, "provenance"):
                cache.validate_provenance({**proof, field: value}, TARGET, root=self.root)
        self.write("codex-rs/shared/src/lib.rs", "changed protocol")
        with self.assertRaisesRegex(ValueError, "provenance"):
            cache.validate_provenance(proof, TARGET, root=self.root)
        proof["sourceFingerprint"] = "2" * 64
        proof["inputFingerprint"] = cache.input_fingerprint("2" * 64, IMAGE, TOOLS)
        with self.assertRaisesRegex(ValueError, "provenance"):
            cache.validate_provenance(proof, TARGET, root=self.root)


class ArtifactTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.staged = self.root / "staged"
        self.runtime = self.staged / "runtime"
        self.runtime.mkdir(parents=True)
        self.helper = self.staged / "codex-voice-host"
        self.helper.write_bytes(b"offline fixture, not native")
        self.helper.chmod(0o755)
        plugins = [f"lib/gstreamer-1.0/libgst{name}.so" for name in PLUGINS]
        libraries = []
        for name in sorted({*plugins, *required_library_paths(TARGET)}):
            path = self.runtime / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"runtime fixture")
            libraries.append({"path": name, "sha256": digest(path)})
        self.receipt = self.runtime / "runtime.json"
        self.receipt.write_text(json.dumps({
            "schemaVersion": 1, "target": TARGET, "sourceCommit": COMMIT,
            "sourceManifestSha256": digest(cache.ROOT / "third_party/voice/sources.json"),
            "developmentOnly": False, "distribution": "publicRelease",
            "libraries": libraries, "plugins": plugins,
        }))
        self.directory = self.root / "cache"
        self.directory.mkdir()
        self.archive = self.directory / f"lean-voice-{TARGET}.tar.gz"
        self.proof_path = self.directory / f"lean-voice-{TARGET}.provenance.json"
        self.write_archive()
        with patch.dict(os.environ, IMAGE):
            fingerprint = cache.input_fingerprint(cache.source_fingerprint(cache.ROOT, TARGET),
                                                  IMAGE, cache.tool_identity())
            cache.seal_artifact(self.directory, self.staged, TARGET, COMMIT,
                                expected_input_fingerprint=fingerprint,
                                expected_workspace_version=cache.workspace_version(cache.ROOT))
        self.proof = json.loads(self.proof_path.read_text())

    def write_archive(self):
        with tarfile.open(self.archive, "w:gz") as archive:
            for name in ("runtime", "codex-voice-host"):
                archive.add(self.staged / name, arcname=name)

    def verify(self):
        output = self.root / "extracted"
        shutil.rmtree(output, ignore_errors=True)
        with patch.dict(os.environ, IMAGE):
            return voice.verify_artifact(self.directory, TARGET, output)

    def test_verified_hit_preserves_original_commit_and_exact_archive_bytes(self):
        original = self.archive.read_bytes()
        self.assertEqual(self.verify()["sourceCommit"], COMMIT)
        self.assertEqual(self.archive.read_bytes(), original)
        with patch.dict(os.environ, {**IMAGE, "ImageVersion": "different"}):
            with self.assertRaisesRegex(ValueError, "runner image"):
                voice.verify_artifact(self.directory, TARGET, self.root / "different-image")

    def test_producer_seals_prebuild_inputs_for_a_fresh_consumer(self):
        producer, consumer = self.root / "producer", self.root / "consumer"
        make_checkout(producer)
        make_checkout(consumer)
        output = io.StringIO()
        with patch.object(voice, "REPO_ROOT", producer), patch.dict(os.environ, IMAGE), \
                patch("sys.argv", ["voice", "identity", "--target", TARGET]), redirect_stdout(output):
            voice.main()
        identity = dict(line.split("=", 1) for line in output.getvalue().splitlines())
        fingerprint = identity["fingerprint"]
        self.assertEqual(identity["key"], f"lean-voice-v{cache.SCHEMA}-{TARGET}-{fingerprint}")
        with patch.object(cache, "ROOT", producer), \
                patch.dict(os.environ, {**IMAGE, "STABLE_GIT_COMMIT": COMMIT}), \
                patch("sys.argv", ["voice", "seal", "--target", TARGET,
                                   "--directory", str(self.directory), "--staged", str(self.staged),
                                   "--expected-input-fingerprint", fingerprint,
                                   "--expected-workspace-version", identity["workspace-version"]]):
            voice.main()
        proof = voice.verify_artifact(self.directory, TARGET, self.root / "fresh-consumer-payload",
                                      root=consumer, current_image=False)
        self.assertEqual(proof["inputFingerprint"], fingerprint)
        self.assertEqual(proof["sourceCommit"], COMMIT)
        self.assertEqual(proof["workspaceVersion"], "1.0.0-lean.1")
        original_archive = self.archive.read_bytes()
        bump_release(consumer)
        (consumer / "codex-rs/cli/src").mkdir()
        (consumer / "codex-rs/cli/src/main.rs").write_text("changed CLI implementation")
        reused = voice.verify_artifact(self.directory, TARGET, self.root / "new-release-payload",
                                       root=consumer, current_image=False)
        self.assertEqual(reused, proof)
        self.assertEqual(self.archive.read_bytes(), original_archive)
        # Real Bazel 9 drift came from adding rules_rs crate metadata facts.
        # Also cover generated source files and non-source identity drift.
        mutations = ("MODULE.bazel.lock", "third_party/voice/generated",
                     "codex-rs/shared/src/lib.rs")
        for name in mutations:
            path = producer / name
            previous = path.read_bytes() if path.exists() else None
            with self.subTest(name=name), patch.object(cache, "ROOT", producer), \
                    patch.dict(os.environ, IMAGE):
                self.proof_path.unlink()
                path.write_bytes((previous or b"") + b"build-time mutation")
                with self.assertRaisesRegex(ValueError, "inputs changed"):
                    cache.seal_artifact(self.directory, self.staged, TARGET, COMMIT,
                                        expected_input_fingerprint=fingerprint,
                                        expected_workspace_version="1.0.0-lean.1")
                self.assertFalse(self.proof_path.exists())
                if previous is None:
                    path.unlink()
                else:
                    path.write_bytes(previous)
            self.proof_path.write_text(json.dumps(proof))
        for image, tools in (({**IMAGE, "ImageVersion": "changed"}, cache.tool_identity()),
                             (IMAGE, {"python": "0.0.0"})):
            with self.subTest(image=image, tools=tools), patch.object(cache, "ROOT", producer), \
                    patch.dict(os.environ, image), patch.object(cache, "tool_identity", return_value=tools):
                with self.assertRaisesRegex(ValueError, "inputs changed"):
                    cache.seal_artifact(self.directory, self.staged, TARGET, COMMIT,
                                        expected_input_fingerprint=fingerprint,
                                        expected_workspace_version="1.0.0-lean.1")

    def legacy_artifact(self, baseline: Path) -> dict:
        make_checkout(baseline)
        receipt = json.loads(self.receipt.read_text())
        receipt["sourceCommit"] = cache.LEGACY_SOURCE_COMMIT
        self.receipt.write_text(json.dumps(receipt))
        self.write_archive()
        source = cache.source_fingerprint(baseline, TARGET, legacy=True)
        proof = {"schemaVersion": 1, "target": TARGET, "sourceCommit": cache.LEGACY_SOURCE_COMMIT,
                 "sourceFingerprint": source, "runnerImage": IMAGE, "toolVersions": cache.tool_identity(),
                 "archiveSha256": digest(self.archive), "helperSha256": digest(self.helper),
                 "runtimeSha256": cache.inventory(self.runtime)}
        proof["inputFingerprint"] = cache.input_fingerprint(source, IMAGE, proof["toolVersions"])
        self.proof_path.write_text(json.dumps(proof))
        return proof

    def test_v1_migration_proves_original_checkout_and_keeps_payload_and_commit(self):
        baseline, consumer = self.root / "baseline", self.root / "consumer"
        original = self.legacy_artifact(baseline)
        shutil.copytree(baseline, consumer)
        bump_release(consumer)
        (consumer / ".github/scripts/lean_voice_cache.py").write_text("new cache validator, not native code")
        archive = self.archive.read_bytes()
        with patch.dict(os.environ, IMAGE):
            key = voice.legacy_identity(baseline, TARGET, root=consumer)
            self.assertEqual(key, f'lean-voice-v1-{TARGET}-{original["inputFingerprint"]}')
            migrated = voice.migrate_artifact(self.directory, TARGET, baseline, root=consumer)
            verified = voice.verify_artifact(self.directory, TARGET, self.root / "migrated-payload", root=consumer)
        self.assertEqual(migrated, verified)
        self.assertEqual(migrated["legacyProvenance"], original)
        self.assertEqual(migrated["sourceCommit"], cache.LEGACY_SOURCE_COMMIT)
        self.assertEqual(migrated["workspaceVersion"], "1.0.0-lean.1")
        self.assertEqual(self.archive.read_bytes(), archive)
        for field in ("sourceCommit", "archiveSha256", "inputFingerprint"):
            proof = {**migrated, "legacyProvenance": {**original, field: "b" * 64}}
            with self.subTest(field=field), self.assertRaisesRegex(ValueError, "original voice provenance"):
                cache.validate_provenance(proof, TARGET, root=consumer)

    def test_v1_migration_rejects_incompatible_source_and_self_attested_old_inputs(self):
        baseline, consumer = self.root / "baseline", self.root / "consumer"
        original = self.legacy_artifact(baseline)
        shutil.copytree(baseline, consumer)
        source = consumer / "codex-rs/shared/src/lib.rs"
        source.write_text("incompatible protocol")
        with patch.dict(os.environ, IMAGE):
            self.assertIsNone(voice.legacy_identity(baseline, TARGET, root=consumer))
            with self.assertRaisesRegex(ValueError, "incompatible"):
                voice.migrate_artifact(self.directory, TARGET, baseline, root=consumer)
        self.assertEqual(json.loads(self.proof_path.read_text()), original)
        source.write_bytes((baseline / "codex-rs/shared/src/lib.rs").read_bytes())
        original["sourceFingerprint"] = "b" * 64
        original["inputFingerprint"] = cache.input_fingerprint("b" * 64, IMAGE, original["toolVersions"])
        self.proof_path.write_text(json.dumps(original))
        with patch.dict(os.environ, IMAGE), self.assertRaisesRegex(ValueError, "input provenance"):
            voice.migrate_artifact(self.directory, TARGET, baseline, root=consumer)
        self.assertEqual(json.loads(self.proof_path.read_text()), original)

    def test_version_changed_during_build_cannot_relabel_original_version(self):
        checkout = self.root / "checkout"
        make_checkout(checkout)
        with patch.object(cache, "ROOT", checkout), patch.dict(os.environ, IMAGE):
            fingerprint = cache.input_fingerprint(cache.source_fingerprint(checkout, TARGET), IMAGE, cache.tool_identity())
            bump_release(checkout)
            with self.assertRaisesRegex(ValueError, "inputs changed"):
                cache.seal_artifact(self.directory, self.staged, TARGET, COMMIT,
                                    expected_input_fingerprint=fingerprint,
                                    expected_workspace_version="1.0.0-lean.1")

    def test_archive_digest_is_checked_before_extraction(self):
        self.archive.write_bytes(self.archive.read_bytes() + b"tamper")
        with self.assertRaisesRegex(ValueError, "archive digest"):
            self.verify()
        self.assertFalse((self.root / "extracted").exists())

    def test_recomputed_archive_digest_cannot_hide_helper_or_runtime_tampering(self):
        for path in (self.helper, self.runtime / required_library_paths(TARGET)[0]):
            original = path.read_bytes()
            with self.subTest(path=path):
                path.write_bytes(original + b"tamper")
                self.write_archive()
                proof = {**self.proof, "archiveSha256": digest(self.archive)}
                self.proof_path.write_text(json.dumps(proof))
                with self.assertRaisesRegex(ValueError, "digest"):
                    self.verify()
                path.write_bytes(original)

    def test_runtime_original_commit_and_inventory_cannot_be_relabelled(self):
        receipt = json.loads(self.receipt.read_text())
        receipt["sourceCommit"] = "b" * 40
        self.receipt.write_text(json.dumps(receipt))
        with self.assertRaisesRegex(ValueError, "original helper build"):
            cache.validate_payload(self.helper, self.runtime, self.proof, TARGET)
        receipt["sourceCommit"] = COMMIT
        self.receipt.write_text(json.dumps(receipt))
        (self.runtime / "unexpected").write_bytes(b"unlisted")
        proof = {**self.proof, "runtimeSha256": cache.inventory(self.runtime)}
        with self.assertRaisesRegex(ValueError, "unlisted"):
            cache.validate_payload(self.helper, self.runtime, proof, TARGET)

    def test_failed_native_smoke_never_exports_voice_stamp(self):
        env_file = self.root / "env"
        with patch.dict(os.environ, {**IMAGE, "GITHUB_ENV": str(env_file)}), \
                patch("sys.argv", ["cache", "verify", "--target", TARGET,
                                   "--directory", str(self.directory), "--export-env"]), \
                patch.object(voice, "native_smoke", side_effect=ValueError("wrong helper stamp")):
            with self.assertRaisesRegex(ValueError, "wrong helper stamp"):
                voice.main()
        self.assertFalse(env_file.exists())

    def test_verified_export_uses_original_voice_commit_not_current_app_commit(self):
        env_file = self.root / "env"
        with patch.dict(os.environ, {**IMAGE, "GITHUB_ENV": str(env_file),
                                     "STABLE_GIT_COMMIT": "b" * 40}), \
                patch("sys.argv", ["voice", "verify", "--target", TARGET,
                                   "--directory", str(self.directory), "--export-env"]), \
                patch.object(voice, "native_smoke") as smoke:
            voice.main()
        self.assertEqual(env_file.read_text(), f"CODEX_VOICE_BUILD_COMMIT={COMMIT}\n")
        self.assertEqual(smoke.call_args.args[1:], (TARGET, COMMIT))


if __name__ == "__main__":
    unittest.main()
