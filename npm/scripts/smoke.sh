#!/usr/bin/env bash
# Local smoke test for the npm distribution layer.
#
# Builds umadev for the current platform, stages it into the right
# `cli-*` sub-package, then invokes the JS shim and verifies it execs
# the real binary by checking `--version` output.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
NPM_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_ROOT="$(cd "$NPM_ROOT/.." && pwd)"

# Map uname → our platform key.
case "$(uname -s)-$(uname -m)" in
  Darwin-arm64)   PLATFORM="darwin-arm64" ;;
  Darwin-x86_64)  PLATFORM="darwin-x64" ;;
  Linux-x86_64)   PLATFORM="linux-x64" ;;
  Linux-aarch64)  PLATFORM="linux-arm64" ;;
  *)
    echo "smoke.sh: unsupported uname: $(uname -s)-$(uname -m)" >&2
    exit 1
    ;;
esac

echo "▶ smoke.sh: building umadev (release) for $PLATFORM..."
(cd "$REPO_ROOT" && cargo build --release --bin umadev --quiet)

echo "▶ smoke.sh: staging into npm/cli-$PLATFORM/"
"$SCRIPT_DIR/stage.sh" "$PLATFORM" "$REPO_ROOT/target/release/umadev"

# Cargo pkgid output varies across cargo versions:
#   `umadev@4.2.0`                       (newer cargo)
#   `path+file://.../umadev#4.2.0`       (older cargo)
# Both forms put the version after the last `#` or `@` — strip everything before it.
EXPECTED_VERSION="$(cd "$REPO_ROOT" && cargo pkgid -p umadev | sed -E 's/.*[#@]//' | tail -n1)"

echo "▶ smoke.sh: invoking JS shim → $(node --version)"
OUTPUT="$(node "$NPM_ROOT/umadev/bin/cli.js" --version 2>&1)"

if [[ "$OUTPUT" == *"$EXPECTED_VERSION"* ]]; then
  echo "✓ smoke.sh: shim resolved + ran binary OK"
  echo "  expected: umadev $EXPECTED_VERSION"
  echo "  got:      $OUTPUT"
else
  echo "✗ smoke.sh: version mismatch" >&2
  echo "  expected to contain: $EXPECTED_VERSION" >&2
  echo "  got:                 $OUTPUT" >&2
  exit 1
fi

# ── `umadev update` must be handled by the SHIM, never by the binary.
#
# The binary lives inside the very npm tree npm is about to replace. If the
# binary is the one running `npm install -g`, Windows refuses to unlink the
# running image: npm installs the new version but cannot delete the tree it
# renamed aside, so it leaves it behind (`node_modules/.umadev-<rand>/`) and
# prints an EPERM wall that reads like a failed upgrade. Handling `update` in
# the shim means nothing under the prefix is open when npm swaps the tree.
#
# This checks the two properties that keep that true: the shim must NOT exec
# the binary for `update`, and it must sweep the staging dirs a previous broken
# upgrade left behind.
echo "▶ smoke.sh: checking the update path stays in the shim"
UPD_TMP="$(mktemp -d)"
trap 'rm -rf "$UPD_TMP"' EXIT

INSTALLED_VERSION="$(node -p "require('$NPM_ROOT/umadev/package.json').version")"

# Point the registry probe at a closed port so it fails INSTANTLY and the
# already-latest check falls back to the stand-in `npm view` on PATH. That keeps
# every assertion below fully offline AND exercises the real fallback chain.
DEAD_REGISTRY="https://127.0.0.1:1"

# Materialize a fake global install of umadev rooted at $1 (which must be a
# `node_modules` dir — that is what marks an install as package-manager-owned).
make_install() {
  mkdir -p "$1/umadev/bin" "$1/@umacloud/cli-$PLATFORM/bin"
  cp "$NPM_ROOT/umadev/bin/cli.js" "$1/umadev/bin/"
  cp "$NPM_ROOT/umadev/package.json" "$1/umadev/"
  cp "$NPM_ROOT/cli-$PLATFORM/package.json" "$1/@umacloud/cli-$PLATFORM/"
  cp "$NPM_ROOT/cli-$PLATFORM/bin/umadev" "$1/@umacloud/cli-$PLATFORM/bin/"
}

