const assert = require("node:assert/strict");
const fs = require("node:fs");
const vm = require("node:vm");

// Execute the captured native function. Only its external IO is controlled.
const { before, after, version } = JSON.parse(fs.readFileSync(0, "utf8"));
const suffix = "\n\nCommunication preferences";

async function invoke(source, args) {
  const requests = [];
  const logs = [];
  const context = {
    TextEncoder,
    Kp: { info: (...values) => logs.push(JSON.stringify(values)) },
    Zm: () => ({ Authorization: "test authorization" }),
    O9s: 4096,
    D9s: { parse: (value) => value },
    codexUserPersonality: { append: (prompt) => prompt + suffix },
    eg: {
      getInstance: () => ({
        fetch: async (url, options) => {
          assert.equal(options.signal, args.signal);
          requests.push({
            url,
            method: options.method,
            body: JSON.parse(options.body),
            headers: JSON.parse(JSON.stringify(options.headers)),
          });
          return {
            text: async () => "unchanged answer SDP",
            headers: {
              get: () => "https://example.invalid/calls/call-1?query=unchanged",
            },
          };
        },
      }),
    },
  };
  if (version === "21434") {
    Object.assign(context, {
      Xa: context.Kp, Ig: context.Zm, Qdc: 256,
      Zdc: context.D9s, ec: context.eg,
    });
  }
  const call = vm.runInNewContext("(" + source + ")", context);
  const result = JSON.parse(JSON.stringify(await call(args)));
  return { requests, logs, result };
}

async function main() {
  for (const realtimeSessionOverrides of [
    undefined,
    { model: "custom-v1" },
    { version: "v3" },
    { version: "v3", model: "custom-v3" },
  ]) {
    for (const threadSource of [undefined, "thread-ź", "x".repeat(4097)]) {
      const args = {
        codexSessionId: "session-1",
        conversationId: "conversation-1",
        initialItems: [
          { role: "user", text: "input" },
          { role: "assistant", text: "output" },
        ],
        offerSdp: "unchanged offer SDP",
        prompt: "Native instructions",
        realtimeSessionId: "realtime-1",
        realtimeSessionOverrides,
        signal: new AbortController().signal,
        threadSource,
        voice: "unchanged voice",
        backendModel: "backend-model",
        backendThinkingEffort: "high-ź",
      };
      const native = await invoke(before, args);
      const patched = await invoke(after, args);
      assert.equal(patched.requests.length, 1);
      assert.equal(native.requests[0].body.session.instructions, args.prompt);
      assert.equal(
        patched.requests[0].body.session.instructions,
        args.prompt + suffix,
      );
      patched.requests[0].body.session.instructions = args.prompt;
      assert.deepEqual(patched, native);
    }
  }
  console.log("12 native call payload comparisons passed");
}

main().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
