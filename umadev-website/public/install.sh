#!/usr/bin/env bash
# UmaDev native installer — no Node, no npm, no sudo.
#
#   curl -fsSL https://umadev.goder.ai/install.sh | bash
#
# Downloads the official release binary for this platform from GitHub Releases,
# verifies its published SHA-256, and installs it to ~/.local/bin (override with
# UMADEV_INSTALL_DIR). Set UMADEV_VERSION to the release you need; the default
# is the latest release. This is the escape hatch for the npm-global EACCES trap
# (`/usr/local/lib` owned by root): nothing here ever needs elevated permissions.
#
# Every release binary embeds and stages the curated knowledge corpus. The npm
# launcher additionally fetches the optional embedding model on demand. A native
# install starts with BM25 retrieval and can use a manually provisioned local
# model through UMADEV_EMBED_MODEL_DIR; `umadev doctor` reports the active path.

set -euo pipefail

REPO="umacloud/umadev"
BIN_DIR="${UMADEV_INSTALL_DIR:-$HOME/.local/bin}"
DEST="$BIN_DIR/umadev"

tmp=""
stage=""
backup=""
preserve_backup=0
replacement_in_progress=0
install_lock=""
version_probe_pid=""
version_probe_output=""
VERSION_PROBE_RESULT=""

MAX_BINARY_BYTES=536870912
MAX_CHECKSUM_BYTES=4096
MAX_RELEASE_HEADERS_BYTES=65536
CURL_CONNECT_TIMEOUT=20
CURL_BINARY_TIMEOUT=900
CURL_METADATA_TIMEOUT=60
CURL_MAX_REDIRECTS=10
BINARY_VERSION_TIMEOUT=10
BINARY_VERSION_KILL_GRACE=2
MAX_BINARY_VERSION_OUTPUT_BYTES=4096

say() { printf '%s\n' "$*"; }
fail() {
  printf 'install.sh: %s\n' "$*" >&2
  exit 1
}

trusted_release_url() {
  case "$1" in
    "https://github.com/${REPO}/releases/download/"*) return 0 ;;
    https://release-assets.githubusercontent.com/*) return 0 ;;
    https://objects.githubusercontent.com/*) return 0 ;;
    https://github-releases.githubusercontent.com/*) return 0 ;;
    *) return 1 ;;
  esac
}

download_bounded() {
  local url="$1"
  local destination="$2"
  local max_bytes="$3"
  local timeout_seconds="$4"
  local effective=""
  local size=""
  rm -f "$destination"
  effective="$(curl -fL --retry 3 \
    --connect-timeout "$CURL_CONNECT_TIMEOUT" \
    --max-time "$timeout_seconds" \
    --retry-max-time "$timeout_seconds" \
    --max-redirs "$CURL_MAX_REDIRECTS" \
    --max-filesize "$max_bytes" \
    --proto '=https' --proto-redir '=https' \
    --output "$destination" --write-out '%{url_effective}' "$url")" \
    || { rm -f "$destination"; return 1; }
  size="$(wc -c < "$destination" | tr -d '[:space:]')"
  [ "$size" -le "$max_bytes" ] \
    || { rm -f "$destination"; return 1; }
  trusted_release_url "$effective" \
    || { printf 'install.sh: refusing redirect outside official GitHub release hosts: %s\n' "$effective" >&2; rm -f "$destination"; return 1; }
}

cleanup() {
  if [ -n "$version_probe_pid" ]; then
    kill -TERM "$version_probe_pid" 2>/dev/null || true
    kill -KILL "$version_probe_pid" 2>/dev/null || true
  fi
  [ -z "$version_probe_output" ] || rm -f "$version_probe_output"
  [ -z "$tmp" ] || rm -rf "$tmp"
  [ -z "$stage" ] || rm -f "$stage"
  if [ -n "$backup" ] && [ "$preserve_backup" -ne 1 ]; then
    rm -f "$backup"
  fi
  [ -z "$install_lock" ] || rm -rf "$install_lock"
}

