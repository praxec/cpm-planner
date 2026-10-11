#!/usr/bin/env bash
# Publish the npm launcher tarball for a release (the `npm` job in release.yml).
# Usage: scripts/npm-publish.sh <tarball> <version> [--dry-run]
#
# A version already on npm is skipped, so re-running is safe. Pre-releases
# (X.Y.Z-rc.N) go to the `next` dist-tag, never `latest`. --dry-run runs
# `npm publish --dry-run` (CI uses it on every PR) and skips the provenance
# attestation, which only a trusted-publishing run can create.
set -euo pipefail

pkg="@matthew-cochran/cpm"
[ "$#" -ge 2 ] || { echo "usage: $0 <tarball> <version> [--dry-run]" >&2; exit 2; }
tarball=$1
version=$2
dry_run=false
case "${3:-}" in
  "") ;;
  --dry-run) dry_run=true ;;
  *) echo "unknown option: $3" >&2; exit 2 ;;
esac

[ -f "$tarball" ] || { echo "tarball not found: $tarball" >&2; exit 1; }
# npm reads a bare `dir/file.tgz` as GitHub shorthand (owner/repo) and tries
# to clone it, so always hand it an explicit path.
case "$tarball" in
  /* | ./* | ../*) ;;
  *) tarball="./$tarball" ;;
esac

packed_version=$(tar -xzOf "$tarball" package/package.json | node -e \
  'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>console.log(JSON.parse(s).version))')
[ "$packed_version" = "$version" ] \
  || { echo "tarball holds version $packed_version, expected $version" >&2; exit 1; }

if [ "$(npm view "${pkg}@${version}" version 2>/dev/null || true)" = "$version" ]; then
  echo "${pkg}@${version} is already on npm; nothing to publish."
  exit 0
fi

dist_tag=latest
case "$version" in *-*) dist_tag=next ;; esac

args=(publish "$tarball" --access public --tag "$dist_tag")
if [ "$dry_run" = true ]; then
  args+=(--dry-run)
else
  args+=(--provenance)
fi
npm "${args[@]}"
