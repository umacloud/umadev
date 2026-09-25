'use strict';

// Unit tests for two pure, network-free classifiers exported by the npm launcher:
//   1. invocationNeedsModel — which verbs trigger the ~224MB embedding-model fetch.
//   2. parseNodeMajor        — the ES5 shim's Node-version gate parser.
// Both are exercised through bin/cli.js (the ES5 shim), which re-exports the
// modern bin/cli-main.js surface plus its own gate helpers. The shim itself must
// also still parse as ECMAScript 5, or ancient Node never reaches that gate.

const assert = require('node:assert/strict');
const { spawnSync } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');

const {
  invocationNeedsModel,
  NEEDS_MODEL,
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
