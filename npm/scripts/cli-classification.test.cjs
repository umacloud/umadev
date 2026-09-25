'use strict';

// Unit tests for the npm launcher's network-free decisions:
//   1. invocationNeedsModel — which verbs trigger the ~224MB embedding-model fetch.
//   2. resolveModelDir      — whether this launch may download it at all (an
//                             explicit model directory, the opt-out, the
//                             back-off after a failure), with https stubbed.
//   3. launcherLang         — the language of the launcher's own notices.
//   4. parseNodeMajor       — the ES5 shim's Node-version gate parser.
// All are exercised through bin/cli.js (the ES5 shim), which re-exports the
// modern bin/cli-main.js surface plus its own gate helpers. The shim itself must
// also still parse as ECMAScript 5, or ancient Node never reaches that gate.

const assert = require('node:assert/strict');
const { spawnSync } = require('node:child_process');
const { EventEmitter } = require('node:events');
const fs = require('node:fs');
const https = require('node:https');
const os = require('node:os');
const path = require('node:path');
const { PassThrough } = require('node:stream');
const test = require('node:test');

const {
  invocationNeedsModel,
  NEEDS_MODEL,
  resolveModelDir,
  launcherLang,
  MODEL_DOWNLOAD_BACKOFF_MS,
  MODEL_DOWNLOAD_FAILURE_NAME,
  parseNodeMajor,
  MIN_NODE_MAJOR,
} = require('../umadev/bin/cli.js');

// Build a process.argv-shaped array: [node, script, ...rest].
function argv(...rest) {
  return ['node', 'cli.js'].concat(rest);
}

test('only retrieval verbs and the bare TUI fetch the model', () => {
  for (const verb of ['run', 'quick', 'redo', 'continue', 'revise']) {
    assert.equal(
      invocationNeedsModel(argv(verb, 'anything')),
      true,
      `${verb} retrieves knowledge → needs the model`,
    );
  }
  // Bare `umadev` with no verb launches the interactive TUI, which retrieves.
  assert.equal(invocationNeedsModel(argv()), true, 'bare TUI needs the model');
});

test('read-only, emergency, and utility verbs never block on the download', () => {
  // The exact verbs called out in the audit — a fresh install must not await a
  // 224MB fetch it never uses.
  const noModel = [
    'rollback',
    'verify',
    'deploy',
    'report',
    'spec',
    'history',
    'usage',
    'lessons',
    'memory',
    'doctor',
    'init',
    'pr',
    'ci',
    'install',
    'uninstall',
    'examples',
    'guide',
    'hook',
    'mcp',
    'skill',
    'knowledge-manage',
    'mcp-manage',
    'adopt',
    'update',
    '--version',
    '-V',
    '--help',
    '-h',
  ];
  for (const verb of noModel) {
    assert.equal(
      invocationNeedsModel(argv(verb)),
      false,
      `${verb} must start instantly (no model download)`,
    );
  }
});

test('a NEW/unknown verb defaults to no download (allow-set, not deny-list)', () => {
  assert.equal(invocationNeedsModel(argv('some-future-verb')), false);
  // The allow-set is exactly the five retrieval verbs.
  assert.deepEqual(
    [...NEEDS_MODEL].sort(),
    ['continue', 'quick', 'redo', 'revise', 'run'],
  );
});

// Answer a stubbed https request with a 404.
function respond404(request, _options, callback) {
  queueMicrotask(() => {
    const response = new PassThrough();
    response.statusCode = 404;
    response.headers = {};
    callback(response);
  });
}

// Run `fn` with the given environment variables set (undefined = unset) and
// every https request counted and answered by `respond` (404 by default), then
// restore both.
async function withLaunchEnv(t, vars, fn, respond = respond404) {
  const saved = {};
  for (const [key, value] of Object.entries(vars)) {
    saved[key] = process.env[key];
    if (value === undefined) delete process.env[key];
    else process.env[key] = value;
  }
  const originalGet = https.get;
  const originalWrite = process.stderr.write;
  const requests = [];
  let stderr = '';
  https.get = (url, options, callback) => {
    requests.push(String(url));
    const request = new EventEmitter();
    request.setTimeout = () => request;
    request.destroy = (error) => {
      if (error) queueMicrotask(() => request.emit('error', error));
    };
    respond(request, options, callback);
    return request;
  };
  process.stderr.write = (chunk) => {
    stderr += String(chunk);
    return true;
  };
  t.after(() => {
    https.get = originalGet;
    process.stderr.write = originalWrite;
    for (const [key, value] of Object.entries(saved)) {
      if (value === undefined) delete process.env[key];
      else process.env[key] = value;
    }
  });
  try {
    const result = await fn();
    return { result, requests, stderr: () => stderr };
  } finally {
    https.get = originalGet;
    process.stderr.write = originalWrite;
  }
}

