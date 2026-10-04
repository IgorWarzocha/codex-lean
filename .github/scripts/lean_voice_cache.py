#!/usr/bin/env python3
"""Native voice compatibility inputs and sealed original-build provenance."""

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
SCHEMA = 2
COMPATIBILITY_POLICY = "workspace-release-version-v1"
RELEASE_VERSION = "<workspace-release-version>"
LEGACY_SOURCE_COMMIT = "19e5ad36f344597a945f43d4e27cb33da0163682"
# These files own the build graph, tool pins, source preparation and release recipe.
# Bazel still compiles with the real DEP_DATA package versions. This identity
# permits reuse of that original build, not byte-identical recompilation, when
# only the inherited workspace release number changes.
LEGACY_FILES = ("codex-rs/Cargo.toml", "codex-rs/Cargo.lock", "MODULE.bazel",
         "MODULE.bazel.lock", "BUILD.bazel", "defs.bzl", "rbe.bzl",
         ".bazelrc", ".bazelversion", ".bazelignore", ".gitattributes",
         "scripts/workspace-status.sh", "scripts/workspace-status.cmd",
         ".github/workflows/lean-release.yml", ".github/scripts/lean_voice.py",
         ".github/scripts/lean_voice_cache.py", ".github/scripts/setup-voice-windows.ps1",
         ".github/scripts/voice-cygwin-inputs.py", ".github/scripts/voice-cygwin-snapshot.json",
         ".github/scripts/voice_windows_tools.py", ".github/scripts/compute-bazel-windows-path.ps1")
FILES = tuple(name for name in LEGACY_FILES if name not in {
    ".github/scripts/lean_voice.py", ".github/scripts/lean_voice_cache.py"})
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


def workspace_manifests(workspace: Path, config: dict) -> set[Path]:
    manifests = set()
    for member in config["workspace"].get("members", []):
        manifests.update(path / "Cargo.toml" for path in workspace.glob(member))
    for dependency in config["workspace"].get("dependencies", {}).values():
        if isinstance(dependency, dict) and "path" in dependency:
            manifests.add(workspace / dependency["path"] / "Cargo.toml")
    return manifests


def cargo_inputs(workspace: Path, config: dict, manifests: set[Path]) -> dict[Path, str]:
    """Ignore release numbering, never external or independently versioned crates."""
    version = config["workspace"]["package"]["version"]
    inherited = set()
    inputs = {}
    for path in manifests:
        manifest = tomllib.loads(path.read_text(encoding="utf-8"))
        package = manifest.get("package", {})
        if package.get("version") == {"workspace": True}:
            inherited.add(package["name"])
        inputs[path] = json_digest(manifest)
    # Normalization is only for the compatibility digest, never the build files.
    config["workspace"]["package"]["version"] = RELEASE_VERSION
    inputs[workspace / "Cargo.toml"] = json_digest(config)
    lock_path = workspace / "Cargo.lock"
    lock = tomllib.loads(lock_path.read_text(encoding="utf-8"))
    qualified = {f"{name} {version}": f"{name} {RELEASE_VERSION}" for name in inherited}
    for package in lock.get("package", []):
        if "source" not in package and package["name"] in inherited:
            if package["version"] != version:
                raise ValueError("workspace package versions in Cargo.lock are not synchronized")
            package["version"] = RELEASE_VERSION
        package["dependencies"] = [qualified.get(dependency, dependency)
                                   for dependency in package.get("dependencies", [])]
    inputs[lock_path] = json_digest(lock)
    return inputs


