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

function platformPackage() {
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
  return pkg;
}

// Resolve the binary belonging to this exact main-package install. Avoid a
// process-wide lookup here: a second global manager can put another UmaDev on
// PATH, which is precisely the split-install state the updater must diagnose.
function resolveInstalledBinary(pkgRoot = PACKAGE_ROOT) {
  const pkg = platformPackage();
  const bin = binaryName();
  // npm can keep replacement optional dependencies nested under the main
  // package during a global upgrade. Prefer that copy because it belongs to
  // this exact main-package version; fresh installs are normally hoisted.
  const nested = path.join(pkgRoot, 'node_modules', ...pkg.split('/'), 'bin', bin);
  if (fs.existsSync(nested)) return nested;

  const hoisted = path.join(path.dirname(pkgRoot), ...pkg.split('/'), 'bin', bin);
  if (fs.existsSync(hoisted)) return hoisted;

  // Local dev — both packages live as siblings under npm/. Use the mapped
  // package name so win32-arm64 correctly reuses cli-win32-x64.
  const packageLeaf = pkg.slice(pkg.lastIndexOf('/') + 1);
  const sibling = path.resolve(
    pkgRoot,
    '..',
    packageLeaf,
    'bin',
    bin,
  );
  if (fs.existsSync(sibling)) return sibling;
  return null;
}

function findBinary() {
  const pkg = platformPackage();
  const resolved = resolveInstalledBinary(PACKAGE_ROOT);
  if (resolved) return resolved;

  console.error(
    `umadev: ${pkg} not installed.\n` +
      'Try: npm install -g umadev --force\n' +
      "(npm 'optionalDependencies' should normally pick the right one.)",
  );
  process.exit(1);
}

function readPackageVersion(pkgDir) {
  try {
    const parsed = JSON.parse(fs.readFileSync(path.join(pkgDir, 'package.json'), 'utf8'));
    return typeof parsed.version === 'string' ? parsed.version : null;
  } catch (_) {
    return null;
  }
}

function binaryVersion(binary) {
  if (!binary) return null;
  try {
    const r = spawnSync(binary, ['--version'], {
      encoding: 'utf8',
      timeout: 10000,
      windowsHide: true,
    });
    if (r.error || r.status !== 0) return null;
    const match = /\bv?(\d+\.\d+\.\d+(?:[-+][^\s]+)?)/.exec(`${r.stdout || ''} ${r.stderr || ''}`);
    return match ? match[1] : null;
  } catch (_) {
    return null;
  }
}

// Main package, optional platform package, and executable are three separate
// artifacts in npm's tree. A successful manager exit is not enough: npm allows
// optional dependency installation to fail, and Windows file locks can leave
// the old platform package beside a new main package.
function installedVersionState(pkgRoot = PACKAGE_ROOT) {
  const binary = resolveInstalledBinary(pkgRoot);
  const platformRoot = binary ? path.dirname(path.dirname(binary)) : null;
  return {
    main: readPackageVersion(pkgRoot),
    platform: platformRoot ? readPackageVersion(platformRoot) : null,
    binary: binaryVersion(binary),
    binaryPath: binary,
  };
}

function versionStateMatches(state, expected) {
  return Boolean(
    expected &&
      state.main === expected &&
      state.platform === expected &&
      state.binary === expected,
  );
}

