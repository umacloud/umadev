'use strict';

const assert = require('node:assert/strict');
const { spawn } = require('node:child_process');
const fs = require('node:fs');
const http = require('node:http');
const os = require('node:os');
const path = require('node:path');
const test = require('node:test');

const {
  installedVersionState,
  resolveInstalledBinary,
  versionStateMatches,
  windowsLockRecoveryMessage,
  REPAIR_COMMANDS,
  runSelfUpdate,
  linuxLibcFromEvidence,
  registryLatestVersion,
} = require('../umadev/bin/cli.js');

const PLATFORM_LEAVES = {
  'darwin-arm64': 'cli-darwin-arm64',
  'darwin-x64': 'cli-darwin-x64',
  'linux-arm64': 'cli-linux-arm64',
  'linux-x64': 'cli-linux-x64',
  'win32-arm64': 'cli-win32-x64',
  'win32-x64': 'cli-win32-x64',
};

test('terminal contract: Linux libc detection distinguishes glibc from musl', () => {
  assert.equal(
    linuxLibcFromEvidence({ header: { glibcVersionRuntime: '2.31' }, sharedObjects: [] }),
    'gnu',
  );
  assert.equal(
    linuxLibcFromEvidence({ header: {}, sharedObjects: ['/lib/ld-musl-aarch64.so.1'] }),
    'musl',
  );
  assert.equal(linuxLibcFromEvidence(null, 'musl libc (aarch64)'), 'musl');
  assert.equal(linuxLibcFromEvidence(null, ''), 'gnu');
});

test('terminal contract: package and executable versions agree through Unicode paths', (t) => {
  const platformLeaf = PLATFORM_LEAVES[`${process.platform}-${process.arch}`];
  assert.ok(platformLeaf, `unsupported CI platform ${process.platform}-${process.arch}`);

  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'umadev 终端契约-'));
  t.after(() =>
    fs.rmSync(root, {
      recursive: true,
      force: true,
      maxRetries: 20,
      retryDelay: 100,
    }),
  );
  const packageRoot = path.join(root, '用户 空格', 'node_modules', 'umadev');
  const platformRoot = path.join(
    root,
    '用户 空格',
    'node_modules',
    '@umacloud',
    platformLeaf,
  );
  const binary = path.join(
    platformRoot,
    'bin',
    process.platform === 'win32' ? 'umadev.exe' : 'umadev',
  );
  fs.mkdirSync(path.join(packageRoot, 'bin'), { recursive: true });
  fs.mkdirSync(path.dirname(binary), { recursive: true });

  const version = process.versions.node;
  fs.writeFileSync(
    path.join(packageRoot, 'package.json'),
    `${JSON.stringify({ name: 'umadev', version })}\n`,
  );
  fs.writeFileSync(
    path.join(platformRoot, 'package.json'),
    `${JSON.stringify({ name: `@umacloud/${platformLeaf}`, version })}\n`,
  );
  fs.copyFileSync(process.execPath, binary);
  fs.chmodSync(binary, 0o755);

  assert.equal(resolveInstalledBinary(packageRoot), binary);
  const state = installedVersionState(packageRoot);
  assert.deepEqual(
    { main: state.main, platform: state.platform, binary: state.binary },
    { main: version, platform: version, binary: version },
  );
  assert.equal(versionStateMatches(state, version), true);

  fs.writeFileSync(
    path.join(platformRoot, 'package.json'),
    `${JSON.stringify({ name: `@umacloud/${platformLeaf}`, version: '0.0.1' })}\n`,
  );
  assert.equal(versionStateMatches(installedVersionState(packageRoot), version), false);
});

test('terminal contract: EPERM guidance names lock holders and exact repair', () => {
  const message = windowsLockRecoveryMessage(REPAIR_COMMANDS.npm);
  for (const evidence of [
    'EPERM',
    'VS Code',
    'Zcode',
    'Codex',
    'PowerShell',
    'terminal',
    REPAIR_COMMANDS.npm,
    'where umadev',
  ]) {
    assert.match(message, new RegExp(evidence.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')));
  }
});

test('terminal contract: an oversized registry response cannot hang update', async (t) => {
  const server = http.createServer((_request, response) => {
    response.writeHead(200, { 'content-type': 'application/json' });
    response.write('x'.repeat(300_000));
    // Deliberately never end the response. The client must resolve from its
    // byte cap rather than waiting for an end/error event that may never come.
  });
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });
  t.after(() => new Promise((resolve) => server.close(resolve)));

  const previous = process.env.UMADEV_REGISTRY_URL;
  const address = server.address();
  process.env.UMADEV_REGISTRY_URL = `http://127.0.0.1:${address.port}`;
  t.after(() => {
    if (previous === undefined) delete process.env.UMADEV_REGISTRY_URL;
    else process.env.UMADEV_REGISTRY_URL = previous;
  });

  const result = await Promise.race([
    registryLatestVersion(),
    new Promise((_, reject) =>
      setTimeout(() => reject(new Error('registry response cap did not settle')), 2_000),
    ),
  ]);
  assert.equal(result, null);
});

