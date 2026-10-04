#!/usr/bin/env python3
"""Exact native voice inputs and sealed artifact provenance, never a commit relabel."""

import hashlib
import json
import os
import platform
import re
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "third_party/voice"))
from package_runtime import runtime_files
from runtime import digest

TARGETS = {"x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu",
           "aarch64-apple-darwin", "x86_64-pc-windows-msvc"}
SCHEMA = 1
# These files own the build graph, tool pins, source preparation and release recipe.
# Cargo workspace version is meaningful to Bazel's DEP_DATA and is not stripped.
FILES = ("codex-rs/Cargo.toml", "codex-rs/Cargo.lock", "MODULE.bazel",
         "MODULE.bazel.lock", "BUILD.bazel", "defs.bzl", "rbe.bzl",
         ".bazelrc", ".bazelversion", ".bazelignore", ".gitattributes",
         "scripts/workspace-status.sh", "scripts/workspace-status.cmd",
         ".github/workflows/lean-release.yml", ".github/scripts/lean_voice.py",
         ".github/scripts/lean_voice_cache.py", ".github/scripts/setup-voice-windows.ps1",
         ".github/scripts/voice-cygwin-inputs.py", ".github/scripts/voice-cygwin-snapshot.json",
         ".github/scripts/voice_windows_tools.py", ".github/scripts/compute-bazel-windows-path.ps1")
TREES = ("third_party/voice", "bazel", "patches", ".github/actions/setup-msvc-env")
IGNORED = {"target", "__pycache__", ".pytest_cache", ".git"}


def local_crates(root: Path) -> set[Path]:
    """Follow all normal/build path dependencies, including platform-specific ones."""
    workspace = root / "codex-rs"
    config = tomllib.loads((workspace / "Cargo.toml").read_text(encoding="utf-8"))
    shared = config["workspace"].get("dependencies", {})
    pending, visited = [workspace / "voice-host"], set()
    while pending:
        crate = pending.pop().resolve(strict=True)
        if not crate.is_relative_to(root.resolve()):
            raise ValueError("voice dependency escapes the checkout")
        if crate in visited:
            continue
        visited.add(crate)
        manifest = tomllib.loads((crate / "Cargo.toml").read_text(encoding="utf-8"))
        tables = [manifest, *manifest.get("target", {}).values()]
        for table in tables:
            for kind in ("dependencies", "build-dependencies"):
                for name, dependency in table.get(kind, {}).items():
                    base = crate
                    if isinstance(dependency, dict) and dependency.get("workspace"):
                        dependency, base = shared[name], workspace
                    if isinstance(dependency, dict) and "path" in dependency:
                        pending.append(base / dependency["path"])
    return visited


def source_fingerprint(root: Path, target: str) -> str:
    if target not in TARGETS:
        raise ValueError("unsupported voice cache target")
    root = root.resolve(strict=True)
    paths = {root / name for name in FILES}
    workspace = root / "codex-rs"
    config = tomllib.loads((workspace / "Cargo.toml").read_text(encoding="utf-8"))
    # rules_rs resolves the entire workspace. Other member manifests can change
    # shared external features without changing Cargo.lock. Their source is not input.
    for member in config["workspace"].get("members", []):
        paths.update(path / "Cargo.toml" for path in workspace.glob(member))
    for dependency in config["workspace"].get("dependencies", {}).values():
        if isinstance(dependency, dict) and "path" in dependency:
            paths.add(workspace / dependency["path"] / "Cargo.toml")
    for directory in [*(root / name for name in TREES), *local_crates(root)]:
        if not directory.is_dir():
            raise ValueError(f"missing voice input tree: {directory}")
        for parent, children, files in os.walk(directory):
            children[:] = [name for name in children if name not in IGNORED]
            if any((Path(parent) / name).is_symlink() for name in children):
                raise ValueError("voice source trees must not contain symbolic links")
            paths.update(Path(parent) / name for name in files)
    records = []
    for path in sorted(paths, key=lambda path: path.relative_to(root).as_posix()):
        if path.is_symlink() or not path.is_file() or not path.resolve().is_relative_to(root):
            raise ValueError(f"voice input must be a regular checkout file: {path}")
        records.append([path.relative_to(root).as_posix(), digest(path)])
    return json_digest({"schemaVersion": SCHEMA, "target": target, "inputs": records})