# A stand-in package manager on PATH. It answers `--version` (so the shim sees it
# as runnable), answers `npm view umadev version` with $3 (so the already-latest
# check never touches the network), and otherwise just records what it was told to
# run — a real global install is never performed.
make_stub() {
  cat > "$UPD_TMP/bin/$1" <<STUB
#!/bin/sh
case "\$1" in
  --version) echo "9.9.9"; exit 0 ;;
  view)      echo "$2"; exit 0 ;;
esac
if [ -n "\${SMOKE_INSTALL_ROOT:-}" ]; then
  node -e '
    const fs = require("node:fs");
    const path = require("node:path");
    for (const dir of [process.argv[1], process.argv[2]]) {
      const file = path.join(dir, "package.json");
      const pkg = JSON.parse(fs.readFileSync(file, "utf8"));
      pkg.version = process.argv[3];
      fs.writeFileSync(file, JSON.stringify(pkg, null, 2) + "\\n");
    }
  ' "\$SMOKE_INSTALL_ROOT/umadev" "\$SMOKE_INSTALL_ROOT/@umacloud/cli-$PLATFORM" "$2"
  printf '%s\n' '#!/bin/sh' 'echo "umadev $2"' > \
    "\$SMOKE_INSTALL_ROOT/@umacloud/cli-$PLATFORM/bin/umadev"
  chmod +x "\$SMOKE_INSTALL_ROOT/@umacloud/cli-$PLATFORM/bin/umadev"
fi
echo "$1-called: \$*"
STUB
  chmod +x "$UPD_TMP/bin/$1"
}

mkdir -p "$UPD_TMP/bin"
make_stub npm  "999.0.0"
make_stub pnpm "999.0.0"
make_stub bun  "999.0.0"

# ── 1. An npm-owned install upgrades via npm, in the shim, sweeping npm's debris.
make_install "$UPD_TMP/node_modules"
# The debris a previous EPERM'd upgrade leaves in an npm prefix.
mkdir -p "$UPD_TMP/node_modules/.umadev-vv1jMlhy" \
         "$UPD_TMP/node_modules/@umacloud/.cli-win32-x64-AbC123"

# The shim finds no platform sub-package under this fake prefix, so if it ever
# tried to exec the binary it would say so — which is exactly what we assert
# against. A `y` on stdin carries it past the confirmation.
UPD_OUT="$(cd "$UPD_TMP" && echo y | PATH="$UPD_TMP/bin:$PATH" \
  SMOKE_INSTALL_ROOT="$UPD_TMP/node_modules" \
  UMADEV_REGISTRY_URL="$DEAD_REGISTRY" \
  node "$UPD_TMP/node_modules/umadev/bin/cli.js" update 2>&1)"

if [[ "$UPD_OUT" != *"npm-called: install -g umadev@latest"* ]]; then
  echo "✗ smoke.sh: the shim did not run the npm upgrade" >&2
  echo "$UPD_OUT" >&2
  exit 1
fi
if [[ "$UPD_OUT" == *"not installed"* || "$UPD_OUT" == *"failed to exec binary"* ]]; then
  echo "✗ smoke.sh: the shim tried to launch the binary to update it" >&2
  echo "$UPD_OUT" >&2
  exit 1
fi
if [[ -d "$UPD_TMP/node_modules/.umadev-vv1jMlhy" ]] ||
   [[ -d "$UPD_TMP/node_modules/@umacloud/.cli-win32-x64-AbC123" ]]; then
  echo "✗ smoke.sh: abandoned npm staging dirs were not swept" >&2
  exit 1
fi
if [[ "$UPD_OUT" != *"upgraded and verified"* ]]; then
  echo "✗ smoke.sh: the shim did not verify the upgraded executable" >&2
  echo "$UPD_OUT" >&2
  exit 1
fi
echo "✓ smoke.sh: update ran in the shim, executable verified, debris swept"