acquire_install_lock() {
  lock_path="$BIN_DIR/.umadev-install.lock"
  for _attempt in {1..30}; do
    if mkdir "$lock_path" 2>/dev/null; then
      install_lock="$lock_path"
      printf '%s\n' "$$" > "$install_lock/pid"
      return 0
    fi

    # SIGKILL cannot run EXIT traps. Reclaim only a lock whose recorded owner
    # is definitely gone; an empty/new lock is left alone because its owner may
    # be between mkdir and writing the pid.
    owner_pid="$(cat "$lock_path/pid" 2>/dev/null || true)"
    if printf '%s\n' "$owner_pid" | LC_ALL=C grep -Eq '^[1-9][0-9]*$' \
      && ! kill -0 "$owner_pid" 2>/dev/null; then
      stale_lock="${lock_path}.stale.$$"
      if mv "$lock_path" "$stale_lock" 2>/dev/null; then
        rm -rf "$stale_lock"
        continue
      fi
    fi
    sleep 1
  done
  fail "another UmaDev installer is updating $DEST; wait for it to finish and retry"
}

normalize_version() {
  normalized="${1#v}"
  printf '%s\n' "$normalized" \
    | LC_ALL=C grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$' \
    || return 1
  printf '%s\n' "$normalized"
}

run_version_probe() {
  local binary="$1"
  local status=0
  local version_probe_bytes=0
  local deadline=0
  local grace_deadline=0
  local timed_out=0
  local output_exceeded=0
  local version_probe_group=0
  VERSION_PROBE_RESULT=""
  version_probe_output="$(mktemp "${TMPDIR:-/tmp}/umadev-version.XXXXXX")" \
    || return 1
  # Bound output at the OS file limit as well as checking its final byte count.
  # A corrupt candidate therefore cannot fill the disk while the deadline runs.
  # Bash monitor mode gives this background probe its own process group on
  # both macOS and Linux. The candidate may fork before returning; a timeout,
  # output cap, or successful leader exit must not leave that descendant alive.
  set -m 2>/dev/null || true
  (ulimit -f 8; exec "$binary" --version) >"$version_probe_output" 2>&1 &
  version_probe_pid=$!
  if kill -0 -- "-$version_probe_pid" 2>/dev/null; then
    version_probe_group=1
  fi
  set +m 2>/dev/null || true
  deadline=$((SECONDS + BINARY_VERSION_TIMEOUT))
  while kill -0 "$version_probe_pid" 2>/dev/null; do
    version_probe_bytes="$(wc -c < "$version_probe_output" | tr -d '[:space:]')"
    if [ "$version_probe_bytes" -ge "$MAX_BINARY_VERSION_OUTPUT_BYTES" ]; then
      output_exceeded=1
    elif [ "$SECONDS" -ge "$deadline" ]; then
      timed_out=1
    else
      sleep 0.1
      continue
    fi
    if [ "$version_probe_group" -eq 1 ]; then
      kill -TERM -- "-$version_probe_pid" 2>/dev/null || true
    else
      kill -TERM "$version_probe_pid" 2>/dev/null || true
    fi
    grace_deadline=$((SECONDS + BINARY_VERSION_KILL_GRACE))
    while kill -0 "$version_probe_pid" 2>/dev/null \
      && [ "$SECONDS" -lt "$grace_deadline" ]; do
      sleep 0.1
    done
    if [ "$version_probe_group" -eq 1 ]; then
      kill -KILL -- "-$version_probe_pid" 2>/dev/null || true
    else
      kill -KILL "$version_probe_pid" 2>/dev/null || true
    fi
    break
  done

  wait "$version_probe_pid" || status=$?
  # A clean group leader can still have forked a child that inherited no pipe.
  # Reap the group before accepting the version rather than trusting the leader.
  if [ "$version_probe_group" -eq 1 ]; then
    kill -KILL -- "-$version_probe_pid" 2>/dev/null || true
  fi
  version_probe_pid=""
  VERSION_PROBE_RESULT="$(cat "$version_probe_output")"
  version_probe_bytes="$(wc -c < "$version_probe_output" | tr -d '[:space:]')"
  if [ "$version_probe_bytes" -ge "$MAX_BINARY_VERSION_OUTPUT_BYTES" ]; then
    output_exceeded=1
  fi
  if [ "$output_exceeded" -eq 1 ]; then
    status=125
  elif [ "$timed_out" -eq 1 ]; then
    status=124
  fi
  rm -f "$version_probe_output"
  version_probe_output=""
  return "$status"
}

