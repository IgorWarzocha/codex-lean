# Deno Notebook runtime

This fork uses a persistent JavaScript and TypeScript runtime by default for Codex's `exec` and `wait` tools. Rust controls Deno's Jupyter kernel through `jupyter-zmq-client`. No Python, JupyterLab or Node controller is required.

## Run

Use a [release package](https://github.com/IgorWarzocha/codex-lean/releases/latest).
Notebook is already the default, so no runtime flag or particular model is needed.
From the extracted package folder, on Linux or macOS:

```sh
./bin/codex --sandbox danger-full-access --cd /path/to/project
```

On Windows, use PowerShell:

```powershell
.\bin\codex.exe --sandbox danger-full-access --cd C:\path\to\project
```

Replace the project path with your own. Use this only in a trusted local working directory. Deno Jupyter always has full filesystem, network and subprocess access. Notebook refuses restricted permissions, managed network proxies and remote or multiple execution environments. See the [quick start](../README.md#start) for sign-in and the [alternative setup](install.md#sandbox-and-api-key-setup) for sandboxed execution.

Notebook uses Deno from `PATH` when available. Otherwise, its first authorized startup downloads the pinned Deno 2.9.7 release from `denoland/deno` into `$CODEX_HOME/notebook/runtime`. The download and extracted executable must match the bundled SHA-256 hashes and sizes before installation. The cached runtime is reused without another download. Nothing is installed globally. Linux, macOS and Windows assets are provided for x86-64 and ARM64.

For persistent configuration:

```toml
[features.code_mode]
runtime = "notebook"
# Optional explicit executable. A missing override fails without downloading.
deno_program = "/absolute/path/to/deno"
# Optional saved profile, applied after restoring saved bindings.
notebook_profile = "daily"
# Heap limit in MiB. Default 4096, allowed range 256 through 65536.
notebook_max_heap_mib = 4096
# Print command output below its metadata. Default false uses JSON.
notebook_plain_command_output = true
```

Code Mode is enabled by default and uses Notebook when `runtime` is omitted. Full-access permissions must still be selected separately. Set `runtime = "v8"` for sandboxed execution. Set `features.code_mode.enabled = false` to disable feature-selected Code Mode. Explicit model catalog tool-mode overrides still apply.

Omit `deno_program` to allow automatic installation. Set it to `"deno"` to require an existing executable on `PATH`, including for offline environments. Network and verification failures are reported rather than falling back to an unverified runtime. Ephemeral threads can share the managed runtime cache without persisting Notebook state.

## Retained state

- A separate persistent Deno kernel belongs to each running Codex thread.
- JavaScript and TypeScript bindings survive cells and live context rollover. Resuming the same thread restores its last saved serializable state.
- New threads restore durable project bindings, including explicitly pinned helpers. Existing live threads remain private and do not receive another thread's changes.
- Checkpoints and named profiles restore values and function definitions. They never replay cells. Functions must be self-contained or use retained global dependencies. Open handles, imports, promises and other unsupported values are reported as skipped.
- Nested tools use Codex's existing dispatcher and tool approvals.
- Cells can yield and continue through `wait`. Only one cell runs per kernel at a time.
- Ordinary JavaScript errors retain the kernel but do not checkpoint the failed cell. Cancellation invalidates live state, and the next execution restores the last completed checkpoint. Terminal kernel failures trigger a bounded recovery attempt. Recovery reports its result and never replays failed work or reverses external side effects. Manual restart and reset remain available if recovery fails.

The agent's `notebook` tool manages status, checkpoints, profiles and pins. Ask it to inspect retained bindings, pin a reusable helper, save a named profile, or prune unpinned temporary state. Pinned functions can run after startup or nested tool results. These hooks run with the same full host access as notebook cells.

To choose a default profile, first save one with `notebook`'s `save` action, then set `features.code_mode.notebook_profile` to its name. Startup and restart attempt the profile after restoring project and private bindings, before startup hooks. Any name collision leaves the entire profile unapplied and preserves restored bindings. Status reports whether the profile loaded. Unavailable profiles produce a notice, while failures applying a profile abort startup. Profile bindings remain thread-private unless pinned. Reset restores durable project state without applying the profile. Ephemeral threads may read a profile without writing state. Explicit `notebook load` also rejects collisions.

`text(await tools.exec_command(...))` and `text(await tools.write_stdin(...))` omit routine timing and chunk metadata. Set `notebook_plain_command_output = true` to show command output on separate lines instead of inside JSON. Exit codes, running session IDs and truncation information remain visible. The returned JavaScript object is unchanged, so code can still inspect every field.

Normal shutdown checkpoints an idle notebook and runs `Symbol.dispose` and `Symbol.asyncDispose` cleanup with a bounded wait. An interrupted or unavailable kernel is terminated without running cleanup. Permission revocation also skips user cleanup code.

Historical cells are saved as bounded `.ipynb` journals. Diagnostics checks that history with Deno's language server, separately from the current kernel's health. Journals are not a recovery script.

Startup context and notebook status list exact-version npm imports found in successful project cells. The agent must ask before using an unlisted package. This inventory is guidance, not a package sandbox or proof of prior approval. Imports are not restored as live modules. Recreate them explicitly or in a pinned startup helper.

State lives under `$CODEX_HOME/notebook`, outside the working tree. These private files contain code and serialized values, not encrypted data. Project state is shared by directories within the same Git repository. Session checkpoints remain thread-private. `--ephemeral` keeps checkpoints in memory and disables profile writes and journals.

## Questions for the user

The agent asks through `tools.request_user_input` inside `exec`, without separate
top-level question tools. `delivery: "wait"` is the default and returns an answer
object after the user responds. `delivery: "async"` returns `{ accepted: true }`
immediately so other work can continue. Async replies arrive as user messages.
Available delivery modes follow the model and collaboration-mode settings.

Both modes use the native CLI and desktop question interfaces. Questions can
offer choices or omit them for free-text answers. Notebook cancellation also
cancels a pending wait.

## Remote notes and history

Codex backend authentication enables remote notes and history independently of token-budget settings. Agents can call them through `exec`, including independent calls in `Promise.all`. Await dependent writes to the same note path.

The default `context_strategy = "notes"` uses this remote storage for continuity
across context resets. Sessions without supported backend authentication must
explicitly select `context_strategy = "compaction"`. See [configuration](config.md#context-continuity).

These are not local files or notebook bindings. Encrypted results are delivered directly to the model. JavaScript receives `{ delivered_to_model: true, call_id }`, not decrypted contents. The call ID matches the model's result, so parallel receipts can be associated with their requests. API-key and other-provider sessions do not expose this backend capability.

## Boundaries

This is a native Codex implementation of the Pi Notebook workflow, not a Pi extension host. Pi custom extensions and ChatGPT desktop plugin packaging are not included. Native notebook code has full host access even though nested Codex tools retain their own approval checks.

On Unix, the controller owns the kernel's process group. On Windows, it assigns the suspended kernel to a non-breakaway Job Object before allowing it to run. Failure to establish ownership rejects startup. Kernel shutdown terminates owned descendants. The Windows path has not yet been exercised on a Windows machine.

Retained functions do not preserve lexical closures. Recreate live connections and imported dependencies in a pinned startup function. A failed startup hook blocks execution until the hook is repaired or unpinned. When the kernel has not started, unpin and reset operate on saved state without running startup hooks. Profile loading rejects name collisions instead of overwriting live bindings.

Checkpoint and journal budgets follow the configured kernel heap: one eighth of the heap, clamped between 8 MiB and 256 MiB. The default 4096 MiB heap gives a 256 MiB persistence budget. Values that cannot fit are reported as skipped. Releasing lexical bindings may require rebuilding the kernel from retained values, so runtime-only handles must be recreated.

## Implementation

`codex-rs/notebook-kernel` owns the Deno process and Jupyter protocol. `codex-rs/notebook` implements Codex's `CodeModeSessionProvider`, output collection and authenticated local tool bridge. Core selects that provider and supplies the Notebook tool contract.

Local Rust and real-Deno tests do not call a model. Live model validation uses `gpt-6-luna` with `model_reasoning_effort="low"`.
