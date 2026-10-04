#!/usr/bin/env python3
"""Fork release boundaries: ref validation, complete packaging and asset verification."""

import argparse
import hashlib
import json
import os
import platform
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import tomllib
import zipfile
from pathlib import Path

# The canonical package builder is repository-local, not an installed dependency.
REPO_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO_ROOT / "scripts"))
sys.path.insert(0, str(Path(__file__).resolve().parent))

from codex_package.archive import write_archive
from codex_package.layout import validate_package_dir, write_json
from codex_package.targets import PACKAGE_VARIANTS, TARGET_SPECS
from lean_voice import add_voice, extract_archive, smoke_voice, validate_voice

REPOSITORY = "IgorWarzocha/codex-lean"
TARGETS = (
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
)
VERSION_PATTERN = re.compile(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)-lean\.(0|[1-9][0-9]*)")


def validate_identity(version: str, commit: str) -> None:
    match = VERSION_PATTERN.fullmatch(version)
    if not match or any(int(part) > 2**64 - 1 for part in match.groups()):
        raise ValueError(f"Expected a fork version such as 0.160.0-lean.1, got {version!r}")
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ValueError("Release commit must be a full lowercase Git SHA")


def validate_ref(repository: str, event: str, ref: str, version: str, commit: str, publish: str) -> bool:
    validate_identity(version, commit)
    if repository != REPOSITORY:
        raise ValueError(f"This release workflow belongs to {REPOSITORY}")
    if event == "push" and ref == f"refs/tags/lean-v{version}":
        return True
    if event == "workflow_dispatch" and ref == "refs/heads/lean" and publish in ("true", "false"):
        return publish == "true"
    raise ValueError("Use a tag matching Cargo.toml, or dispatch on the lean branch")


def voice_matrix(scope: str, publish: bool) -> dict:
    if scope not in ("all", "windows-voice"):
        raise ValueError("Build scope must be all or windows-voice")
    if publish and scope != "all":
        raise ValueError("Publishing requires all four complete packages")
    entries = [
        {"runner": "ubuntu-24.04", "target": "x86_64-unknown-linux-gnu", "prefix": "linux_x86_64"},
        {"runner": "ubuntu-24.04-arm", "target": "aarch64-unknown-linux-gnu", "prefix": "linux_aarch64"},
        {"runner": "macos-15", "target": "aarch64-apple-darwin", "prefix": "macos_aarch64"},
        {"runner": "windows-2025", "target": "x86_64-pc-windows-msvc", "prefix": "windows_x86_64"},
    ]
    if scope == "windows-voice":
        entries = [entry for entry in entries if entry["target"] == "x86_64-pc-windows-msvc"]
    return {"include": entries}


def check() -> None:
    with (REPO_ROOT / "codex-rs/Cargo.toml").open("rb") as source:
        version = tomllib.load(source)["workspace"]["package"]["version"]
    commit = os.environ["GITHUB_SHA"]
    publish = validate_ref(os.environ["GITHUB_REPOSITORY"], os.environ["GITHUB_EVENT_NAME"],
                           os.environ["GITHUB_REF"], version, commit, os.environ["INPUT_PUBLISH"])
    scope = os.environ.get("INPUT_SCOPE") or "all"
    matrix = voice_matrix(scope, publish)
    if scope == "all":
        render_release_notes(version, commit)
    print(f"version={version}\ncommit={commit}\ntag=lean-v{version}\npublish={str(publish).lower()}")
    print(f"scope={scope}\nvoice_matrix={json.dumps(matrix, separators=(',', ':'))}")


def runner() -> None:
    target = os.environ["TARGET"].replace("-unknown-linux-gnu", "-unknown-linux-musl")
    expected = {
        TARGETS[0]: ("Linux", "x86_64"),
        TARGETS[1]: ("Linux", "aarch64"),
        TARGETS[2]: ("Darwin", "arm64"),
        TARGETS[3]: ("Windows", "amd64"),
    }[target]
    actual = (platform.system(), platform.machine().lower())
    if actual != (expected[0], expected[1].lower()):
        raise ValueError(f"Expected native {expected}, got {actual}")
    print(f"Native runner: {actual}, CPUs: {os.cpu_count()}, free SSD: {shutil.disk_usage(REPO_ROOT).free // 2**30} GiB")


