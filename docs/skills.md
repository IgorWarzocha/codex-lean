# Skills

Codex discovers skills through its native configured sources. The `skills` tool loads the catalog and instructions on demand instead of adding the full catalog to every session prompt.

## Copy existing skills into Lean

**Copy, do not move.** Keep the original skills and their configuration unchanged. Work only on the copies under `~/.codex-lean/skills`, using the isolated home from the [desktop setup guide](desktop.md). Do not symlink the skill trees together: editing a linked skill would also change the original.

Flat skill packages already work. Categorizing them is optional. A long `SKILL.md` also works without conversion; split it only when separate references make the instructions easier to load on demand.

For example, copy this existing package:

```text
~/.codex/skills/deploy/
  SKILL.md
  scripts/check.sh
  assets/template.yaml
```

Then organize and edit only the Lean copy:

```text
~/.codex-lean/skills/
  communication/SKILL.md
  operations/
    deploy/
      SKILL.md
      references/
        checks.md
        rollback.md
      scripts/check.sh
      assets/template.yaml
```

`communication` stays uncategorized. `deploy` appears under `operations` in `skills list`. The first directory below the skills root supplies the category; it is not part of the skill's name. Do not put a `SKILL.md` in a category folder unless that folder is itself meant to be a skill. Repository skills appear under `session` regardless of their folder grouping.

### Copy the whole package

Choose individual trusted packages, not another tool's whole configuration directory. Keep scripts, assets, existing references, and optional `agents/openai.yaml` alongside `SKILL.md`. Do not copy generated `.system` skills or plugin caches as personal skills.

This example copies one package and refuses an existing destination or symlinked source content. Change the source and destination to the skill you selected. Verify that the destination's parent directories are real Lean directories, not links back to the original tree.

```sh
python3 - <<'PY'
from pathlib import Path
import shutil

source = Path.home() / ".codex/skills/deploy"
destination = Path.home() / ".codex-lean/skills/operations/deploy"
if not (source / "SKILL.md").is_file():
    raise SystemExit("Source must be a complete skill package containing SKILL.md")
if source.is_symlink() or any(path.is_symlink() for path in source.rglob("*")):
    raise SystemExit("Review source symlinks before making an independent copy")
destination.parent.mkdir(parents=True, exist_ok=True)
shutil.copytree(source, destination)
PY
```

The same approach works for a selected package from another agent's skill directory. Copying instructions does not install that agent's extensions or make its tool names available in Lean. Review tool calls, absolute paths, dependencies, and script permissions in the copy. Do not execute imported scripts just to discover what they do. If the destination already exists, compare it with the source before deciding which edits to bring across; do not merge or overwrite it blindly.

### Make the copy lean

Keep valid YAML frontmatter. A useful entry point has a stable name, a short description saying when to load it, and the instructions needed on every use:

```markdown
---
name: deploy
description: "Use before deploying a service or planning its rollback."
---

Read `references/checks.md` before deployment.
Read `references/rollback.md` before changing a running release.
Confirm the target environment and the rollback plan before making changes.
```

Keep the original operational rules and failure handling. Transfer branch-specific detail from the copied `SKILL.md` into the copy's `references/` files, leaving explicit instructions for when to read each one. References are not automatically loaded just because they exist. Small skills can stay in one file.

Preserve package-relative paths when copying. When splitting a document, fix links whose location changed. Replace absolute paths that still point to the original package. Do not duplicate every reference back into the entry point, and do not rename the skill merely because its category changed.

### Check shared sources and verify

`CODEX_HOME` isolates the home-specific skill tree, not every skill source. Native discovery also reads shared `~/.agents/skills`, applicable repository skill directories, and enabled plugins. A copied skill can therefore coexist with another active skill of the same name. Categories do not create name namespaces.

Inspect the active catalog and source paths. If you copied a shared skill and want only the copy active in Lean, disable the **original path in Lean's configuration**, not in the original skill tree. For example, add this to `~/.codex-lean/config.toml`, replacing the example with the original skill's actual absolute path:

```toml
[[skills.config]]
path = "/home/you/.agents/skills/deploy/SKILL.md"
enabled = false
```

Do not disable by name when both copies have that name. Do not remove the shared original. For repository-specific instructions, check the catalog from the actual project rather than assuming a global catalog describes every session.

Start a fresh Lean thread after copying. Ask the agent to list the relevant category, read the copied skill, and read one qualified reference. Confirm that the reported source paths point into the Lean copy and that referenced scripts and assets exist. Compare the originals before and after to confirm they were not changed. A representative task can then check that the skill loads for the right situation; it need not perform a real deployment.

An agent doing this migration should inventory the selected packages, copy without clobbering destinations, reorganize only the copies, check shared-source collisions, and report copied paths and any instructions it adapted. It should not move, delete, rewrite, or link the originals.

## Load skills on demand

In Code Mode and Notebook Mode:

```js
text(await tools.skills("list"))
text(await tools.skills("list code session"))
text(await tools.skills("read communication codebase-hygiene"))
text(await tools.skills("read codebase-hygiene testing"))
```

`list` groups skills by category. Repository skills appear under `session`. `read` accepts an exact skill name or a provided package locator. Additional exact skill names load those packages. Other selectors resolve Markdown references across the available skills.

A read containing only reference selectors returns those references and their source paths, without repeating the primary skill. Qualify ambiguous references with `skill-name/references/reference-name`, or use their listed source paths. Full package-contained resource locators also work for executor and cloud skills. Local package inventories include paths for scripts and assets. Executor inventories use authority-bearing resource locators and provide a named environment root for filesystem access. Cloud resource locators stay with their provider and are not local paths.

Reads return complete selected instructions. Output over 48 KiB is rejected. Read fewer packages or select references when a result is too large. Native skill disablement, explicit invocation and source access remain in effect.

For additional native installation and authoring options, see the [upstream documentation](https://developers.openai.com/codex/skills).