// A structurally valid model directory, as `modelPresent` checks it.
function writeValidModel(dir) {
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(path.join(dir, 'config.json'), '{"hidden_size":384}\n');
  fs.writeFileSync(path.join(dir, 'tokenizer.json'), '{"model":{}}\n');
  const header = Buffer.from('{}');
  const prefix = Buffer.alloc(8);
  prefix.writeBigUInt64LE(BigInt(header.length));
  fs.writeFileSync(
    path.join(dir, 'model.safetensors'),
    Buffer.concat([prefix, header, Buffer.alloc(1024 * 1024)]),
  );
}

function scratchHome(t) {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'umadev-model-launch-'));
  t.after(() => fs.rmSync(home, { recursive: true, force: true }));
  return home;
}

function launchVars(home, extra = {}) {
  return {
    HOME: home,
    USERPROFILE: home,
    XDG_CONFIG_HOME: undefined,
    UMADEV_EMBED_MODEL_DIR: undefined,
    UMADEV_NO_MODEL_DOWNLOAD: undefined,
    UMADEV_MODEL_BASE_URL: undefined,
    ...extra,
  };
}

test('a valid UMADEV_EMBED_MODEL_DIR is honoured and never downloads', async (t) => {
  const home = scratchHome(t);
  const provided = path.join(home, 'provided-model');
  writeValidModel(provided);
  const run = await withLaunchEnv(t, launchVars(home, { UMADEV_EMBED_MODEL_DIR: provided }), () =>
    resolveModelDir(argv('run', 'x')),
  );
  assert.deepEqual(run.requests, [], 'a download started despite the provided model');
  assert.equal(run.result, null, 'the binary reads UMADEV_EMBED_MODEL_DIR itself');
  assert.equal(fs.existsSync(path.join(home, '.umadev', 'embed-model')), false);
});

test('UMADEV_NO_MODEL_DOWNLOAD opts out of the download', async (t) => {
  const home = scratchHome(t);
  const run = await withLaunchEnv(t, launchVars(home, { UMADEV_NO_MODEL_DOWNLOAD: '1' }), () =>
    resolveModelDir(argv()),
  );
  assert.deepEqual(run.requests, []);
  assert.equal(run.result, null);
});

test('a failed download backs off for a day instead of retrying every launch', async (t) => {
  const home = scratchHome(t);
  const vars = launchVars(home, { LANG: 'en_US.UTF-8', LC_ALL: undefined, LC_MESSAGES: undefined });
  fs.mkdirSync(path.join(home, '.umadev'), { recursive: true });
  fs.writeFileSync(path.join(home, '.umadev', 'config.toml'), 'lang = "en"\n');

  const first = await withLaunchEnv(t, vars, () => resolveModelDir(argv('quick', 'x')));
  assert.equal(first.result, null);
  assert.ok(first.requests.length > 0, 'the first launch should try the download');
  const marker = path.join(home, '.umadev', 'embed-model', MODEL_DOWNLOAD_FAILURE_NAME);
  assert.ok(fs.existsSync(marker), 'the failure was not recorded');
  assert.match(first.stderr(), /did not finish/);
  assert.match(first.stderr(), /UMADEV_NO_MODEL_DOWNLOAD=1/);
  assert.doesNotMatch(first.stderr(), /[一-鿿]/, 'an English user got Chinese notices');

  const second = await withLaunchEnv(t, vars, () => resolveModelDir(argv('run', 'x')));
  assert.deepEqual(second.requests, [], 'the next launch downloaded again');
  assert.equal(second.result, null);

  // Once the back-off has passed, the next launch tries again.
  const recorded = JSON.parse(fs.readFileSync(marker, 'utf8'));
  recorded.failedAt = new Date(Date.now() - MODEL_DOWNLOAD_BACKOFF_MS - 60_000).toISOString();
  fs.writeFileSync(marker, JSON.stringify(recorded));
  const third = await withLaunchEnv(t, vars, () => resolveModelDir(argv('run', 'x')));
  assert.ok(third.requests.length > 0, 'the download never retried after the back-off');

  // A marker left by another release does not hold back this one's download.
  recorded.failedAt = new Date().toISOString();
  recorded.version = '0.0.1';
  fs.writeFileSync(marker, JSON.stringify(recorded));
  const upgraded = await withLaunchEnv(t, vars, () => resolveModelDir(argv('run', 'x')));
  assert.ok(upgraded.requests.length > 0, "an older release's failure blocked this download");
});