def sha256(path: Path) -> str:
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def archive_name(version: str, target: str) -> str:
    suffix = ".zip" if TARGET_SPECS[target].is_windows else ".tar.gz"
    return f"codex-lean-{version}-{target}{suffix}"


def finalize_package(package: Path, version: str, target: str, commit: str, bwrap_digest: str) -> None:
    validate_identity(version, commit)
    spec = TARGET_SPECS[target]
    validate_package_dir(package, PACKAGE_VARIANTS["codex"], spec, include_zsh=not spec.is_windows)
    if spec.is_linux and (not re.fullmatch(r"[0-9a-f]{64}", bwrap_digest)
                          or sha256(package / "codex-resources/bwrap") != bwrap_digest):
        raise ValueError("Packaged bwrap differs from the digest embedded before the Codex build")
    metadata_path = package / "codex-package.json"
    metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
    if metadata["version"] != version:
        raise ValueError("Package version does not match release version")
    validate_voice(package, target, version, commit)
    voice_manifest = json.loads((package / "codex-resources/voice/manifest.json").read_text(encoding="utf-8"))
    metadata["forkRelease"] = {
        "repository": REPOSITORY, "commit": commit, "tag": f"lean-v{version}",
        "voiceBundled": True, "signed": False, "notarized": False,
        "voiceBuildCommit": voice_manifest["voiceBuildCommit"],
        "voiceInputFingerprint": voice_manifest["voiceInputFingerprint"],
        "zshBundled": not spec.is_windows,
        "bwrapSha256": bwrap_digest if spec.is_linux else None,
    }
    write_json(metadata_path, metadata)
    (package / "README.txt").write_text(
        f"Codex Lean {version}\nSource commit: {commit}\nTarget: {target}\n\n"
        f"Voice source commit: {voice_manifest['voiceBuildCommit']}\n"
        "Includes the input-verified native voice helper and private GStreamer audio runtime.\n"
        "Linux CLI uses musl. Native voice requires glibc 2.28 or newer.\n"
        "macOS and Windows executables are unsigned. macOS is not notarized.\n"
        "Keep bin, codex-resources and codex-path together. Add bin to PATH.\n"
        "ripgrep is bundled. Patched zsh is bundled on Linux and macOS, not Windows.\n",
        encoding="utf-8",
    )
    for name in ("LICENSE", "NOTICE"):
        shutil.copyfile(REPO_ROOT / name, package / name)


