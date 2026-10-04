# Configuration

For basic configuration instructions, see [this documentation](https://developers.openai.com/codex/config-basic).

For advanced configuration instructions, see [this documentation](https://developers.openai.com/codex/config-advanced).

For a full configuration reference, see [this documentation](https://developers.openai.com/codex/config-reference).

## CLI settings

Use `/settings` in the TUI for context management, Code Mode, subagent tools and
experimental features. `/voice settings` also includes voice continuity policies.
Pickers show configured defaults for the current project, not the running thread's
effective state. Saves report rejected writes, overrides and failed readback.
Context and runtime choices apply to new threads. They never migrate retained
history or checkpoints or change permissions. Voice client policies require a
new client launch and thread.

Settings are also available before a thread can start. Run `codex settings` to
list choices, then save a default with `codex settings set <setting> <choice>`.
For example, API-key sessions can select ordinary compaction and sandboxed Code
Mode without opening a thread or editing TOML:

```sh
codex settings set context compaction
codex settings set code-mode v8
```

`code-mode off` disables configured Code Mode without deleting Notebook profiles
or runtime options. A model that explicitly requires Code Mode can still select
it. Notebook requires a single local environment, full-access permissions and
Deno. Selecting it never grants access. `multi-agent off` disables both tool
generations. `subagent-wait` independently controls the V2 wait tool, which is off
by default. It is not required for automatic child-result delivery or resumption
of an idle parent. Stop and session shutdown prevent that automatic resumption.

`/experimental` includes Beta features and selected UnderDevelopment features
with public metadata from the feature registry. Internal, removed, deprecated
and unsupported controls remain hidden. Entries retain their lifecycle stage,
prerequisites and new-thread or restart notices. Enabling a flag does not guarantee
that the provider, client or current session supports its tools.

## Communication style

Create `codex_personality.md` in your Codex home (`~/.codex` by default) with your
preferred tone and communication style. Codex appends those preferences to both
normal text and realtime voice instructions, explicitly labelling them as the
user's preferred communication styles. Native model instructions, task-routing
rules, and permission controls remain in place. The file needs no version header
or maintained copy of Codex's system prompt.

To use a different user-owned UTF-8 Markdown file, set a top-level path in
`config.toml` on the app-server machine:

```toml
personality_file = "/absolute/path/to/codex_personality.md"
```

Normal text reads preferences when configuration loads; start a new session or
reload configuration to pick up edits. Voice rereads the file for each new call,
including replacement calls. No rebuild is needed. Codex never creates or
overwrites the file. An absent default file or an empty file adds nothing and
leaves native instructions unchanged. A missing explicitly configured file, an
unreadable existing file, invalid UTF-8, or a file larger than 64 KiB produces a
visible error rather than silently ignoring preferences. Relative paths follow
the usual configuration-layer path resolution.

Existing text and voice base-instruction overrides retain their precedence.
Preferences append after the selected base, including an explicitly empty base;
an empty preferences file never clears that base. Voice settings points to the
shared file location.

## Voice continuity

Brief spoken delegation acknowledgements are optional. Omitting the setting
keeps the server default; an explicit start request takes precedence:

```toml
[realtime]
delegation_ack_filler = true
```

Voice now carries bounded public context from the current host thread, not other
threads or workspace scans. Supplied app-server `initialItems` (including an
empty array) or `includeStartupContext = false` opt out of the inferred seed.
The existing startup-context override retains its precedence. Opaque native
checkpoints remain with the host agent rather than being decoded for voice.
The first successful activation greets briefly, with a continuation greeting
when the current thread already has a public user message. Replacement calls
and sideband reconnections do not greet again.

The TUI forwards public progress and final results for both typed and spoken
tasks. It never forwards raw reasoning; completed public reasoning summaries
require an explicit `model_reasoning_summary` setting and remain disabled by
`hide_agent_reasoning = true`. Permissions and structured questions keep their
normal visible controls. To keep the older spoken-delegation, final-only policy:

```toml
[realtime]
screenless = false
```

An established media call may be replaced once after transport loss. This is
separate from sideband reconnection and startup retry; device, permission, and
authentication errors do not trigger automatic replacement. Host compaction or
notes rollover refreshes voice serially after pending host work and speech settle.
Preparation keeps the old call active, and a preparation error keeps it usable.
Replacement preserves microphone mute and carries finalized conversational text,
without admitting an extra host turn. These TUI lifecycle policies can be disabled:

```toml
[realtime]
auto_resume = false
refresh_on_context_change = false
```

## Context continuity

`context_strategy` defaults to `"notes"`. The agent saves useful task state in
remote notes and reads those notes after a context reset. Earlier conversation
remains available through history, but is not automatically carried into the new
window.

Notes requires an OpenAI Codex backend provider and Codex backend authentication.
An unsupported session fails at startup with an actionable error. It does not
silently switch to summarization. API-key sessions and other providers should
explicitly select ordinary compaction with `codex settings set context compaction`
or the equivalent configuration:

```toml
context_strategy = "compaction"
```

The strategy selects continuity independently of the legacy context-management
and token-budget activation flags. Managed requirements can restrict the selected
strategy. Native token budgets and checkpoint reminders still apply in notes mode.

In Notes mode, `model_context_window` selects a working budget, not an execution
cutoff. Early reminders remain. At the selected budget, the agent receives an
urgent instruction to stop work, save a notes checkpoint, and call `new_context`
immediately. Crossing that budget does not itself force summarization or cancel
running tools.

Execution headroom is calculated against the model's advertised maximum instead.
For a 272,000-token selected window and an 872,000-token maximum, the urgent
reminder arrives at 272,000 tokens. With the usual 95% usable-window setting,
the execution guard remains at 828,400 tokens. When the selected budget approaches
the maximum, the urgent reminder arrives earlier to leave room for checkpointing.
These thresholds follow model metadata and the selected window, not fixed GPT
window sizes. Ordinary compaction keeps its existing thresholds.

The CLI and desktop context indicators show the raw selected working budget in
either strategy. A configured 272,000-token window displays 272,000, not the 258,400
usable tokens or the 828,400 Notes execution ceiling. In Notes mode, usage can
exceed the displayed budget while the agent saves notes and calls `new_context`.
The fresh context keeps the selected budget. Admission headroom and ordinary
compaction thresholds are unchanged.

Idle rollover is optional and disabled when the setting is absent. To reset a
notes window before the next user turn after 25 minutes idle:

```toml
context_strategy = "notes"
context_idle_rollover_minutes = 25
```

The idle interval must be a positive integer. Idle rollover uses saved settlement
and checkpoint state, not a background timer. A fresh notes checkpoint is required
for idle rollover.

For ordinary compaction, `compaction_retention_tokens` accepts `16000`, `32000`,
or `64000`, with `64000` as the default user-message retention budget:

```toml
context_strategy = "compaction"
compaction_retention_tokens = 32000
```

V2 compaction uses a separate 872,000-token input budget, independent of
`model_context_window`. Request trimming occurs only when the estimated compaction
input exceeds that budget.

## Lifecycle hooks

Admins can set top-level `allow_managed_hooks_only = true` in
`requirements.toml` to ignore user, project, and session hook configs while
still allowing managed hooks from requirements and managed config layers. This
setting is only supported in `requirements.toml`; putting it in `config.toml`
does not enable managed-hooks-only mode.
