#!/usr/bin/env bash
# Stage a prebuilt umadev binary into the matching platform package.
#
# Used by:
#   - Local smoke testing (no need to publish first)
#   - The release pipeline (CI builds N platforms, calls this N times)
#
# Idempotent: re-running with the same args atomically replaces the staged
# executable, then writes a SHA-256 provenance manifest that binds those bytes
# to the package version, release tag, target, and source commit. Do not
# overwrite an executable in place: on macOS a previously executed Mach-O can
# retain vnode/code-signing state and make the next launch stall, while Windows
# commonly refuses an in-place write to a mapped image.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
NPM_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

usage() {
  cat <<'USAGE'
Usage: stage.sh <platform> <binary-path>

  platform     darwin-arm64 | darwin-x64 | linux-x64 | linux-arm64 |
               linux-musl-x64 | linux-musl-arm64 | win32-x64
  binary-path  path to the prebuilt umadev[.exe] binary

Environment:
  UMADEV_RELEASE_TAG       v<package-version> in a tagged release; otherwise local
  UMADEV_RELEASE_COMMIT    full Git object id (defaults to GITHUB_SHA or HEAD)

Examples:
  stage.sh darwin-arm64 target/release/umadev
  stage.sh win32-x64    target/x86_64-pc-windows-msvc/release/umadev.exe
USAGE
  exit 1
}

[[ $# -eq 2 ]] || usage
PLATFORM="$1"
BINARY="$2"

[[ -f "$BINARY" ]] || { echo "stage.sh: binary not found: $BINARY" >&2; exit 1; }

case "$PLATFORM" in
  darwin-arm64|darwin-x64|linux-x64|linux-arm64|linux-musl-x64|linux-musl-arm64)
    BIN_NAME="umadev"
    ;;
  win32-x64)
    BIN_NAME="umadev.exe"
    ;;
  *)
    echo "stage.sh: unsupported platform: $PLATFORM" >&2
    exit 1
    ;;
esac

DEST_DIR="$NPM_ROOT/cli-$PLATFORM/bin"
[[ -d "$NPM_ROOT/cli-$PLATFORM" ]] || {
  echo "stage.sh: no sub-package npm/cli-$PLATFORM/ (typo?)" >&2
  exit 1
}
mkdir -p "$DEST_DIR"
STAGED_TMP="$DEST_DIR/.${BIN_NAME}.tmp.$$"
trap 'rm -f "$STAGED_TMP"' EXIT
cp "$BINARY" "$STAGED_TMP"
chmod +x "$STAGED_TMP"
mv -f "$STAGED_TMP" "$DEST_DIR/$BIN_NAME"
trap - EXIT

RELEASE_TAG="${UMADEV_RELEASE_TAG:-local}"
RELEASE_COMMIT="${UMADEV_RELEASE_COMMIT:-${GITHUB_SHA:-}}"
if [[ -z "$RELEASE_COMMIT" ]]; then
  RELEASE_COMMIT="$(git -C "$NPM_ROOT/.." rev-parse HEAD 2>/dev/null || true)"
fi
[[ -n "$RELEASE_COMMIT" ]] || {
  echo "stage.sh: cannot determine the source commit for the staged binary" >&2
  exit 1
}
node "$SCRIPT_DIR/release-provenance.mjs" write \
  "$NPM_ROOT" "$PLATFORM" "$RELEASE_TAG" "$RELEASE_COMMIT"

echo "stage.sh: $BINARY → $DEST_DIR/$BIN_NAME"
