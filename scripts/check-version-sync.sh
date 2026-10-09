#!/usr/bin/env bash
# Fail when any version field in server.json differs from Cargo.toml's package version.
# Usage: scripts/check-version-sync.sh [cargo_toml] [server_json]
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cargo_toml="${1:-$root/Cargo.toml}"
server_json="${2:-$root/server.json}"

# First `version = "..."` inside the [package] table.
cargo_version="$(awk '
  /^\[/ { in_pkg = ($0 == "[package]") }
  in_pkg && /^version[[:space:]]*=/ { gsub(/.*=[[:space:]]*"|".*/, ""); print; exit }
' "$cargo_toml")"
[ -n "$cargo_version" ] || { echo "could not read version from $cargo_toml" >&2; exit 2; }

# Top-level version plus every packages[].version.
versions="$(jq -r '[.version] + [.packages[]? | .version] | .[] // empty' "$server_json")"
[ -n "$versions" ] || { echo "no version fields found in $server_json" >&2; exit 2; }

status=0
while IFS= read -r v; do
  if [ "$v" != "$cargo_version" ]; then
    echo "server.json version $v != Cargo.toml version $cargo_version" >&2
    status=1
  fi
done <<<"$versions"

[ "$status" -eq 0 ] && echo "version sync ok: $cargo_version"
exit "$status"
