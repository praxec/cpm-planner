#!/bin/sh
# install.sh — download, verify, and install the cpm-planner MCP server.
#
# Downloads a prebuilt binary from the praxec/cpm-planner GitHub release, verifies its
# SHA-256 against the release's checksums.sha256, extracts it safely, and
# atomically installs it into a user-local managed directory. No Rust, Cargo,
# or Git is required on the client.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/praxec/cpm-planner/main/scripts/install.sh | sh
#   ./install.sh --version vX.Y.Z --install-dir "$HOME/.local/bin"
#   ./install.sh --print-target
#
# Options:
#   --version TAG        Release tag to install (default: latest)
#   --install-dir DIR    Managed destination directory (default: $HOME/.local/bin)
#   --base-url URL       Release base URL (default: https://github.com/praxec/cpm-planner/releases)
#   --print-target       Print the resolved target triple and exit (no network)
#   --dry-run            Print resolved URLs and exit (no install)
#   -h, --help           Show this help
#
# Environment overrides (mirrors, air-gapped hosts, and tests):
#   PRAXEC_REPO PRAXEC_BIN PRAXEC_VERSION PRAXEC_INSTALL_DIR PRAXEC_BASE_URL
#   PRAXEC_OS PRAXEC_ARCH PRAXEC_MAX_BYTES
#
# This installer only ever writes the binary into --install-dir. Application
# state and configuration live elsewhere (see the README) and are never touched.
set -eu

DEFAULT_REPO="praxec/cpm-planner"
DEFAULT_BIN="cpm-planner"

REPO="${PRAXEC_REPO:-$DEFAULT_REPO}"
BIN="${PRAXEC_BIN:-$DEFAULT_BIN}"
VERSION="${PRAXEC_VERSION:-latest}"
INSTALL_DIR="${PRAXEC_INSTALL_DIR:-${HOME:-/tmp}/.local/bin}"
BASE_URL="${PRAXEC_BASE_URL:-https://github.com/$REPO/releases}"
MAX_BYTES="${PRAXEC_MAX_BYTES:-134217728}"
PRINT_TARGET=0
DRY_RUN=0

say() { printf 'install: %s\n' "$*" >&2; }
die() { printf 'install: error: %s\n' "$*" >&2; exit 1; }

usage() {
  cat <<'USAGE'
usage: install.sh [--version TAG] [--install-dir DIR] [--base-url URL]
                  [--print-target] [--dry-run]
USAGE
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --version) VERSION="${2:?--version needs a tag}"; shift 2;;
    --version=*) VERSION="${1#*=}"; shift;;
    --install-dir) INSTALL_DIR="${2:?--install-dir needs a path}"; shift 2;;
    --install-dir=*) INSTALL_DIR="${1#*=}"; shift;;
    --base-url) BASE_URL="${2:?--base-url needs a URL}"; shift 2;;
    --base-url=*) BASE_URL="${1#*=}"; shift;;
    --print-target) PRINT_TARGET=1; shift;;
    --dry-run) DRY_RUN=1; shift;;
    -h|--help) usage; exit 0;;
    *) die "unknown argument: $1";;
  esac
done

detect_os() {
  if [ -n "${PRAXEC_OS:-}" ]; then printf '%s' "$PRAXEC_OS"; return 0; fi
  _s=$(uname -s 2>/dev/null || echo unknown)
  case "$_s" in
    Linux) printf 'linux';;
    Darwin) printf 'darwin';;
    *) die "unsupported operating system '$_s'; install.sh supports Linux and macOS. On Windows use install.ps1. No source-build fallback is attempted.";;
  esac
}

detect_arch() {
  if [ -n "${PRAXEC_ARCH:-}" ]; then printf '%s' "$PRAXEC_ARCH"; return 0; fi
  _m=$(uname -m 2>/dev/null || echo unknown)
  case "$(detect_os)" in
    darwin)
      # Rosetta reports x86_64; prefer the native Apple Silicon arch.
      if [ "$_m" = "x86_64" ]; then
        _translated=$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)
        _native_arm=$(sysctl -n hw.optional.arm64 2>/dev/null || echo 0)
        if [ "$_translated" = "1" ] || [ "$_native_arm" = "1" ]; then _m=arm64; fi
      fi
      ;;
  esac
  case "$_m" in
    x86_64|amd64|AMD64) printf 'x86_64';;
    arm64|aarch64|ARM64) printf 'aarch64';;
    *) die "unsupported CPU architecture '$_m'; no prebuilt binary is published for it. No source-build fallback is attempted.";;
  esac
}

