# Codex Lean

An unofficial fork of OpenAI Codex for long-running agent work. Persistent tools, notes-based context resets, shorter instructions, and voice that keeps you informed without watching the terminal.

## What changes from upstream

- **Tools keep their state.** A persistent JavaScript and TypeScript Notebook retains variables and reusable helpers across tool calls and context resets. Saved state restores without replaying old commands.
- **Context resets use notes.** The agent checkpoints useful state, starts a fresh context, and retrieves older conversation when needed. Notes is the default. Ordinary compaction remains available.
- **Less context spent on instructions.** Shorter system and tool prompts, skills read on demand, and nested `AGENTS.md` instructions loaded when the agent reaches the relevant files.
- **Coordinated subagents.** V2 agents inherit tools and share a message board for decisions, dependencies, and findings. A child finishing resumes its idle parent automatically. Stopping the parent prevents that resumption.
- **Voice follows the work.** Spoken progress and results, context refresh after rollover, and recovery from a dropped call. These changes belong to the CLI, not the ChatGPT desktop app.
- **Your preferences, not a copied system prompt.** One `codex_personality.md` file sets the tone for text and voice. `/settings` exposes the fork's context, tool-runtime, and agent controls.

[Notebook](docs/notebook.md) · [Context management](docs/config.md#context-continuity) · [Skills](docs/skills.md) · [Voice](docs/config.md#voice-continuity) · [Settings](docs/config.md)

## Download

**Release 0.160.0-lean.2.** Download the complete package for your machine:

| Platform | Download |
| --- | --- |
| Linux, Intel or AMD 64-bit | [Linux x64](https://github.com/IgorWarzocha/codex-lean/releases/download/lean-v0.160.0-lean.2/codex-lean-0.160.0-lean.2-x86_64-unknown-linux-musl.tar.gz) |
| Linux, ARM64 | [Linux ARM64](https://github.com/IgorWarzocha/codex-lean/releases/download/lean-v0.160.0-lean.2/codex-lean-0.160.0-lean.2-aarch64-unknown-linux-musl.tar.gz) |
| macOS, Apple Silicon | [macOS ARM64](https://github.com/IgorWarzocha/codex-lean/releases/download/lean-v0.160.0-lean.2/codex-lean-0.160.0-lean.2-aarch64-apple-darwin.tar.gz) |
| Windows, Intel or AMD 64-bit | [Windows x64](https://github.com/IgorWarzocha/codex-lean/releases/download/lean-v0.160.0-lean.2/codex-lean-0.160.0-lean.2-x86_64-pc-windows-msvc.zip) |

[Latest release and checksums](https://github.com/IgorWarzocha/codex-lean/releases/latest)

Extract the archive into its own folder and keep the whole package together. It includes the CLI, tool helpers, and native voice runtime. **No Rust build is needed.** Notebook downloads a verified Deno runtime on first use if needed.

Linux voice requires glibc 2.28 or newer. macOS and Windows packages are not developer-signed. See [platform requirements and download verification](docs/install.md).

OpenAI's installers, npm package, and Homebrew package install upstream Codex, not this fork.

## Bring your existing setup

**We recommend a separate `~/.codex-lean` home, seeded with copies of your current Codex instructions, preferences, and skills.** Keep stock `~/.codex` untouched. Lean otherwise uses `~/.codex` too, so set `CODEX_HOME` on every Lean invocation, including login. The commands below use the separate home.

Copy your home-level `AGENTS.md` and any `codex_personality.md` you use. Review and transfer the settings you want from `config.toml`, checking absolute paths and permission choices rather than overwriting Lean's configuration. Copy selected skill packages intact into `~/.codex-lean/skills`, including their references, scripts, and assets. Do not move the originals or symlink the two homes together. Sign in separately instead of copying credentials.

**This carries over your setup, not a live conversation.** A fresh Lean home does not automatically inherit stock threads, history, or remembered context. Keep old conversations in stock Codex. For ongoing work, bring a written handoff into a new Lean thread with the goal, decisions, relevant paths, and next steps. Do not expect switching backends to resume the same working state.

Skills have a few important quirks:

- **Discovery is not loading.** Lean's `skills` tool lists and reads skills on demand instead of placing the full catalog in every session prompt.
- **A separate home is not complete skill isolation.** Shared `~/.agents/skills`, repository skills, and enabled plugins can still contribute skills. Check the catalog from your actual project. Disable unwanted duplicate source paths in Lean's configuration without changing the originals.
- **Categories are folders, not namespaces.** Flat packages work. A category folder must not contain its own `SKILL.md` unless it is itself a skill. Repository skills appear under `session`, and moving a package into a category does not resolve duplicate skill names.

Follow the [skill-copying and verification guide](docs/skills.md#copy-existing-skills-into-lean), then confirm a fresh Lean thread reads the intended copies. The [side-by-side setup](docs/install.md#run-alongside-upstream-codex) covers a dedicated launcher.

## Start

The default setup uses **ChatGPT sign-in for Notes** and **full access for Notebook**. Only use Notebook in a trusted local project: its code can access your filesystem, network, and subprocesses. Prefer a sandbox or use an API key? Follow the [alternative setup](docs/install.md#sandbox-and-api-key-setup) instead.

Open a terminal in the extracted package folder, sign in, then launch in your project. Replace the example project path with your own.

**Linux and macOS**

```sh
CODEX_HOME="$HOME/.codex-lean" ./bin/codex login
CODEX_HOME="$HOME/.codex-lean" ./bin/codex --sandbox danger-full-access --cd /path/to/project
```

**Windows PowerShell**

```powershell
$env:CODEX_HOME = "$HOME\.codex-lean"
.\bin\codex.exe login
.\bin\codex.exe --sandbox danger-full-access --cd C:\path\to\project
```

Use a terminal dedicated to Lean for the PowerShell commands so stock Codex does not inherit the Lean home. Use `/settings` inside Codex to change defaults. `codex settings` also works before starting a thread. For side-by-side use, create a [scoped `codex-lean` launcher](docs/desktop.md#install-on-the-machine-that-runs-the-work) rather than replacing stock `codex`. Keep the executable in its package.

## Use with ChatGPT Desktop

ChatGPT Desktop can use Lean as its Codex app-server backend, locally or over its built-in SSH connection. The GUI stays installed as usual. Lean runs on the machine doing the work, so your desktop and laptop can both connect to one Lean installation on a server.

Keep stock Codex installed. Give Lean its own package directory, `codex-lean` command, and `~/.codex-lean` home. Configure the GUI's backend selection, not just the executable's name. New Lean threads have separate settings and history. The CLI's voice changes do not replace ChatGPT Desktop's voice implementation.

The [desktop and server setup guide](docs/desktop.md) covers installation, sign-in, local GUI launch, SSH routing, verification, updates, and rollback. The Linux setup has been verified with real desktop and laptop connections to the same server.

To have an agent do the setup, give it this task:

> Follow `docs/desktop.md` to install Codex Lean alongside my existing Codex and configure ChatGPT Desktop to use it. Inspect the installed app's supported backend overrides and my shell startup files first. Preserve stock Codex and its data. Use a separate Lean home and sign-in. Copy my existing instructions, preferences, and selected intact skill packages into Lean, reviewing configuration paths and permissions. Follow `docs/skills.md` to verify discovery and shared-source conflicts without changing the originals. Explain what does not carry over, including old conversations and live working state. For a remote setup, configure the execution server and verify the connection from each client. Prove the GUI handshake reaches the Lean version and home, then run a small tool task. Report the changed paths and how to switch back. Do not patch the desktop bundle or expose an unauthenticated network listener.

## Updates and source builds

Download a new package from [this fork's releases](https://github.com/IgorWarzocha/codex-lean/releases/latest) to update. Codex Lean does not run upstream's replacement installer.

Release `0.160.0-lean.2` includes automatic parent resumption. The optional `wait_agent` tool remains disabled by default. See the [release notes](https://github.com/IgorWarzocha/codex-lean/releases/tag/lean-v0.160.0-lean.2).

[Build from source](docs/install.md#build-from-source) · [Run alongside upstream Codex](docs/install.md#run-alongside-upstream-codex) · [Report a fork issue](https://github.com/IgorWarzocha/codex-lean/issues)

Based on [OpenAI Codex](https://github.com/openai/codex), under the [Apache-2.0 License](LICENSE). This is not an official OpenAI release.
