# Desktop personality patcher

An opt-in ASAR patch that appends your communication preferences to **Codex desktop's native text and realtime voice instructions**. It preserves Codex's own prompts and tool handoffs. Python 3.10 or newer is required, without additional packages.

The patcher writes separate artifacts. It never installs them, changes its inputs, or overwrites existing outputs. An optional Linux hook reapplies the patch after package updates. Compatibility is checked against the specific native instruction code it edits, not the app version or whole-archive hash. Unrelated code changes and renamed bundle chunks are allowed. Unfamiliar instruction code, ambiguous owners, and already-patched archives are rejected.

## Prepare a patched archive

From this repository, check the installed archive:

```sh
python3 scripts/desktop-voice-prompt/patch.py \
  --asar /usr/lib/chatgpt/resources/app.asar --check
```

Create your own UTF-8 Markdown file at `$CODEX_HOME/codex_personality.md`, normally `~/.codex/codex_personality.md`. Write communication preferences, not copies of Codex's tool schemas or native instructions. The script never creates or reads your personality file. The app reads it at runtime.

```sh
python3 scripts/desktop-voice-prompt/patch.py \
  --asar /usr/lib/chatgpt/resources/app.asar \
  --output "$HOME/app.personality.asar"
```

To select another absolute path on the app's machine, add `--personality-file "$HOME/path/to/codex_personality.md"`. Only that path is embedded, not the contents. The desktop patch does not read `personality_file` from `config.toml`. Use the explicit flag if you want a non-default file.

A missing default file or an empty file adds nothing. An existing unreadable file, invalid UTF-8, or content over 64 KiB causes an error rather than silently ignoring preferences. An explicitly selected file must exist. The Pi-owned `~/.pi/agent/REALTIME-SYSTEM-PROMPT.md` and resolved aliases are deliberately excluded.

## macOS

Use the archive inside the app bundle. The current official DMG names the app ChatGPT.app, despite its Codex bundle identity. Supply its matching `Info.plist` so the script can generate updated ASAR integrity metadata:

```sh
python3 scripts/desktop-voice-prompt/patch.py \
  --asar "/Applications/ChatGPT.app/Contents/Resources/app.asar" \
  --info-plist "/Applications/ChatGPT.app/Contents/Info.plist" \
  --output "$HOME/app.personality.asar" \
  --output-info-plist "$HOME/Info.personality.plist"
```

The original plist must match the original ASAR. Only its `Resources/app.asar` integrity hash changes. No Electron integrity protection is disabled. Modifying a signed app invalidates its signature. Generating these artifacts does **not** re-sign the app. macOS installation requires a separate signing step on a Mac. A patched Mac app has not been launched during validation.

## Installation and removal

Installation is a separate, deliberate action. Fully quit the desktop app first. Install the generated archive with the installation's normal owner and mode, usually `root:root` and `0644` on Linux. Keep the matching `app.asar.unpacked` directory unchanged. On macOS, the generated plist and a valid signature are also required. Preparing an archive does not need root.

Restart the app after installation. Start a new text conversation to get newly composed instructions. Stop and restart voice to pick up changes. The file is reread when desktop text developer instructions are composed and whenever a native realtime call is created, including replacement calls. Existing text instructions and ongoing voice calls are not hot-updated.

To remove the patch, quit the app and reinstall its package. No restore backup is required. Updates replace the patch unless you install the hook below. Run `--check` against a new unpatched archive before generating another patch manually. If an integrity or signing check rejects the result, reinstall the original rather than disabling that protection.

## Keep personality after pacman updates

On Arch Linux and Omarchy, install the optional hook with your desktop account and an existing personality file:

```sh
sudo python3 scripts/desktop-voice-prompt/install_hook.py \
  --user "$USER" \
  --personality-file "$HOME/.local/state/codex-desktop-personality-test/codex_personality.md"
```

Git and internet access are required for automatic reapplication. The installer copies the hook into root-owned `/opt/codex-desktop-personality/` and registers `/etc/pacman.d/hooks/95-codex-desktop-personality.hook`. It rejects user-owned, writable, or symlinked parent directories without changing their permissions. Your Markdown stays user-owned and is read by the app, not imported as code.

