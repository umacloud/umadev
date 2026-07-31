#!/usr/bin/env bash
# Version management -- single source of truth. Bumps Cargo.toml (the binary
# reads it via env!("CARGO_PKG_VERSION")) AND every npm/ package.json, so the
# installed `umadev --version` always equals the published npm version.
# Usage: npm/scripts/bump-version.sh 1.0.4
set -euo pipefail
[[ $# -eq 1 && "$1" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || {
  echo "usage: $(basename "$0") <x.y.z>   e.g. $(basename "$0") 1.0.4" >&2; exit 1
}
NEW="$1"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
perl -i -pe "s/^version = \"\d+\.\d+\.\d+\"/version = \"$NEW\"/" "$ROOT/Cargo.toml"
node - "$ROOT/npm" "$NEW" <<'NODE'
const fs = require('node:fs');
const path = require('node:path');
const [root, next] = process.argv.slice(2);
for (const entry of fs.readdirSync(root, { withFileTypes: true })) {
  const file = path.join(root, entry.name, 'package.json');
  if (!entry.isDirectory() || !fs.existsSync(file)) continue;
  const pkg = JSON.parse(fs.readFileSync(file, 'utf8'));
  pkg.version = next;
  if (pkg.name === 'umadev') {
    for (const name of Object.keys(pkg.optionalDependencies || {})) {
      if (name.startsWith('@umacloud/')) pkg.optionalDependencies[name] = next;
    }
  }
  fs.writeFileSync(file, `${JSON.stringify(pkg, null, 2)}\n`);
}
NODE
( cd "$ROOT" && cargo check -p umadev-spec >/dev/null )
echo "version -> $NEW  (Cargo.toml + npm/*/package.json + Cargo.lock)"
echo "next: commit release $NEW, push main, and wait for that exact SHA's CI"
echo "then: run the Release workflow_dispatch validation on main and wait for green"
echo "last: create the immutable v$NEW tag on that verified SHA and push only that tag"
