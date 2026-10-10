# Releasing

This is the maintainer runbook for cutting a cpm-planner release. A tag
`vX.Y.Z` on `main` runs [`.github/workflows/release.yml`](../.github/workflows/release.yml),
which builds and publishes the GitHub release, the container image and the MCP
registry entry. crates.io and npm are published by hand afterwards.

Throughout, `X.Y.Z` is the new version and `vX.Y.Z` its tag.

## 1. Bump the version

On a branch off `dev`, set the same version in every place that carries it:

- `Cargo.toml`: `[package] version`.
- `Cargo.lock`: run `cargo check` (or `cargo update -p cpm-planner`) so the
  lock file records the new version.
- `server.json`: the top-level `version`, `packages[].version`, and the tag of
  the OCI identifier (`ghcr.io/praxec/cpm-planner:X.Y.Z`).
- `npm/package.json`: `version` (the npm launcher downloads the release with
  this version, so it must match).

Then check them:

```sh
scripts/check-version-sync.sh
```

It fails when any `server.json` version or OCI tag, or the `npm/package.json`
version, differs from `Cargo.toml`.
The release workflow repeats the check against the tag and refuses to build on
a mismatch. Also update version pins in the README (installer `--version`
examples and the `cargo install --git ... --tag` line).

## 2. Update the CHANGELOG

In [CHANGELOG.md](../CHANGELOG.md), rename `## [Unreleased]` to
`## [X.Y.Z] - YYYY-MM-DD` and add a fresh empty `## [Unreleased]` above it.
Follow Keep a Changelog: group entries under Added, Changed, Fixed and so on.

Open the bump as a pull request into `dev` and merge it once CI is green.

## 3. Release PR from `dev` to `main`

Open a pull request with base `main` and head `dev`. The gitflow guard
(`gitflow-guard.yml`) rejects any other head branch. Wait for every required
check, then merge it with a **merge commit**, not squash or rebase, so `main`
and `dev` keep a shared history.

## 4. Tag `main`

```sh
git fetch origin
git checkout main
git pull --ff-only origin main
git tag -a vX.Y.Z -m "vX.Y.Z"
git push origin vX.Y.Z
```

Tag only commits on `main`: the workflow triggers on any `v*` tag. The tag must
match `vMAJOR.MINOR.PATCH` (an optional pre-release or build suffix is
allowed) and equal the `Cargo.toml` and `server.json` versions.

The workflow then runs these jobs:

1. **validate + draft release**: checks the tag against the versions and
   creates a draft GitHub release with generated notes.
2. **build**: six targets (Linux, macOS and Windows on x86_64 and ARM64). Each
   archive is smoke-tested with `scripts/mcp-smoke.mjs` (MCP `initialize` and
   `tools/list`) and uploaded to the draft.
3. **aggregate metadata + publish**: writes `checksums.sha256` and
   `release-manifest.json` (it fails if the matrix is incomplete), uploads them
   with `install.sh` and `install.ps1`, runs `npm pack` in `npm/` and attaches
   `matthew-cochran-cpm-X.Y.Z.tgz`, and publishes the release. It does not run
   `npm publish` and has no npm token.
4. **container image**: builds the image, smoke-tests it, then pushes
   `ghcr.io/praxec/cpm-planner` for linux/amd64 and linux/arm64 with the tags
   `X.Y.Z`, `X.Y` and `latest`.
5. **MCP registry**: publishes `server.json` as `io.github.praxec/cpm-planner`
   with a pinned, checksum-verified `mcp-publisher` over GitHub OIDC. This job
   is best-effort (`continue-on-error`), so a failure does not fail the
   release; check its log.

## 5. Verify the release

Assets: the release page lists six archives, `checksums.sha256`,
`release-manifest.json`, `install.sh`, `install.ps1` and
`matthew-cochran-cpm-X.Y.Z.tgz`.

```sh
gh release view vX.Y.Z --json assets --jq '.assets[].name'
gh release download vX.Y.Z --pattern checksums.sha256 --pattern '*linux-gnu.tar.gz'
sha256sum -c --ignore-missing checksums.sha256
```

Installer, on a clean shell (repeat with `install.ps1` on Windows):