def voice_build_recipe(path: Path) -> str:
    """Hash native setup/build/staging, not cache transport or validation code."""
    workflow = path.read_text(encoding="utf-8")
    voice = workflow.split("\n  voice:\n", 1)[1].split("\n  build:\n", 1)[0]
    header, steps = voice.split("    steps:\n", 1)
    excluded = {
        "Calculate exact native inputs from this checkout and runner image",
        "Restore only the exact sealed native voice artifact",
        "Checkout original published voice sources for verified cache migration",
        "Calculate compatible original native voice cache key",
        "Restore the compatible original sealed native voice artifact",
        "Migrate verified original provenance without changing native bytes",
        "Test Windows receipt permissions before native compilation",
        "Verify native voice runner and report resource budget",
        "Verify native provenance, bytes, build stamp and private runtime initialization",
        "Save immutable voice bytes only after native validation",
        "Report remaining disk after native build",
    }
    recipe = [header]
    seen = set()
    for step in re.split(r"(?=^      - )", steps, flags=re.MULTILINE):
        name = re.match(r"      - name: ([^\n]+)", step)
        if name:
            seen.add(name[1])
            if name[1] in excluded:
                continue
        if "      - uses: actions/upload-artifact@" in step:
            continue
        # Drop only cache selection. Platform conditions still affect tool setup.
        condition = re.search(r"^        if:([^\n]*)\n", step, flags=re.MULTILINE)
        if condition:
            native_condition = re.sub(r"(?: && )?steps\.voice-(?:legacy-)?cache\.outputs\.cache-hit != 'true'",
                                      "", condition[1]).strip()
            step = step.replace(condition[0], f"        if: {native_condition}\n" if native_condition else "")
        if name and name[1] == "Stage and seal the matching runtime":
            step = re.sub(r"^          EXPECTED_VOICE_(?:INPUT_FINGERPRINT|WORKSPACE_VERSION):.*\n",
                          "", step, flags=re.MULTILINE)
            step = step.replace("        env:\n        run:", "        run:")
            # The preceding strip, codesign and runtime sealing affect shipped
            # bytes. Skip only the provenance command, not subsequent commands.
            lines, skipping = [], False
            for line in step.splitlines():
                if line.startswith("          python .github/scripts/lean_voice.py seal"):
                    skipping = True
                if skipping:
                    if any(operator in line for operator in ("&&", "||", ";")):
                        raise ValueError("cache provenance command must not contain native shell commands")
                    skipping = line.endswith("\\")
                else:
                    lines.append(line)
            step = "\n".join(lines)
        recipe.append(step.rstrip())
    if not {"Build same-commit native helper and private audio runtime",
            "Stage and seal the matching runtime"}.issubset(seen):
        raise ValueError("native voice workflow recipe boundaries changed")
    return json_digest(recipe)


def source_fingerprint(root: Path, target: str, *, legacy: bool = False) -> str:
    if target not in TARGETS:
        raise ValueError("unsupported voice cache target")
    root = root.resolve(strict=True)
    paths = {root / name for name in (LEGACY_FILES if legacy else FILES)}
    workspace = root / "codex-rs"
    config = tomllib.loads((workspace / "Cargo.toml").read_text(encoding="utf-8"))
    # rules_rs resolves the entire workspace. Other member manifests can change
    # shared external features without changing Cargo.lock. Their source is not input.
    crates = local_crates(root)
    manifests = workspace_manifests(workspace, config) | {crate / "Cargo.toml" for crate in crates}
    paths.update(manifests)
    for directory in [*(root / name for name in TREES), *crates]:
        if not directory.is_dir():
            raise ValueError(f"missing voice input tree: {directory}")
        for parent, children, files in os.walk(directory):
            children[:] = [name for name in children if name not in IGNORED]
            if any((Path(parent) / name).is_symlink() for name in children):
                raise ValueError("voice source trees must not contain symbolic links")
            paths.update(Path(parent) / name for name in files)
    cargo = {} if legacy else cargo_inputs(workspace, config, manifests)
    if not legacy:
        workflow = root / ".github/workflows/lean-release.yml"
        cargo[workflow] = voice_build_recipe(workflow)
    records = []
    for path in sorted(paths, key=lambda path: path.relative_to(root).as_posix()):
        if path.is_symlink() or not path.is_file() or not path.resolve().is_relative_to(root):
            raise ValueError(f"voice input must be a regular checkout file: {path}")
        # Package versions remain real compiler inputs. Refuse this compatibility
        # exception if local voice code starts consuming their environment values.
        if not legacy and path.suffix == ".rs" and any(path.is_relative_to(crate) for crate in crates):
            if b"CARGO_PKG_VERSION" in path.read_bytes():
                raise ValueError("voice code consumes CARGO_PKG_VERSION, review release-version compatibility")
        records.append([path.relative_to(root).as_posix(), cargo[path] if path in cargo else digest(path)])
    identity = {"schemaVersion": 1 if legacy else SCHEMA, "target": target, "inputs": records}
    if not legacy:
        identity["compatibilityPolicy"] = COMPATIBILITY_POLICY
    return json_digest(identity)


