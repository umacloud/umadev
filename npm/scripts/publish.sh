#!/usr/bin/env bash
# Publish the entire npm distribution: seven cli-* packages and the knowledge
# corpus first, then the main `umadev` package last.
#
# Assumes:
#   - `stage.sh` has already populated each `npm/cli-<platform>/bin/`
#     with the matching prebuilt binary and tag/commit provenance manifest.
#   - authentication is explicitly selected by the GitHub tag workflow. The
#     token path is scoped to npm-production and still uses GitHub OIDC solely
#     to generate provenance for every published tarball.
#   - All package.json versions are aligned (this script does NOT bump).
#
# Use `--dry-run` to validate without actually publishing.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
NPM_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
# shellcheck source=publish-registry.sh
source "$SCRIPT_DIR/publish-registry.sh"

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
  "cli-linux-musl-x64"
  "cli-linux-musl-arm64"
  "cli-win32-x64"
)

node "$SCRIPT_DIR/verify-version-lock.mjs"

RELEASE_VERSION="$(node -p "require('$NPM_ROOT/umadev/package.json').version")"
EXPECTED_TAG="v$RELEASE_VERSION"
if [[ "${GITHUB_ACTIONS:-}" == "true" ]]; then
  if [[ "${GITHUB_EVENT_NAME:-}" != "push" \
      || "${GITHUB_REF:-}" != "refs/tags/$EXPECTED_TAG" \
      || "${GITHUB_REF_NAME:-}" != "$EXPECTED_TAG" ]]; then
    echo "publish.sh: production publishing is allowed only for a pushed $EXPECTED_TAG tag" >&2
    echo "            workflow_dispatch is validation-only, even when run against a tag" >&2
    exit 1
  fi
  EXPECTED_COMMIT="${GITHUB_SHA:-}"
elif [[ -n "$DRY_RUN" ]]; then
  EXPECTED_COMMIT="$(git -C "$NPM_ROOT/.." rev-parse HEAD)"
  if ! git -C "$NPM_ROOT/.." tag --points-at "$EXPECTED_COMMIT" | grep -Fxq "$EXPECTED_TAG"; then
    echo "publish.sh: dry-run HEAD is not tagged $EXPECTED_TAG" >&2
    exit 1
  fi
else
  echo "publish.sh: production publishing is restricted to the GitHub tag-push workflow" >&2
  echo "            use --dry-run locally; do not inject an npm token into this script" >&2
  exit 1
fi

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

# A file existing at the right path is not proof that it belongs to that
# package. This exact mistake can make every non-macOS install fail with
# "exec format error" while `npm publish` itself reports success. Inspect the
# executable headers before packing or touching the registry.
node "$SCRIPT_DIR/verify-platform-binaries.mjs" "$NPM_ROOT"

# Header checks prove OS/CPU/libc placement. The release provenance manifests
# independently prove that every exact byte sequence came from this pushed tag
# and commit, and fail closed if any staged binary was replaced after staging.
node "$SCRIPT_DIR/release-provenance.mjs" verify \
  "$NPM_ROOT" "$RELEASE_VERSION" "$EXPECTED_TAG" "$EXPECTED_COMMIT"

# Pack every package before the first publish. This catches missing files and
# freezes the exact tarballs used by every later retry. Pack from temporary
# release roots so every distributed MIT package contains the repository's
# license notice without duplicating that file across nine source directories.
PACK_ROOT="$(mktemp -d)"
trap 'rm -rf "$PACK_ROOT"' EXIT
LICENSE_SOURCE="$NPM_ROOT/../LICENSE"
KNOWLEDGE_SOURCE="$NPM_ROOT/../knowledge"
KNOWLEDGE_STAGE="$PACK_ROOT/knowledge-corpus"
[[ -f "$LICENSE_SOURCE" ]] || {
  echo "publish.sh: repository LICENSE not found" >&2
  exit 1
}
[[ -d "$KNOWLEDGE_SOURCE" ]] || {
  echo "publish.sh: knowledge/ not found" >&2
  exit 1
}
mkdir -p "$KNOWLEDGE_STAGE"
cp "$NPM_ROOT/knowledge-corpus/package.json" "$KNOWLEDGE_STAGE/"
cp -R "$KNOWLEDGE_SOURCE/." "$KNOWLEDGE_STAGE/"
cp "$LICENSE_SOURCE" "$KNOWLEDGE_STAGE/LICENSE"