```sh
curl -fsSL https://github.com/praxec/cpm-planner/releases/latest/download/install.sh | sh -s -- --version vX.Y.Z
node scripts/mcp-smoke.mjs ~/.local/bin/cpm-planner
```

Container image:

```sh
docker pull ghcr.io/praxec/cpm-planner:X.Y.Z
node scripts/mcp-smoke.mjs docker run -i --rm ghcr.io/praxec/cpm-planner:X.Y.Z
```

MCP registry: confirm the registry job succeeded, or look the server up in the
registry and check that its version is `X.Y.Z`.

## 6. Publish to crates.io (manual)

The workflow does not publish the crate. From the tagged commit, with a
crates.io token configured (`cargo login`):

```sh
git checkout vX.Y.Z
cargo publish --dry-run --locked
cargo package --list --locked   # no docs/superpowers, .github or scripts
cargo publish --locked
```

Until this step runs, crates.io keeps serving the previous version, which is why
the README installs from the git tag. docs.rs builds the new version on its own
a few minutes after publishing.

## 7. Publish to npm (manual)

The workflow packs the npm launcher (`npm/`, package `@matthew-cochran/cpm`)
and attaches the tarball to the release, but never publishes it. The launcher
downloads the GitHub release whose version equals its own, so publish only
after the release in step 4 is public and its assets are verified (step 5).

Publish the tarball the workflow built, not a local `npm pack`, so npm gets
exactly the file attached to the release. You need to be logged in to npm
(`npm login`) as an owner of the `@matthew-cochran` scope, with 2FA:

```sh
mkdir -p /tmp/cpm-npm && cd /tmp/cpm-npm
gh release download vX.Y.Z --repo praxec/cpm-planner --pattern 'matthew-cochran-cpm-X.Y.Z.tgz'
tar -tzf matthew-cochran-cpm-X.Y.Z.tgz   # package/{LICENSE,README.md,package.json,bin/,lib/} only, no tests
npm publish ./matthew-cochran-cpm-X.Y.Z.tgz --access public   # prompts for the 2FA code
```

Verify from a clean cache, outside the repository:

```sh
npx -y @matthew-cochran/cpm@X.Y.Z --version
node scripts/mcp-smoke.mjs npx -y @matthew-cochran/cpm@X.Y.Z   # from a checkout of vX.Y.Z
```

The first command prints `cpm-planner X.Y.Z` on stdout after a one-time
download whose progress goes to stderr. The smoke test checks that the launcher
keeps stdout clean for MCP (`initialize` and `tools/list`).

## Rollback

- **A build failed before publishing.** The release is still a draft. Fix the
  problem on `dev`, then delete the draft and the tag and re-release with the
  next patch version (or re-push the same tag if nothing was published):

  ```sh
  gh release delete vX.Y.Z --yes
  git push origin :refs/tags/vX.Y.Z
  git tag -d vX.Y.Z
  ```

  Re-running the workflow on an existing draft is also safe: assets are
  uploaded with `--clobber`, and an existing release is left as is.
- **A bad release was published.** Do not reuse its version. Mark the GitHub
  release as a pre-release or edit its notes to warn users, then ship a fixed
  `X.Y.(Z+1)`. The installers resolve `releases/latest`, so they pick up the
  fix as soon as it is published.
- **A bad crate was published.** crates.io versions cannot be deleted or
  overwritten. Yank it so new lock files do not pick it up, then publish a fix:

  ```sh
  cargo yank --version X.Y.Z cpm-planner
  # undo: cargo yank --version X.Y.Z --undo cpm-planner
  ```

- **A bad image was pushed.** Push the fixed release, which moves `latest` and
  `X.Y`. Delete the bad `X.Y.Z` tag from the GHCR package settings only if it is
  harmful to keep.
- **A bad npm version was published.** npm versions cannot be reused.
  Deprecate it and publish the fixed version:

  ```sh
  npm deprecate @matthew-cochran/cpm@X.Y.Z "broken release, use X.Y.(Z+1)"
  ```

  `npm unpublish` works only within 72 hours and blocks the version number
  for good, so prefer deprecation.
- **A bad MCP registry entry.** Publish the fixed version; the registry lists
  the newest version.