def workspace_version(root: Path) -> str:
    return tomllib.loads((root / "codex-rs/Cargo.toml").read_text(encoding="utf-8"))["workspace"]["package"]["version"]


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
                        current_image: bool = False, legacy: bool = False) -> str:
    source = source_fingerprint(root, target, legacy=legacy)  # Independent checkout proof.
    commit = provenance.get("sourceCommit", "")
    if legacy:
        if commit != LEGACY_SOURCE_COMMIT:
            raise ValueError("unsupported original voice build for migration")
    elif (provenance.get("compatibilityPolicy") != COMPATIBILITY_POLICY
          or not isinstance(provenance.get("workspaceVersion"), str) or not provenance["workspaceVersion"]):
        raise ValueError("voice cache compatibility provenance mismatch")
    if (provenance.get("schemaVersion") != (1 if legacy else SCHEMA) or provenance.get("target") != target
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
    original = provenance.get("legacyProvenance")
    if original is not None:
        fields = ("target", "sourceCommit", "runnerImage", "toolVersions", "archiveSha256", "helperSha256", "runtimeSha256")
        if (original.get("schemaVersion") != 1 or original.get("sourceCommit") != LEGACY_SOURCE_COMMIT
                or any(original.get(field) != provenance.get(field) for field in fields)
                or original.get("inputFingerprint") != input_fingerprint(
                    original.get("sourceFingerprint"), original.get("runnerImage", {}), original.get("toolVersions", {}))):
            raise ValueError("migrated original voice provenance mismatch")
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
                  expected_input_fingerprint: str, expected_workspace_version: str) -> None:
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ValueError("voice source commit must be a full Git SHA")
    suffix = ".exe" if target.endswith("-windows-msvc") else ""
    source = source_fingerprint(ROOT, target)
    image = image_identity()
    tools = tool_identity()
    fingerprint = input_fingerprint(source, image, tools)
    # Seal the inputs used for the pre-build cache key, not a mutated checkout.
    if fingerprint != expected_input_fingerprint or workspace_version(ROOT) != expected_workspace_version:
        raise ValueError("voice inputs changed after cache identity was calculated")
    provenance = {"schemaVersion": SCHEMA, "target": target, "sourceCommit": commit,
                  "compatibilityPolicy": COMPATIBILITY_POLICY, "workspaceVersion": workspace_version(ROOT),
                  "sourceFingerprint": source, "runnerImage": image,
                  "toolVersions": tools, "inputFingerprint": fingerprint,
                  "archiveSha256": digest(directory / f"lean-voice-{target}.tar.gz"),
                  "helperSha256": digest(staged / f"codex-voice-host{suffix}"),
                  "runtimeSha256": inventory(staged / "runtime")}
    validate_payload(staged / f"codex-voice-host{suffix}", staged / "runtime", provenance, target)
    (directory / f"lean-voice-{target}.provenance.json").write_text(json.dumps(provenance, indent=2) + "\n", encoding="utf-8")
