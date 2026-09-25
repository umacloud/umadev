#!/usr/bin/env node
// SPDX-License-Identifier: MIT
// umadev-governance: allow-es5-bootstrap
//
// ─────────────────────────────────────────────────────────────────────────────
// umadev — ES5-only launch shim. THIS FILE MUST PARSE ON ANCIENT NODE.
//
// A SyntaxError is a PARSE-time failure of the WHOLE file: if the Node-version
// check lived alongside modern syntax (const/let, arrow functions, template
// literals, the `node:` import prefix), an old-Node user would only ever see
// `SyntaxError: Use of const in strict mode` — the friendly "please upgrade"
// message could never run, because the file never parses. So this shim is kept
// deliberately in the ES5 subset (var + function only, no const/let, no arrows,
// no template literals, no `node:` prefix, no optional chaining), checks the Node
// version, and ONLY THEN require()s bin/cli-main.js — which holds today's modern
// implementation and may use any syntax it likes.
// ─────────────────────────────────────────────────────────────────────────────

'use strict';

// Minimum Node major the launcher (and package.json `engines`) requires.
var MIN_NODE_MAJOR = 18;

// Parse the leading integer of a Node version string (e.g. "v18.17.0" -> 18).
// Returns 0 for empty/garbage so an unreadable version fails the floor safely.
function parseNodeMajor(raw) {
  var m = /^v?(\d+)\./.exec(String(raw == null ? '' : raw));
  return m ? parseInt(m[1], 10) : 0;
}

function currentNodeVersion() {
  return (process.versions && process.versions.node) || '';
}

// Gate BEFORE touching the modern file. On too-old Node, print a clear upgrade
// message and exit non-zero instead of crashing with a cryptic parse error.
// (No trailing comma after a last call argument either: that is ES2017.)
if (parseNodeMajor(currentNodeVersion()) < MIN_NODE_MAJOR) {
  process.stderr.write(
    'UmaDev requires Node >= ' +
      MIN_NODE_MAJOR +
      ', but you have ' +
      (currentNodeVersion() || 'an unknown version') +
      '.\n' +
      'Please upgrade Node.js (https://nodejs.org) and re-run.\n'
  );
  process.exit(1);
}

// Node is new enough — hand off to the modern implementation. Re-export its full
// API so npm/scripts/*.test.cjs and smoke.sh (which require() bin/cli.js) still
// see the same surface, plus the two ES5-gate helpers so the version gate itself
// is unit-testable.
var cliMain = require('./cli-main.js');
module.exports = cliMain;
module.exports.parseNodeMajor = parseNodeMajor;
module.exports.MIN_NODE_MAJOR = MIN_NODE_MAJOR;

// Run only when invoked as the CLI entry point (how npm's bin shim launches us).
// When merely require()d by a test, this is false, so main() does not run.
if (require.main === module) {
  cliMain.main();
}
