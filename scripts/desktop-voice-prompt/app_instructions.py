"""Opt-in replacement of verified default desktop prose, never composed instructions."""

import hashlib
import json
import re

from asar import Asar, UnsupportedBundle
from regions import packed_module


# Exact defaults and complete instruction composition from Linux 26.930.31730.
# No other release's app prose has been approved for this optional mode.
OWNERS = {
    "bootstrap": (
        r"\.vite/build/bootstrap-[^/]+\.js",
        r"var [\w$]+=5e3,[\w$]+=16384,[\w$]+=",
        "12773414b44fbf1e5562dc92b7a67098966d917ae855abaed33c09c4c45f3dac",
    ),
    "worker": (
        r"\.vite/build/worker\.js",
        r"var [\w$]+=\[\{id:`hotkeyWindow`",
        "0f645105271f3aac2edbec125cd833a9a09cef7ea32e0f260f108f9058126bd3",
    ),
}
START = (
    r"function [\w$]+\(e\)\{return"
    + re.escape("`<app-context>\\n${e.trim()}\\n</app-context>`}var ")
    + r"[\w$]+=`# Codex desktop context"
)
SECTIONS = (
    (
        r"`# Codex desktop context(?:\\.|[^`\\])*`",
        "# Codex desktop\n"
        "- Display local images, video and audio with ![alt](/absolute/path), including audio playback. Use absolute paths for media and workspace file references.\n"
        "- Prefer native inline media or local outputs returned by tools. For remote images, prefer Markdown embeds where the app's URL-safety policy permits. For media that cannot display directly, including remote video and audio, use an available preview or display tool. Link a usable result URL only as a last resort. Never download remote media to bypass display restrictions.\n"
        "- Format web URLs as Markdown links.",
    ),
    (
        r"'### Thread Coordination(?:\\.|[^'\\])*'",
        "### Thread Coordination\n"
        "- Discover relevant thread tools for app chat management. Use create_thread only when the user explicitly asks for a new sidebar chat. These chats are user-owned. For subtasks, use multi-agent tools, including explicit subagent requests.\n"
        "- Prefer compact wait_threads snapshots over repeated read_thread calls. Creation is asynchronous, so explicitly check progress. Do not narrate unchanged snapshots. Leave approval and user-input requests to the user.\n"
        '- After successful create_thread, emit ::created-thread{threadId="..."} or, for queued worktree setup, ::created-thread{clientThreadId="..."} on its own line in the final response.',
    ),
    (
        r'"### Sidebar Organization(?:\\.|[^"\\])*"',
        "### Sidebar Organization\n"
        "- Discover sidebar and project tools when asked to organise app chats or projects. Moving an item into the pinned section pins it.",
    ),
)


def slim_defaults(bundle: Asar) -> dict[str, bytes]:
    replacements = {}
    for owner, (path_pattern, end_pattern, fingerprint) in OWNERS.items():
        path = packed_module(bundle, path_pattern)
        source = bundle.read(path).decode("utf-8")
        starts = list(re.finditer(START, source))
        ends = list(re.finditer(end_pattern, source))
        if len(starts) != 1 or len(ends) != 1 or ends[0].start() <= starts[0].end():
            raise UnsupportedBundle(
                f"desktop {owner} default instruction owner missing or ambiguous"
            )
        start, end = starts[0].start(), ends[0].start()
        region = source[start:end]
        if hashlib.sha256(region.encode()).hexdigest() != fingerprint:
            raise UnsupportedBundle(
                f"unfamiliar desktop {owner} default instructions or composer"
            )
        for pattern, text in SECTIONS:
            matches = list(re.finditer(pattern, region))
            if len(matches) != 1:
                raise UnsupportedBundle(
                    f"desktop {owner} default instruction literal missing or ambiguous"
                )
            match = matches[0]
            region = region[: match.start()] + json.dumps(text) + region[match.end() :]
        replacements[path] = (source[:start] + region + source[end:]).encode()
    return replacements
