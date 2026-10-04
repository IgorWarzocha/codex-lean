"""Require verified compatible public voice resources and exercise the installed runtime."""

import argparse
import concurrent.futures
import json
import os
import re
import shutil
import struct
import subprocess
import sys
import tarfile
import tempfile
import zipfile
from pathlib import Path
from typing import BinaryIO

REPO_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO_ROOT / "third_party/voice"))

from assemble_package import assemble
from package_runtime import runtime_files
from runtime import digest
import lean_voice_cache as cache


def voice_target(target: str) -> str:
    return target.removesuffix("-musl") + "-gnu" if target.endswith("-musl") else target


def extract_archive(archive: Path, output: Path) -> None:
    """Extract only regular package files. Do not execute downloaded artifacts here."""
    if archive.suffix == ".zip":
        with zipfile.ZipFile(archive) as source:
            for entry in source.infolist():
                path = Path(entry.filename)
                mode = entry.external_attr >> 16
                if path.is_absolute() or ".." in path.parts or "\\" in entry.filename or mode & 0o170000 == 0o120000:
                    raise ValueError("Unsafe release archive entry")
            source.extractall(output)
    else:
        with tarfile.open(archive, "r:gz") as source:
            if any(not (entry.isfile() or entry.isdir()) for entry in source.getmembers()):
                raise ValueError("Release archives must contain only regular files and directories")
            source.extractall(output, filter="data")


def verify_artifact(directory: Path, target: str, output: Path, *, root: Path = REPO_ROOT,
                    current_image: bool = True, legacy: bool = False) -> dict:
    proof = directory / f"lean-voice-{target}.provenance.json"
    archive = directory / f"lean-voice-{target}.tar.gz"
    if ({path.name for path in directory.iterdir()} != {proof.name, archive.name}
            or proof.is_symlink() or not proof.is_file()):
        raise ValueError("voice artifact must contain exactly a regular archive and provenance")
    provenance = json.loads(proof.read_text(encoding="utf-8"))
    cache.validate_provenance(provenance, target, root=root, current_image=current_image, legacy=legacy)
    if archive.is_symlink() or digest(archive) != provenance["archiveSha256"]:
        raise ValueError("voice archive digest mismatch")
    output.mkdir()  # Never mix old and new extraction trees.
    extract_archive(archive, output)
    suffix = ".exe" if target.endswith("-windows-msvc") else ""
    helper = output / f"codex-voice-host{suffix}"
    cache.validate_payload(helper, output / "runtime", provenance, target)
    expected = {f"codex-voice-host{suffix}", *(f"runtime/{name}" for name in provenance["runtimeSha256"])}
    if set(cache.inventory(output)) != expected:
        raise ValueError("voice archive has unexpected files")
    return provenance


def legacy_identity(legacy_root: Path, target: str, *, root: Path = REPO_ROOT) -> str | None:
    if cache.source_fingerprint(legacy_root, target) != cache.source_fingerprint(root, target):
        return None
    source = cache.source_fingerprint(legacy_root, target, legacy=True)
    fingerprint = cache.input_fingerprint(source, cache.image_identity(), cache.tool_identity())
    return f"lean-voice-v1-{target}-{fingerprint}"


def migrate_artifact(directory: Path, target: str, legacy_root: Path, *, root: Path = REPO_ROOT) -> dict:
    """Re-attest verified v1 bytes under the explicit release-version policy."""
    if legacy_identity(legacy_root, target, root=root) is None:
        raise ValueError("original voice build is incompatible with this checkout")
    with tempfile.TemporaryDirectory(prefix="lean-voice-migrate-") as temporary:
        original = verify_artifact(directory, target, Path(temporary) / "payload",
                                   root=legacy_root, legacy=True)
    source = cache.source_fingerprint(root, target)
    proof = {**original, "schemaVersion": cache.SCHEMA,
             "compatibilityPolicy": cache.COMPATIBILITY_POLICY,
             "workspaceVersion": cache.workspace_version(legacy_root),
             "sourceFingerprint": source,
             "inputFingerprint": cache.input_fingerprint(source, original["runnerImage"], original["toolVersions"]),
             "legacyProvenance": original}
    cache.validate_provenance(proof, target, root=root, current_image=True)
    (directory / f"lean-voice-{target}.provenance.json").write_text(json.dumps(proof, indent=2) + "\n", encoding="utf-8")
    return proof


