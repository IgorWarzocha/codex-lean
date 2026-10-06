"""Compatibility belongs to the instruction code we edit, not an app release."""

import hashlib
import re
from dataclasses import dataclass

from asar import Asar, UnsupportedBundle


# Captured from Linux 26.930.21537, 26.930.31730, 26.930.41038, 26.930.61225,
# and macOS 26.930.31730. The later Linux calls only rename minifier identifiers.
# These are code-region fingerprints, not copied prompts or protocol schemas.
FINGERPRINTS = {
    "text": {
        "4d1009145aafadfc653b2ba11a3f9be18d62e82f900d02a33aa05744abeffaf5",
        "f4d4bbe4aacdb97581292a06c3228ae92dcb8a3c3fd7ff89d29aeb59c555bbb8",
    },
    "rpc": {"abdb4bc77855a4566b117625888c1dbcd40a33702953b205d2f5d1ab0a0e5269"},
    "call": {
        "12f0d6dcb290fbae55dbbc7318aa929543c959e51976019d3ac46eae26ff0b75",
        "c16e4cecfbf7e2fda56de3fea0bf6d3479c3d50568966e1ba8f6c0590a9a75b4",
        "efb9d9140960a8d048c48502c3d30060e797e116a9056314a7a29e1c74acf3bf",
        "2510228c0a4280c870f03ff92f218cb8cfc8fab254cf0e0c1d56315afcb893e9",
        "7ea4cdce4ae39c1d84fb86d95526ae9d2b33a385788ac7c4d48417df0a505fba",
    },
}
MARKERS = ("codex-user-personality-v2", "codex-user-voice-prompt-v1")


@dataclass(frozen=True)
class CodeRegion:
    kind: str
    source: str

    @property
    def fingerprint(self) -> str:
        canonical = re.sub(
            r"^async function [\w$]+", "async function FUNCTION", self.source
        )
        return hashlib.sha256(canonical.encode()).hexdigest()


@dataclass(frozen=True)
class BundleLayout:
    main: str
    preload: str
    initial: str
    text: CodeRegion
    rpc: CodeRegion
    call: CodeRegion


def packed_module(bundle: Asar, pattern: str) -> str:
    paths = [name for name in bundle.entries if re.fullmatch(pattern, name)]
    if len(paths) != 1:
        raise UnsupportedBundle(
            f"expected one packed module matching {pattern}, found {len(paths)}"
        )
    bundle.read(paths[0])
    return paths[0]


def extract(source: str, kind: str, start_pattern: str, end_marker: str) -> CodeRegion:
    starts = list(re.finditer(start_pattern, source))
    if len(starts) != 1:
        raise UnsupportedBundle(f"native {kind} instruction owner missing or ambiguous")
    start = starts[0].start()
    end = source.find(end_marker, starts[0].end())
    if end == -1:
        raise UnsupportedBundle(f"native {kind} instruction boundary is unfamiliar")
    region = CodeRegion(kind, source[start:end])
    if region.fingerprint not in FINGERPRINTS[kind]:
        raise UnsupportedBundle(
            f"unfamiliar native {kind} instruction code ({region.fingerprint})"
        )
    return region


def inspect_regions(bundle: Asar) -> BundleLayout:
    main_path = packed_module(bundle, r"\.vite/build/main-[^/]+\.js")
    initial_path = packed_module(bundle, r"webview/assets/app-initial-[^/]+\.js")
    preload_path = ".vite/build/preload.js"
    main = bundle.read(main_path).decode("utf-8")
    initial = bundle.read(initial_path).decode("utf-8")
    preload = bundle.read(preload_path).decode("utf-8")
    if any(
        marker in source for source in (main, initial, preload) for marker in MARKERS
    ):
        raise UnsupportedBundle("bundle is already patched; use an unpatched archive")
    if not main.startswith("const e=require("):
        raise UnsupportedBundle("unsupported main-process module")
    if (
        not preload.startswith('let e=require("electron");')
        or preload.count("contextBridge.exposeInMainWorld(`electronBridge`,z)") != 1
    ):
        raise UnsupportedBundle("unsupported sandboxed preload bridge")
    text = extract(
        main,
        "text",
        r"async getProjectAwareDeveloperInstructions\(",
        "async isNonGitWorkspace(",
    )
    rpc = extract(
        initial,
        "rpc",
        r"async function [\w$]+\(\{codexResponseHandoffPrefix:e=",
        "async function",
    )
    call = extract(
        initial,
        "call",
        r"async function [\w$]+\(\{codexSessionId:e,conversationId:t,initialItems:n,offerSdp:r,prompt:i,",
        "var ",
    )
    return BundleLayout(main_path, preload_path, initial_path, text, rpc, call)


def append_text(region: CodeRegion) -> str:
    prefix, expression = region.source.rsplit("return ", 1)
    return (
        prefix
        + "return globalThis.__codexUserPersonality.append(await "
        + expression[:-1]
        + ")}"
    )


def append_voice(region: CodeRegion) -> str:
    if region.kind == "rpc":
        return region.source.replace(
            "...s==null?{}:{prompt:s}",
            "...s==null?{}:{prompt:globalThis.codexUserPersonality.append(s)}",
        )
    return region.source.replace(
        "instructions:i", "instructions:globalThis.codexUserPersonality.append(i)"
    )
