"""Locate instruction edit boundaries without approving particular app builds."""

import re
from dataclasses import dataclass

from asar import Asar, UnsupportedBundle

MARKERS = ("codex-user-personality-v2", "codex-user-voice-prompt-v1")
IDENTIFIER = r"[A-Za-z_$][\w$]*"


@dataclass(frozen=True)
class CodeRegion:
    kind: str
    source: str


@dataclass(frozen=True)
class BundleLayout:
    main: str
    preload: str
    initial: str
    voice: str
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


def quoted_end(source: str, start: int) -> int:
    quote = source[start]
    index = start + 1
    while index < len(source):
        if source[index] == "\\":
            index += 2
        elif source[index] == quote:
            return index + 1
        elif quote == "`" and source.startswith("${", index):
            index = group_end(source, index + 1)
        else:
            index += 1
    raise UnsupportedBundle("unterminated JavaScript literal at instruction boundary")


def group_end(source: str, start: int) -> int:
    """Skip a balanced minified JS group, including templates and regex literals."""
    closing = {"(": ")", "[": "]", "{": "}"}[source[start]]
    index = start + 1
    expression = True
    previous_word = None
    while index < len(source):
        char = source[index]
        if char.isspace():
            index += 1
            continue
        if char == closing:
            return index + 1
        if char in ")]}":
            raise UnsupportedBundle("unbalanced JavaScript instruction boundary")
        if source.startswith("//", index):
            end = source.find("\n", index + 2)
            index = len(source) if end == -1 else end + 1
            continue
        if source.startswith("/*", index):
            end = source.find("*/", index + 2)
            if end == -1:
                break
            index = end + 2
            continue
        if char in "\"'`":
            index = quoted_end(source, index)
            expression = False
            previous_word = None
            continue
        if char == "/" and expression:
            # Regex character classes may contain braces and slashes.
            index += 1
            in_class = False
            while index < len(source):
                token = source[index]
                index += 1
                if token == "\\":
                    index += 1
                elif token == "[":
                    in_class = True
                elif token == "]":
                    in_class = False
                elif token == "/" and not in_class:
                    break
            else:
                break
            expression = False
            previous_word = None
            continue
        if char in "([{":
            index = group_end(source, index)
            expression = char == "(" and previous_word in {
                "if",
                "while",
                "for",
                "with",
                "switch",
                "catch",
            }
            previous_word = None
            continue
        word = re.match(IDENTIFIER + r"|\d+(?:\.\d+)?", source[index:])
        if word:
            expression = word[0] in {
                "return",
                "throw",
                "case",
                "void",
                "typeof",
                "delete",
                "yield",
                "await",
                "in",
                "of",
                "instanceof",
                "new",
            }
            previous_word = word[0]
            index += len(word[0])
        else:
            expression = char not in "."
            previous_word = None
            index += 1
    raise UnsupportedBundle("unterminated JavaScript instruction boundary")


def body_start(source: str, start: int = 0) -> int:
    opening = source.find("(", start)
    if opening == -1:
        raise UnsupportedBundle("native instruction parameters missing")
    boundary = group_end(source, opening)
    while boundary < len(source) and source[boundary].isspace():
        boundary += 1
    if boundary >= len(source) or source[boundary] != "{":
        raise UnsupportedBundle("native instruction function body missing")
    return boundary


def extract(source: str, kind: str, start_pattern: str) -> CodeRegion:
    starts = list(re.finditer(start_pattern, source))
    if len(starts) != 1:
        raise UnsupportedBundle(f"native {kind} instruction owner missing or ambiguous")
    start = starts[0].start()
    end = group_end(source, body_start(source, start))
    return CodeRegion(kind, source[start:end])


def parameter(source: str, name: str) -> str | None:
    header = source[: body_start(source)]
    match = re.search(rf"(?:\{{|,)\s*{name}:({IDENTIFIER})(?=[,=}}])", header)
    return match[1] if match else None