PACKAGE_DIRS=()
for pkg in "${PLATFORM_PACKAGES[@]}"; do
  stage="$PACK_ROOT/stage-$pkg"
  case "$pkg" in
    cli-win32-*) bin="umadev.exe" ;;
    *)           bin="umadev" ;;
  esac
  mkdir -p "$stage/bin"
  cp "$NPM_ROOT/$pkg/package.json" "$stage/package.json"
  cp "$NPM_ROOT/$pkg/bin/$bin" "$stage/bin/$bin"
  cp "$NPM_ROOT/$pkg/bin/umadev.provenance.json" "$stage/bin/umadev.provenance.json"
  cp "$LICENSE_SOURCE" "$stage/LICENSE"
  PACKAGE_DIRS+=("$stage")
done

MAIN_STAGE="$PACK_ROOT/stage-umadev"
mkdir -p "$MAIN_STAGE"
cp -R "$NPM_ROOT/umadev/." "$MAIN_STAGE/"
cp "$LICENSE_SOURCE" "$MAIN_STAGE/LICENSE"
PACKAGE_DIRS+=("$KNOWLEDGE_STAGE" "$MAIN_STAGE")

TARBALLS=()
for dir in "${PACKAGE_DIRS[@]}"; do
  # The token is present only in the protected publish step. Even though the
  # exact file contract rejects lifecycle scripts, npm must not execute a
  # prepack/prepare payload before that contract gets a chance to inspect it.
  pack_json="$(npm pack "$dir" --ignore-scripts --pack-destination "$PACK_ROOT" --json)"
  filename="$(node -e '
    const fs = require("node:fs");
    const result = JSON.parse(fs.readFileSync(0, "utf8"));
    if (!Array.isArray(result) || !result[0]?.filename) process.exit(1);
    process.stdout.write(result[0].filename);
  ' <<<"$pack_json")"
  tarball="$PACK_ROOT/$filename"
  if ! tar -xOf "$tarball" package/LICENSE >/dev/null; then
    echo "publish.sh: packed tarball is missing package/LICENSE: $filename" >&2
    exit 1
  fi
  if [[ "$(basename "$dir")" == stage-cli-* ]] \
      && ! tar -xOf "$tarball" package/bin/umadev.provenance.json >/dev/null; then
    echo "publish.sh: packed platform tarball is missing binary provenance: $filename" >&2
    exit 1
  fi
  TARBALLS+=("$tarball")
done

# The tarball, not the source directory, is the release boundary. Reject every
# lifecycle script and every unexpected file before the first registry write.
# This is the deterministic guard for the 1.0.74 ChainDrop incident, where an
# otherwise plausible package gained `preinstall`, setup.mjs, and math_init.js.
node "$SCRIPT_DIR/release-package-contract.mjs" "$RELEASE_VERSION" "${TARBALLS[@]}"

integrity_of() {
  node - "$1" <<'NODE'
const crypto = require('node:crypto');
const fs = require('node:fs');
const digest = crypto.createHash('sha512').update(fs.readFileSync(process.argv[2])).digest('base64');
process.stdout.write(`sha512-${digest}`);
NODE
}

