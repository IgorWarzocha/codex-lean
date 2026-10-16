"""Replace isolated default desktop literals, never their composers."""

import json
import re

from asar import Asar, UnsupportedBundle
from regions import packed_module

# Owners may move between process chunks and a shared module. Composer code and
# default prose may evolve; only unambiguous standalone literals are editable.
OWNERS = {
    "bootstrap": r"\.vite/build/bootstrap-[^/]+\.js",
    "worker": r"\.vite/build/worker\.js",
}
SHARED = r"\.vite/build/src-[^/]+\.js"
START = r"`# Codex desktop context"
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
    shared = [
        path
        for path in bundle.entries
        if re.fullmatch(SHARED, path)
        and re.search(START, bundle.read(path).decode("utf-8"))
    ]
    if len(shared) > 1:
        raise UnsupportedBundle("desktop shared instruction owner is ambiguous")
    if shared and any(
        re.search(START, bundle.read(path).decode("utf-8"))
        for path in bundle.entries
        if any(re.fullmatch(pattern, path) for pattern in OWNERS.values())
    ):
        raise UnsupportedBundle("desktop shared and legacy instruction owners coexist")
    owners = {"shared": SHARED} if shared else OWNERS
    for owner, path_pattern in owners.items():
        path = shared[0] if owner == "shared" else packed_module(bundle, path_pattern)
        source = bundle.read(path).decode("utf-8")
        edits = []
        for pattern, text in SECTIONS:
            matches = list(re.finditer(pattern, source))
            if len(matches) != 1:
                raise UnsupportedBundle(
                    f"desktop {owner} default instruction literal missing or ambiguous"
                )
            match = matches[0]
            literal = match.group()
            # A standalone binding, not a concatenation, template expression or
            # prematurely closed string. Leave all bytes outside it untouched.
            if (
                not re.search(r"[\w$]+\s*=\s*$", source[: match.start()])
                or not re.match(r"\s*[,;]", source[match.end() :])
                or (literal[0] != "`" and re.search(r"[\r\n]", literal))
                or (literal[0] == "`" and re.search(r"(?<!\\)(?:\\\\)*\$\{", literal))
            ):
                raise UnsupportedBundle(
                    f"desktop {owner} default instruction literal boundary is unfamiliar"
                )
            edits.append((match.start(), match.end(), json.dumps(text)))
        for start, end, replacement in sorted(edits, reverse=True):
            source = source[:start] + replacement + source[end:]
        replacements[path] = source.encode()
    return replacements
