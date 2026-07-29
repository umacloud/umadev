#!/usr/bin/env bash
# UmaDev native installer — no Node, no npm, no sudo.
#
#   curl -fsSL https://umadev.goder.ai/install.sh | bash
#
# Downloads the official release binary for this platform from GitHub Releases,
# verifies its published SHA-256, and installs it to ~/.local/bin (override with
# UMADEV_INSTALL_DIR). Pin a version with UMADEV_VERSION=1.0.68; the default is
# the latest release. This is the escape hatch for the npm-global EACCES trap
# (`/usr/local/lib` owned by root): nothing here ever needs elevated permissions.
#
# The npm install remains the batteries-included path (bundled knowledge corpus
# + on-demand embedding model). The native binary degrades gracefully without
# them — `umadev doctor` reports anything missing.

set -euo pipefail

REPO="umacloud/umadev"
BIN_DIR="${UMADEV_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf '%s\n' "$*"; }
fail() {
  printf 'install.sh: %s\n' "$*" >&2
  exit 1
}

os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Darwin)
    case "$arch" in
      arm64 | aarch64) target="aarch64-apple-darwin" ;;
      x86_64) target="x86_64-apple-darwin" ;;
      *) fail "unsupported macOS architecture: $arch" ;;
    esac
    ;;
  Linux)
    # musl builds are self-contained — they run on any distro regardless of the
    # installed glibc, which is exactly what a generic installer needs.
    case "$arch" in
      x86_64 | amd64) target="x86_64-unknown-linux-musl" ;;
      aarch64 | arm64) target="aarch64-unknown-linux-musl" ;;
      *) fail "unsupported Linux architecture: $arch" ;;
    esac
    ;;
  *)
    fail "unsupported OS: $os (Windows: irm https://umadev.goder.ai/install.ps1 | iex)"
    ;;
esac

asset="umadev-${target}"
if [ -n "${UMADEV_VERSION:-}" ]; then
  base="https://github.com/${REPO}/releases/download/v${UMADEV_VERSION}"
else
  base="https://github.com/${REPO}/releases/latest/download"
fi

command -v curl >/dev/null 2>&1 || fail "curl is required"
if command -v sha256sum >/dev/null 2>&1; then
  sha_cmd() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
  sha_cmd() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
  fail "sha256sum or shasum is required to verify the download"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

say "Downloading ${asset} ..."
curl -fL --retry 3 --proto '=https' -o "$tmp/umadev" "$base/$asset" \
  || fail "download failed: $base/$asset"
curl -fL --retry 3 --proto '=https' -o "$tmp/umadev.sha256" "$base/$asset.sha256" \
  || fail "checksum download failed: $base/$asset.sha256"

expected="$(cut -d' ' -f1 <"$tmp/umadev.sha256" | tr -d '[:space:]')"
actual="$(sha_cmd "$tmp/umadev")"
[ -n "$expected" ] || fail "empty published checksum"
[ "$expected" = "$actual" ] \
  || fail "SHA-256 mismatch (expected $expected, got $actual) — refusing to install"

mkdir -p "$BIN_DIR"
chmod +x "$tmp/umadev"
mv -f "$tmp/umadev" "$BIN_DIR/umadev"

say "Installed: $BIN_DIR/umadev"
"$BIN_DIR/umadev" --version || true

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *)
    say ""
    say "NOTE: $BIN_DIR is not on your PATH. Add it, e.g.:"
    say "  echo 'export PATH=\"$BIN_DIR:\$PATH\"' >> ~/.zshrc && source ~/.zshrc"
    ;;
esac

say "Run 'umadev doctor' to check bases and optional components."
