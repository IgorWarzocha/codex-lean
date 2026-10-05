#!/usr/bin/env python3
"""Append user communication preferences to Codex desktop text and voice instructions."""

import argparse
import json
import os
import plistlib
import sys
from pathlib import Path

from asar import Asar, UnsupportedBundle
from regions import BundleLayout, append_text, append_voice, inspect_regions


HERE = Path(__file__).resolve().parent


def verify_bundle(bundle: Asar) -> tuple[str, BundleLayout]:
    try:
        package = json.loads(bundle.read("package.json"))
    except (ValueError, UnicodeError) as error:
        raise UnsupportedBundle("invalid package.json") from error
    if not isinstance(package, dict) or (
        package.get("name"),
        package.get("productName"),
    ) != ("openai-codex-electron", "Codex"):
        raise UnsupportedBundle("archive does not identify itself as Codex desktop")
    version = package.get("version")
    if not isinstance(version, str) or not version:
        raise UnsupportedBundle("package.json has no app version")
    return version, inspect_regions(bundle)


def validate_prompt_path(path: Path) -> Path:
    if not path.is_absolute():
        raise ValueError(
            "--personality-file must be an absolute path on the app's machine"
        )
    # This task explicitly excludes Igor's Pi-owned prompt, including symlink aliases.
    protected = Path.home() / ".pi/agent/REALTIME-SYSTEM-PROMPT.md"
    if path.resolve() == protected.resolve():
        raise ValueError(
            "the Pi-owned REALTIME-SYSTEM-PROMPT.md is outside this script's scope"
        )
    return path


def mac_info(path: Path, bundle: Asar):
    raw = path.read_bytes()
    try:
        document = plistlib.loads(raw)
    except plistlib.InvalidFileException as error:
        raise UnsupportedBundle("invalid macOS Info.plist") from error
    integrity = (
        document.get("ElectronAsarIntegrity") if isinstance(document, dict) else None
    )
    entry = integrity.get("Resources/app.asar") if isinstance(integrity, dict) else None
    if not isinstance(entry, dict) or entry.get("algorithm") != "SHA256":
        raise UnsupportedBundle("unfamiliar macOS ASAR integrity metadata")
    if entry.get("hash") != bundle.header_sha256:
        raise UnsupportedBundle(
            "Info.plist integrity hash does not match the input ASAR"
        )
    return document, plistlib.FMT_BINARY if raw.startswith(
        b"bplist00"
    ) else plistlib.FMT_XML


def write_outputs(
    bundle, layout, prompt, output, info, output_info, *, slim_app_instructions=False
):
    created = []
    try:
        bundle.write(
            output,
            replacements(
                bundle, layout, prompt, slim_app_instructions=slim_app_instructions
            ),
        )
        created.append(output)
        if info is not None:
            document, format = info
            document["ElectronAsarIntegrity"]["Resources/app.asar"]["hash"] = Asar(
                output
            ).header_sha256
            data = plistlib.dumps(document, fmt=format, sort_keys=False)
            with output_info.open("xb") as target:
                created.append(output_info)
                target.write(data)
                target.flush()
                os.fsync(target.fileno())
    except (OSError, ValueError):
        for path in reversed(created):
            path.unlink(missing_ok=True)
        raise


def optional_app_instructions(bundle: Asar, enabled: bool) -> dict[str, bytes]:
    if not enabled:
        return {}
    # Installed Git bootstraps fetch the original six-file manifest. Keep their
    # append-only path independent of this opt-in repository module.
    from app_instructions import slim_defaults

    return slim_defaults(bundle)