On every app update, the hook fetches the latest commit from [`IgorWarzocha/codex-lean`, branch `lean`](https://github.com/IgorWarzocha/codex-lean/tree/lean). It extracts only the fixed patcher source files into a root-owned temporary directory and runs that version in a fresh Python process. It ignores user Git configuration and credentials, disables Git hooks, and permits only HTTPS. No checkout, submodule, persistent clone, or old archive backup is created. The fetched commit ID is printed with the hook output. Concurrent invocations are locked out.

**Enabling this hook trusts future code on that Git branch to run as root.** HTTPS verifies the server, not the safety of each commit. The installed launcher and Git-fetching bootstrap stay fixed until you rerun the installer. Patcher updates on the branch take effect automatically.

The hook runs after any package installs or upgrades `usr/lib/chatgpt/resources/app.asar`, including `chatgpt-bin` updates through pacman, yay, and Omarchy. It gracefully closes the selected user's ChatGPT desktop executable before fetching Git. Shutdown interrupts any active desktop conversation. Only processes running `/usr/lib/chatgpt/ChatGPT` are targeted, including the deleted executable left running after a package upgrade. CLI agents and `cua_node` workers are not signalled. The hook waits up to ten seconds for Electron to exit. If shutdown fails or the app restarts during shutdown, it warns and refuses to patch rather than force-killing the app.

A compatible archive is staged on the same filesystem and atomically replaced with its original owner and mode. No original archive backup is kept. The unpacked directory stays untouched. The app remains closed even if Git or compatibility checks fail. Reopen it after the update.

If native instruction code is incompatible, the updated archive stays unchanged and Codex uses its native personality. If Git cannot be fetched or the fetched sources are invalid, the hook leaves the archive unchanged rather than silently using an old patcher. The hook prints a warning, logs it under `codex-desktop-personality`, and sends a persistent desktop notification when your session is available. Operational failures are also reported. All paths through the launcher return success so the personality hook does not turn the app update into a failed update. A two-minute timeout prevents a stuck updater from holding up updates indefinitely. When no desktop session is available, the printed warning and journal remain available:

```sh
journalctl -t codex-desktop-personality
```

After adjusting compatibility checks, commit and push the patcher to that branch. To close the app, fetch the patcher, and retry on an unpatched archive, run:

```sh
sudo /opt/codex-desktop-personality/pacman-hook.sh \
  "$USER" "$HOME/.local/state/codex-desktop-personality-test/codex_personality.md"
```

Already-patched archives are deliberately rejected rather than patched twice. For a deliberate offline retry, the installed snapshot remains available as `/opt/codex-desktop-personality/pacman_hook.py`. Run it with `sudo python3 -E -s -B`, `--user`, and `--personality-file`; it does not fetch Git. To disable automatic reapplication, remove the hook and its installed code:

```sh
sudo rm /etc/pacman.d/hooks/95-codex-desktop-personality.hook
sudo rm -r /opt/codex-desktop-personality
```

Your personality file is not removed. Reinstall the app package afterwards if you also want to remove the active patch.

## What changes

The main process appends a `<user_communication_preferences>` block after the result of `getProjectAwareDeveloperInstructions`. The block identifies the text as the user's preferred communication style, subordinate to native instructions, safety requirements, and tool handoffs.

For voice, a narrow preload bridge supplies the same block to the native `thread/realtime/start` prompt and the client-owned `/wham/realtime/calls` instructions. Both keep the native prompt as an unchanged prefix. Existing-call attachment keeps its native handoff request unchanged. No bundled consumer ChatGPT wingman override is modified.

The renderer can request only the configured file. Access is restricted to the app's top-level `app://-` frames. Native routing, authentication, SDP, WebRTC, tools, delegation, and handoff fields remain unchanged. No native prompt or tool schema is copied into this repository.

Offline validation covered Linux 26.930.21537, 26.930.31730, and 26.930.41038, plus the official macOS Apple Silicon DMG 26.930.31730. It compared all untouched archive entries and executed the actual native instruction owners before and after transformation with controlled boundary inputs. Linux laptop personality behavior has also been confirmed in a live user test. macOS launch and native tool execution remain unverified.

## Tests

Python 3.10 or newer and Node.js 22 or newer are required. Tests use temporary files, without network access or an installed app.

```sh
python3 -m unittest discover -s scripts/desktop-voice-prompt -p 'test_*.py' -v
```
