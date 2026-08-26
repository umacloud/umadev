'use strict';

const assert = require('node:assert/strict');
const { spawnSync } = require('node:child_process');
const path = require('node:path');
const test = require('node:test');

const helper = path.resolve(__dirname, 'publish-registry.sh');

function runBash(body, env = {}) {
  return spawnSync('bash', ['-c', body, 'bash', helper], {
    encoding: 'utf8',
    env: { ...process.env, ...env },
  });
}

test('npm publish waits beyond the old six-read visibility window', () => {
  const result = runBash(
    String.raw`
      source "$1"
      counter="$(mktemp)"
      trap 'rm -f "$counter"' EXIT
      printf '0' >"$counter"
      remote_integrity() {
        reads="$(cat "$counter")"
        reads=$((reads + 1))
        printf '%s' "$reads" >"$counter"
        if ((reads >= 8)); then printf '%s' 'sha512-expected'; fi
      }
      wait_for_remote_integrity '@umatech/example' '1.2.3' 'sha512-expected'
      cat "$counter"
    `,
    {
      UMADEV_NPM_VISIBILITY_ATTEMPTS: '10',
      UMADEV_NPM_VISIBILITY_DELAY_SECONDS: '0',
    },
  );

  assert.equal(result.status, 0, result.stderr);
  assert.equal(result.stdout, '8');
});

test('npm publish rejects visible content with a different integrity', () => {
  const result = runBash(
    String.raw`
      source "$1"
      remote_integrity() { printf '%s' 'sha512-other'; }
      wait_for_remote_integrity '@umatech/example' '1.2.3' 'sha512-expected'
    `,
    {
      UMADEV_NPM_VISIBILITY_ATTEMPTS: '10',
      UMADEV_NPM_VISIBILITY_DELAY_SECONDS: '0',
    },
  );

  assert.equal(result.status, 2);
  assert.match(result.stderr, /different contents/);
});

test('only immutable-version duplicate errors are recoverable', () => {
  const duplicate = runBash(
    String.raw`source "$1"; recoverable_duplicate_publish 'You cannot publish over the previously published versions: 1.2.3'`,
  );
  const forbidden = runBash(
    String.raw`source "$1"; recoverable_duplicate_publish 'E403 authentication token lacks publish permission'`,
  );

  assert.equal(duplicate.status, 0, duplicate.stderr);
  assert.notEqual(forbidden.status, 0);
});