def replacements(
    bundle: Asar,
    layout: BundleLayout,
    prompt: Path | None,
    *,
    slim_app_instructions: bool = False,
) -> dict[str, bytes]:
    app_instructions = optional_app_instructions(bundle, slim_app_instructions)
    config = {"path": str(prompt) if prompt else None, "required": prompt is not None}
    main = (
        (HERE / "runtime-main.js")
        .read_text()
        .replace("__CODEX_PERSONALITY_CONFIG__", json.dumps(config, ensure_ascii=True))
    )
    preload = (HERE / "runtime-preload.js").read_bytes()
    native_main = bundle.read(layout.main).decode("utf-8")
    initial = bundle.read(layout.initial).decode("utf-8")
    native_main = native_main.replace(layout.text.source, append_text(layout.text))
    for region in (layout.rpc, layout.call):
        initial = initial.replace(region.source, append_voice(region))
    return {
        **app_instructions,
        layout.main: main.encode() + native_main.encode(),
        layout.preload: preload + bundle.read(layout.preload),
        layout.initial: initial.encode(),
    }


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--asar", required=True, type=Path, help="pristine app.asar to read"
    )
    parser.add_argument(
        "--check", action="store_true", help="verify support without writing"
    )
    parser.add_argument(
        "--slim-app-instructions",
        action="store_true",
        help="also slim verified default desktop text guidance; off by default, without changing realtime instructions",
    )
    parser.add_argument(
        "--personality-file",
        type=Path,
        help="absolute user-owned personality path; otherwise use the app's CODEX_HOME/codex_personality.md",
    )
    parser.add_argument("--output", type=Path, help="new ASAR path, never overwritten")
    parser.add_argument(
        "--info-plist",
        type=Path,
        help="original macOS app Info.plist with ASAR integrity metadata",
    )
    parser.add_argument(
        "--output-info-plist", type=Path, help="new Info.plist path, never overwritten"
    )
    args = parser.parse_args(argv)
    if args.check and (args.personality_file or args.output or args.output_info_plist):
        parser.error("--check cannot be combined with personality or output arguments")
    if not args.check and args.output is None:
        parser.error("patching requires --output")
    if not args.check and bool(args.info_plist) != bool(args.output_info_plist):
        parser.error(
            "macOS patching requires both --info-plist and --output-info-plist"
        )
    try:
        bundle = Asar(args.asar)
        version, layout = verify_bundle(bundle)
        info = mac_info(args.info_plist, bundle) if args.info_plist else None
        if args.check:
            optional_app_instructions(bundle, args.slim_app_instructions)
            print(
                f"Compatible: Codex {version}, verified native text and voice instruction code"
            )
            return 0
        prompt = (
            validate_prompt_path(args.personality_file)
            if args.personality_file
            else None
        )
        if args.output.resolve() == args.asar.resolve():
            raise ValueError(
                "output must differ from input; in-place patching is not supported"
            )
        inputs = {args.asar.resolve()}
        if args.info_plist:
            inputs.add(args.info_plist.resolve())
        outputs = [args.output] + (
            [args.output_info_plist] if args.output_info_plist else []
        )
        if len({path.resolve() for path in outputs}) != len(outputs) or any(
            path.resolve() in inputs for path in outputs
        ):
            raise ValueError("outputs must be distinct from each other and all inputs")
        if any(path.exists() or path.is_symlink() for path in outputs):
            raise FileExistsError("an output path already exists")
        write_outputs(
            bundle,
            layout,
            prompt,
            args.output,
            info,
            args.output_info_plist,
            slim_app_instructions=args.slim_app_instructions,
        )
        print(f"Created {args.output}. Input unchanged. Not installed.")
        print(
            f"Text and voice append preferences from {prompt or '$CODEX_HOME/codex_personality.md'} at runtime."
        )
        print("Keep the matching app.asar.unpacked directory when installing manually.")
        if args.slim_app_instructions:
            print(
                "Slimmed verified default desktop text guidance. Native voice unchanged."
            )
        if info is not None:
            print(
                f"Created {args.output_info_plist} with the new ASAR integrity hash. macOS code signing still requires a separate step."
            )
        return 0
    except (OSError, ValueError, RecursionError, ImportError) as error:
        print(f"Cannot patch: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