target_triple() {
  case "$1-$2" in
    linux-x86_64) printf 'x86_64-unknown-linux-gnu';;
    linux-aarch64) printf 'aarch64-unknown-linux-gnu';;
    darwin-x86_64) printf 'x86_64-apple-darwin';;
    darwin-aarch64) printf 'aarch64-apple-darwin';;
    *) die "unsupported platform $1/$2";;
  esac
}

asset_ext() {
  case "$1" in
    linux|darwin) printf 'tar.gz';;
    *) die "unsupported operating system '$1'";;
  esac
}

OS=$(detect_os)
ARCH=$(detect_arch)
TARGET=$(target_triple "$OS" "$ARCH")
EXT=$(asset_ext "$OS")
ASSET="${BIN}-${TARGET}.${EXT}"
if [ "$VERSION" = "latest" ]; then
  RELEASE_URL="${BASE_URL}/latest/download"
else
  RELEASE_URL="${BASE_URL}/download/${VERSION}"
fi
DOWNLOAD_URL="${RELEASE_URL}/${ASSET}"
CHECKSUM_URL="${RELEASE_URL}/checksums.sha256"

if [ "$PRINT_TARGET" -eq 1 ]; then
  printf '%s\n' "$TARGET"
  exit 0
fi

if [ "$DRY_RUN" -eq 1 ]; then
  say "platform: $OS/$ARCH -> $TARGET"
  say "asset:    $DOWNLOAD_URL"
  say "checksum: $CHECKSUM_URL"
  exit 0
fi

command -v curl >/dev/null 2>&1 || die "curl is required to download release assets"
command -v tar >/dev/null 2>&1 || die "tar is required to extract release assets"

work="$(mktemp -d "${TMPDIR:-/tmp}/cpm-planner.install.XXXXXX")"
cleanup() { rm -rf "$work"; }
trap cleanup EXIT HUP INT TERM

bounded_download() {
  _url="$1"; _dest="$2"
  say "downloading $_url"
  curl -fsSL --retry 3 --retry-delay 1 --max-time 180 --max-filesize "$MAX_BYTES" -o "$_dest" "$_url" \
    || die "download failed: $_url"
  _size=$(wc -c < "$_dest" | tr -d ' ')
  [ "$_size" -gt 0 ] || die "downloaded asset is empty: $_url"
  [ "$_size" -le "$MAX_BYTES" ] || die "downloaded asset exceeds ${MAX_BYTES} bytes: $_url"
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | awk '{print $1}'
  elif command -v openssl >/dev/null 2>&1; then openssl dgst -sha256 "$1" | awk '{print $NF}'
  else die "no SHA-256 tool found (need sha256sum, shasum, or openssl)"; fi
}

bounded_download "$DOWNLOAD_URL" "$work/$ASSET"
bounded_download "$CHECKSUM_URL" "$work/checksums.sha256"

expected=$(awk -v a="$ASSET" '$2 == a || $2 == "*" a { print $1; exit }' "$work/checksums.sha256")
[ -n "$expected" ] || die "no checksum entry for $ASSET in checksums.sha256"
actual=$(sha256_of "$work/$ASSET")
[ "$actual" = "$expected" ] || die "checksum mismatch for $ASSET (expected $expected, got $actual)"
say "checksum verified: $actual"

# Reject unsafe archive layouts before extracting anything.
tar -tzf "$work/$ASSET" > "$work/list" || die "not a readable .tar.gz archive: $ASSET"
[ -s "$work/list" ] || die "archive is empty: $ASSET"
while IFS= read -r entry; do
  case "$entry" in
    /*) die "refusing absolute path in archive: $entry";;
    ..|../*|*/..|*/../*) die "refusing parent-directory path in archive: $entry";;
  esac
done < "$work/list"
if tar -tvzf "$work/$ASSET" | awk '{ print substr($1,1,1) }' | grep -qE '^[lhbcp]'; then
  die "refusing archive containing links or special files: $ASSET"
fi

mkdir -p "$work/extract"
tar -xzf "$work/$ASSET" -C "$work/extract" --no-same-owner 2>/dev/null \
  || tar -xzf "$work/$ASSET" -C "$work/extract"

src=$(find "$work/extract" -type f -name "$BIN" -print | head -n 1)
[ -n "$src" ] || die "binary '$BIN' not found inside $ASSET"
[ ! -L "$src" ] || die "refusing to install a symlink"

mkdir -p "$INSTALL_DIR" || die "cannot create install dir: $INSTALL_DIR"
dest="$INSTALL_DIR/$BIN"
tmpdest="$INSTALL_DIR/.$BIN.tmp.$$"
cp "$src" "$tmpdest"
chmod 0755 "$tmpdest"
mv -f "$tmpdest" "$dest"
say "installed $BIN $VERSION -> $dest"