remote_integrity() {
  # A publish can be accepted by the registry before every packument replica
  # exposes it. Force revalidation on every poll instead of reading npm's local
  # cache, otherwise a successful first publish can look absent for the whole
  # retry window.
  npm view "$1@$2" dist.integrity --json --prefer-online 2>/dev/null | node -e '
    const fs = require("node:fs");
    try {
      const value = JSON.parse(fs.readFileSync(0, "utf8"));
      if (typeof value === "string") process.stdout.write(value);
    } catch (_) {}
  '
}

remote_version() {
  # --prefer-online forces a registry revalidation so a retry loop re-reads the
  # live dist-tag instead of a cached packument from an earlier (stale) read.
  npm view "$1@$2" version --json --prefer-online 2>/dev/null | node -e '
    const fs = require("node:fs");
    try {
      const value = JSON.parse(fs.readFileSync(0, "utf8"));
      if (typeof value === "string") process.stdout.write(value);
    } catch (_) {}
  '
}

# Validate the complete registry state before publishing one package. In
# particular, an old tag rerun must never move `latest` backwards.
if [[ -z "$DRY_RUN" ]]; then
  # Authentication must be selected explicitly; merely injecting a credential
  # can never switch the release path. Token bytes live only in the environment,
  # while the temporary npmrc contains the literal substitution placeholder.
  case "${UMADEV_NPM_AUTH_MODE:-}" in
    token)
      if [[ -z "${NPM_TOKEN:-}" || -n "${NODE_AUTH_TOKEN:-}" ]]; then
        echo "publish.sh: token mode requires only NPM_TOKEN" >&2
        exit 1
      fi
      if [[ "${NPM_CONFIG_PROVENANCE:-}" != "true" ]]; then
        echo "publish.sh: token mode requires npm provenance" >&2
        exit 1
      fi
      expected_userconfig="${RUNNER_TEMP:-}/umadev-release.npmrc"
      if [[ -z "${RUNNER_TEMP:-}" \
          || "${NPM_CONFIG_USERCONFIG:-}" != "$expected_userconfig" \
          || ! -f "$expected_userconfig" \
          || -L "$expected_userconfig" \
          || "$(stat -c '%a' "$expected_userconfig")" != "600" \
          || "$(wc -l < "$expected_userconfig" | tr -d ' ')" != "1" \
          || "$(cat "$expected_userconfig")" != '//registry.npmjs.org/:_authToken=${NPM_TOKEN}' ]]; then
        echo "publish.sh: token mode requires the scoped npm-production userconfig" >&2
        exit 1
      fi
      PUBLISH_AUTH_LABEL="token authentication with GitHub provenance"
      ;;
    oidc)
      if [[ -n "${NODE_AUTH_TOKEN:-}" || -n "${NPM_TOKEN:-}" ]]; then
        echo "publish.sh: OIDC mode refuses reusable npm credentials" >&2
        exit 1
      fi
      if [[ "${UMADEV_TRUSTED_PUBLISHING:-}" != "1" ]]; then
        echo "publish.sh: OIDC mode requires Trusted Publishing" >&2
        exit 1
      fi
      PUBLISH_AUTH_LABEL="Trusted Publishing"
      ;;
    *)
      echo "publish.sh: select token or oidc authentication explicitly" >&2
      exit 1
      ;;
  esac
  for tarball in "${TARBALLS[@]}"; do
    manifest="$(tar -xOf "$tarball" package/package.json)"
    name="$(node -p 'JSON.parse(process.argv[1]).name' "$manifest")"
    version="$(node -p 'JSON.parse(process.argv[1]).version' "$manifest")"
    local_integrity="$(integrity_of "$tarball")"
    latest="$(remote_version "$name" latest || true)"
    if [[ -n "$latest" ]] && ! node - "$latest" "$version" <<'NODE'