def json_digest(value: object) -> str:
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def image_identity() -> dict[str, str]:
    image = {name: os.environ.get(name, "") for name in ("ImageOS", "ImageVersion")}
    if not all(image.values()):
        raise ValueError("native runner ImageOS and ImageVersion are required")
    return image


def tool_identity() -> dict[str, str]:
    # setup-python's "3.12" selector can advance independently of the runner image.
    return {"python": platform.python_version()}


def input_fingerprint(source: str, image: dict, tools: dict) -> str:
    if set(image) != {"ImageOS", "ImageVersion"} or not all(isinstance(v, str) and v for v in image.values()):
        raise ValueError("invalid native runner image identity")
    if set(tools) != {"python"} or not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", tools["python"]):
        raise ValueError("invalid native Python tool identity")
    return json_digest({"sourceFingerprint": source, "runnerImage": image, "toolVersions": tools})


def inventory(root: Path) -> dict[str, str]:
    files = {}
    for path in root.rglob("*"):
        if path.is_symlink() or not (path.is_file() or path.is_dir()):
            raise ValueError("voice artifact contains a non-regular file")
        if path.is_file():
            files[path.relative_to(root).as_posix()] = digest(path)
    return files


def validate_provenance(provenance: dict, target: str, *, root: Path = ROOT,
                        current_image: bool = False) -> str:
    source = source_fingerprint(root, target)  # Independent checkout proof, not the artifact's claim.
    commit = provenance.get("sourceCommit", "")
    if (provenance.get("schemaVersion") != SCHEMA or provenance.get("target") != target
            or not re.fullmatch(r"[0-9a-f]{40}", commit)
            or provenance.get("sourceFingerprint") != source
            or provenance.get("inputFingerprint") != input_fingerprint(
                source, provenance.get("runnerImage", {}), provenance.get("toolVersions", {}))):
        raise ValueError("voice cache input provenance mismatch")
    if current_image and provenance["runnerImage"] != image_identity():
        raise ValueError("voice cache runner image mismatch")
    if current_image and provenance["toolVersions"] != tool_identity():
        raise ValueError("voice cache Python tool version mismatch")
    for field in ("archiveSha256", "helperSha256"):
        if not re.fullmatch(r"[0-9a-f]{64}", provenance.get(field, "")):
            raise ValueError("invalid voice artifact digest")
    return commit


def validate_payload(helper: Path, runtime: Path, provenance: dict, target: str, *, packaged: bool = False) -> None:
    expected = runtime_files(runtime.resolve(strict=True), target, public_release=True)
    receipt = json.loads((runtime / "runtime.json").read_text(encoding="utf-8"))
    if receipt["sourceCommit"] != provenance["sourceCommit"]:
        raise ValueError("voice runtime source commit differs from original helper build")
    actual = expected if packaged else inventory(runtime)
    if actual != provenance.get("runtimeSha256") or digest(helper) != provenance["helperSha256"]:
        raise ValueError("voice artifact payload digest mismatch")
    # Stage/seal ships only the sealed receipt and its listed runtime libraries.
    if actual != expected:
        raise ValueError("voice artifact has unlisted runtime files")


def seal_artifact(directory: Path, staged: Path, target: str, commit: str, *,
                  expected_input_fingerprint: str) -> None:
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ValueError("voice source commit must be a full Git SHA")
    suffix = ".exe" if target.endswith("-windows-msvc") else ""
    source = source_fingerprint(ROOT, target)
    image = image_identity()
    tools = tool_identity()
    fingerprint = input_fingerprint(source, image, tools)
    # Seal the inputs used for the pre-build cache key, not a mutated checkout.
    if fingerprint != expected_input_fingerprint:
        raise ValueError("voice inputs changed after cache identity was calculated")
    provenance = {"schemaVersion": SCHEMA, "target": target, "sourceCommit": commit,
                  "sourceFingerprint": source, "runnerImage": image,
                  "toolVersions": tools, "inputFingerprint": fingerprint,
                  "archiveSha256": digest(directory / f"lean-voice-{target}.tar.gz"),
                  "helperSha256": digest(staged / f"codex-voice-host{suffix}"),
                  "runtimeSha256": inventory(staged / "runtime")}
    validate_payload(staged / f"codex-voice-host{suffix}", staged / "runtime", provenance, target)
    (directory / f"lean-voice-{target}.provenance.json").write_text(json.dumps(provenance, indent=2) + "\n", encoding="utf-8")
