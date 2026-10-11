#!/usr/bin/env bash
# Fail when any version field in server.json, or the npm launcher's
# npm/package.json version, differs from Cargo.toml's package version.
# Usage: scripts/check-version-sync.sh [cargo_toml] [server_json] [npm_package_json]
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cargo_toml="${1:-$root/Cargo.toml}"
server_json="${2:-$root/server.json}"
npm_package_json="${3:-$root/npm/package.json}"

# First `version = "..."` inside the [package] table.
cargo_version="$(awk '
  /^\[/ { in_pkg = ($0 == "[package]") }
  in_pkg && /^version[[:space:]]*=/ { gsub(/.*=[[:space:]]*"|".*/, ""); print; exit }
' "$cargo_toml")"
[ -n "$cargo_version" ] || { echo "could not read version from $cargo_toml" >&2; exit 2; }

# Top-level version plus every non-OCI packages[].version; a missing field is a
# failure. The MCP registry rejects a `version` on OCI packages: their version
# is the identifier's tag, checked below.
versions="$(jq -r '([.version] + [.packages[]? | select(.registryType != "oci") | .version]) | map(if . == null then "<missing>" else . end) | .[]' "$server_json")"
[ -n "$versions" ] || { echo "no version fields found in $server_json" >&2; exit 2; }
# OCI package identifiers carry the tag (registry/name:tag); it must match too.
tags="$(jq -r '.packages[]? | select(.registryType == "oci") | (.identifier | split(":") | if length > 1 then .[-1] else "<missing>" end)' "$server_json")"

status=0
oci_with_version="$(jq -r '[.packages[]? | select(.registryType == "oci" and has("version"))] | length' "$server_json")"
if [ "$oci_with_version" != 0 ]; then
  echo "server.json OCI packages must not have a version field (the MCP registry rejects it); put the version in the identifier tag" >&2
  status=1
fi
for v in $versions $tags; do
  if [ "$v" != "$cargo_version" ]; then
    echo "server.json version $v != Cargo.toml version $cargo_version" >&2
    status=1
  fi
done

npm_version="$(jq -r '.version // "<missing>"' "$npm_package_json")"
if [ "$npm_version" != "$cargo_version" ]; then
  echo "npm/package.json version $npm_version != Cargo.toml version $cargo_version" >&2
  status=1
fi

[ "$status" -eq 0 ] && echo "version sync ok: $cargo_version"
exit "$status"