verify_binary_version() {
  local binary="$1"
  local wanted="$2"
  local phase="$3"
  local probe_status=0
  local output=""
  local reported=""
  local normalized_reported=""

  run_version_probe "$binary" || probe_status=$?
  output="$VERSION_PROBE_RESULT"
  if [ "$probe_status" -eq 124 ]; then
    printf 'install.sh: %s failed: candidate --version timed out after %ss\n' \
      "$phase" "$BINARY_VERSION_TIMEOUT" >&2
    return 1
  fi
  if [ "$probe_status" -eq 125 ]; then
    printf 'install.sh: %s failed: candidate --version output exceeded %s bytes\n' \
      "$phase" "$MAX_BINARY_VERSION_OUTPUT_BYTES" >&2
    return 1
  fi
  if [ "$probe_status" -ne 0 ]; then
    printf 'install.sh: %s failed: candidate did not run successfully: %s\n' \
      "$phase" "${output:-no output}" >&2
    return 1
  fi

  reported="$(printf '%s\n' "$output" | awk '
    NF == 2 && tolower($1) == "umadev" {
      version = $2
      sub(/^[vV]/, "", version)
      print version
    }
  ')"
  if ! normalized_reported="$(normalize_version "$reported")" \
    || [ "$normalized_reported" != "$wanted" ]; then
    printf 'install.sh: %s failed: expected UmaDev %s, got %s\n' \
      "$phase" "$wanted" "${output:-no version output}" >&2
    return 1
  fi
}

rollback_install() {
  if [ -n "$backup" ] && [ -f "$backup" ]; then
    if mv -f "$backup" "$DEST"; then
      backup=""
      return 0
    fi
    preserve_backup=1
    printf 'install.sh: automatic rollback failed; the previous binary is preserved at %s\n' \
      "$backup" >&2
    return 1
  fi

  if rm -f "$DEST"; then
    return 0
  fi
  printf 'install.sh: could not remove the failed first-install binary at %s\n' "$DEST" >&2
  return 1
}

handle_signal() {
  signal_status="$1"
  if [ "$replacement_in_progress" -eq 1 ]; then
    rollback_install || true
    replacement_in_progress=0
  fi
  exit "$signal_status"
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
requested_version="${UMADEV_VERSION:-}"
if [ -n "$requested_version" ]; then
  version="$(normalize_version "$requested_version")" \
    || fail "invalid UMADEV_VERSION: $requested_version"
  base="https://github.com/${REPO}/releases/download/v${version}"
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
trap cleanup EXIT
trap 'handle_signal 129' HUP
trap 'handle_signal 130' INT
trap 'handle_signal 143' TERM

say "Downloading ${asset} ..."
if [ -z "$requested_version" ]; then
  # Following this URL all the way to the asset CDN loses the release tag from
  # url_effective. Resolve only GitHub's first redirect, validate it, then fetch
  # both binary and checksum from that immutable version path.
  latest_headers="$(curl -fsSI --retry 3 \
    --connect-timeout "$CURL_CONNECT_TIMEOUT" \
    --max-time "$CURL_METADATA_TIMEOUT" \
    --retry-max-time "$CURL_METADATA_TIMEOUT" \
    --max-redirs 0 --proto '=https' --proto-redir '=https' \
    "https://github.com/${REPO}/releases/latest/download/$asset")" \
    || fail "could not resolve latest $asset"
  [ "$(printf '%s' "$latest_headers" | wc -c | tr -d '[:space:]')" -le "$MAX_RELEASE_HEADERS_BYTES" ] \
    || fail "latest release metadata exceeded $MAX_RELEASE_HEADERS_BYTES bytes"
  release_url="$(printf '%s\n' "$latest_headers" | awk '
    tolower($1) == "location:" { gsub(/\r/, "", $2); print $2; exit }
  ')"
  case "$release_url" in
    "https://github.com/${REPO}/releases/download/v"*) ;;
    *) fail "GitHub returned an unexpected latest-release redirect: ${release_url:-missing}" ;;
  esac
  release_path="${release_url#*/releases/download/v}"
  release_tag="${release_path%%/*}"
  version="$(normalize_version "$release_tag")" \
    || fail "invalid release version selected by GitHub: $release_tag"
  base="https://github.com/${REPO}/releases/download/v${version}"
