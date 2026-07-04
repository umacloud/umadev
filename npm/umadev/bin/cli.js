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
// Render one frame of the download progress bar (in place, via \r). Block-glyph
// bar + percent + downloaded/total + live speed; ANSI-colored only on a TTY so a
// piped/redirected install stays clean.
function drawBar(label, got, total, startTime) {
  const tty = process.stderr.isTTY;
  const c = (code) => (tty ? '\x1b[' + code + 'm' : '');
  const w = 22;
  const ratio = total > 0 ? Math.min(1, got / total) : 0;
  const fill = Math.round(ratio * w);
  const bar = c('38;5;45') + '█'.repeat(fill) + c('0') + c('38;5;238') + '░'.repeat(w - fill) + c('0');
  const pct = String(Math.floor(ratio * 100)).padStart(3);
  const mb = (got / 1048576).toFixed(1);
  const tot = (total / 1048576).toFixed(0);
  const sec = (Date.now() - startTime) / 1000;
  const spd = sec > 0.3 ? (got / 1048576 / sec).toFixed(1) + ' MB/s' : '…';
  process.stderr.write(
    '\r  ' + c('1') + label + c('0') + '  ' + bar + '  ' + c('1') + pct + '%' + c('0') +
      c('2') + '  ·  ' + mb + '/' + tot + ' MB  ·  ' + spd + c('0') + '   ',
  );
}
// Per-attempt idle timeout. Short on purpose: the model is OPTIONAL (RAG works
// locally via BM25 without it), and a stalled source must fail over to the next
// base fast instead of making first-run feel hung. A HEALTHY download is never
// cut off — this is a no-progress (socket-idle) timeout, and the live progress
// bar keeps ticking while bytes flow. The old 120s made a single dead source
// block for two minutes, times several bases and files.
const DOWNLOAD_IDLE_TIMEOUT_MS = 20000;
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
        let lastDraw = 0;
        const startTime = Date.now();
        const tmp = dest + '.part';
        const out = fs.createWriteStream(tmp);
        // Draw the bar at 0% the instant the response starts — on a slow link the
        // first 1% can take a while, and a silent gap reads as "stuck / failed".
        if (withBar && total > 0) drawBar(label, 0, total, startTime);
        res.on('data', (chunk) => {
          got += chunk.length;
          if (withBar && total > 0) {
            const now = Date.now();
            const pct = Math.floor((got / total) * 100);
            // Redraw on each new percent OR every ~250ms — keeps the live speed
            // ticking even while a single percent of a large file streams in.
            if (pct !== lastPct || now - lastDraw > 250) {
              lastPct = pct;
              lastDraw = now;
              drawBar(label, got, total, startTime);
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
            if (withBar && total > 0) {
              drawBar(label, total, total, startTime);
              process.stderr.write('\n');
            }
            resolve();
          }),
        );
        out.on('error', reject);
      },
    );
    req.on('error', reject);
    req.setTimeout(DOWNLOAD_IDLE_TIMEOUT_MS, () =>
      req.destroy(new Error('idle timeout')),
    );
  });
}
// Ordered list of base URLs to try for the model files. An explicit override
// (UMADEV_MODEL_BASE_URL) wins; otherwise EVERYONE pulls the SAME upstream f32 from
// HuggingFace (international) / hf-mirror.com (China), region-ordered, with the
// GitHub fp16 release as a last-resort fallback — so the download is consistent in
// size everywhere and the model is always reachable.
function releaseBases(version) {
  if (process.env.UMADEV_MODEL_BASE_URL) {
    return [process.env.UMADEV_MODEL_BASE_URL.replace(/\/+$/, '')];
  }
  // GitHub Release ships the quantized fp16 model (~224MB, smaller). HuggingFace
  // and its China mirror hf-mirror.com serve the upstream f32 model (~448MB —
  // bigger, but the candle loader handles either). hf-mirror is the FAST + reliable
  // source inside mainland China, where github.com's release CDN is slow and the
  // community GitHub proxies are flaky for release-asset URLs.
  const gh = 'https://github.com/umacloud/umadev/releases/download/v' + version;
  const ghProxies = ['https://ghproxy.net/' + gh, 'https://ghfast.top/' + gh];
  const hf = 'https://huggingface.co/intfloat/multilingual-e5-small/resolve/main';
  const hfMirror = 'https://hf-mirror.com/intfloat/multilingual-e5-small/resolve/main';
  let cn = false;
  try {
    const opts = Intl.DateTimeFormat().resolvedOptions();
    const tz = opts.timeZone || '';
    const loc = (process.env.LANG || process.env.LC_ALL || '') + ' ' + (opts.locale || '');
    cn =
      /Shanghai|Chongqing|Urumqi|Harbin|Hong_Kong|Macau/.test(tz) ||
      /zh[_-]?(CN|Hans)/i.test(loc);
  } catch (_) {
    /* default to international order */
  }
  // Unified on the upstream f32 model from HuggingFace (international) + its China
  // mirror hf-mirror.com, so BOTH regions download the SAME ~448MB f32 — consistent
  // everywhere (no more "some get 200MB, some 400MB"). China: hf-mirror first (fast
  // in CN); international: huggingface.co first. The GitHub Release fp16 (~224MB) +
  // proxies stay only as a LAST-RESORT fallback if both HF endpoints are down (the
  // candle loader casts either precision to f32, so a fallback still loads).
  return cn ? [hfMirror, hf, gh, ...ghProxies] : [hf, hfMirror, gh, ...ghProxies];
}
// Try each base for `name` in order; resolve on first success, throw the last
// error if all fail. A China mirror can cover a blocked github.com (or vice
// versa) with zero user configuration.
async function downloadFile(bases, name, dest, withBar, label) {
  let lastErr;
  for (const base of bases) {
    try {
      await downloadTo(base + '/' + name, dest, withBar, label);
      return;
    } catch (e) {
      lastErr = e;
    }
  }
  throw lastErr || new Error('no source reachable');
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
  const bases = releaseBases(version);
  try {
    fs.mkdirSync(dir, { recursive: true });
    process.stderr.write(
      '\n  本地向量检索模型缺失,正在下载 multilingual-e5-small(国内自动走镜像)…\n',
    );
    process.stderr.write(
      '  一次性下载;之后完全本地、运行时无需联网。失败不影响使用(降级为 BM25)。\n',
    );
    await downloadFile(bases, 'config.json', path.join(dir, 'config.json'), false, '');
    await downloadFile(bases, 'tokenizer.json', path.join(dir, 'tokenizer.json'), true, '下载分词器  ');
    await downloadFile(
      bases,
      'model.safetensors',
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

// Subcommands that do NOT drive the agent runtime — they never retrieve
// knowledge, so they must not trigger the one-time vector-model download.
// Without this, `umadev update` (and `--version` / `--help` / `doctor` / …) would
// appear to hang on a machine that doesn't have the model yet, while it streams
// in — which reads as "the update command broke". The model is fetched lazily on
// the first command that actually needs it (the TUI / run / quick / …).
const NO_MODEL_COMMANDS = new Set([
  'update', 'install', 'uninstall', 'init',
  '--version', '-V', 'version',
  '--help', '-h', 'help',
  'doctor', 'mcp', 'hook', 'ci',
  'usage', 'lessons', 'history',
  'examples', 'guide',
  'mcp-manage', 'skill', 'knowledge-manage', 'pr',
]);

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
  // A utility command (update/--version/--help/doctor/…) skips the model fetch so
  // it returns instantly even before the model is installed; the agent runtime
  // commands still fetch it lazily on first use.
  const firstArg = process.argv[2] || '';
  const needsModel = !NO_MODEL_COMMANDS.has(firstArg);
  // Prefer a bundled npm model package (dev / sibling layout); otherwise fetch
  // it on demand into ~/.umadev/embed-model (the binary's model_dir() fallback).
  let modelDir = findModelDir();
  if (needsModel && !modelDir) modelDir = await ensureModel();
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
