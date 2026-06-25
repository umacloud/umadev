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
const https = require('node:https');
const os = require('node:os');

// Node platform/arch → our sub-package name.
const PLATFORM_PACKAGES = {
  'darwin-arm64': '@umacloud/cli-darwin-arm64',
  'darwin-x64': '@umacloud/cli-darwin-x64',
  'linux-x64': '@umacloud/cli-linux-x64',
  'linux-arm64': '@umacloud/cli-linux-arm64',
  'win32-x64': '@umacloud/cli-win32-x64',
  // Windows on ARM runs x64 binaries via built-in emulation; reuse the x64 build.
  'win32-arm64': '@umacloud/cli-win32-x64',
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

// ── Local embedding model — ensure it's on disk, else download it (with a
// progress bar) from THIS version's GitHub Release. Checked on EVERY launch
// (a cheap stat); the ~224MB fp16 model is too large for npm, so it's a
// one-time fetch into ~/.umadev/embed-model. Fail-open: any failure launches
// anyway and the binary degrades to BM25 lexical retrieval, retrying next time.
function homeDir() {
  return process.env.HOME || process.env.USERPROFILE || os.homedir();
}
function modelTargetDir() {
  return path.join(homeDir(), '.umadev', 'embed-model');
}
const MODEL_FILES = ['config.json', 'tokenizer.json', 'model.safetensors'];
function modelPresent(dir) {
  return MODEL_FILES.every((f) => {
    try {
      return fs.statSync(path.join(dir, f)).size > 0;
    } catch (_) {
      return false;
    }
  });
}
// Download one URL to `dest`, following redirects (GitHub → CDN), drawing a
// progress bar when `withBar`. Resolves on success, rejects on any error.
function downloadTo(url, dest, withBar, label) {
  return new Promise((resolve, reject) => {
    const req = https.get(
      url,
      { headers: { 'User-Agent': 'umadev-cli', Accept: 'application/octet-stream' } },
      (res) => {
        if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
          res.resume();
          downloadTo(res.headers.location, dest, withBar, label).then(resolve, reject);
          return;
        }
        if (res.statusCode !== 200) {
          res.resume();
          reject(new Error('HTTP ' + res.statusCode));
          return;
        }
        const total = parseInt(res.headers['content-length'] || '0', 10);
        let got = 0;
        let lastPct = -1;
        const tmp = dest + '.part';
        const out = fs.createWriteStream(tmp);
        res.on('data', (chunk) => {
          got += chunk.length;
          if (withBar && total > 0) {
            const pct = Math.floor((got / total) * 100);
            if (pct !== lastPct) {
              lastPct = pct;
              const w = 24;
              const fill = Math.round((pct / 100) * w);
              const bar = '#'.repeat(fill) + '-'.repeat(w - fill);
              const mb = (got / 1048576).toFixed(0);
              const tot = (total / 1048576).toFixed(0);
              process.stderr.write(
                '\r  ' + label + ' [' + bar + '] ' + pct + '%  (' + mb + '/' + tot + ' MB)',
              );
            }
          }
        });
        res.pipe(out);
        out.on('finish', () =>
          out.close((e) => {
            if (e) return reject(e);
            try {
              fs.renameSync(tmp, dest);
            } catch (er) {
              return reject(er);
            }
            if (withBar) process.stderr.write('\n');
            resolve();
          }),
        );
        out.on('error', reject);
      },
    );
    req.on('error', reject);
    req.setTimeout(120000, () => req.destroy(new Error('timeout')));
  });
}
async function ensureModel() {
  const dir = modelTargetDir();
  if (modelPresent(dir)) return dir; // already installed — fast path, no network
  let version = '0.0.0';
  try {
    version = require('../package.json').version;
  } catch (_) {
    /* keep default */
  }
  const base = 'https://github.com/umacloud/umadev/releases/download/v' + version;
  try {
    fs.mkdirSync(dir, { recursive: true });
    process.stderr.write(
      '\n  本地向量检索模型缺失,正在下载 (multilingual-e5-small · fp16 · ~224MB)…\n',
    );
    process.stderr.write(
      '  一次性下载;之后完全本地、运行时无需联网。失败不影响使用(降级为 BM25)。\n',
    );
    await downloadTo(base + '/config.json', path.join(dir, 'config.json'), false, '');
    await downloadTo(base + '/tokenizer.json', path.join(dir, 'tokenizer.json'), false, '');
    await downloadTo(
      base + '/model.safetensors',
      path.join(dir, 'model.safetensors'),
      true,
      '下载向量模型',
    );
    process.stderr.write('  本地向量模型就绪 ✓\n\n');
    return dir;
  } catch (e) {
    process.stderr.write(
      '\n  [提示] 向量模型下载未完成 (' +
        e.message +
        ');本次用 BM25 检索,下次启动重试。\n\n',
    );
    return null;
  }
}

async function main() {
  const binary = findBinary();
  // npm artifact round-trips (upload/download-artifact in CI) can strip the
  // executable bit off the prebuilt binary; restore it defensively before exec.
  try {
    fs.chmodSync(binary, 0o755);
  } catch (_) {
    // read-only install dir or already +x — spawnSync below reports real errors
  }
  const extraEnv = {};
  // Prefer a bundled npm model package (dev / sibling layout); otherwise fetch
  // it on demand into ~/.umadev/embed-model (the binary's model_dir() fallback).
  let modelDir = findModelDir();
  if (!modelDir) modelDir = await ensureModel();
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
}

main();