def package() -> None:
    version, commit, target = (os.environ[key] for key in ("RELEASE_VERSION", "RELEASE_COMMIT", "TARGET"))
    validate_identity(version, commit)
    spec = TARGET_SPECS[target]
    binaries = REPO_ROOT / "codex-rs/target" / target / "release"
    package_dir = REPO_ROOT / "lean-package" / target
    args = [sys.executable, str(REPO_ROOT / "scripts/build_codex_package.py"),
            "--target", target, "--package-version", version, "--package-dir", str(package_dir),
            "--entrypoint-bin", str(binaries / f"codex{spec.exe_suffix}"),
            "--code-mode-host-bin", str(binaries / f"codex-code-mode-host{spec.exe_suffix}")]
    if spec.is_linux:
        args.extend(["--bwrap-bin", str(binaries / "bwrap")])
    if spec.is_windows:
        for name in ("codex-command-runner", "codex-windows-sandbox-setup"):
            args.extend([f"--{name}-bin", str(binaries / f"{name}.exe")])
    # Every source binary is supplied, so packaging cannot start an unlocked build.
    subprocess.run(args, check=True)
    package_dir = add_voice(package_dir, target, version, commit)
    finalize_package(package_dir, version, target, commit, os.environ.get("CODEX_BWRAP_SHA256", ""))
    executable = package_dir / "bin" / f"codex{spec.exe_suffix}"
    result = subprocess.run([str(executable), "--version"], check=True, capture_output=True, text=True, timeout=60)
    if result.stdout.strip() != f"Codex Lean {version}":
        raise ValueError(f"Packaged CLI identity mismatch: {result.stdout!r}")
    for path, flag in ((executable, "--help"),
                       (package_dir / "bin" / f"codex-code-mode-host{spec.exe_suffix}", "--help"),
                       (package_dir / "codex-path" / spec.rg_name, "--version")):
        subprocess.run([str(path), flag], check=True, timeout=60)
    if not spec.is_windows:
        subprocess.run([str(package_dir / "codex-resources/zsh/bin/zsh"), "--version"], check=True, timeout=60)
    if spec.is_linux:
        subprocess.run([str(package_dir / "codex-resources/bwrap"), "--version"], check=True, timeout=60)
    smoke_voice(package_dir, target, os.environ["CODEX_VOICE_BUILD_COMMIT"])
    output_dir = REPO_ROOT / "lean-dist"
    output_dir.mkdir(exist_ok=True)
    output = output_dir / archive_name(version, target)
    write_archive(package_dir, output, force=False)
    output.with_name(output.name + ".sha256").write_text(f"{sha256(output)}  {output.name}\n", encoding="utf-8")


def read_archive_metadata(path: Path) -> dict:
    if path.suffix == ".zip":
        with zipfile.ZipFile(path) as archive:
            return json.loads(archive.read("codex-package.json"))
    with tarfile.open(path, "r:gz") as archive:
        source = archive.extractfile("codex-package.json")
        if source is None:
            raise ValueError("Archive has no package manifest")
        with source:
            return json.load(source)


def verify_assets(directory: Path, version: str, commit: str) -> str:
    validate_identity(version, commit)
    names = [archive_name(version, target) for target in TARGETS]
    expected = set(names + [name + ".sha256" for name in names])
    if {path.name for path in directory.iterdir()} != expected:
        raise ValueError("Release requires exactly one archive and checksum for every platform")
    checksums = []
    for target, name in zip(TARGETS, names):
        archive = directory / name
        checksum = f"{sha256(archive)}  {name}\n"
        if (directory / (name + ".sha256")).read_text(encoding="utf-8") != checksum:
            raise ValueError(f"Checksum mismatch: {name}")
        metadata = read_archive_metadata(archive)
        release = metadata["forkRelease"]
        if (metadata["version"] != version or metadata["target"] != target
                or metadata["variant"] != "codex" or release["repository"] != REPOSITORY
                or release["commit"] != commit or release["tag"] != f"lean-v{version}"
                or release["voiceBundled"] is not True or release["signed"] is not False
                or release["notarized"] is not False
                or release["zshBundled"] is not (not TARGET_SPECS[target].is_windows)):
            raise ValueError(f"Release provenance mismatch: {name}")
        with tempfile.TemporaryDirectory(prefix="lean-release-verify-") as temporary:
            package = Path(temporary)
            extract_archive(archive, package)
            validate_package_dir(package, PACKAGE_VARIANTS["codex"], TARGET_SPECS[target],
                                 include_zsh=not TARGET_SPECS[target].is_windows)
            validate_voice(package, target, version, commit)
            voice_manifest = json.loads((package / "codex-resources/voice/manifest.json").read_text(encoding="utf-8"))
            if (release.get("voiceBuildCommit") != voice_manifest["voiceBuildCommit"]
                    or release.get("voiceInputFingerprint") != voice_manifest["voiceInputFingerprint"]):
                raise ValueError(f"Release voice provenance mismatch: {name}")
            if TARGET_SPECS[target].is_linux and sha256(package / "codex-resources/bwrap") != release["bwrapSha256"]:
                raise ValueError(f"Release bwrap digest mismatch: {name}")
        checksums.append(checksum)
    return "".join(checksums)


