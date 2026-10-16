const assert = require("node:assert/strict");
const fs = require("node:fs");
const vm = require("node:vm");

// Execute the captured native function. Only its external IO is controlled.
const { before, after, version, text, rpc } = JSON.parse(
  fs.readFileSync(0, "utf8"),
);
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
      Xa: context.Kp,
      Ig: context.Zm,
      Qdc: 256,
      Zdc: context.D9s,
      ec: context.eg,
    });
  }
  if (version === "71326") {
    Object.assign(context, {
      Qa: context.Kp,
      Bg: context.Zm,
      Qdc: 256,
      Zdc: context.D9s,
      Qs: context.eg,
    });
  }
  if (version === "31256") {
    Object.assign(context, {
      Lo: context.Kp,
      yg: context.Zm,
      Sou: 256,
      xou: context.D9s,
      oc: context.eg,
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
        model: realtimeSessionOverrides?.model,
      };
      const native = await invoke(before, args);
      const patched = await invoke(after, args);
      assert.equal(patched.requests.length, 1);
      if (version === "31256") {
        const request = native.requests[0];
        assert.equal(request.body.sdp, args.offerSdp);
        assert.equal(
          request.body.session.model,
          args.model ?? "gpt-live-1-codex",
        );
        assert.equal(
          request.body.session.voice_session_id,
          args.realtimeSessionId,
        );
        assert.equal(request.body.session.audio.output.voice, args.voice);
        assert.equal(request.headers["Session-Id"], args.codexSessionId);
        assert.equal(request.headers["Thread-Id"], args.conversationId);
        const metadata = JSON.parse(request.headers["x-codex-turn-metadata"]);
        assert.equal(metadata.backend_model_name, args.backendModel);
        assert.equal(
          metadata.backend_thinking_effort,
          args.backendThinkingEffort,
        );
        assert.equal(
          metadata.thread_source,
          threadSource && threadSource.length <= 256 ? threadSource : undefined,
        );
        assert.equal(native.result.answerSdp, "unchanged answer SDP");
        assert.equal(native.result.callId, "call-1");
      }
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
  if (version === "31256") {
    await checkExistingCallRpc();
    await checkTextComposer();
  }
}

async function checkExistingCallRpc() {
  assert.equal(
    rpc.after,
    rpc.before,
    "existing-call attachment needs no prompt edit",
  );
  const invokeRpc = async (source) => {
    const requests = [];
    const call = vm.runInNewContext("(" + source + ")");
    await call({
      callId: "native-call-id",
      conversationId: "native-conversation",
      codexResponseItemPrefix: "native-prefix",
      codexResponsesAsItems: true,
      outputModality: "audio",
      realtimeEndInstructions: "native end",
      realtimeStartInstructions: "native start",
      realtimeSessionId: "native-session",
      realtimeSessionOverrides: {
        codexResponseHandoffChannelPrefixes: ["channel"],
        codexResponseHandoffMode: "mode",
      },
      manager: { sendRequest: async (...args) => requests.push(args) },
    });
    return JSON.parse(JSON.stringify(requests));
  };
  const native = await invokeRpc(rpc.before);
  assert.deepEqual(await invokeRpc(rpc.after), native);
  assert.equal(native[0][0], "thread/realtime/start");
  assert.deepEqual(native[0][1].transport, {
    type: "existingCall",
    callId: "native-call-id",
  });
  assert.equal(Object.hasOwn(native[0][1], "prompt"), false);
  console.log("Existing-call RPC unchanged");
}

async function checkTextComposer() {
  let comparisons = 0;
  for (const hostId of ["local", "ssh:remote"]) {
    for (const instructionOverrides of [
      undefined,
      { custom: "native override" },
    ]) {
      for (const hostedAutomationsEnabled of [false, true]) {
        const invokeText = async (source) => {
          const calls = [];
          const context = {
            a: {
              Gt: (options) => {
                calls.push(["compose", options]);
                return JSON.stringify(options);
              },
            },
            i: {
              jn: Object.fromEntries(
                [
                  "branchPrefix",
                  "commitInstructions",
                  "pullRequestInstructions",
                ].map((key) => [key, { key }]),
              ),
            },
            __codexUserPersonality: { append: (value) => value + suffix },
          };
          const owner = vm.runInNewContext("({" + source + "})", context);
          Object.assign(owner, {
            customInstructions: { read: () => "native custom instructions" },
            getAppServerClientForHostIdOrThrow: (id) => ({ host: id }),
            isWorkspaceDependenciesFeatureEnabled: async (id) => {
              calls.push(["dependencies", id]);
              return true;
            },
            isNonGitWorkspace: async (cwd, client) => {
              calls.push(["git", cwd, client]);
              return false;
            },
            getSettingsStore: () => ({
              getEffective: (key) => "native setting: " + key,
            }),
            habitatAutomationsService: {
              getMigrationOwner: () => "native owner",
            },
          });
          const result = await owner.getProjectAwareDeveloperInstructions({
            baseInstructions: "native base",
            cwd: "/native/workspace",
            hostId,
            hostedAutomationsEnabled,
            instructionOverrides,
            includeProseDetailLevelInstructions: true,
            sidebarSectionToolsEnabled: true,
            threadToolsEnabled: true,
            worktreeToolsEnabled: true,
            threadId: "native-thread",
          });
          return { result, calls: JSON.parse(JSON.stringify(calls)) };
        };
        const native = await invokeText(text.before);
        const patched = await invokeText(text.after);
        assert.equal(patched.result, native.result + suffix);
        assert.deepEqual(patched.calls, native.calls);
        comparisons++;
      }
    }
  }
  console.log(`${comparisons} native text composer comparisons passed`);
}

main().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
