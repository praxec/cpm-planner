#!/bin/sh
# release-manifest.sh — aggregate a complete release asset matrix into
# checksums.sha256 and a machine-readable release-manifest.json.
#
# Fails unless all six native targets are present, so an incomplete matrix
# can never be published. The manifest records, per target: os, arch, target,
# asset filename, binary name, and digest; plus the source SHA and version.
set -eu

ASSETS_DIR=""; OUT_DIR=""; VERSION=""; SOURCE_SHA=""; BINARY=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --assets-dir) ASSETS_DIR="${2:?--assets-dir needs a path}"; shift 2;;
    --assets-dir=*) ASSETS_DIR="${1#*=}"; shift;;
    --out-dir) OUT_DIR="${2:?--out-dir needs a path}"; shift 2;;
    --out-dir=*) OUT_DIR="${1#*=}"; shift;;
    --version) VERSION="${2:?--version needs a value}"; shift 2;;
    --version=*) VERSION="${1#*=}"; shift;;
    --source-sha) SOURCE_SHA="${2:?--source-sha needs a value}"; shift 2;;
    --source-sha=*) SOURCE_SHA="${1#*=}"; shift;;
    --binary) BINARY="${2:?--binary needs a value}"; shift 2;;
    --binary=*) BINARY="${1#*=}"; shift;;
    -h|--help) echo "usage: release-manifest.sh --assets-dir DIR --version V --source-sha SHA --binary NAME [--out-dir DIR]"; exit 0;;
    *) echo "release-manifest: unknown argument: $1" >&2; exit 2;;
  esac
done

[ -n "$ASSETS_DIR" ] || { echo "release-manifest: --assets-dir is required" >&2; exit 2; }
[ -n "$VERSION" ]    || { echo "release-manifest: --version is required" >&2; exit 2; }
[ -n "$SOURCE_SHA" ] || { echo "release-manifest: --source-sha is required" >&2; exit 2; }
[ -n "$BINARY" ]     || { echo "release-manifest: --binary is required" >&2; exit 2; }
[ -d "$ASSETS_DIR" ] || { echo "release-manifest: assets dir not found: $ASSETS_DIR" >&2; exit 2; }
OUT_DIR="${OUT_DIR:-$ASSETS_DIR}"
mkdir -p "$OUT_DIR"

TARGETS="x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu x86_64-apple-darwin aarch64-apple-darwin x86_64-pc-windows-msvc aarch64-pc-windows-msvc"

os_for()   { case "$1" in *-linux-*) echo linux;; *-apple-darwin) echo darwin;; *-pc-windows-msvc) echo windows;; esac; }
arch_for() { case "$1" in x86_64-*) echo x86_64;; aarch64-*) echo aarch64;; esac; }
ext_for()  { case "$1" in *-pc-windows-msvc) echo zip;; *) echo tar.gz;; esac; }

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | awk '{print $1}'
  else openssl dgst -sha256 "$1" | awk '{print $NF}'; fi
}

checksums_tmp="$OUT_DIR/.checksums.sha256.tmp.$$"
entries_tmp="$OUT_DIR/.manifest-entries.tmp.$$"
: > "$checksums_tmp"
: > "$entries_tmp"

for t in $TARGETS; do
  ext=$(ext_for "$t")
  asset="$BINARY-$t.$ext"
  path="$ASSETS_DIR/$asset"
  if [ ! -f "$path" ]; then
    echo "release-manifest: missing required asset: $asset (complete matrix required)" >&2
    rm -f "$checksums_tmp" "$entries_tmp"
    exit 1
  fi
  digest=$(sha256_of "$path")
  printf '%s  %s\n' "$digest" "$asset" >> "$checksums_tmp"
  printf '%s\t%s\t%s\t%s\tsha256:%s\n' "$(os_for "$t")" "$(arch_for "$t")" "$t" "$asset" "$digest" >> "$entries_tmp"
done

LC_ALL=C sort -k2 "$checksums_tmp" > "$OUT_DIR/checksums.sha256"
rm -f "$checksums_tmp"

{
  printf '{\n'
  printf '  "schemaVersion": 1,\n'
  printf '  "name": "%s",\n' "$BINARY"
  printf '  "version": "%s",\n' "$VERSION"
  printf '  "sourceSha": "%s",\n' "$SOURCE_SHA"
  printf '  "targets": [\n'
  first=1
  while IFS='	' read -r os arch target asset digest; do
    [ -n "$os" ] || continue
    if [ "$first" -eq 1 ]; then first=0; else printf ',\n'; fi
    printf '    { "os": "%s", "arch": "%s", "target": "%s", "asset": "%s", "binary": "%s", "digest": "%s" }' \
      "$os" "$arch" "$target" "$asset" "$BINARY" "$digest"
  done < "$entries_tmp"
  printf '\n  ]\n}\n'
} > "$OUT_DIR/release-manifest.json"
rm -f "$entries_tmp"
echo "release-manifest: wrote checksums.sha256 and release-manifest.json for $VERSION ($SOURCE_SHA)" >&2