# ── 2. A closed stdin must read as "no" — a scripted caller that never answered
# must not be upgraded behind its back.
UPD_OUT="$(cd "$UPD_TMP" && PATH="$UPD_TMP/bin:$PATH" \
  SMOKE_INSTALL_ROOT="$UPD_TMP/node_modules" \
  UMADEV_REGISTRY_URL="$DEAD_REGISTRY" \
  node "$UPD_TMP/node_modules/umadev/bin/cli.js" update < /dev/null 2>&1)"
if [[ "$UPD_OUT" == *"npm-called: install"* ]]; then
  echo "✗ smoke.sh: update proceeded without an answer" >&2
  exit 1
fi
echo "✓ smoke.sh: an unanswered update aborts"

# ── 3. A pnpm-owned install must upgrade with PNPM, not npm. Running `npm i -g` on
# a pnpm install does not replace it — it drops a SECOND copy in npm's prefix and
# whichever bin dir wins PATH decides which binary actually runs.
PNPM_ROOT="$UPD_TMP/pnpm/global/5/node_modules"
make_install "$PNPM_ROOT"
UPD_OUT="$(cd "$UPD_TMP" && PATH="$UPD_TMP/bin:$PATH" \
  SMOKE_INSTALL_ROOT="$PNPM_ROOT" \
  UMADEV_REGISTRY_URL="$DEAD_REGISTRY" \
  node "$PNPM_ROOT/umadev/bin/cli.js" update -y 2>&1)"
if [[ "$UPD_OUT" != *"pnpm-called: add -g umadev@latest"* ]]; then
  echo "✗ smoke.sh: a pnpm-owned install did not upgrade via pnpm" >&2
  echo "$UPD_OUT" >&2
  exit 1
fi
if [[ "$UPD_OUT" == *"npm-called: install"* ]]; then
  echo "✗ smoke.sh: a pnpm-owned install shelled out to npm anyway" >&2
  echo "$UPD_OUT" >&2
  exit 1
fi
echo "✓ smoke.sh: a pnpm-owned install upgrades via pnpm"

# ── 4. Same for a bun-owned install ($BUN_INSTALL/install/global).
BUN_ROOT="$UPD_TMP/.bun/install/global/node_modules"
make_install "$BUN_ROOT"
UPD_OUT="$(cd "$UPD_TMP" && PATH="$UPD_TMP/bin:$PATH" \
  SMOKE_INSTALL_ROOT="$BUN_ROOT" \
  UMADEV_REGISTRY_URL="$DEAD_REGISTRY" \
  node "$BUN_ROOT/umadev/bin/cli.js" update -y 2>&1)"
if [[ "$UPD_OUT" != *"bun-called: add -g umadev@latest"* ]]; then
  echo "✗ smoke.sh: a bun-owned install did not upgrade via bun" >&2
  echo "$UPD_OUT" >&2
  exit 1
fi
if [[ "$UPD_OUT" == *"npm-called: install"* ]]; then
  echo "✗ smoke.sh: a bun-owned install shelled out to npm anyway" >&2
  echo "$UPD_OUT" >&2
  exit 1
fi
echo "✓ smoke.sh: a bun-owned install upgrades via bun"

# ── 5. Already on the latest version: say so and reinstall NOTHING.
make_install "$UPD_TMP/node_modules"
mkdir -p "$UPD_TMP/latest-bin"
cat > "$UPD_TMP/latest-bin/npm" <<STUB
#!/bin/sh
case "\$1" in
  --version) echo "9.9.9"; exit 0 ;;
  view)      echo "$INSTALLED_VERSION"; exit 0 ;;
esac
echo "npm-called: \$*"
STUB
chmod +x "$UPD_TMP/latest-bin/npm"
UPD_OUT="$(cd "$UPD_TMP" && PATH="$UPD_TMP/latest-bin:$PATH" \
  UMADEV_REGISTRY_URL="$DEAD_REGISTRY" \
  node "$UPD_TMP/node_modules/umadev/bin/cli.js" update -y 2>&1)"
if [[ "$UPD_OUT" != *"Already on the latest version"* ]]; then
  echo "✗ smoke.sh: an up-to-date install did not short-circuit" >&2
  echo "$UPD_OUT" >&2
  exit 1
fi
if [[ "$UPD_OUT" == *"npm-called: install"* ]]; then
  echo "✗ smoke.sh: an up-to-date install reinstalled itself anyway" >&2
  echo "$UPD_OUT" >&2
  exit 1
