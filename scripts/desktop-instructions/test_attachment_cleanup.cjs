const assert = require("node:assert/strict");
const fs = require("node:fs");
const vm = require("node:vm");

const { before, after } = JSON.parse(fs.readFileSync(0, "utf8"));
function invoke(source) {
  const calls = [];
  let callback;
  const managers = new Map(
    ["local", "durable", "remote-ssh-discovered:server"].map((id) => [id, {
      cleanupPendingPastedTextAttachments() {
        calls.push(id);
        return Promise.resolve();
      },
    }]),
  );
  const s = {
    getAll: () => [...managers.keys()].map((id) => ({ getHostId: () => id })),
    getImplForHostId: (id) => managers.get(id),
    addRegistryCallback: (fn) => { callback = fn; },
  };
  const S = { current: new Set() };
  vm.runInNewContext(`(function(s,S,Isc){${source}0;})`, {})(s, S, () => {});
  callback();
  return { calls, seenDurable: S.current.has(managers.get("durable")) };
}
assert.deepEqual(invoke(before).calls, ["local", "durable", "remote-ssh-discovered:server"]);
const repaired = invoke(after);
assert.deepEqual(repaired.calls, ["local", "remote-ssh-discovered:server"]);
assert.equal(repaired.seenDurable, false);
console.log("Native local and SSH cleanup preserved; environmentless cloud cleanup skipped");
