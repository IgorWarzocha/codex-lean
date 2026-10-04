# Codex Lean with ChatGPT Desktop

Use Lean as the app-server behind ChatGPT Desktop while keeping stock Codex installed. The GUI is the client. Lean runs locally or on the server selected through the GUI's SSH connection.

This guide was verified with Codex Lean `0.160.0-lean.2` and Linux ChatGPT Desktop `26.930.31730`, including two remote clients connected to one Linux server. Desktop environment overrides are implementation-specific, not a stable public integration contract. Recheck them when the desktop app changes. Do not patch its bundle to force compatibility.

## Install on the machine that runs the work

Download and [verify the complete package](install.md#verify-and-extract). Extract it into a new versioned directory, for example:

```text
~/.local/share/codex-lean/releases/0.160.0-lean.2/
  bin/
  codex-package.json
  codex-resources/
  codex-path/
```

Keep the package intact. Do not overwrite the existing `codex` command, the GUI's bundled executable, or anything inside stock `~/.codex/packages`.

For a first installation, create a stable link and private home:

```sh
mkdir -p "$HOME/.local/bin"
install -d -m 700 "$HOME/.codex-lean"
ln -s releases/0.160.0-lean.2 "$HOME/.local/share/codex-lean/current"
```

Create `~/.local/bin/codex-lean` with these contents, then run `chmod 755 ~/.local/bin/codex-lean`:

```sh
#!/bin/sh
export CODEX_HOME="$HOME/.codex-lean"
exec "$HOME/.local/share/codex-lean/current/bin/codex" "$@"
```

Ensure `~/.local/bin` is on your command path. Check both commands before continuing:

```sh
codex --version
codex-lean --version
codex-lean login
codex-lean login status
```

Stock `codex` should still resolve to its original installation. Lean should report the release version. Sign in separately rather than copying or linking stock credentials. On a headless server, use `codex-lean login --device-auth` and complete the displayed browser flow. Sign in before starting the daemon. If it was already running, restart it afterward so it loads the new credentials.

Lean's home holds separate settings, threads, caches, and daemon sockets. Old stock conversations do not move into it. Start a new Lean thread for the first check.

Notebook requires full access. For a trusted execution host, set `sandbox_mode = "danger-full-access"` in `~/.codex-lean/config.toml` and select compatible permissions in the GUI. Do not overwrite existing configuration or silently expand permissions. To retain sandboxing, run `codex-lean settings set code-mode v8` instead. API-key users also need `codex-lean settings set context compaction`.

## Run the GUI locally

Quit the existing GUI instance first. Otherwise Electron can hand the launch to the old process with its old backend.

For the tested Linux package:

```sh
CODEX_HOME="$HOME/.codex-lean" \
CODEX_CLI_PATH="$HOME/.local/bin/codex-lean" \
CODEX_APP_SERVER_FORCE_CLI=1 \
chatgpt
```

`CODEX_CLI_PATH` selects the executable. `CODEX_APP_SERVER_FORCE_CLI=1` selects the CLI transport instead of a previously configured WebSocket endpoint. For another desktop package, use its actual application executable rather than assuming it is named `chatgpt`.

To make this the normal menu launch, put these exports in a user-owned launcher that executes the original GUI binary with `"$@"`. Point a user desktop-entry override at that launcher. On Linux, copy the installed desktop entry to `~/.local/share/applications/` under the same filename and change only its `Exec` entry. Use absolute paths in `Exec`. Leave the package-owned launcher untouched.

These overrides belong to this local launch, not your entire login session. In particular, **do not set `CODEX_APP_SERVER_FORCE_CLI=1` on clients using the built-in SSH route below**. It disables that SSH WebSocket transport too.

## Connect from a desktop or laptop over SSH

Install and sign into Lean on the **server**. The clients need their normal ChatGPT Desktop app and working SSH access, not another Lean installation.

In the tested app, the SSH connection enters the server's interactive login shell with `CODEX_REMOTE_PAYLOAD` set. Its launch code prepends `${CODEX_INSTALL_DIR:-$HOME/.local/bin}` to `PATH` and uses `CODEX_HOME` for the socket location. Configure those variables for that app launch only.

On a server using **zsh**, add this block to the user-owned `~/.zshenv`, preserving its other contents:

```sh
# ChatGPT SSH sessions use Lean. Ordinary terminal sessions keep stock Codex.
if [ -n "${CODEX_REMOTE_PAYLOAD:-}" ]; then
  export CODEX_INSTALL_DIR="$HOME/.local/share/codex-lean/current/bin"
  export CODEX_HOME="$HOME/.codex-lean"
fi
```

For another shell, inspect which startup files its interactive login mode actually reads and adapt the environment block there. The `CODEX_REMOTE_PAYLOAD` marker is specific to this desktop implementation. Verify it in the installed app before relying on it. Never replace or evaluate the payload in your startup file.

This selects Lean for all of this user's ChatGPT SSH connections to the server. It does not rename stock Codex or change ordinary SSH commands. No `sshd` environment-forwarding change is needed. A client-side absolute `CODEX_CLI_PATH` is not a remote path override: this app's SSH resolver only accepts a bare command name there.

Start the isolated server through Lean's own daemon command:

```sh
codex-lean app-server daemon start
codex-lean app-server daemon version
```

The first start copies the complete invoking package into `~/.codex-lean/packages/app-server-daemon` and pins it. The version response must identify the Lean version and the socket under `~/.codex-lean`. Do not run an upstream installer to prepare this daemon.

In each client GUI, connect to the server's normal SSH alias. Leave the client's local Codex backend alone. Disconnect and reconnect an existing server connection so it reruns the remote launch. Do not kill unrelated Codex processes. If a stale stock daemon must be stopped, identify its exact process and coordinate interruption of its users first.

The GUI can start the selected app-server on a later connection when no server is running. The detached daemon command is not a boot service. SSH and the private Unix socket provide the transport; this setup opens no TCP listener or public port.

## Verify the backend, not just the command name

An agent performing the setup should check all of these:

1. In an ordinary server shell, `command -v codex` and `codex --version` still identify stock Codex.
2. From **each client**, exercise the server's app-style login environment. For the zsh example, run this through that client's SSH connection:

   ```sh
   ssh server 'CODEX_REMOTE_PAYLOAD=lean-check zsh -lic '\''PATH="${CODEX_INSTALL_DIR:-$HOME/.local/bin}:$PATH"; export PATH; command -v codex; codex app-server daemon version'\'''
   ```

3. Open the actual GUI connection. Confirm a successful `initialize` handshake in the desktop logs, not merely a successful shell command. On the tested Linux package, logs live under `~/.local/state/codex/logs/`. The protocol's initialize result reports `userAgent` with the Lean version and `codexHome` pointing to the Lean home. On Linux, `/proc/<pid>/exe` and the selected process's `CODEX_HOME` provide additional executable and home evidence.
4. Start a fresh thread with the intended permissions and run a small tool task. Reading a randomly generated test file proves a real tool ran; asking for arithmetic alone does not. Confirm successful completion and the expected answer. Notes and Notebook settings can be inspected with `codex-lean settings`.

Logs may contain prompts, file contents, and credentials. Inspect only the required fields and do not publish raw traffic logs. A successful protocol handshake is not a promise that every GUI feature works. CLI voice changes remain CLI-only.

## Update or switch back

For an update, verify and extract the new package separately. Close local Lean GUI instances and coordinate interruption of remote work. Retarget the `current` symlink to the new complete package, then update the separately pinned daemon package:

```sh
codex-lean app-server daemon update --from-cli --yes
codex-lean app-server daemon version
```

The update command can restart a running daemon. Reopen or reconnect the GUI and verify its version again. Plain `daemon update` is not the fork update path. Upstream replacement installers are disabled in Lean.

To return to stock, close the Lean GUI, remove its user desktop-entry override or restore the previous `Exec`, and remove only the Lean environment block from the server's shell startup file. Stop the isolated daemon with `codex-lean app-server daemon stop`, then launch the original GUI or reconnect through SSH. Keep both homes intact unless you separately intend to delete their data. No stock executable or stock history needs restoring.
