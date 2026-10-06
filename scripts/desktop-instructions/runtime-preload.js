/* codex-user-personality-v2 */
(() => {
  const { contextBridge, ipcRenderer } = require("electron");
  contextBridge.exposeInMainWorld("codexUserPersonality", {
    append: (native) => {
      const result = ipcRenderer.sendSync("codex-user-personality:read");
      if (!result || result.ok !== true || typeof result.text !== "string") {
        throw new Error(
          `Codex personality: ${result?.error ?? "loader unavailable"}`,
        );
      }
      if (!result.text) return native;
      if (typeof native !== "string")
        throw new Error(
          "Cannot append personality: native instructions are unavailable",
        );
      return native + "\n\n" + result.text;
    },
  });
})();
