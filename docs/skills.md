# Skills

Codex discovers skills through its native configured sources. The `skills` tool loads the catalog and instructions on demand instead of adding the full catalog to every session prompt.

## Copy existing skills into Lean

**Copy intact packages, do not move or rewrite them.** Keep the original skills and their configuration unchanged. Place independent copies under `~/.codex-lean/skills`, using the isolated home from the [desktop setup guide](desktop.md). Preserve every package's contents and internal directory layout. Do not symlink the skill trees together.

Flat skill packages already work. Categorizing them is optional and changes only where the complete package lives, not its instructions or internal organization.

For example, copy this existing package:

```text
~/.codex/skills/deploy/
  SKILL.md
  references/checks.md
  scripts/check.sh
  assets/template.yaml
```

Place the unchanged copy under a category directory:

```text
~/.codex-lean/skills/
  communication/SKILL.md
  operations/
    deploy/
      SKILL.md
      references/checks.md
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

The same approach works for a selected package from another skill directory. If the destination already exists, compare the two packages and resolve which copy to keep before proceeding. Do not merge or overwrite it blindly. Preserve file permissions and package-relative paths, and do not rename the skill merely because its category changed.

### Check shared sources and verify

`CODEX_HOME` isolates the home-specific skill tree, not every skill source. Native discovery also reads shared `~/.agents/skills`, applicable repository skill directories, and enabled plugins. A copied skill can therefore coexist with another active skill of the same name. Categories do not create name namespaces.

Inspect the active catalog and source paths. If you copied a shared skill and want only the copy active in Lean, disable the **original path in Lean's configuration**, not in the original skill tree. For example, add this to `~/.codex-lean/config.toml`, replacing the example with the original skill's actual absolute path:

```toml
[[skills.config]]
path = "/home/you/.agents/skills/deploy/SKILL.md"
enabled = false
```

Do not disable by name when both copies have that name. Do not remove the shared original. For repository-specific instructions, check the catalog from the actual project rather than assuming a global catalog describes every session.

Compare the copied file inventory and hashes with the source to confirm that the complete package arrived unchanged. Compare the originals before and after to confirm they were not changed either. Start a fresh Lean thread, list the relevant category, and read the copied skill. Confirm that its reported source path points into the Lean copy.

An agent doing this migration should inventory the selected packages, copy them intact into the chosen category directories without clobbering destinations, verify discovery and shared-source collisions, and report the source and destination paths. It should not move, delete, rewrite, split, or otherwise edit any skill contents, including those in the copies.

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