fi
echo "✓ smoke.sh: an already-latest install short-circuits"

# ── 6. npm may nest replacement optional dependencies under the main package
# during a global upgrade. That matching copy must win over a stale hoisted one.
NESTED_ROOT="$UPD_TMP/nested/node_modules"
make_install "$NESTED_ROOT"
mkdir -p "$NESTED_ROOT/umadev/node_modules/@umacloud/cli-$PLATFORM/bin"
cp "$NPM_ROOT/cli-$PLATFORM/package.json" \
  "$NESTED_ROOT/umadev/node_modules/@umacloud/cli-$PLATFORM/"
cp "$NPM_ROOT/cli-$PLATFORM/bin/umadev" \
  "$NESTED_ROOT/umadev/node_modules/@umacloud/cli-$PLATFORM/bin/umadev"
cat > "$NESTED_ROOT/@umacloud/cli-$PLATFORM/bin/umadev" <<STUB
#!/bin/sh
echo "umadev 0.0.1"
STUB
chmod +x "$NESTED_ROOT/@umacloud/cli-$PLATFORM/bin/umadev" \
  "$NESTED_ROOT/umadev/node_modules/@umacloud/cli-$PLATFORM/bin/umadev"
node -e '
  const path = require("node:path");
  const shim = require(process.argv[1]);
  const pkgRoot = process.argv[2];
  const resolved = shim.resolveInstalledBinary(pkgRoot);
  const expected = path.join(pkgRoot, "node_modules");
  if (!resolved || !resolved.startsWith(expected)) {
    console.error(`nested platform package was not preferred: ${resolved}`);
    process.exit(1);
  }
  if (!shim.versionStateMatches(shim.installedVersionState(pkgRoot), process.argv[3])) {
    console.error("nested platform package did not form a consistent version state");
    process.exit(1);
  }
' "$NPM_ROOT/umadev/bin/cli.js" "$NESTED_ROOT/umadev" "$INSTALLED_VERSION"
echo "✓ smoke.sh: a nested replacement platform package is resolved first"

# ── 7. The exact reported split: package.json is current but the optional
# platform executable is stale. This MUST repair, never short-circuit on the
# main package and never print success before executing the replacement binary.
cat > "$UPD_TMP/node_modules/@umacloud/cli-$PLATFORM/bin/umadev" <<STUB
#!/bin/sh
echo "umadev 0.0.1"
STUB
chmod +x "$UPD_TMP/node_modules/@umacloud/cli-$PLATFORM/bin/umadev"
cat > "$UPD_TMP/latest-bin/npm" <<STUB
#!/bin/sh
case "\$1" in
  --version) echo "9.9.9"; exit 0 ;;
  view)      echo "$INSTALLED_VERSION"; exit 0 ;;
esac
cp "$NPM_ROOT/cli-$PLATFORM/bin/umadev" \
  "$UPD_TMP/node_modules/@umacloud/cli-$PLATFORM/bin/umadev"
echo "npm-called: \$*"
STUB
chmod +x "$UPD_TMP/latest-bin/npm"
UPD_OUT="$(cd "$UPD_TMP" && PATH="$UPD_TMP/latest-bin:$PATH" \
  UMADEV_REGISTRY_URL="$DEAD_REGISTRY" \
  node "$UPD_TMP/node_modules/umadev/bin/cli.js" update -y 2>&1)"
if [[ "$UPD_OUT" == *"Nothing to do"* ]] ||
   [[ "$UPD_OUT" != *"Version split detected"* ]] ||
   [[ "$UPD_OUT" != *"upgraded and verified"* ]]; then
  echo "✗ smoke.sh: a current package with a stale executable was not repaired" >&2
  echo "$UPD_OUT" >&2
  exit 1
fi
echo "✓ smoke.sh: a package/executable version split is detected and repaired"