def add_voice(package: Path, target: str, version: str, commit: str) -> Path:
    native_target = voice_target(target)
    archive = REPO_ROOT / "lean-voice-artifact" / f"lean-voice-{native_target}.tar.gz"
    staged = REPO_ROOT / "lean-voice-input" / native_target
    staged.parent.mkdir(parents=True, exist_ok=True)
    provenance = verify_artifact(archive.parent, native_target, staged)
    voice_commit = provenance["sourceCommit"]
    if os.environ.get("CODEX_VOICE_BUILD_COMMIT") != voice_commit:
        raise ValueError("compiled app voice commit differs from verified artifact")
    suffix = ".exe" if target.endswith("-windows-msvc") else ""
    output = REPO_ROOT / "lean-voice-package" / target
    output.parent.mkdir(exist_ok=True)
    assemble(package, staged / f"codex-voice-host{suffix}", native_target, commit, output,
             runtime=staged / "runtime", release_version=version, voice_build_commit=voice_commit)
    voice = output / "codex-resources/voice"
    proof = voice / "provenance.json"
    proof.write_text(json.dumps(provenance, indent=2) + "\n", encoding="utf-8")
    manifest_path = voice / "manifest.json"
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    manifest["voiceInputFingerprint"] = provenance["inputFingerprint"]
    manifest["sha256"]["codex-resources/voice/provenance.json"] = digest(proof)
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    return output


def validate_voice(package: Path, target: str, version: str, commit: str) -> None:
    voice = package / "codex-resources/voice"
    files = runtime_files(voice.resolve(strict=True), voice_target(target), public_release=True)
    receipt = json.loads((voice / "runtime.json").read_text(encoding="utf-8"))
    manifest = json.loads((voice / "manifest.json").read_text(encoding="utf-8"))
    provenance = json.loads((voice / "provenance.json").read_text(encoding="utf-8"))
    voice_commit = cache.validate_provenance(provenance, voice_target(target))
    if (receipt["sourceCommit"] != voice_commit or manifest["schemaVersion"] != 1
            or manifest["buildCommit"] != commit or manifest["appVersion"] != version
            or manifest.get("voiceBuildCommit") != voice_commit
            or manifest.get("voiceInputFingerprint") != provenance["inputFingerprint"]
            or manifest["appTarget"] != target or manifest["voiceTarget"] != voice_target(target)):
        raise ValueError("Voice commit, input provenance, app version or target mismatch")
    suffix = ".exe" if target.endswith("-windows-msvc") else ""
    helper = f"codex-resources/voice/bin/codex-voice-host{suffix}"
    cache.validate_payload(package / helper, voice, provenance, voice_target(target), packaged=True)
    required = {f"bin/codex{suffix}", helper, *(f"codex-resources/voice/{name}" for name in files)}
    required.update(f"codex-resources/voice/{name}" for name in ("NOTICE.md", "sources.json", "licenses/LGPL-2.1.txt", "provenance.json"))
    if suffix:
        required.update(("codex-resources/voice/bin/vcruntime140.dll", "codex-resources/voice/windows-crt.json",
                         "codex-resources/voice/bin/gstreamer-1.0-0.dll"))
    elif target.endswith("-musl"):
        required.add("codex-resources/voice/lib/libgstreamer-1.0.so.0")
    else:
        required.add("codex-resources/voice/lib/libgstreamer-1.0.0.dylib")
    hashes = manifest["sha256"]
    if not required.issubset(hashes):
        raise ValueError("Voice manifest omits required app, helper or runtime files")
    for name, expected in hashes.items():
        path = package / name
        if (Path(name).is_absolute() or ".." in Path(name).parts or path.is_symlink()
                or not path.is_file() or not path.resolve().is_relative_to(package.resolve())
                or not re.fullmatch(r"[0-9a-f]{64}", expected) or digest(path) != expected):
            raise ValueError(f"Voice package digest mismatch: {name}")
    actual = {path.relative_to(package).as_posix() for path in voice.rglob("*") if path.is_file()}
    expected = {name for name in hashes if name.startswith("codex-resources/voice/")}
    if actual != expected | {"codex-resources/voice/manifest.json"}:
        raise ValueError("Voice package has unlisted runtime files")


def read_frame(stream: BinaryIO) -> dict:
    def read_exact(length: int) -> bytes:
        chunks = bytearray()
        while len(chunks) < length:
            chunk = stream.read(length - len(chunks))
            if not chunk:
                raise ValueError("Voice helper closed before replying")
            chunks.extend(chunk)
        return bytes(chunks)

    length = struct.unpack(">I", read_exact(4))[0]
    # Keep aligned with codex-realtime-webrtc's MAX_FRAME_BYTES.
    if not 0 < length <= 128 * 1024:
        raise ValueError("Invalid voice helper frame length")
    return json.loads(read_exact(length))