function describeVersionState(state) {
  return `main=${state.main || 'missing'}, platform=${state.platform || 'missing'}, binary=${state.binary || 'unreadable'}`;
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
// (a cheap integrity check — size + JSON parse + safetensors header, not just
// existence, so a corrupt cache re-downloads); the ~224MB fp16 model is too large
// for npm, so it's a one-time fetch into ~/.umadev/embed-model. Fail-open: any failure launches
// anyway and the binary degrades to BM25 lexical retrieval, retrying next time.
function homeDir() {
  return process.env.HOME || process.env.USERPROFILE || os.homedir();
}
function modelTargetDir() {
  return path.join(homeDir(), '.umadev', 'embed-model');
}
const MODEL_FILES = ['config.json', 'tokenizer.json', 'model.safetensors'];
// A real multilingual-e5-small safetensors is tens of MB (fp16 ~224MB, f32
// ~448MB); anything under 1 MiB is a truncated/garbage download, never a real
// model. The JSON sidecars (config.json / tokenizer.json) are far smaller.
const MIN_SAFETENSORS_BYTES = 1048576; // 1 MiB
// Largest JSON sidecar we fully JSON.parse on the (per-launch) presence check.
// config.json is well under this; tokenizer.json (XLM-R vocab) can be ~17MB —
// for a file that large a full parse every launch would tax startup, so we do a
// cheap structural completeness check instead (opens with '{', closes with '}').
const JSON_FULL_PARSE_LIMIT = 4 * 1048576; // 4 MiB

// Read `length` bytes from `filePath` at byte `position` (fewer if near EOF).
// Returns a Buffer of the bytes actually read. Throws on I/O error (callers
// treat a throw as "corrupt/absent"). Used to sniff file headers/footers without
// slurping a multi-hundred-MB model into memory.
function readSlice(filePath, position, length) {
  const fd = fs.openSync(filePath, 'r');
  try {
    const buf = Buffer.alloc(length);
    const n = fs.readSync(fd, buf, 0, length, position);
    return buf.subarray(0, n);
  } finally {
    fs.closeSync(fd);
  }
}

// Is a JSON model sidecar (config.json / tokenizer.json) a complete, non-trivial
// JSON object — not empty, not truncated, not binary garbage? A small file is
// fully JSON.parsed and must be a non-empty object; a large file (tokenizer.json)
// is checked structurally so the hot launch path never pays a ~17MB parse. Any
// read/parse error returns false so the caller re-downloads it. Never throws.
function jsonModelFileLooksValid(filePath) {
  try {
    const size = fs.statSync(filePath).size;
    if (size <= 0) return false;
    if (size <= JSON_FULL_PARSE_LIMIT) {
      const parsed = JSON.parse(fs.readFileSync(filePath, 'utf8'));
      return (
        parsed !== null &&
        typeof parsed === 'object' &&
        Object.keys(parsed).length > 0
      );
    }
    const head = readSlice(filePath, 0, 64).toString('utf8').replace(/^\s+/, '');
    const tail = readSlice(filePath, Math.max(0, size - 64), 64)
      .toString('utf8')
      .replace(/\s+$/, '');
    return head.startsWith('{') && tail.endsWith('}');
  } catch (_) {
    return false;
  }
}

// Does model.safetensors look structurally intact WITHOUT loading it? The format
// is `<u64 LE header-length N><N-byte JSON header><tensor data>`, so a sane size
// floor PLUS a header-length prefix that actually fits inside the file catches a
// truncated download (whose prefix points past EOF) or garbage. Any read error
// returns false so the caller re-downloads. Never throws.
function safetensorsLooksValid(filePath) {
  try {
    const size = fs.statSync(filePath).size;
    if (size < MIN_SAFETENSORS_BYTES) return false;
    const prefix = readSlice(filePath, 0, 8);
    if (prefix.length < 8) return false;
    const headerLen = prefix.readBigUInt64LE(0);
    return headerLen > 0n && 8n + headerLen <= BigInt(size);
  } catch (_) {
    return false;
  }
}

// One model file is "usable" when it exists AND passes a cheap integrity check
// for its kind. A file that exists but is CORRUPT (truncated download / garbage)
// is treated as absent so `modelPresent` is false and the wrapper re-downloads —
// the P3 self-heal fix (a non-empty corrupt cache previously healed never).
function modelFileValid(dir, name) {
  const filePath = path.join(dir, name);
  if (name.endsWith('.safetensors')) return safetensorsLooksValid(filePath);
  if (name.endsWith('.json')) return jsonModelFileLooksValid(filePath);
  try {
    return fs.statSync(filePath).size > 0;
  } catch (_) {
    return false;
  }
}
function modelPresent(dir) {
  return MODEL_FILES.every((f) => modelFileValid(dir, f));
}
// Remove any existing model files (and leftover `.part` temporaries) so a
// re-download starts from a clean slate — called before re-fetching a cache that
// failed `modelPresent`. Fail-open: unlink errors are ignored (a fresh download's
// atomic rename overwrites anyway); never throws.
function clearModelFiles(dir) {
  for (const f of MODEL_FILES) {
    for (const p of [path.join(dir, f), path.join(dir, f + '.part')]) {
      try {
        fs.unlinkSync(p);
      } catch (_) {
        /* missing or locked — the re-download's rename overwrites it */
      }
    }
  }
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
  if (modelPresent(dir)) return dir; // already installed & intact — fast path, no network
  let version = '0.0.0';
  try {
    version = require('../package.json').version;
  } catch (_) {
    /* keep default */
  }
  const bases = releaseBases(version);
  try {
    fs.mkdirSync(dir, { recursive: true });
    // modelPresent() was false — either absent (first run) or a corrupt/partial
    // cache. Drop any bad or leftover `.part` files first so this fetch self-heals
    // a corrupt download instead of being shadowed by it (P3).
    clearModelFiles(dir);
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

// ── The `sudo npm i -g` footgun, reported at RUNTIME.
//
// Why here and not in postinstall: npm 7+ SWALLOWS lifecycle-script output
// (postinstall stdout/stderr is only shown with --foreground-scripts, or when
// the script exits nonzero — which we must never do). The shim, by contrast,
// runs with inherited stdio on every launch, so this is the one channel that
// actually reaches the user.
//
// What it catches: an install whose files are ROOT-OWNED while the user is not
// root — i.e. `sudo npm i -g umadev`. It runs fine today, but it leaves a
// root-owned tree in the npm prefix, so every LATER non-root npm command on
// that prefix (`npm update -g`, `npm i -g <anything>`) dies with EACCES and npm
// aborts the whole transaction — taking the user's OTHER global packages (their
// base CLI: @anthropic-ai/claude-code, @openai/codex) down with it.
//
// Shown at most ONCE (a marker under the user-owned ~/.umadev, never the
// root-owned install dir). Fail-open: any error here is swallowed — a cosmetic
// advisory must never keep the agent from starting.

// Is `p` owned by root while WE are not root — i.e. the `sudo npm i -g` footgun?
// False on Windows (no uid) and when we are root ourselves (a root-owned tree is
// then consistent). Never throws: an unreadable path reads as "not root-owned".
function isRootOwned(p) {
  try {
    if (process.platform === 'win32' || typeof process.getuid !== 'function') return false;
    if (process.getuid() === 0) return false;
    return fs.statSync(p).uid === 0;
  } catch (_) {
    return false;
  }
}

// The one copy of the sudo-footgun diagnosis + repair. Used by BOTH the once-per-
// machine launch warning and by `umadev update`, which must refuse to hand a
// root-owned prefix to a package manager (see rootOwnedRefusal).
function sudoFootgunLines() {
  return [
    '',
    '  [warn] umadev was installed with `sudo` — its files are root-owned.',
    '         It runs, but LATER npm commands you run as yourself on this prefix',
    '         (`npm update -g`, `npm i -g <anything>`) will fail with EACCES, and npm',
    '         aborts the whole transaction — so your OTHER global packages, including',
    '         your base CLI (@anthropic-ai/claude-code / @openai/codex), can no longer',
    '         be updated either. This is the classic `sudo npm` footgun, not a umadev bug.',
    '         用 sudo 安装会留下 root 属主文件,之后普通用户跑 npm 会 EACCES,并连带影响其它全局包。',
    '',
    '  Repair (no sudo from here on):',
    '           sudo npm uninstall -g umadev',
    '           sudo chown -R $(whoami) ~/.npm',
    '           npm config set prefix ~/.npm-global',
    '           export PATH="$HOME/.npm-global/bin:$PATH"   # add to ~/.zshrc or ~/.bashrc',
    '           npm i -g umadev',
    '',
  ];
}

function warnIfRootOwnedInstall(binary) {
  try {
    if (!isRootOwned(binary)) return;

    const marker = path.join(homeDir(), '.umadev', '.sudo-install-warned');
    if (fs.existsSync(marker)) return;

    const w = (s) => process.stderr.write(s + '\n');
    for (const line of sudoFootgunLines()) w(line);
    w('  Run `umadev doctor` for the full diagnosis. (This notice is shown once.)');
    w('');

    fs.mkdirSync(path.dirname(marker), { recursive: true });
    fs.writeFileSync(marker, new Date().toISOString());
  } catch (_) {
    /* advisory only — never block the launch */
  }
}

// ── Self-update, run from the SHIM and never from the binary.
//
// The updater must not be the thing being updated. `umadev` on Windows is
// `umadev.cmd` -> node (this shim) -> umadev.exe, so if the EXE runs `npm
// install -g`, npm renames the old tree aside and then cannot delete it — the
// .exe inside it is the running image, and Windows refuses to unlink a mapped
// executable (EPERM). npm still installs the new version, but it leaves the
// renamed tree behind as garbage that accumulates on every upgrade, and it
// prints a wall of red that reads like a failed install.
//
// Handling `update` HERE means the binary is never launched, so nothing under
// the npm prefix is open when npm swaps the tree, and npm's own cleanup works.
// POSIX doesn't have the unlink restriction, but there is no reason to keep two
// code paths: the shim is the right place on every platform.
const PACKAGE_ROOT = path.resolve(__dirname, '..');

// Is this a JS-package-manager install (as opposed to a dev checkout / a
// `cargo install` build)? Every one of npm / pnpm / yarn / bun materializes a
// global install under a `node_modules` dir; a cargo/manual binary never does, and
// falls through to the Rust binary's own self-updater.
function isPackageManaged(pkgRoot) {
  return path.resolve(pkgRoot).split(path.sep).includes('node_modules');
}

// ── Which package manager OWNS this install?
//
// The upgrade must run through the manager that installed us. Running `npm i -g`
// on a pnpm-owned install does not upgrade it — it drops a SECOND copy into npm's
// prefix, and whichever bin dir comes first on PATH wins, so the user "updates"
// and still runs the old binary.
//
// Detection is EVIDENCE-BASED: it looks at where this copy actually lives (and at
// the manager's own home env var), never at which manager happens to be on PATH —
// a dev box typically has all four installed.
//
//   pnpm  $PNPM_HOME/global/<n>/node_modules/umadev, files in .../global/<n>/.pnpm/…
//         (macOS ~/Library/pnpm, Linux ~/.local/share/pnpm, Windows %LOCALAPPDATA%\pnpm)
//   bun   $BUN_INSTALL/install/global/node_modules/umadev   (default ~/.bun)
//   yarn  ~/.config/yarn/global/node_modules/umadev            (classic, POSIX)
//         ~/.yarn/global/…  |  %LOCALAPPDATA%\Yarn\Data\global\…
//   npm   <prefix>/lib/node_modules/umadev — the default, and the fallback
//
// Each manager's global-upgrade command. Constant strings — nothing from argv is
// ever interpolated into a shell command.
const UPGRADE_COMMANDS = {
  pnpm: 'pnpm add -g umadev@latest',
  yarn: 'yarn global add umadev@latest',
  bun: 'bun add -g umadev@latest',
  npm: 'npm install -g umadev@latest',
};

function detectPackageManager(pkgRoot, env = process.env) {
  const p = path.resolve(pkgRoot);
  // Compare on a normalized, lowercased path: Windows mixes separators and cases
  // (`%LOCALAPPDATA%\Yarn\Data\global`).
  const norm = p.split(path.sep).join('/').toLowerCase();
  const under = (root) => {
    if (!root) return false;
    const base = path.resolve(root).split(path.sep).join('/').toLowerCase();
    return norm === base || norm.startsWith(base.replace(/\/+$/, '') + '/');
  };

  if (norm.includes('/.pnpm/') || norm.includes('/pnpm/global/') || under(env.PNPM_HOME)) {
    return 'pnpm';
  }
  if (norm.includes('/.bun/install/global/') || under(env.BUN_INSTALL)) return 'bun';
  if (
    norm.includes('/yarn/global/') ||
    norm.includes('/.yarn/global/') ||
    norm.includes('/yarn/data/global/') ||
    under(env.YARN_GLOBAL_FOLDER)
  ) {
    return 'yarn';
  }
  return 'npm';
}

// Can we actually execute this manager? `shell: true` so Windows resolves the
// `.cmd` / `.ps1` shims npm-family tools install themselves as.
function managerRunnable(mgr) {
  try {
    const r = spawnSync(mgr + ' --version', { stdio: 'ignore', shell: true });
    return !r.error && r.status === 0;
  } catch (_) {
    return false;
  }
}

// npm stages a replacement by renaming the old package dir to `.<name>-<rand>`
// and deleting it afterwards. A delete that hit EPERM (see above) leaves that
// directory behind forever. Sweep the ones belonging to us — they are npm's own
// abandoned temp dirs, and by the time we run, nothing in them is in use.
// Fail-open: any error is ignored; a failed sweep must never block an upgrade.
function sweepAbandonedStagingDirs(pkgRoot) {
  const staging = /^\.(umadev|cli-|knowledge|model-)[^/\\]*$/;
  const roots = [
    path.dirname(pkgRoot), // …/node_modules
    path.join(path.dirname(pkgRoot), '@umacloud'), // …/node_modules/@umacloud
  ];
  let swept = 0;
  for (const root of roots) {
    let entries;
    try {
      entries = fs.readdirSync(root, { withFileTypes: true });
    } catch (_) {
      continue;
    }
    for (const e of entries) {
      if (!e.isDirectory() || !staging.test(e.name)) continue;
      try {
        fs.rmSync(path.join(root, e.name), { recursive: true, force: true });
        swept += 1;
      } catch (_) {
        /* still locked by another process — leave it, try again next time */
      }
    }
  }
  return swept;
}

// One line from stdin, synchronously — works for a TTY and for piped input.
// EOF (or a closed stdin) reads as "no", so a non-interactive caller that did
// not pass --yes aborts instead of silently upgrading.
function readLineSync() {
  const buf = Buffer.alloc(1);
  let out = '';
  for (;;) {
    let n;
    try {
      n = fs.readSync(0, buf, 0, 1, null);
    } catch (err) {
      if (err && err.code === 'EAGAIN') continue; // TTY not ready yet
      return out; // EOF / closed stdin / not readable
    }
    if (n === 0) break;
    const ch = buf.toString('utf8');
    if (ch === '\n') break;
    if (ch !== '\r') out += ch;
  }
  return out;
}

// ── "Am I already on the latest?" — so `umadev update` on a current install is a
// no-op instead of a multi-hundred-MB reinstall.
//
// Fail-open by contract: any failure returns null and the caller just proceeds with
// the upgrade. An offline / firewalled / private-registry box must never be BLOCKED
// from updating by the check that was only meant to save it a reinstall.

// The registry to ask. Honors npm's own config (`npm_config_registry`, set for a
// company mirror / npmmirror.com) and an explicit override, so a user behind a
// private registry checks THEIR registry, not npmjs.org.
function registryBase() {
  const raw =
    process.env.UMADEV_REGISTRY_URL ||
    process.env.npm_config_registry ||
    'https://registry.npmjs.org';
  return raw.replace(/\/+$/, '');
}

// The registry lookup is a convenience, not a gate — keep it snappy.
const REGISTRY_TIMEOUT_MS = 5000;

// Cheap, dependency-free registry query: one GET of the `latest` dist-tag document
// (a few hundred bytes), short timeout. Resolves to a version string or null.
function registryLatestVersion() {
  return new Promise((resolve) => {
    let settled = false;
    const done = (v) => {
      if (!settled) {
        settled = true;
        resolve(v);
      }
    };
    let url;
    try {
      url = registryBase() + '/umadev/latest';
      const client = url.startsWith('http://') ? require('node:http') : https;
      const req = client.get(
        url,
        { headers: { 'User-Agent': 'umadev-cli', Accept: 'application/json' } },
        (res) => {
          if (res.statusCode !== 200) {
            res.resume();
            return done(null);
          }
          let body = '';
          res.setEncoding('utf8');
          res.on('data', (c) => {
            body += c;
            if (body.length > 262144) req.destroy(); // a sane cap; never buffer a huge doc
          });
          res.on('end', () => {
            try {
              const v = JSON.parse(body).version;
              done(typeof v === 'string' ? v : null);
            } catch (_) {
              done(null);
            }
          });
          res.on('error', () => done(null));
        },
      );
      req.on('error', () => done(null));
      req.setTimeout(REGISTRY_TIMEOUT_MS, () => {
        req.destroy();
        done(null);
      });
    } catch (_) {
      done(null);
    }
  });
}

// Fallback when the direct GET fails (proxy-only network, custom CA, …): ask npm,
// which already knows the user's proxy/registry/auth config. Returns null if npm is
// absent or errors.
function npmViewLatestVersion() {
  try {
    const r = spawnSync('npm view umadev version', {
      encoding: 'utf8',
      shell: true,
      timeout: 20000,
    });
    if (r.error || r.status !== 0 || !r.stdout) return null;
    const v = r.stdout.trim().split(/\s+/).pop();
    return /^\d+\.\d+\.\d+/.test(v || '') ? v : null;
  } catch (_) {
    return null;
  }
}

async function latestPublishedVersion() {
  return (await registryLatestVersion()) || npmViewLatestVersion();
}

// Is `current` >= `latest`? Used to short-circuit an update that would change
// nothing. Non-semver on either side falls back to string equality — an unknown
// version is never treated as "up to date" unless it is literally identical.
function versionAtLeast(current, latest) {
  const parse = (v) => {
    const m = /^v?(\d+)\.(\d+)\.(\d+)/.exec(String(v || '').trim());
    return m ? [Number(m[1]), Number(m[2]), Number(m[3])] : null;
  };
  const a = parse(current);
  const b = parse(latest);
  if (!a || !b) return String(current) === String(latest);
  for (let i = 0; i < 3; i += 1) {
    if (a[i] !== b[i]) return a[i] > b[i];
  }
  return true;
}

// A root-owned install (`sudo npm i -g umadev`) cannot be upgraded by a package
// manager running as the user: it dies with EACCES PART-WAY THROUGH and aborts the
// whole global transaction, which can leave the user's OTHER global packages (their
// base CLI) broken. So: name the problem and print the repair instead of running the
// manager. Returns the lines to print, or null when the install is upgradable.
function rootOwnedRefusal(pkgDir, mgr) {
  if (!isRootOwned(pkgDir)) return null;
  const cmd = UPGRADE_COMMANDS[mgr] || UPGRADE_COMMANDS.npm;
  return [
    '',
    `umadev: this install is root-owned, so \`${cmd}\` would fail with EACCES`,
    '        half-way through — and an aborted global transaction can break your OTHER',
    '        global packages. Not running it.',
    ...sudoFootgunLines(),
    '  Then re-run `umadev update`.',
    '',
  ];
}

// `umadev update`. Returns true if it handled the command (the caller must then
// exit without launching the binary), false to fall through to the binary — a
// dev/cargo build is nobody's package install, and the binary self-updates from the
// GitHub Release on its own.
async function runSelfUpdate(args, pkgRoot = PACKAGE_ROOT) {
  if (!isPackageManaged(pkgRoot)) return false;

  const mgr = detectPackageManager(pkgRoot);
  const command = UPGRADE_COMMANDS[mgr];

  // BEFORE anything else — no registry call, no manager, no prompt.
  const refusal = rootOwnedRefusal(pkgRoot, mgr);
  if (refusal) {
    for (const line of refusal) console.error(line);
    process.exitCode = 1;
    return true;
  }

  const before = installedVersionState(pkgRoot);
  const current = before.main || 'unknown';
  console.log(`UmaDev ${current} is installed (via ${mgr}).`);
  if (before.main && !versionStateMatches(before, before.main)) {
    console.warn(`Version split detected: ${describeVersionState(before)}.`);
    console.warn('The platform package or executable will be repaired even if the main package is current.');
  }

  const force = args.includes('--force');
  let latest = null;
  if (!force) {
    latest = await latestPublishedVersion();
    if (latest && versionAtLeast(current, latest) && versionStateMatches(before, current)) {
      console.log(`Already on the latest version (${latest}). Nothing to do.`);
      return true;
    }
    if (latest) console.log(`Latest published version: ${latest}.`);
  }

  // Upgrade with the manager that OWNS this install. If its binary is somehow not
  // runnable (uninstalled since, or not on this PATH), npm is the universal
  // fallback; if npm is missing too, we cannot do it for them — print the exact
  // command and fail loudly rather than silently installing a shadow copy.
  let useCommand = command;
  if (!managerRunnable(mgr)) {
    if (mgr !== 'npm' && managerRunnable('npm')) {
      console.log(`\`${mgr}\` is not runnable here — falling back to npm.`);
      useCommand = UPGRADE_COMMANDS.npm;
    } else {
      console.error(
        `\numadev: \`${mgr}\` owns this install but is not runnable, and npm is not\n` +
          'available either. Upgrade it yourself with:\n' +
          `    ${command}\n`,
      );
      process.exitCode = 1;
      return true;
    }
  }

  const yes = args.includes('-y') || args.includes('--yes');
  if (!yes) {
    process.stdout.write(`Upgrade now via \`${useCommand}\`? [y/N] `);
    const reply = readLineSync().trim().toLowerCase();
    if (reply !== 'y' && reply !== 'yes') {
      console.log('Aborted.');
      return true;
    }
  }

  sweepAbandonedStagingDirs(pkgRoot);

  // A constant command string — nothing from argv reaches the shell.
  const r = spawnSync(useCommand, { stdio: 'inherit', shell: true });
  if (r.error || r.status !== 0) {
    console.error(
      '\numadev: the upgrade did not complete. Run it yourself to see why:\n' +
        `    ${useCommand}\n`,
    );
    process.exitCode = 1;
    return true;
  }

  // The manager's own cleanup normally succeeds now that the binary was never
  // launched; sweep once more in case an unrelated process held something open.
  sweepAbandonedStagingDirs(pkgRoot);
  const after = installedVersionState(pkgRoot);
  const expected = latest || after.main;
  if (!versionStateMatches(after, expected)) {
    console.error(`\numadev: upgrade verification failed (${describeVersionState(after)}).`);
    if (expected) console.error(`        Expected all three artifacts to be ${expected}.`);
    console.error(
      '        On Windows, close VS Code/Zcode/Codex and any terminal still running UmaDev,\n' +
        '        then repair the optional platform package with:\n' +
        '          npm install -g umadev@latest --force\n' +
        '        If `where umadev` prints multiple paths, remove the stale install.',
    );
    process.exitCode = 1;
    return true;
  }
  console.log(`[ok] UmaDev ${expected} upgraded and verified (${path.basename(after.binaryPath)}).`);
  return true;
}

async function main() {
  // Before anything else: `update` must not launch the binary it replaces.
  if ((process.argv[2] || '') === 'update') {
    if (await runSelfUpdate(process.argv.slice(3))) {
      process.exit(process.exitCode || 0);
    }
  }
  const binary = findBinary();
  warnIfRootOwnedInstall(binary);
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
    // A present-but-unexecutable ELF reports ENOENT — the kernel is reporting the
    // missing *ELF interpreter*, not the missing binary. On Linux that is almost
    // always musl (Alpine): we ship glibc builds, and the message as-is reads like
    // "the file isn't there", which sends people hunting the wrong bug.
    if (
      result.error.code === 'ENOENT' &&
      process.platform === 'linux' &&
      fs.existsSync(binary)
    ) {
      console.error(
        '\numadev: the binary IS present, so this is a C-library mismatch —\n' +
          '  the prebuilt Linux binaries are glibc builds, and this system looks like musl\n' +
          '  (Alpine). Options: use a glibc image (e.g. node:20-bookworm / debian / ubuntu),\n' +
          '  add glibc compatibility, or build from source:\n' +
          '    cargo install --git https://github.com/umacloud/umadev umadev\n',
      );
    }
    process.exit(1);
  }

  process.exit(result.status === null ? 1 : result.status);
}

// Run only when invoked as the CLI (which is how npm's bin shim launches us).
// Being `require`-able keeps the update logic — package-manager detection, the
// already-latest short-circuit, the root-owned refusal — testable from
// npm/scripts/smoke.sh without a network or a second global install.
if (require.main === module) {
  main();
}

module.exports = {
  detectPackageManager,
  isPackageManaged,
  isRootOwned,
  rootOwnedRefusal,
  runSelfUpdate,
  installedVersionState,
  versionStateMatches,
  resolveInstalledBinary,
  versionAtLeast,
  UPGRADE_COMMANDS,
};