test('Ctrl+C during the download skips it and launches with BM25', async (t) => {
  const home = scratchHome(t);
  const listenersBefore = process.listenerCount('SIGINT');
  let started;
  const requestStarted = new Promise((resolve) => {
    started = resolve;
  });
  // A request that never answers, like a stalled link, but honours the abort
  // signal the way node:https does.
  const hang = (request, options) => {
    options.signal.addEventListener('abort', () => request.emit('error', new Error('aborted')));
    started();
  };
  const run = await withLaunchEnv(
    t,
    launchVars(home, { LANG: 'zh_TW.UTF-8', LC_ALL: undefined, LC_MESSAGES: undefined }),
    async () => {
      const pending = resolveModelDir(argv());
      await requestStarted;
      process.emit('SIGINT');
      return pending;
    },
    hang,
  );
  assert.equal(run.result, null);
  assert.equal(run.requests.length, 1);
  assert.equal(process.listenerCount('SIGINT'), listenersBefore, 'the Ctrl+C handler leaked');
  assert.match(run.stderr(), /按 Ctrl\+C 可略過/);
  assert.match(run.stderr(), /已略過向量模型下載/);
  const marker = path.join(home, '.umadev', 'embed-model', MODEL_DOWNLOAD_FAILURE_NAME);
  assert.equal(JSON.parse(fs.readFileSync(marker, 'utf8')).reason, 'skipped');
});

test('an intact model cache is used without any network', async (t) => {
  const home = scratchHome(t);
  const cache = path.join(home, '.umadev', 'embed-model');
  writeValidModel(cache);
  const run = await withLaunchEnv(t, launchVars(home), () => resolveModelDir(argv()));
  assert.deepEqual(run.requests, []);
  assert.equal(run.result, cache);
});

test('launcher notices follow the configured or detected language', async (t) => {
  const home = scratchHome(t);
  const base = launchVars(home, { LC_ALL: undefined, LC_MESSAGES: undefined, LANGUAGE: undefined });
  const config = path.join(home, '.umadev', 'config.toml');
  fs.mkdirSync(path.dirname(config), { recursive: true });

  await withLaunchEnv(t, { ...base, LANG: 'zh_CN.UTF-8' }, () => {
    fs.writeFileSync(config, '# saved by /lang\nlang = "en"\n\n[pipeline]\nlang = "zh-TW"\n');
    assert.equal(launcherLang(), 'en', 'the saved language wins over the locale');
    fs.writeFileSync(config, "lang = 'zh_TW'\n");
    assert.equal(launcherLang(), 'zh-TW');
    fs.writeFileSync(config, '[pipeline]\nlang = "en"\n');
    assert.equal(launcherLang(), 'zh-CN', 'only the top-level key is the UI language');
  });
  fs.rmSync(config);
  await withLaunchEnv(t, { ...base, LANG: 'zh_TW.UTF-8' }, () => {
    assert.equal(launcherLang(), 'zh-TW');
  });
  await withLaunchEnv(t, { ...base, LANG: 'zh_HK.UTF-8' }, () => {
    assert.equal(launcherLang(), 'zh-TW');
  });
  await withLaunchEnv(t, { ...base, LANG: 'zh_CN.UTF-8' }, () => {
    assert.equal(launcherLang(), 'zh-CN');
  });
  const xdg = path.join(home, 'xdg');
  fs.mkdirSync(path.join(xdg, 'umadev'), { recursive: true });
  fs.writeFileSync(path.join(xdg, 'umadev', 'config.toml'), 'lang = "en"\n');
  await withLaunchEnv(t, { ...base, LANG: 'zh_CN.UTF-8', XDG_CONFIG_HOME: xdg }, () => {
    assert.equal(launcherLang(), 'en', '$XDG_CONFIG_HOME/umadev/config.toml is the config');
  });
});

