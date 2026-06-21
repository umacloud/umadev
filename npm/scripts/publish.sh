#!/usr/bin/env bash
# Publish the entire npm distribution: every cli-* platform package
# first (so the main package's optionalDependencies resolve), then the
# main `umadev` package last.
#
# Assumes:
#   - `stage.sh` has already populated each `npm/cli-<platform>/bin/`
#     with the matching prebuilt binary.
#   - `npm whoami` is logged in with publish rights to the `@umadev`
#     scope and to the `umadev` name.
#   - All package.json versions are aligned (this script does NOT bump).
#
# Use `--dry-run` to validate without actually publishing.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
NPM_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

DRY_RUN=""
if [[ "${1:-}" == "--dry-run" ]]; then
  DRY_RUN="--dry-run"
  echo "▶ publish.sh: DRY RUN (nothing will actually publish)"
fi

PLATFORM_PACKAGES=(
  "cli-darwin-arm64"
  "cli-darwin-x64"
  "cli-linux-x64"
  "cli-linux-arm64"
  "cli-win32-x64"
)

# 1) Verify every platform package has its binary staged.
for pkg in "${PLATFORM_PACKAGES[@]}"; do
  case "$pkg" in
    cli-win32-*) bin="umadev.exe" ;;
    *)           bin="umadev" ;;
  esac
  if [[ ! -f "$NPM_ROOT/$pkg/bin/$bin" ]]; then
    echo "publish.sh: missing $NPM_ROOT/$pkg/bin/$bin" >&2
    echo "             run stage.sh for this platform first" >&2
    exit 1
  fi
done

# 2) Publish each platform package (scoped, public access).
for pkg in "${PLATFORM_PACKAGES[@]}"; do
  echo "▶ publish.sh: npm publish $pkg..."
  (cd "$NPM_ROOT/$pkg" && npm publish --access public $DRY_RUN)
done

# 2b) Publish the platform-independent model package first (the main
#     package depends on it, so it must exist on the registry before
#     the main publish below).
if [[ -f "$NPM_ROOT/model-e5-small/model.safetensors" ]]; then
  echo "publish.sh: npm publish model-e5-small..."
  (cd "$NPM_ROOT/model-e5-small" && npm publish --access public $DRY_RUN)
else
  echo "publish.sh: skipping model-e5-small (weights not fetched)" >&2
fi

# 2c) Publish the knowledge corpus package (main depends on it). Stage the
#     repo's knowledge/ tree into it first (CI / ephemeral).
echo "publish.sh: staging + npm publish @umacloud/knowledge..."
if [[ -d "$NPM_ROOT/../knowledge" ]]; then
  cp -R "$NPM_ROOT/../knowledge/." "$NPM_ROOT/knowledge-corpus/"
  (cd "$NPM_ROOT/knowledge-corpus" && npm publish --access public $DRY_RUN)
else
  echo "publish.sh: skipping @umacloud/knowledge (knowledge/ not found)" >&2
fi

# 3) Publish the main package last (so its optionalDependencies resolve
#    against versions that already exist on the registry).
echo "▶ publish.sh: npm publish umadev (main)..."
(cd "$NPM_ROOT/umadev" && npm publish --access public $DRY_RUN)

echo "✓ publish.sh: done"
