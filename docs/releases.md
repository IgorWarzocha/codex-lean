# Releasing Codex Lean

Codex Lean uses its own GitHub Actions workflow, `lean-release.yml`, and publishes
archives to this fork's GitHub Releases. Binaries are not committed to Git. The
upstream release workflow depends on OpenAI infrastructure and is not the fork's
release path.

## Versioning

Versions retain the upstream base and add a fork revision:

```text
0.160.0-lean.1
```

The version in `codex-rs/Cargo.toml` is authoritative. Keep workspace package
versions in `codex-rs/Cargo.lock` synchronized without upgrading dependencies.
Release tags use `lean-v<VERSION>`, for example `lean-v0.160.0-lean.1`.
Never move a published tag or replace its assets. Use a new fork revision for a
changed release.

## Release notes

Every full release build requires `docs/release-notes/<VERSION>.md` for the
workspace version. Start with `# Codex Lean <VERSION>` and a `## Changes` section
containing user-facing change bullets. Review those changes against the commits
since the previous release. Explain what users can now do or which failure was
fixed, not cache keys, packaging receipts, or CI implementation details. A
packaging-only release should name its user-visible repair or compatibility change.

Keep unchanged fork features separate from new changes. Do not copy a previous
release's change list into a new version. The workflow rejects missing notes,
mismatched titles, empty change lists, and TODO or TBD placeholders before full
builds start. It checks the notes again before publication and never falls back
to an earlier version. The Windows voice diagnostic scope does not require notes.
Editorial review still owns the accuracy and usefulness of the bullets.

The release script appends a short download and platform notice with links to
installation and validation details at the release commit. To preview a body
without building or publishing, set the exact version and source commit:

```sh
CODEX_REPO_ROOT="$PWD" RELEASE_VERSION=0.160.0-lean.2 \
RELEASE_COMMIT=19e5ad36f344597a945f43d4e27cb33da0163682 \
python .github/scripts/lean_release.py notes
```

Review the generated `lean-release-notes.md`. This command does not verify assets,
create a tag, or publish a release. Published notes can be corrected without
moving the tag or replacing its assets.

## Build and publish

Run the **Codex Lean release** workflow from the repository's Actions page, selecting
the `lean` branch. Leave publishing disabled to produce CI artifacts only.
Enable publishing to create a GitHub release marked Latest after every platform
has built, packaged, and passed its smoke checks. The release tag identifies the exact
commit that was built, not whichever commit happens to be latest when the jobs
finish.

After publication succeeds, update the README's release version and download
links against the published assets. Remove any source-only caveat for fixes now
included in that release. Do not advertise a download before its assets exist.

With GitHub CLI:

```sh
# Build and validate without publishing.
gh workflow run lean-release.yml --repo IgorWarzocha/codex-lean --ref lean

# Build, validate, and publish a release.
gh workflow run lean-release.yml --repo IgorWarzocha/codex-lean --ref lean -f publish=true
```

To diagnose a Windows native voice failure without rebuilding the other platforms:

```sh
gh workflow run lean-release.yml --repo IgorWarzocha/codex-lean --ref lean -f scope=windows-voice
```

This diagnostic scope produces only the intermediate Windows voice artifact. It
does not build CLI packages and cannot publish a release. After it passes, use the
default `all` scope to build and validate all complete packages for the release commit.
Rerunning an old job does not pick up a source fix; dispatch on the updated branch.

A matching `lean-v<VERSION>` tag can also start a release. Tag and workspace
versions must agree. A failed platform prevents publication; inspect and fix the
failure rather than dropping that platform from the release being validated.

The workflow uses public GitHub-hosted runners and pinned build actions. Build
jobs have read-only repository access. Only the publication job can write release
assets. It does not publish to OpenAI's package registries or use OpenAI signing
credentials.

## Reusing build outputs

Rust package jobs cache downloaded crates and compiled third-party dependencies.
Caches are separated by target, compiler, build settings and native toolchain
inputs. Cargo still runs with `--locked` and checks dependency fingerprints before
reusing compiled outputs. A changed lockfile can reuse compatible dependencies
from an earlier cache without accepting stale release binaries.

Workspace crates and final executables are not retained in this dependency cache.
The CLI and package helpers rebuild from the release checkout, with the current
app and verified voice stamps. Release optimization, packaging and smoke checks
are unchanged. Cache misses use the normal build path.

The first successful run seeds the cache. Dispatch on the default `lean` branch
to make caches available to later tag runs. GitHub scopes caches written by tag
runs to that tag, so different tags cannot reuse each other's entries.

### Native voice

The workflow reuses a sealed voice helper and audio runtime only when its exact
build-input cache key matches. Each platform has its own key. The key covers voice
sources and their local dependencies, the shared voice protocol, Cargo manifests
and lockfile, native runtime inputs, Bazel configuration and patches, build recipe,
and runner image. CLI-only and bundled-skill source changes do not invalidate it.
Workspace version changes do invalidate it because those versions are compiler
inputs.

A cache hit skips native compilation, not validation. The workflow checks the
artifact's provenance and digests, embeds the original voice commit into the new
CLI, and runs the packaged voice handshake and runtime smoke checks. The release
records separate app and voice commits. Reused binaries are never relabeled as
new builds. Any platform failure still blocks publication.

The first run with caching enabled builds and seeds the cache. An absent or evicted
cache entry triggers a rebuild. Earlier workflow runs do not populate this cache.
An invalid restored artifact fails validation rather than silently shipping it.

## Package contents and boundaries

The existing package builder owns the layout, helper selection, and verified
downloads. Releases contain optimized, stripped binaries, a package manifest,
the Code Mode host, ripgrep, platform-specific sandbox helpers, and the matching
native voice helper and audio runtime. Linux packages embed the digest of the
finalized bubblewrap binary before compiling the CLI.

Archives are named `codex-lean-<VERSION>-<TARGET>.tar.gz`, or `.zip` on Windows.
`SHA256SUMS` records their digests. Checksums detect changed downloads; they do not
replace review of the source or trust in the publishing account.

Initial targets are Linux x64 and ARM64, Apple Silicon macOS, and Windows x64.
Smoke checks run the packaged binaries on each native runner. They do not prove
support for older operating systems, live provider authentication, or every
interactive feature.

macOS and Windows packages are not developer-signed or notarized. The voice helper
and privately bundled audio runtime must pass their packaging and runtime checks
before publication. A platform build failure blocks the release; omitting voice
is not a fallback. CI does not establish a successful live microphone call.

Users update by downloading a new fork package. The fork does not run upstream's
automatic installer, which would replace Codex Lean with stock Codex.