test('the ES5 shim Node-version gate parses the major correctly', () => {
  assert.equal(parseNodeMajor('v18.17.0'), 18);
  assert.equal(parseNodeMajor('20.1.2'), 20);
  assert.equal(parseNodeMajor('v8.9.4'), 8);
  assert.equal(parseNodeMajor(''), 0, 'empty → 0 (fails the floor safely)');
  assert.equal(parseNodeMajor(null), 0, 'null → 0');
  assert.equal(parseNodeMajor('garbage'), 0, 'garbage → 0');
  assert.equal(MIN_NODE_MAJOR, 18, 'gate floor matches package.json engines');
  // The gate would reject an ancient runtime and accept a supported one.
  assert.ok(parseNodeMajor('v8.0.0') < MIN_NODE_MAJOR, 'Node 8 is rejected');
  assert.ok(parseNodeMajor('v18.0.0') >= MIN_NODE_MAJOR, 'Node 18 is accepted');
});

// Node strips a leading hashbang before compiling a CommonJS file on every
// version, so the parse checks see the shim the way Node does. The newline is
// kept so reported line numbers match the file.
function shimSource() {
  const file = path.resolve(__dirname, '../umadev/bin/cli.js');
  return fs.readFileSync(file, 'utf8').replace(/^#![^\n]*/, '');
}

// Blank out comments and string literals so the lexical guard below only sees
// code. The shim has no template literals or regex literals containing quotes.
function codeOnly(source) {
  let out = '';
  let i = 0;
  while (i < source.length) {
    const two = source.slice(i, i + 2);
    if (two === '//') {
      while (i < source.length && source[i] !== '\n') i += 1;
    } else if (two === '/*') {
      const end = source.indexOf('*/', i + 2);
      i = end === -1 ? source.length : end + 2;
      out += ' ';
    } else if (source[i] === "'" || source[i] === '"') {
      const quote = source[i];
      i += 1;
      while (i < source.length && source[i] !== quote) i += source[i] === '\\' ? 2 : 1;
      i += 1;
      out += '""';
    } else {
      out += source[i];
      i += 1;
    }
  }
  return out;
}

test('the ES5 launch shim parses as ECMAScript 5', () => {
  const source = shimSource();

  // A real ES5 parse with the acorn copy Node bundles for its own REPL.
  const probe = spawnSync(
    process.execPath,
    [
      '--expose-internals',
      '-e',
      `let acorn;
       try { acorn = require('internal/deps/acorn/acorn/dist/acorn'); }
       catch (_) { process.stdout.write('unavailable'); process.exit(0); }
       try { acorn.parse(process.argv[1], { ecmaVersion: 5, sourceType: 'script' }); process.stdout.write('ok'); }
       catch (error) { process.stdout.write('error: ' + error.message); }`,
      source,
    ],
    { encoding: 'utf8' },
  );
  assert.ok(
    probe.stdout === 'ok' || probe.stdout === 'unavailable',
    `bin/cli.js is not ECMAScript 5: ${probe.stdout || probe.stderr}`,
  );

  // A lexical guard that holds even where that parser is unavailable: no
  // ES2015+ syntax, and no trailing comma in a call or parameter list (ES2017).
  const code = codeOnly(source);
  for (const [pattern, what] of [
    [/\b(?:const|let|class)\b/, 'const/let/class'],
    [/=>/, 'an arrow function'],
    [/`/, 'a template literal'],
    [/\.\.\./, 'spread or rest syntax'],
    [/\?\.|\?\?/, 'optional chaining or nullish coalescing'],
    [/,\s*\)/, 'a trailing comma before `)`'],
  ]) {
    assert.doesNotMatch(code, pattern, `bin/cli.js uses ${what}, which ancient Node cannot parse`);
  }
  assert.doesNotMatch(source, /require\(\s*['"]node:/, 'bin/cli.js requires a `node:` module');
});
