const assert = require("node:assert/strict");
const fs = require("node:fs");
const vm = require("node:vm");

const { before, after, sections, version } = JSON.parse(
  fs.readFileSync(0, "utf8"),
);
const symbols =
  version === "61225"
    ? {
        bootstrap: {
          boundary: "var TR=",
          defaults: ["iR", "lR", "dR"],
          compose: "wR(options)",
        },
        worker: {
          boundary: "var _Se=",
          defaults: ["oSe", "r$", "dSe"],
          compose: "aSe(hSe(options))",
        },
      }
    : {
        bootstrap: {
          boundary: "var wR=",
          defaults: ["rR", "cR", "uR"],
          compose: "CR(options)",
        },
        worker: {
          boundary: "var hSe=",
          defaults: ["iSe", "r$", "lSe"],
          compose: "rSe(pSe(options))",
        },
      };
const evaluate = (source, owner) => {
  // Only the captured complete instruction region executes. No Electron,
  // network, filesystem or truncated non-instruction boundary code executes.
  const start = source.indexOf("function ");
  const end = source.indexOf(symbols[owner].boundary);
  assert(end > start);
  const heartbeat = (id) => id === "heartbeat";
  const context = vm.createContext({ _k: heartbeat, vk: heartbeat });
  vm.runInContext(source.slice(start, end), context);
  return context;
};

let comparisons = 0;
for (const owner of ["bootstrap", "worker"]) {
  const native = evaluate(before[owner], owner);
  const slim = evaluate(after[owner], owner);
  const expected = evaluate(before[owner], owner);
  const names = symbols[owner].defaults;
  const defaults = names.map((name) => vm.runInContext(name, native));
  names.forEach((name, i) => {
    vm.runInContext(`${name}=${JSON.stringify(sections[i])}`, expected);
  });
  const compose = (context, options) => {
    context.options = options;
    return vm.runInContext(symbols[owner].compose, context);
  };
  for (let mask = 0; mask < 64; mask++) {
    for (let overrides = 0; overrides < 4; overrides++) {
      for (const heartbeat of [false, true]) {
        for (const nonGit of [false, true]) {
          // Native-looking user text must never be filtered as app boilerplate.
          const base = defaults.join("\n\n") + "\nUSER_BASE_SENTINEL";
          const options = {
            baseInstructions: base,
            gitSettings: {
              branchPrefix: "branch_SENTINEL",
              commitInstructions: "commit_SENTINEL",
              pullRequestInstructions: "pr_SENTINEL",
            },
            threadId: heartbeat ? "heartbeat" : "ordinary",
            isNonGitWorkspace: nonGit,
            threadToolsEnabled: !!(mask & 1),
            sidebarSectionToolsEnabled: !!(mask & 2),
            worktreeToolsEnabled: !!(mask & 4),
            workspaceDependenciesEnabled: !!(mask & 8),
            includeProseDetailLevelInstructions: !!(mask & 16),
            hostedAutomationsEnabled: !!(mask & 32),
            instructionOverrides: {
              desktopContextSection:
                overrides & 1 ? defaults[0] + "OVERRIDE_DESKTOP" : undefined,
              workspaceDependenciesSection:
                overrides & 2 ? defaults[1] + "OVERRIDE_DEPS" : undefined,
              worktreeInstructionsIncluded: true,
            },
          };
          const original = compose(native, options);
          const actual = compose(slim, options);
          assert.equal(actual, compose(expected, options));
          const prefix = owner === "bootstrap" ? base + "\n\n" : "";
          assert(actual.startsWith(prefix));
          const app = actual.slice(prefix.length);
          assert.equal(
            app.includes("### Worktrees"),
            options.worktreeToolsEnabled,
          );
          assert.equal(
            app.includes("### Non-technical UI"),
            options.includeProseDetailLevelInstructions,
          );
          assert.equal(
            app.includes("OVERRIDE_DEPS"),
            options.workspaceDependenciesEnabled && !!(overrides & 2),
          );
          assert.equal(app.includes("OVERRIDE_DESKTOP"), !!(overrides & 1));
          for (const marker of [
            "::code-comment",
            ":codex-followup",
            "codex://review?pr=",
          ]) {
            assert.equal(actual.includes(marker), original.includes(marker));
          }
          if (owner === "bootstrap") {
            assert.equal(app.includes("commit_SENTINEL"), !nonGit);
            assert.equal(app.includes("pr_SENTINEL"), !nonGit);
            assert.equal(app.includes("branch_SENTINEL"), !nonGit);
            assert.equal(app.includes("## Heartbeats"), heartbeat);
            assert.equal(
              app.includes("jawbone_id"),
              options.hostedAutomationsEnabled,
            );
          }
          comparisons++;
        }
      }
    }
  }
}
console.log(`${comparisons} native composer comparisons passed`);
