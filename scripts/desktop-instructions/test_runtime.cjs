const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const vm = require("node:vm");
const { test } = require("node:test");
const { TextDecoder } = require("node:util");

function loader(config, codexHome, environment = { CODEX_HOME: codexHome }) {
  let handler;
  let bridge;
  const frame = { url: "app://-/index.html" };
  const sender = { mainFrame: frame };
  const electron = {
    ipcMain: {
      on: (channel, callback) => {
        assert.equal(channel, "codex-user-personality:read");
        handler = callback;
      },
    },
    ipcRenderer: {
      sendSync: (channel) => {
        assert.equal(channel, "codex-user-personality:read");
        const event = { sender, senderFrame: frame };
        handler(event);
        return event.returnValue;
      },
    },
    contextBridge: {
      exposeInMainWorld: (name, value) => {
        assert.equal(name, "codexUserPersonality");
        bridge = value;
      },
    },
  };
  const imports = (name) => {
    if (name === "electron") return electron;
    if (name === "node:fs") return fs;
    if (name === "node:util") return { TextDecoder };
    if (name === "node:path") return path;
    if (name === "node:os") return { homedir: () => codexHome };
    throw new Error("Unexpected import: " + name);
  };
  const main = fs
    .readFileSync(path.join(__dirname, "runtime-main.js"), "utf8")
    .replace("__CODEX_PERSONALITY_CONFIG__", JSON.stringify(config));
  const context = {
    require: imports,
    URL,
    Buffer,
    process: { env: environment },
  };
  vm.runInNewContext(main, context);
  vm.runInNewContext(
    fs.readFileSync(path.join(__dirname, "runtime-preload.js"), "utf8"),
    { require: imports },
  );
  return {
    main: context.__codexUserPersonality,
    bridge,
    electron,
    handler,
    sender,
  };
}

test("personality appends to native text and voice, reloads, and preserves defaults", async () => {
  const directory = fs.mkdtempSync(
    path.join(os.tmpdir(), "codex-personality-test-"),
  );
  const personality = path.join(directory, "codex_personality.md");
  try {
    const optional = loader({ path: null, required: false }, directory);
    const explicit = loader({ path: personality, required: true }, directory);
    const native = "Codex native instructions, tools and handoffs.\n";
    assert.equal(optional.bridge.append(native), native);
    assert.equal(optional.main.append(native), native);
    assert.throws(() => explicit.bridge.append(native), /ENOENT/);
    const homeDefault = loader({ path: null, required: false }, directory, {});
    assert.equal(homeDefault.bridge.append(native), native);
    fs.mkdirSync(path.join(directory, ".codex"));
    fs.writeFileSync(
      path.join(directory, ".codex", "codex_personality.md"),
      "Home-directory preference",
    );
    assert.ok(
      homeDefault.bridge.append(native).includes("Home-directory preference"),
    );
    for (const text of ["", "  \n"]) {
      fs.writeFileSync(personality, text);
      assert.equal(explicit.main.append(native), native);
      assert.equal(explicit.bridge.append(native), native);
    }
    fs.writeFileSync(personality, "Speak plainly. café ☕\n");
    const expected = explicit.main.append(native);
    assert.ok(
      expected.startsWith(native + "\n\n<user_communication_preferences>\n"),
    );
    assert.ok(expected.includes("user's preferred communication style"));
    assert.ok(
      expected.endsWith(
        "Speak plainly. café ☕\n\n</user_communication_preferences>",
      ),
    );
    assert.equal(explicit.bridge.append(native), expected);
    fs.writeFileSync(personality, "Updated style");
    assert.ok(explicit.bridge.append(native).includes("Updated style"));
    assert.ok(!explicit.bridge.append(native).includes("Speak plainly."));
    assert.throws(
      () => explicit.bridge.append(null),
      /native instructions are unavailable/,
    );
    fs.writeFileSync(personality, "x".repeat(65536));
    assert.ok(explicit.main.append(native).includes("x".repeat(65536)));
    for (const invalid of [Buffer.from([0xff]), "x".repeat(65537)]) {
      fs.writeFileSync(personality, invalid);
      assert.throws(() => explicit.bridge.append(native), /Codex personality:/);
      assert.throws(() => optional.main.append(native), /Codex personality:/);
    }
    fs.unlinkSync(personality);
    fs.mkdirSync(personality);
    assert.throws(() => explicit.bridge.append(native), /regular UTF-8 file/);
    fs.rmdirSync(personality);
    fs.writeFileSync(personality, "Native composition remains intact");
    for (const frame of [
      { url: "https://example.com/" },
      { url: "app://fs/@fs/private" },
      { url: "app://-/index.html" },
      { url: "not a URL" },
    ]) {
      const event = { sender: explicit.sender, senderFrame: frame };
      explicit.handler(event);
      assert.equal(event.returnValue.ok, false);
      assert.equal(event.returnValue.text, undefined);
    }
    const transformed = JSON.parse(process.env.CODEX_TRANSFORMED_REGIONS);
    const context = {
      __codexUserPersonality: explicit.main,
      codexUserPersonality: explicit.bridge,
      native: () => native,
    };
    const text = vm.runInNewContext(
      "({" + transformed.text + "}).getProjectAwareDeveloperInstructions",
      context,
    );
    assert.equal(await text("native input"), explicit.main.append(native));
    let request;
    const transport = { type: "webrtc", sdp: "unchanged SDP" };
    const rpc = vm.runInNewContext("(" + transformed.rpc + ")", context);
    await rpc({
      prompt: native,
      transport,
      manager: {
        sendRequest: async (method, params) => {
          assert.equal(method, "thread/realtime/start");
          request = params;
        },
      },
    });
    assert.equal(request.prompt, explicit.main.append(native));
    assert.equal(request.transport, transport);
    const call = vm.runInNewContext("(" + transformed.call + ")", context);
    for (const overrides of [null, { version: "v3" }]) {
      assert.equal(
        (await call({ prompt: native, realtimeSessionOverrides: overrides }))
          .instructions,
        explicit.main.append(native),
      );
    }
    explicit.electron.ipcRenderer.sendSync = () => undefined;
    assert.throws(() => explicit.bridge.append(native), /loader unavailable/);
    fs.unlinkSync(personality);
    const protectedPrompt = path.join(
      directory,
      ".pi",
      "agent",
      "REALTIME-SYSTEM-PROMPT.md",
    );
    fs.mkdirSync(path.dirname(protectedPrompt), { recursive: true });
    fs.writeFileSync(
      protectedPrompt,
      "A fake protected prompt owned by this test only",
    );
    fs.symlinkSync(protectedPrompt, personality);
    assert.throws(
      () => optional.main.append(native),
      /outside this patch's scope/,
    );
  } finally {
    fs.rmSync(directory, { recursive: true, force: true });
  }
});