# ── 8. A ROOT-OWNED install (`sudo npm i -g`) must be REFUSED with the repair, not
# handed to a package manager that dies with EACCES half-way through and aborts the
# whole global transaction (taking the user's other global packages with it).
#
# A root-owned dir cannot be fabricated without root, so we use a real one: /tmp is
# uid 0 on macOS and Linux. Symlinking it in as the `node_modules` component gives
# the shim a package root that IS genuinely root-owned. Skipped when the test itself
# runs as root (a root-owned tree is then consistent and there is nothing to refuse).
if [[ "$(id -u)" -eq 0 ]]; then
  echo "· smoke.sh: running as root — root-owned refusal check skipped"
else
  mkdir -p "$UPD_TMP/rootcase"
  ln -s /tmp "$UPD_TMP/rootcase/node_modules"
  set +e
  ROOT_OUT="$(cd "$UPD_TMP" && PATH="$UPD_TMP/bin:$PATH" \
    UMADEV_REGISTRY_URL="$DEAD_REGISTRY" \
    node -e '
      const shim = require(process.argv[1]);
      shim.runSelfUpdate(["-y"], process.argv[2]).then((handled) => {
        if (!handled) { console.error("not handled"); process.exit(2); }
        process.exit(process.exitCode || 0);
      });
    ' "$NPM_ROOT/umadev/bin/cli.js" "$UPD_TMP/rootcase/node_modules" 2>&1)"
  ROOT_RC=$?
  set -e
  if [[ "$ROOT_RC" -eq 0 ]]; then
    echo "✗ smoke.sh: a root-owned install did not fail the update" >&2
    echo "$ROOT_OUT" >&2
    exit 1
  fi
  if [[ "$ROOT_OUT" != *"root-owned"* ]] ||
     [[ "$ROOT_OUT" != *"sudo npm uninstall -g umadev"* ]]; then
    echo "✗ smoke.sh: a root-owned install did not print the repair" >&2
    echo "$ROOT_OUT" >&2
    exit 1
  fi
  if [[ "$ROOT_OUT" == *"-called:"* ]]; then
    echo "✗ smoke.sh: a root-owned install ran the package manager anyway" >&2
    echo "$ROOT_OUT" >&2
    exit 1
  fi
  echo "✓ smoke.sh: a root-owned install refuses with the repair"
fi

# ── 8. Ownership detection, per layout. The e2e cases above cover npm/pnpm/bun;
# this pins yarn's layouts and the env-var evidence (PNPM_HOME / BUN_INSTALL), which
# have no fabricable global install here.
node -e '
  const assert = require("node:assert");
  const { detectPackageManager, versionAtLeast } = require(process.argv[1]);
  const cases = [
    ["/usr/local/lib/node_modules/umadev", {}, "npm"],
    ["/home/u/Library/pnpm/global/5/node_modules/umadev", {}, "pnpm"],
    ["/home/u/Library/pnpm/global/5/.pnpm/umadev@1.0.0/node_modules/umadev", {}, "pnpm"],
    ["/home/u/.bun/install/global/node_modules/umadev", {}, "bun"],
    ["/home/u/.config/yarn/global/node_modules/umadev", {}, "yarn"],
    ["/home/u/.yarn/global/node_modules/umadev", {}, "yarn"],
    ["/c/Users/u/AppData/Local/Yarn/Data/global/node_modules/umadev", {}, "yarn"],
    // Evidence from the manager home, for a layout we did not hard-code.
    ["/opt/pnpm-store/x/node_modules/umadev", { PNPM_HOME: "/opt/pnpm-store" }, "pnpm"],
    ["/opt/bunhome/install/global/node_modules/umadev", { BUN_INSTALL: "/opt/bunhome" }, "bun"],
    // A pnpm HOME that does NOT contain this install must not claim it.
    ["/usr/local/lib/node_modules/umadev", { PNPM_HOME: "/opt/pnpm-store" }, "npm"],
  ];
  for (const [p, env, want] of cases) {
    assert.strictEqual(detectPackageManager(p, env), want, p + " -> " + want);
  }
  assert.ok(versionAtLeast("1.0.40", "1.0.40"));
  assert.ok(versionAtLeast("1.0.41", "1.0.40"));
  assert.ok(!versionAtLeast("1.0.39", "1.0.40"));
  assert.ok(!versionAtLeast("1.0.40", "1.1.0"));
' "$NPM_ROOT/umadev/bin/cli.js"
echo "✓ smoke.sh: package-manager ownership is detected from the install layout"
