#!/usr/bin/env python3
"""Fetch the approved Git branch before running its desktop personality patcher."""

import argparse
import fcntl
import os
import re
import stat
import subprocess
import sys
import tempfile
from pathlib import Path

from pacman_hook import close_app, report, validate_personality


LIBRARY = Path("/opt/codex-desktop-personality")
REPOSITORY = "https://github.com/IgorWarzocha/codex-lean.git"
BRANCH = "lean"
SUBTREE = "scripts/desktop-voice-prompt"
PATCHER_SOURCES = (
    "asar.py",
    "regions.py",
    "patch.py",
    "runtime-main.js",
    "runtime-preload.js",
    "pacman_hook.py",
)


def trusted_directory(directory: Path) -> None:
    # Root must never import hook code through a user-writable or symlinked tree.
    for ancestor in reversed((directory, *directory.parents)):
        if not ancestor.exists():
            ancestor.mkdir(mode=0o755)
        metadata = ancestor.lstat()
        if (
            not stat.S_ISDIR(metadata.st_mode)
            or metadata.st_uid != 0
            or metadata.st_mode & 0o022
        ):
            raise ValueError(
                f"hook directory is not root-owned and protected: {ancestor}"
            )


def git(arguments: list[str], directory: Path, environment: dict[str, str]) -> bytes:
    result = subprocess.run(
        [
            "/usr/bin/timeout",
            "--signal=TERM",
            "--kill-after=5s",
            "60s",
            "/usr/bin/git",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "credential.helper=",
            "-c",
            "http.followRedirects=false",
            *arguments,
        ],
        cwd=directory,
        env=environment,
        capture_output=True,
        check=False,
    )
    if result.returncode:
        detail = result.stderr.decode("utf-8", errors="replace").strip()[-2000:]
        raise RuntimeError(
            f"Git fetch/read failed (status {result.returncode}): {detail}"
        )
    return result.stdout


def fetch_patcher(directory: Path) -> tuple[str, Path]:
    environment = {
        "PATH": "/usr/bin:/bin",
        "HOME": str(directory),
        "GIT_CONFIG_NOSYSTEM": "1",
        "GIT_CONFIG_GLOBAL": "/dev/null",
        "GIT_TERMINAL_PROMPT": "0",
        "GIT_ALLOW_PROTOCOL": "https",
    }
    # No checkout, submodules, hooks, credentials, or user Git configuration.
    repository = directory / "repository.git"
    git(
        [
            "clone",
            "--bare",
            "--depth=1",
            "--filter=blob:none",
            "--single-branch",
            "--branch",
            BRANCH,
            REPOSITORY,
            str(repository),
        ],
        directory,
        environment,
    )
    commit = git(["rev-parse", "HEAD"], repository, environment).decode().strip()
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ValueError("Git returned an invalid commit ID")
    output = directory / "patcher"
    output.mkdir(mode=0o700)
    for name in PATCHER_SOURCES:
        path = f"{SUBTREE}/{name}"
        entry = git(["ls-tree", commit, "--", path], repository, environment)
        if not re.fullmatch(
            rb"100644 blob [0-9a-f]{40}\t" + path.encode() + rb"\n", entry
        ):
            raise ValueError(
                f"Git patcher source is missing or not a regular file: {path}"
            )
        data = git(["show", f"{commit}:{path}"], repository, environment)
        if not data or len(data) > 256 * 1024:
            raise ValueError(f"Git patcher source is empty or too large: {path}")
        (output / name).write_bytes(data)
    return commit, output


def update(user: str, personality: Path, archive: Path) -> None:
    if os.geteuid() != 0:
        raise PermissionError("run the Git update hook with sudo")
    personality = validate_personality(user, personality)
    trusted_directory(LIBRARY)
    descriptor = os.open(
        LIBRARY / ".update.lock", os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600
    )
    with os.fdopen(descriptor, "w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        close_app(user)
        with tempfile.TemporaryDirectory(
            prefix=".git-update-", dir=LIBRARY
        ) as temporary:
            commit, patcher = fetch_patcher(Path(temporary))
            print(
                f"Codex personality: fetched {REPOSITORY} ({BRANCH}) at {commit}",
                flush=True,
            )
            # A fresh process avoids retaining imports from the installed snapshot.
            subprocess.run(
                [
                    "/usr/bin/timeout",
                    "--signal=INT",
                    "--kill-after=5s",
                    "45s",
                    "/usr/bin/python3",
                    "-E",
                    "-s",
                    "-B",
                    str(patcher / "pacman_hook.py"),
                    "--user",
                    user,
                    "--personality-file",
                    str(personality),
                    "--asar",
                    str(archive),
                ],
                cwd=patcher,
                env={"PATH": "/usr/bin:/bin"},
                check=True,
            )


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--user", required=True)
    parser.add_argument("--personality-file", type=Path, required=True)
    parser.add_argument(
        "--asar", type=Path, default=Path("/usr/lib/chatgpt/resources/app.asar")
    )
    args = parser.parse_args(argv)
    try:
        update(args.user, args.personality_file, args.asar)
    except Exception as error:
        # Do not silently use an old snapshot when the requested Git refresh fails.
        report(
            args.user,
            f"Git personality update failed: {error}. The app update completed. "
            "Personality may not have been reapplied. Check journalctl -t codex-desktop-personality.",
            warning=True,
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