const parse = (value) => {
  const match = /^(\d+)\.(\d+)\.(\d+)/.exec(value);
  if (!match) process.exit(2);
  return match.slice(1).map(Number);
};
const [latest, target] = process.argv.slice(2).map(parse);
for (let i = 0; i < 3; i += 1) {
  if (latest[i] > target[i]) process.exit(1);
  if (latest[i] < target[i]) process.exit(0);
}
NODE
    then
      echo "publish.sh: refusing to move $name latest backwards ($latest -> $version)" >&2
      exit 1
    fi
    existing="$(remote_integrity "$name" "$version" || true)"
    if [[ -n "$existing" && "$existing" != "$local_integrity" ]]; then
      echo "publish.sh: $name@$version exists with different contents" >&2
      exit 1
    fi
  done
fi

for tarball in "${TARBALLS[@]}"; do
  manifest="$(tar -xOf "$tarball" package/package.json)"
  name="$(node -p 'JSON.parse(process.argv[1]).name' "$manifest")"
  version="$(node -p 'JSON.parse(process.argv[1]).version' "$manifest")"
  local_integrity="$(integrity_of "$tarball")"

  if [[ -n "$DRY_RUN" ]]; then
    echo "▶ publish.sh: validated $name@$version ($local_integrity)"
    continue
  fi

  existing="$(remote_integrity "$name" "$version" || true)"
  if [[ -n "$existing" ]]; then
    [[ "$existing" == "$local_integrity" ]] || {
      echo "publish.sh: $name@$version exists with different contents" >&2
      exit 1
    }
    echo "▶ publish.sh: $name@$version already published and identical; skipping"
    continue
  fi

  echo "▶ publish.sh: npm publish $name@$version through $PUBLISH_AUTH_LABEL..."
  publish_status=0
  publish_output="$(npm publish "$tarball" --ignore-scripts --access public --tag latest 2>&1)" || publish_status=$?
  printf '%s\n' "$publish_output"

  if ((publish_status != 0)); then
    # npm can accept a new immutable version, then keep its public packument
    # stale long enough for a retry to attempt the same publish. That retry gets
    # E403 even though its exact tarball is already durable. Only this precise
    # duplicate-version family is recoverable; authentication/authorization and
    # every other publish failure remain immediate hard failures.
    if ! recoverable_duplicate_publish "$publish_output"; then
      echo "publish.sh: npm publish failed for $name@$version" >&2
      exit "$publish_status"
    fi
    echo "▶ publish.sh: registry accepted $name@$version earlier; waiting for public visibility"
  fi

  if ! wait_for_remote_integrity "$name" "$version" "$local_integrity"; then
    echo "publish.sh: registry did not expose the published $name@$version tarball after $VISIBILITY_ATTEMPTS checks" >&2
    exit 1
  fi
done

# Every package is published directly under `latest` in dependency order, with
# `umadev` last. There is no later `npm dist-tag` mutation: publishing the
# launcher last preserves the atomic user-facing boundary and confines the
# temporary registry credential to the publish operations themselves.
if [[ -z "$DRY_RUN" ]]; then
  for tarball in "${TARBALLS[@]}"; do
    manifest="$(tar -xOf "$tarball" package/package.json)"
    name="$(node -p 'JSON.parse(process.argv[1]).name' "$manifest")"
    version="$(node -p 'JSON.parse(process.argv[1]).version' "$manifest")"
    # npm registry reads are eventually consistent. Poll the publish-created tag
    # before moving to the next package; no separately authenticated mutation is
    # allowed here.
    tagged=""
    for ((attempt = 1; attempt <= VISIBILITY_ATTEMPTS; attempt += 1)); do
      tagged="$(remote_version "$name" latest || true)"
      [[ "$tagged" == "$version" ]] && break
      if ((attempt < VISIBILITY_ATTEMPTS)); then
        sleep "$VISIBILITY_DELAY_SECONDS"
      fi
    done
    [[ "$tagged" == "$version" ]] || {
      echo "publish.sh: latest for $name is $tagged, expected $version" >&2
      exit 1
    }
  done
fi

echo "✓ publish.sh: done"