test(
  'terminal contract: a real running Windows executable can never become update success',
  { skip: process.platform !== 'win32', timeout: 30000 },
  async (t) => {
    const root = fs.mkdtempSync(path.join(os.tmpdir(), 'umadev 占用更新-'));
    const nodeModules = path.join(root, '用户 空格', 'node_modules');
    const packageRoot = path.join(nodeModules, 'umadev');
    const platformRoot = path.join(nodeModules, '@umacloud', 'cli-win32-x64');
    const binary = path.join(platformRoot, 'bin', 'umadev.exe');
    const binDir = path.join(root, 'manager bin');
    fs.mkdirSync(path.join(packageRoot, 'bin'), { recursive: true });
    fs.mkdirSync(path.dirname(binary), { recursive: true });
    fs.mkdirSync(binDir, { recursive: true });
    fs.copyFileSync(path.resolve(__dirname, '../umadev/bin/cli.js'), path.join(packageRoot, 'bin', 'cli.js'));

    const expected = '999.0.0';
    fs.writeFileSync(
      path.join(packageRoot, 'package.json'),
      `${JSON.stringify({ name: 'umadev', version: expected })}\n`,
    );
    fs.writeFileSync(
      path.join(platformRoot, 'package.json'),
      `${JSON.stringify({ name: '@umacloud/cli-win32-x64', version: expected })}\n`,
    );
    fs.copyFileSync(process.execPath, binary);

    const lockProbe = path.join(root, 'replace-locked.cjs');
    const lockResult = path.join(root, 'lock-result.txt');
    fs.writeFileSync(
      lockProbe,
      `'use strict';\n` +
        `const fs = require('node:fs');\n` +
        `try { fs.copyFileSync(process.execPath, process.argv[2]); fs.writeFileSync(process.argv[3], 'replaced'); }\n` +
        `catch (error) { fs.writeFileSync(process.argv[3], String(error && error.code || error)); }\n`,
    );
    const manager = path.join(binDir, 'npm.cmd');
    fs.writeFileSync(
      manager,
      `@echo off\r\n` +
        `if "%1"=="--version" (echo 9.9.9& exit /b 0)\r\n` +
        `if "%1"=="view" (echo ${expected}& exit /b 0)\r\n` +
        `"${process.execPath}" "${lockProbe}" "${binary}" "${lockResult}"\r\n` +
        `exit /b 0\r\n`,
    );

    const holder = spawn(binary, ['-e', 'setInterval(() => {}, 1000)'], {
      stdio: 'ignore',
      windowsHide: true,
    });
    await new Promise((resolve, reject) => {
      holder.once('spawn', resolve);
      holder.once('error', reject);
    });

    const saved = {
      path: process.env.PATH,
      registry: process.env.UMADEV_REGISTRY_URL,
      exitCode: process.exitCode,
      error: console.error,
      warn: console.warn,
    };
    const diagnostics = [];
    process.env.PATH = `${binDir}${path.delimiter}${saved.path || ''}`;
    process.env.UMADEV_REGISTRY_URL = 'https://127.0.0.1:1';
    process.exitCode = undefined;
    console.error = (...args) => diagnostics.push(args.join(' '));
    console.warn = (...args) => diagnostics.push(args.join(' '));

    t.after(async () => {
      if (holder.exitCode === null) {
        const exited = new Promise((resolve) => holder.once('exit', resolve));
        holder.kill();
        await exited;
      }
      process.env.PATH = saved.path;
      if (saved.registry === undefined) delete process.env.UMADEV_REGISTRY_URL;
      else process.env.UMADEV_REGISTRY_URL = saved.registry;
      process.exitCode = saved.exitCode;
      console.error = saved.error;
      console.warn = saved.warn;
      // Windows releases a terminated process's handle to its .exe slightly
      // AFTER the 'exit' event, so an immediate rmdir of the dir holding it hits
      // ENOTEMPTY. maxRetries/retryDelay is Node's built-in backoff for exactly
      // this (it retries EBUSY/ENOTEMPTY/EPERM), so cleanup waits out the handle.
      fs.rmSync(root, {
        recursive: true,
        force: true,
        maxRetries: 20,
        retryDelay: 100,
      });
    });

    assert.equal(await runSelfUpdate(['--yes'], packageRoot), true);
    assert.equal(process.exitCode, 1, 'a locked stale executable must make update fail');
    assert.match(fs.readFileSync(lockResult, 'utf8'), /^(EPERM|EBUSY|EACCES)$/);
    const text = diagnostics.join('\n');
    assert.match(text, /upgrade verification failed/);
    assert.match(text, /Windows EPERM/);
    assert.match(text, /npm install -g umadev@latest --force/);
    assert.match(text, /where umadev/);
    assert.doesNotMatch(text, /upgraded and verified|repaired and verified/);
  },
);