def render_release_notes(version: str, commit: str) -> str:
    """Require this version's curated changes, then append download boundaries."""
    validate_identity(version, commit)
    source = REPO_ROOT / "docs/release-notes" / f"{version}.md"
    if not source.is_file():
        raise ValueError(f"Missing curated release notes: {source}")
    curated = source.read_text(encoding="utf-8").strip()
    if curated.splitlines()[:1] != [f"# Codex Lean {version}"]:
        raise ValueError(f"Release notes title must match version {version}: {source}")
    changes = re.search(r"^## Changes\n(.*?)(?=^## |\Z)", curated, re.MULTILINE | re.DOTALL)
    if not changes or not re.search(r"^- \S", changes.group(1), re.MULTILINE):
        raise ValueError(f"Release notes require user-facing bullets under '## Changes': {source}")
    if re.search(r"\b(TODO|TBD|PLACEHOLDER)\b", curated, re.IGNORECASE):
        raise ValueError(f"Replace release notes placeholders: {source}")
    docs = f"https://github.com/{REPOSITORY}/blob/{commit}/docs"
    return (
        f"{curated}\n\n"
        "## Downloads\n\n"
        "Complete packages are available for Linux x64 and ARM64, Apple Silicon macOS, "
        "and Windows x64. Keep the extracted package together. "
        "Verify downloads with `SHA256SUMS` or the per-archive `.sha256` files.\n\n"
        "Linux voice requires glibc 2.28 or newer. macOS and Windows packages are not "
        "developer-signed. macOS is not notarized. Native voice passes CI runtime checks, "
        "but CI does not test live microphone or speaker use. "
        f"See [installation and platform requirements]({docs}/install.md) "
        f"and [release validation]({docs}/releases.md).\n\n"
        f"Built from [`{commit}`](https://github.com/{REPOSITORY}/commit/{commit}).\n"
    )


def notes() -> None:
    """Render a body for review without building, tagging or publishing."""
    body = render_release_notes(os.environ["RELEASE_VERSION"], os.environ["RELEASE_COMMIT"])
    (REPO_ROOT / "lean-release-notes.md").write_text(body, encoding="utf-8")


def verify() -> None:
    version, commit = os.environ["RELEASE_VERSION"], os.environ["RELEASE_COMMIT"]
    body = render_release_notes(version, commit)
    directory = REPO_ROOT / "lean-dist"
    checksums = verify_assets(directory, version, commit)
    (directory / "SHA256SUMS").write_text(checksums, encoding="utf-8")
    (REPO_ROOT / "lean-release-notes.md").write_text(body, encoding="utf-8")


def tag() -> None:
    """Create a lightweight tag at the tested SHA, or reject a conflicting tag."""
    version, commit = os.environ["RELEASE_VERSION"], os.environ["RELEASE_COMMIT"]
    validate_identity(version, commit)
    if os.environ["GH_REPO"] != REPOSITORY or os.environ["RELEASE_TAG"] != f"lean-v{version}":
        raise ValueError("Unexpected release repository or tag")
    release_tag = f"lean-v{version}"
    prefix = f"repos/{REPOSITORY}"
    result = subprocess.run(["gh", "api", f"{prefix}/git/matching-refs/tags/{release_tag}"],
                            check=True, capture_output=True, text=True)
    refs = json.loads(result.stdout)
    if any(ref["ref"] == f"refs/tags/{release_tag}" for ref in refs):
        result = subprocess.run(["gh", "api", f"{prefix}/commits/{release_tag}", "--jq", ".sha"],
                                check=True, capture_output=True, text=True)
        if result.stdout.strip() != commit:
            raise ValueError("Existing release tag points to a different commit")
    else:
        subprocess.run(["gh", "api", "--method", "POST", f"{prefix}/git/refs",
                        "-f", f"ref=refs/tags/{release_tag}", "-f", f"sha={commit}"], check=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("check", "runner", "package", "verify", "notes", "tag"))
    args = parser.parse_args()
    {"check": check, "runner": runner, "package": package, "verify": verify, "notes": notes, "tag": tag}[args.command]()
