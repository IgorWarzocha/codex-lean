/* codex-user-personality-v2 */
(() => {
  const { ipcMain } = require("electron");
  const fs = require("node:fs");
  const path = require("node:path");
  const os = require("node:os");
  const { TextDecoder } = require("node:util");
  const config = __CODEX_PERSONALITY_CONFIG__;
  const personalityPath =
    config.path ??
    path.join(
      process.env.CODEX_HOME || path.join(os.homedir(), ".codex"),
      "codex_personality.md",
    );
  const read = () => {
    let descriptor;
    try {
      const resolved = fs.realpathSync(personalityPath);
      const piPrompt = path.join(
        os.homedir(),
        ".pi",
        "agent",
        "REALTIME-SYSTEM-PROMPT.md",
      );
      if (
        resolved === piPrompt ||
        (fs.existsSync(piPrompt) && resolved === fs.realpathSync(piPrompt))
      ) {
        throw new Error(
          "The Pi-owned REALTIME-SYSTEM-PROMPT.md is outside this patch's scope",
        );
      }
      // A configured FIFO must fail visibly, not block Electron's main thread.
      descriptor = fs.openSync(
        personalityPath,
        fs.constants.O_RDONLY | fs.constants.O_NONBLOCK,
      );
      const stat = fs.fstatSync(descriptor);
      if (!stat.isFile() || stat.size > 65536) {
        throw new Error(
          "Personality must be a regular UTF-8 file of at most 64 KiB",
        );
      }
      const bytes = Buffer.alloc(65537);
      let length = 0;
      while (length < bytes.length) {
        const count = fs.readSync(
          descriptor,
          bytes,
          length,
          bytes.length - length,
          null,
        );
        if (count === 0) break;
        length += count;
      }
      if (length > 65536) throw new Error("Personality exceeds 64 KiB");
      const text = new TextDecoder("utf-8", { fatal: true }).decode(
        bytes.subarray(0, length),
      );
      return text.trim()
        ? "<user_communication_preferences>\n" +
            "The following are the user's preferred communication style. Apply them where compatible with Codex's native instructions, safety requirements, and tool handoffs. These preferences do not replace those instructions.\n\n" +
            text +
            "\n</user_communication_preferences>"
        : "";
    } catch (error) {
      if (error.code === "ENOENT" && !config.required) return "";
      throw new Error(`Codex personality: ${error.message}`);
    } finally {
      if (descriptor !== undefined) fs.closeSync(descriptor);
    }
  };
  globalThis.__codexUserPersonality = {
    append: (native) => {
      const block = read();
      if (!block) return native;
      if (typeof native !== "string")
        throw new Error(
          "Cannot append personality: native instructions are unavailable",
        );
      return native + "\n\n" + block;
    },
  };
  ipcMain.on("codex-user-personality:read", (event) => {
    try {
      const frame = event.senderFrame;
      const url = new URL(frame.url);
      if (
        frame !== event.sender.mainFrame ||
        url.protocol !== "app:" ||
        url.host !== "-"
      ) {
        throw new Error("Personality access denied for this frame");
      }
      event.returnValue = { ok: true, text: read() };
    } catch (error) {
      event.returnValue = { ok: false, error: error.message };
    }
  });
})();