def smoke_voice(package: Path, target: str, commit: str) -> None:
    """Handshake and load all packaged GStreamer plugins without opening devices."""
    suffix = ".exe" if target.endswith("-windows-msvc") else ""
    helper = package / "codex-resources/voice/bin" / f"codex-voice-host{suffix}"
    result = subprocess.run([str(helper), "--build-commit"], check=True, capture_output=True, text=True, timeout=30)
    if result.stdout.strip() != commit:
        raise ValueError("Voice executable was compiled from a different commit")
    env = {**os.environ, "GST_PLUGIN_PATH": "", "GST_PLUGIN_PATH_1_0": "",
           "GST_PLUGIN_SYSTEM_PATH": "", "GST_PLUGIN_SYSTEM_PATH_1_0": "",
           "GST_REGISTRY": "NUL" if suffix else "/dev/null",
           "GST_REGISTRY_UPDATE": "no", "GST_REGISTRY_FORK": "no"}
    with tempfile.TemporaryFile() as errors:
        process = subprocess.Popen([str(helper)], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                   stderr=errors, env=env)
        reader = concurrent.futures.ThreadPoolExecutor(max_workers=1)
        try:
            assert process.stdin is not None and process.stdout is not None
            exchanges = (
                ({"type": "hello", "protocol": 1, "buildCommit": commit}, {"type": "ready"}),
                ({"type": "initializeRuntime"}, {"type": "runtimeReady"}),
                ({"type": "close"}, {"type": "closed"}),
            )
            for request, expected in exchanges:
                payload = json.dumps(request).encode()
                process.stdin.write(struct.pack(">I", len(payload)) + payload)
                process.stdin.flush()
                response = reader.submit(read_frame, process.stdout).result(timeout=30)
                if response != expected:
                    raise ValueError(f"Unexpected voice runtime reply: {response!r}")
            process.stdin.close()
            if process.wait(timeout=10) != 0:
                raise ValueError("Voice helper did not shut down cleanly")
        except BaseException:
            errors.seek(0)
            print(errors.read(8192).decode(errors="replace"), file=sys.stderr)
            raise
        finally:
            if process.poll() is None:
                process.kill()
            process.wait(timeout=10)
            reader.shutdown(wait=True, cancel_futures=True)
            if process.stdin is not None:
                process.stdin.close()
            if process.stdout is not None:
                process.stdout.close()


def native_smoke(staged: Path, target: str, commit: str) -> None:
    """Use the helper's physical private-package layout, including Windows CRT DLLs."""
    with tempfile.TemporaryDirectory(prefix="lean-voice-smoke-") as temporary:
        package = Path(temporary)
        voice = package / "codex-resources/voice"
        shutil.copytree(staged / "runtime", voice)
        suffix = ".exe" if target.endswith("-windows-msvc") else ""
        (voice / "bin").mkdir(exist_ok=True)
        shutil.copy2(staged / f"codex-voice-host{suffix}", voice / "bin" / f"codex-voice-host{suffix}")
        smoke_voice(package, target, commit)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("identity", "legacy-identity", "migrate", "seal", "verify"))
    parser.add_argument("--target", default=os.environ.get("VOICE_TARGET") or os.environ.get("TARGET"))
    parser.add_argument("--directory", type=Path, default=REPO_ROOT / "lean-voice-cache")
    parser.add_argument("--staged", type=Path)
    parser.add_argument("--expected-input-fingerprint")
    parser.add_argument("--expected-workspace-version")
    parser.add_argument("--legacy-root", type=Path)
    parser.add_argument("--smoke", action="store_true")
    parser.add_argument("--export-env", action="store_true")
    args = parser.parse_args()
    if args.command == "identity":
        fingerprint = cache.input_fingerprint(cache.source_fingerprint(REPO_ROOT, args.target),
                                              cache.image_identity(), cache.tool_identity())
        print(f"key=lean-voice-v{cache.SCHEMA}-{args.target}-{fingerprint}")
        print(f"fingerprint={fingerprint}")
        print(f"workspace-version={cache.workspace_version(REPO_ROOT)}")
    elif args.command in ("legacy-identity", "migrate"):
        if args.legacy_root is None:
            parser.error(f"{args.command} requires --legacy-root")
        if args.command == "legacy-identity":
            key = legacy_identity(args.legacy_root, args.target)
            print(f"key={key or ''}")
        else:
            migrate_artifact(args.directory, args.target, args.legacy_root)
    elif args.command == "seal":
        if args.staged is None:
            parser.error("seal requires --staged")
        if args.expected_input_fingerprint is None:
            parser.error("seal requires --expected-input-fingerprint from pre-build identity")
        if args.expected_workspace_version is None:
            parser.error("seal requires --expected-workspace-version from pre-build identity")
        cache.seal_artifact(args.directory, args.staged, args.target, os.environ["STABLE_GIT_COMMIT"],
                            expected_input_fingerprint=args.expected_input_fingerprint,
                            expected_workspace_version=args.expected_workspace_version)
    else:
        with tempfile.TemporaryDirectory(prefix="lean-voice-verify-") as temporary:
            staged = Path(temporary) / "payload"
            provenance = verify_artifact(args.directory, args.target, staged)
            # Export only a stamp proven by the executable, before Cargo embeds it.
            if args.smoke or args.export_env:
                native_smoke(staged, args.target, provenance["sourceCommit"])
            if args.export_env:
                with Path(os.environ["GITHUB_ENV"]).open("a", encoding="utf-8") as env:
                    env.write(f"CODEX_VOICE_BUILD_COMMIT={provenance['sourceCommit']}\n")


if __name__ == "__main__":
    main()
