#!/usr/bin/env python3
"""Install a root-owned pacman post-update personality hook."""

import argparse
import os
import shlex
import sys
import tempfile
from pathlib import Path

from pacman_hook import validate_personality
from git_update import LIBRARY, PATCHER_SOURCES, trusted_directory


HOOK = Path("/etc/pacman.d/hooks/95-codex-desktop-personality.hook")
SOURCES = (
    *PATCHER_SOURCES,
    "git_update.py",
    "pacman-hook.sh",
)


def hook_text(user: str, personality: Path, library: Path = LIBRARY) -> str:
    command = shlex.join([str(library / "pacman-hook.sh"), user, str(personality)])
    return (
        "[Trigger]\n"
        "Operation = Install\n"
        "Operation = Upgrade\n"
        "Type = Path\n"
        "Target = usr/lib/chatgpt/resources/app.asar\n\n"
        "[Action]\n"
        "Description = Reapply Codex desktop personality if compatible\n"
        "When = PostTransaction\n"
        f"Exec = {command}\n"
    )


def publish(path: Path, data: bytes, mode: int) -> None:
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(dir=path.parent, delete=False) as target:
            temporary = Path(target.name)
            target.write(data)
            target.flush()
            os.fchmod(target.fileno(), mode)
            os.fsync(target.fileno())
        os.replace(temporary, path)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def install(user: str, personality: Path) -> None:
    if os.geteuid() != 0:
        raise PermissionError("run the hook installer with sudo")
    personality = validate_personality(user, personality)
    if any(character in user + str(personality) for character in "\r\n\0"):
        raise ValueError("hook arguments must not contain control characters")
    source = Path(__file__).resolve().parent
    files = {name: (source / name).read_bytes() for name in SOURCES}
    trusted_directory(LIBRARY)
    trusted_directory(HOOK.parent)
    for name, data in files.items():
        publish(LIBRARY / name, data, 0o755 if name.endswith(".sh") else 0o644)
    # Register last so a new hook never points at a partially installed library.
    publish(HOOK, hook_text(user, personality).encode(), 0o644)


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--user", required=True)
    parser.add_argument("--personality-file", required=True, type=Path)
    args = parser.parse_args(argv)
    try:
        install(args.user, args.personality_file)
    except (OSError, ValueError, KeyError) as error:
        print(f"Cannot install hook: {error}", file=sys.stderr)
        return 1
    print(
        f"Installed {HOOK}. Future app updates fetch the Git patcher and reapply preferences."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
