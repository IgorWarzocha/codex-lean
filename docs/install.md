# Installing Codex Lean

Most users should download a [complete release package](https://github.com/IgorWarzocha/codex-lean/releases/latest). No compiler or global Deno installation is needed. Follow the [README quick start](../README.md#start) after extracting it.

OpenAI's installer scripts, `@openai/codex`, and the Homebrew `codex` package install upstream Codex, not Codex Lean.

## Platform requirements

| Platform | Archive name ends with |
| --- | --- |
| Linux, Intel or AMD 64-bit | `x86_64-unknown-linux-musl.tar.gz` |
| Linux, ARM64 | `aarch64-unknown-linux-musl.tar.gz` |
| macOS, Apple Silicon | `aarch64-apple-darwin.tar.gz` |
| Windows, Intel or AMD 64-bit | `x86_64-pc-windows-msvc.zip` |

Linux voice requires **glibc 2.28 or newer**, even though the CLI uses musl. The complete package is not suitable for musl-only distributions such as a standard Alpine installation. There is no Intel Mac or Windows ARM64 release package yet.

macOS and Windows packages are **not developer-signed**. macOS packages are not notarized. The operating system may block or warn about them; do not disable system-wide security protections to run a download.

Each package passes build and smoke checks on its native CI runner. Those checks do not establish support for every older OS or verify a live microphone call. Voice also needs audio devices and account access to the realtime service.

## Verify and extract

Download the archive and its matching `.sha256` file from the same release. In the download directory:

- Linux: `sha256sum -c <archive-name>.sha256`
- macOS: `shasum -a 256 -c <archive-name>.sha256`
- Windows PowerShell: run `Get-FileHash <archive-name> -Algorithm SHA256` and compare the hash with the `.sha256` file.

Replace `<archive-name>` with the downloaded filename. Do not run the package if verification fails. Checksums detect changed bytes; they do not replace trust in the publisher.

Extract into a new folder. Keep `codex-package.json`, `bin`, `codex-resources`, and `codex-path` together. Moving only `codex` breaks access to its packaged helpers. The package includes the CLI, Code Mode host, ripgrep, platform-specific helpers, and native voice with its audio libraries.

Run `./bin/codex --version` from the extracted folder. On Windows, use `.\bin\codex.exe --version`. Then follow [Start](../README.md#start).

## Sandbox and API-key setup

Notebook needs full access. To keep Codex's sandbox instead, select V8 Code Mode before starting a thread:

```sh
./bin/codex settings set code-mode v8
```

For API keys or other providers, also choose ordinary compaction. Notes requires Codex backend authentication and remote storage:

```sh
./bin/codex settings set context compaction
```

Then launch without `--sandbox danger-full-access`:

```sh
./bin/codex --cd /path/to/project
```

On Windows, replace `./bin/codex` with `.\bin\codex.exe` and use your Windows project path. Authentication still follows the selected provider. For OpenAI API-key login, pipe the key through stdin with `codex login --with-api-key`; do not put a secret in the command's arguments.

These settings persist for new threads. They do not convert an already-running thread. You can change them later through `/settings`. See [configuration](config.md) for provider setup and other controls.

## Run alongside upstream Codex

Codex Lean uses `~/.codex` unless `CODEX_HOME` is set. To keep separate sign-in, settings, history, and caches, use a different home on **every** invocation, including login:

```sh
CODEX_HOME="$HOME/.codex-lean" ./bin/codex login
CODEX_HOME="$HOME/.codex-lean" ./bin/codex --sandbox danger-full-access --cd /path/to/project
```

In PowerShell, set `$env:CODEX_HOME = "$HOME\.codex-lean"` in a terminal dedicated to Lean before the equivalent commands. Do not export the Lean home globally if you also run stock Codex. Use a scoped launcher instead. The [desktop setup guide](desktop.md) shows a `codex-lean` launcher and how to use Lean through a local GUI or an SSH-connected server.

## Update or remove

To update, extract a new [Codex Lean release](https://github.com/IgorWarzocha/codex-lean/releases/latest) into a separate folder, stop the old process, and launch the new package. If you added the old `bin` directory to `PATH`, update that entry too. `codex update` directs you to this fork's releases; it does not install upstream Codex.

To remove the application, delete its extracted folder and remove its `PATH` entry. Your Codex home remains. It contains settings, sign-in state, history, and Notebook data, so deleting the application does not delete that data.

## Build from source

Source builds are for development. Use the Rust toolchain pinned in [`codex-rs/rust-toolchain.toml`](../codex-rs/rust-toolchain.toml) and the native compiler prerequisites for your platform.

```sh
git clone --branch lean https://github.com/IgorWarzocha/codex-lean.git
cd codex-lean/codex-rs
CARGO_PROFILE_DEV_DEBUG=0 cargo build --locked -p codex-cli --bin codex
./target/debug/codex --version
```

The command above builds a development CLI, not the complete release package. In particular, voice needs its matching native helper and bundled audio runtime. Building the release distribution follows the [release workflow](releases.md), which also configures the pinned native dependencies and verified V8 artifacts.

Install contributor tools such as `just`, `cargo-nextest`, and DotSlash only if you need their development tasks. From the repository, `just fmt`, `just fix -p <crate>`, and `just test -p <crate>` target formatting, lint fixes, and tests. Avoid `--all-features` for routine builds because it increases compilation and disk use.

## Diagnostics

Run `codex doctor` for installation, authentication, and runtime diagnostics. For a plaintext TUI log:

```sh
codex -c log_dir=./.codex-log
tail -F ./.codex-log/codex-tui.log
```

The normal TUI stores diagnostics in bounded local stores. `codex exec` prints its logs inline and defaults to `RUST_LOG=error`. See Rust's [`RUST_LOG` documentation](https://docs.rs/env_logger/latest/env_logger/#enabling-logging) for log filtering.