def inspect_regions(bundle: Asar) -> BundleLayout:
    main_path = packed_module(bundle, r"\.vite/build/main-[^/]+\.js")
    initial_path = packed_module(bundle, r"webview/assets/app-initial-[^/]+\.js")
    preload_path = ".vite/build/preload.js"
    main = bundle.read(main_path).decode("utf-8")
    initial = bundle.read(initial_path).decode("utf-8")
    preload = bundle.read(preload_path).decode("utf-8")
    # Voice may be split out of app-initial. Locate its endpoint owner, not a
    # chunk basename; reject competing owners instead of choosing one.
    voice_modules = [
        name
        for name, entry in bundle.entries.items()
        if re.fullmatch(r"webview/assets/[^/]+\.js", name)
        and not entry.get("unpacked")
        and "/wham/realtime/calls" in bundle.read(name).decode("utf-8")
    ]
    if len(voice_modules) != 1:
        raise UnsupportedBundle("native voice module missing or ambiguous")
    voice_path = voice_modules[0]
    voice = bundle.read(voice_path).decode("utf-8")
    if any(
        marker in source
        for source in (main, initial, preload, voice)
        for marker in MARKERS
    ):
        raise UnsupportedBundle("bundle is already patched; use an unpatched archive")
    if not re.match(rf"const {IDENTIFIER}=require\(", main):
        raise UnsupportedBundle("unsupported main-process module")
    if (
        not re.match(rf'let {IDENTIFIER}=require\("electron"\);', preload)
        or len(
            re.findall(
                rf"contextBridge\.exposeInMainWorld\(`electronBridge`,{IDENTIFIER}\)",
                preload,
            )
        )
        != 1
    ):
        raise UnsupportedBundle("unsupported sandboxed preload bridge")
    text = extract(main, "text", r"async getProjectAwareDeveloperInstructions\(")
    # Parameter order, local variable names, models and metadata are not gates.
    header = rf"async function {IDENTIFIER}\(\{{"
    rpc = extract(
        voice,
        "rpc",
        header
        + r"(?=[^{}]*\bmanager:)(?=[^{}]*\bcodexResponsesAsItems:|[^{}]*\bcodexResponseHandoffPrefix:)",
    )
    call = extract(voice, "call", header + r"(?=[^{}]*\bofferSdp:)(?=[^{}]*\bprompt:)")
    if "`thread/realtime/start`" not in rpc.source:
        raise UnsupportedBundle("native realtime start request missing")
    if "/wham/realtime/calls" not in call.source:
        raise UnsupportedBundle("native realtime call endpoint missing")
    # --check exercises exactly the same boundary validation as writing.
    append_text(text)
    append_voice(rpc)
    append_voice(call)
    skip_cloud_attachment_cleanup(initial, text)
    return BundleLayout(
        main_path, preload_path, initial_path, voice_path, text, rpc, call
    )


def skip_cloud_attachment_cleanup(source: str, text: CodeRegion) -> str:
    # This fix owns only the registry startup loop, not text instructions.
    pattern = (
        rf"let (?P<callback>{IDENTIFIER})=\(\)=>\{{for\(let (?P<host>{IDENTIFIER}) of "
        rf"(?P<registry>{IDENTIFIER})\.getAll\(\)\)\{{let (?P<impl>{IDENTIFIER})="
        r"(?P=registry)\.getImplForHostId\((?P=host)\.getHostId\(\)\);"
        rf"(?P=impl)==null\|\|(?P<seen>{IDENTIFIER})\.current\.has\((?P=impl)\)\|\|"
        r"\((?P=seen)\.current\.add\((?P=impl)\),"
        rf"(?P=impl)\.cleanupPendingPastedTextAttachments\(\)\.catch\({IDENTIFIER}\)\)\}}\}},"
        rf"{IDENTIFIER}=(?P=registry)\.addRegistryCallback\((?P=callback)\);return (?P=callback)\(\),"
    )
    matches = list(re.finditer(pattern, source))
    if not matches and "cleanupPendingPastedTextAttachments().catch(" not in source:
        return source
    if len(matches) != 1:
        raise UnsupportedBundle("attachment cleanup owner missing or ambiguous")
    match = matches[0]
    loop = match[0]
    boundary = loop.index("{let ") + 1
    guarded = (
        loop[:boundary]
        + f"if({match['host']}.getHostId()===`durable`)continue;"
        + loop[boundary:]
    )
    return source[: match.start()] + guarded + source[match.end() :]


def append_text(region: CodeRegion) -> str:
    boundary = body_start(region.source)
    if group_end(region.source, boundary) != len(region.source):
        raise UnsupportedBundle("native text method body missing")
    # Preserve every native return and lexical this/arguments, including callbacks.
    return (
        region.source[: boundary + 1]
        + "return globalThis.__codexUserPersonality.append(await (async()=>{"
        + region.source[boundary + 1 : -1]
        + "})())}"
    )


def append_voice(region: CodeRegion) -> str:
    prompt = parameter(region.source, "prompt")
    if region.kind == "rpc":
        if prompt is None:
            # New clients create the call themselves; this RPC only attaches it.
            if re.search(
                r"transport:\{type:`existingCall`,callId:" + IDENTIFIER + r"\}",
                region.source,
            ) and not re.search(r"\bprompt\s*:", region.source):
                return region.source
            raise UnsupportedBundle(
                "native realtime prompt or existing-call attachment missing"
            )
        target = (
            rf"\.\.\.{re.escape(prompt)}==null\?\{{\}}:\{{prompt:{re.escape(prompt)}\}}"
        )
        if len(re.findall(target, region.source)) != 1:
            raise UnsupportedBundle(
                "native realtime prompt boundary missing or ambiguous"
            )
        return re.sub(
            target,
            f"...{prompt}==null?{{}}:{{prompt:globalThis.codexUserPersonality.append({prompt})}}",
            region.source,
        )
    if prompt is None:
        raise UnsupportedBundle("native call prompt parameter missing")
    target = rf"\binstructions:{re.escape(prompt)}(?=[,}}])"
    if not re.search(target, region.source) or len(
        re.findall(target, region.source)
    ) != len(re.findall(r"\binstructions:", region.source)):
        raise UnsupportedBundle(
            "native call instruction boundary missing or inconsistent"
        )
    return re.sub(
        target,
        f"instructions:globalThis.codexUserPersonality.append({prompt})",
        region.source,
    )
