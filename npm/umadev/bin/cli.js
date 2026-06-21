#!/usr/bin/env node
// SPDX-License-Identifier: MIT
//
// umadev — thin JS shim. npm picks the matching `@umacloud/cli-*`
// platform sub-package (via optionalDependencies + the `os` / `cpu`
// fields in each sub-package). This shim resolves that sub-package's
// prebuilt Rust binary and exec's it with the user's argv.
//
// The shim is deliberately minimal:
//   - no dependencies (zero install-time cost beyond node itself)
//   - no parsing of argv (every flag goes straight to the binary)
//   - stdio is inherited so the ratatui TUI gets a real TTY

'use strict';

const { spawnSync } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');

// Node platform/arch → our sub-package name.
const PLATFORM_PACKAGES = {
  'darwin-arm64': '@umacloud/cli-darwin-arm64',
  'darwin-x64': '@umacloud/cli-darwin-x64',
  'linux-x64': '@umacloud/cli-linux-x64',
  'linux-arm64': '@umacloud/cli-linux-arm64',
  'win32-x64': '@umacloud/cli-win32-x64',
};

function platformKey() {
  return `${process.platform}-${process.arch}`;
}

function binaryName() {
  return process.platform === 'win32' ? 'umadev.exe' : 'umadev';
}

function findBinary() {
  const key = platformKey();
  const pkg = PLATFORM_PACKAGES[key];
  if (!pkg) {
    const supported = Object.keys(PLATFORM_PACKAGES).join(', ');
    console.error(
      `umadev: unsupported platform ${key}. Supported: ${supported}.`,
    );
    console.error(
      'Open an issue at https://github.com/umacloud/umadev/issues',
    );
    process.exit(1);
  }
  const bin = binaryName();

  // 1) Published case — npm installed the sibling platform package.
  try {
    return require.resolve(`${pkg}/bin/${bin}`);
  } catch (_) {
    /* fall through */
  }

  // 2) Local dev — both packages live as siblings under npm/.
  const sibling = path.resolve(
    __dirname,
    '..',
    '..',
    `cli-${process.platform}-${process.arch}`,
    'bin',
    bin,
  );
  if (fs.existsSync(sibling)) return sibling;

  console.error(
    `umadev: ${pkg} not installed.\n` +
      'Try: npm install -g umadev --force\n' +
      "(npm 'optionalDependencies' should normally pick the right one.)",
  );
  process.exit(1);
}

// Resolve the platform-independent bundled embedding model (a regular
// dependency, shipped once for all platforms). Pointing the binary at it via
// UMADEV_EMBED_MODEL_DIR enables offline local embeddings with zero user setup.
// Fail-open: if the model package is absent the binary degrades to BM25.
function findModelDir() {
  try {
    return path.dirname(require.resolve('@umacloud/model-e5-small/package.json'));
  } catch (_) {
    const sibling = path.resolve(__dirname, '..', '..', 'model-e5-small');
    if (fs.existsSync(path.join(sibling, 'tokenizer.json'))) return sibling;
  }
  return null;
}

// Resolve the platform-independent bundled knowledge corpus (a regular
// dependency). Pointing the binary at it via UMADEV_KNOWLEDGE_DIR means end
// users get the full curated 400+ file KB even in a bare project; the project's
// own knowledge/ (if any) still wins. Fail-open: absent -> BM25 over nothing.
function findKnowledgeDir() {
  try {
    return path.dirname(require.resolve('@umacloud/knowledge/package.json'));
  } catch (_) {
    const sibling = path.resolve(__dirname, '..', '..', '..', 'knowledge');
    if (fs.existsSync(path.join(sibling, 'frontend'))) return sibling;
  }
  return null;
}

const binary = findBinary();
const extraEnv = {};
const modelDir = findModelDir();
if (modelDir && !process.env.UMADEV_EMBED_MODEL_DIR) {
  extraEnv.UMADEV_EMBED_MODEL_DIR = modelDir;
}
const knowledgeDir = findKnowledgeDir();
if (knowledgeDir && !process.env.UMADEV_KNOWLEDGE_DIR) {
  extraEnv.UMADEV_KNOWLEDGE_DIR = knowledgeDir;
}
const spawnOpts = { stdio: 'inherit' };
if (Object.keys(extraEnv).length > 0) {
  spawnOpts.env = { ...process.env, ...extraEnv };
}
const result = spawnSync(binary, process.argv.slice(2), spawnOpts);

if (result.error) {
  console.error(`umadev: failed to exec binary: ${result.error.message}`);
  process.exit(1);
}

process.exit(result.status === null ? 1 : result.status);