fi

download_bounded "$base/$asset" "$tmp/umadev" \
  "$MAX_BINARY_BYTES" "$CURL_BINARY_TIMEOUT" \
  || fail "download failed: $base/$asset"

# Pin the checksum request to the release selected above. This prevents a
# moving `latest` redirect from pairing a binary from one release with the
# checksum of the next release.
download_bounded "$base/$asset.sha256" "$tmp/umadev.sha256" \
  "$MAX_CHECKSUM_BYTES" "$CURL_METADATA_TIMEOUT" \
  || fail "checksum download failed: $base/$asset.sha256"

expected="$(awk 'NF { print tolower($1); exit }' "$tmp/umadev.sha256")"
actual="$(sha_cmd "$tmp/umadev" | tr '[:upper:]' '[:lower:]')"
printf '%s\n' "$expected" | LC_ALL=C grep -Eq '^[0-9a-f]{64}$' \
  || fail "invalid published checksum"
[ "$expected" = "$actual" ] \
  || fail "SHA-256 mismatch (expected $expected, got $actual) — refusing to install"

chmod +x "$tmp/umadev"
verify_binary_version "$tmp/umadev" "$version" "downloaded binary verification" \
  || fail "downloaded binary does not match release v$version — existing installation was not changed"

mkdir -p "$BIN_DIR"
acquire_install_lock
stage="$(mktemp "$BIN_DIR/.umadev-stage.XXXXXX")" \
  || fail "could not create a staging file in $BIN_DIR"
cp "$tmp/umadev" "$stage" || fail "could not stage the verified binary in $BIN_DIR"
chmod +x "$stage"
verify_binary_version "$stage" "$version" "staged binary verification" \
  || fail "staged binary does not match release v$version — existing installation was not changed"

had_existing=0
if [ -e "$DEST" ] || [ -L "$DEST" ]; then
  [ -f "$DEST" ] && [ ! -L "$DEST" ] \
    || fail "refusing to replace non-regular install target: $DEST"
  had_existing=1
  backup="$(mktemp "$BIN_DIR/.umadev-backup.XXXXXX")" \
    || fail "could not reserve a rollback file in $BIN_DIR"
  rm -f "$backup"
  cp -p "$DEST" "$backup" \
    || fail "could not back up the existing binary — it was not changed"
fi

# `stage` is on the same filesystem as DEST, so this rename replaces the path
# atomically on macOS/Linux. The prior bytes remain available in `backup` until
# the installed binary has passed the same exact version check.
replacement_in_progress=1
if ! mv -f "$stage" "$DEST"; then
  replacement_in_progress=0
  fail "could not replace $DEST — the existing binary was not changed"
fi
stage=""

if ! verify_binary_version "$DEST" "$version" "installed binary verification"; then
  if rollback_install; then
    if [ "$had_existing" -eq 1 ]; then
      fail "installed binary verification failed; the previous binary was restored"
    fi
    fail "installed binary verification failed; the incomplete first install was removed"
  fi
  fail "installed binary verification failed and automatic rollback could not complete"
fi
replacement_in_progress=0

if [ -n "$backup" ]; then
  rm -f "$backup"
  backup=""
fi

say "Installed: $DEST (v$version)"

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *)
    say ""
    say "NOTE: $BIN_DIR is not on your PATH. Add it, e.g.:"
    say "  echo 'export PATH=\"$BIN_DIR:\$PATH\"' >> ~/.zshrc && source ~/.zshrc"
    ;;
esac

say "Run 'umadev doctor' to check bases and optional components."
