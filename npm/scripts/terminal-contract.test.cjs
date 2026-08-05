'use strict';

const assert = require('node:assert/strict');
const { spawn } = require('node:child_process');
const fs = require('node:fs');
const http = require('node:http');
const https = require('node:https');
const os = require('node:os');
const path = require('node:path');
const { EventEmitter } = require('node:events');
const { PassThrough } = require('node:stream');
const test = require('node:test');

const {
  installedVersionState,
  resolveInstalledBinary,
  versionStateMatches,
  windowsLockRecoveryMessage,
  REPAIR_COMMANDS,
  runSelfUpdate,
  linuxLibcFromEvidence,
  registryLatestRelease,
  validateTrustedUpdateManifest,
  exactUpdateCommand,
  sweepAbandonedStagingDirs,
  ABANDONED_STAGING_MIN_AGE_MS,
  ensureModelCacheDirectory,
  downloadTo,
  MAX_MODEL_REDIRECTS,
  versionAtLeast,
  acquireModelDownloadLock,
  releaseModelDownloadLock,
  modelDownloadTempPath,
  MODEL_DOWNLOAD_LOCK_NAME,
} = require('../umadev/bin/cli.js');

function trustedUpdateManifest(version) {
  const dependencies = [
    '@umacloud/cli-darwin-arm64',
    '@umacloud/cli-darwin-x64',
    '@umacloud/cli-linux-arm64',
    '@umacloud/cli-linux-musl-arm64',
    '@umacloud/cli-linux-musl-x64',
    '@umacloud/cli-linux-x64',
    '@umacloud/cli-win32-x64',
    '@umacloud/knowledge',
  ];
  return {
    name: 'umadev',
    version,
    repository: { type: 'git', url: 'git+https://github.com/umacloud/umadev.git' },
    bin: { umadev: 'bin/cli.js' },
    optionalDependencies: Object.fromEntries(dependencies.map((name) => [name, version])),
    dist: {
      tarball: `https://registry.npmjs.org/umadev/-/umadev-${version}.tgz`,
      integrity: `sha512-${Buffer.alloc(64, 1).toString('base64')}`,
      fileCount: 5,
      signatures: [{ keyid: 'test', sig: 'test' }],
      attestations: {
        url: `https://registry.npmjs.org/-/npm/v1/attestations/umadev@${version}`,
        provenance: { predicateType: 'https://slsa.dev/provenance/v1' },
      },
    },
  };
}

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

test('terminal contract: updater follows strict SemVer precedence', () => {
  assert.equal(versionAtLeast('1.0.71-beta.1', '1.0.71'), false);
  assert.equal(versionAtLeast('1.0.71', '1.0.71-beta.99'), true);
  assert.equal(versionAtLeast('1.0.71-beta.2', '1.0.71-beta.11'), false);
  assert.equal(versionAtLeast('1.0.71-beta.11', '1.0.71-beta.2'), true);
  assert.equal(versionAtLeast('1.0.71-1', '1.0.71-alpha'), false);
  assert.equal(versionAtLeast('1.0.71-alpha', '1.0.71-alpha.1'), false);
  assert.equal(versionAtLeast('1.0.71+local.2', '1.0.71+build.9'), true);
  assert.equal(versionAtLeast('1.0.72-rc.1', '1.0.71'), true);
  assert.equal(versionAtLeast('1.0.071', '1.0.71'), false);
  assert.equal(versionAtLeast('1.0.071', '1.0.071'), true);
});

test('terminal contract: updater accepts only inert Trusted Publishing releases', () => {
  const clean = trustedUpdateManifest('1.0.73');
  assert.deepEqual(validateTrustedUpdateManifest(clean), { trusted: true, version: '1.0.73' });

  const lifecyclePayload = structuredClone(clean);
  lifecyclePayload.version = '1.0.74';
  lifecyclePayload.scripts = { preinstall: 'node setup.mjs' };
  for (const name of Object.keys(lifecyclePayload.optionalDependencies)) {
    lifecyclePayload.optionalDependencies[name] = '1.0.74';
  }
  lifecyclePayload.dist.tarball = 'https://registry.npmjs.org/umadev/-/umadev-1.0.74.tgz';
  lifecyclePayload.dist.attestations.url =
    'https://registry.npmjs.org/-/npm/v1/attestations/umadev@1.0.74';
  assert.match(validateTrustedUpdateManifest(lifecyclePayload).reason, /lifecycle scripts/);

  const noProvenance = structuredClone(clean);
  delete noProvenance.dist.attestations;
  assert.match(validateTrustedUpdateManifest(noProvenance).reason, /provenance/);

  const splitDependency = structuredClone(clean);
  splitDependency.optionalDependencies['@umacloud/knowledge'] = '1.0.72';
  assert.match(validateTrustedUpdateManifest(splitDependency).reason, /dependency set/);

  assert.equal(
    exactUpdateCommand('npm', '1.0.73', true),
    'npm install -g umadev@1.0.73 --registry=https://registry.npmjs.org --force',
  );
  assert.throws(() => exactUpdateCommand('npm', 'latest; touch /tmp/owned'));
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
    registryLatestRelease(),
    new Promise((_, reject) =>
      setTimeout(() => reject(new Error('registry response cap did not settle')), 2_000),
    ),
  ]);
  assert.deepEqual(result, {
    status: 'untrusted',
    reason: 'registry manifest exceeded 256 KiB',
  });
});

test('terminal contract: updater cleanup preserves fresh package-manager staging', (t) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'umadev-staging-cleanup-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const nodeModules = path.join(root, 'node_modules');
  const packageRoot = path.join(nodeModules, 'umadev');
  const fresh = path.join(nodeModules, '.umadev-FreshTxn');
  const stale = path.join(nodeModules, '.umadev-StaleTxn');
  const unrelated = path.join(nodeModules, '.another-package-StaleTxn');
  for (const directory of [packageRoot, fresh, stale, unrelated]) {
    fs.mkdirSync(directory, { recursive: true });
  }
  const now = Date.now();
  const old = new Date(now - ABANDONED_STAGING_MIN_AGE_MS - 60_000);
  fs.utimesSync(stale, old, old);
  fs.utimesSync(unrelated, old, old);

  assert.equal(sweepAbandonedStagingDirs(packageRoot, now), 1);
  assert.equal(fs.existsSync(fresh), true, 'a fresh/possibly active transaction was deleted');
  assert.equal(fs.existsSync(stale), false, 'a conservatively stale UmaDev staging dir remains');
  assert.equal(fs.existsSync(unrelated), true, 'cleanup crossed into another package');
});

test('terminal contract: concurrent model download lock is bounded and recoverable', async (t) => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'umadev-model-lock-'));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  const first = await acquireModelDownloadLock(dir, { waitMs: 100, pollMs: 10 });
  t.after(() => releaseModelDownloadLock(first));
  const ownerPath = path.join(dir, MODEL_DOWNLOAD_LOCK_NAME, 'owner.json');
  const ownerBefore = fs.readFileSync(ownerPath, 'utf8');

  await assert.rejects(
    acquireModelDownloadLock(dir, { waitMs: 100, pollMs: 10 }),
    /another UmaDev process is downloading/,
  );
  assert.equal(fs.readFileSync(ownerPath, 'utf8'), ownerBefore, 'an active lock was replaced');
  releaseModelDownloadLock(first);

  const afterRelease = await acquireModelDownloadLock(dir, { waitMs: 100, pollMs: 10 });
  releaseModelDownloadLock(afterRelease);

  const exited = spawn(process.execPath, ['-e', 'process.exit(0)'], { stdio: 'ignore' });
  const deadPid = exited.pid;
  await new Promise((resolve, reject) => {
    exited.once('exit', resolve);
    exited.once('error', reject);
  });
  const stalePath = path.join(dir, MODEL_DOWNLOAD_LOCK_NAME);
  fs.mkdirSync(stalePath);
  fs.writeFileSync(
    path.join(stalePath, 'owner.json'),
    `${JSON.stringify({ pid: deadPid, token: 'stale', startedAt: new Date().toISOString() })}\n`,
  );
  const recovered = await acquireModelDownloadLock(dir, { waitMs: 500, pollMs: 10 });
  assert.notEqual(recovered.token, 'stale');
  releaseModelDownloadLock(recovered);
});

test('terminal contract: model cache directories must not be symlinks or junctions', (t) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'umadev-model-cache-root-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));

  const ordinary = path.join(root, 'ordinary', '.umadev', 'embed-model');
  fs.mkdirSync(path.dirname(path.dirname(ordinary)), { recursive: true });
  assert.equal(ensureModelCacheDirectory(ordinary), ordinary);

  const externalRoot = path.join(root, 'external-root');
  const linkedRootParent = path.join(root, 'linked-root-parent');
  const linkedRoot = path.join(linkedRootParent, '.umadev');
  fs.mkdirSync(externalRoot);
  fs.mkdirSync(linkedRootParent);
  try {
    fs.symlinkSync(externalRoot, linkedRoot, process.platform === 'win32' ? 'junction' : 'dir');
  } catch (error) {
    if (process.platform === 'win32' && error && error.code === 'EPERM') {
      t.skip('Windows runner did not grant directory junction creation');
      return;
    }
    throw error;
  }
  assert.throws(
    () => ensureModelCacheDirectory(path.join(linkedRoot, 'embed-model')),
    /refusing linked or non-directory model cache path/,
  );

  const linkedLeafRoot = path.join(root, 'linked-leaf-parent', '.umadev');
  const externalLeaf = path.join(root, 'external-leaf');
  fs.mkdirSync(linkedLeafRoot, { recursive: true });
  fs.mkdirSync(externalLeaf);
  fs.symlinkSync(
    externalLeaf,
    path.join(linkedLeafRoot, 'embed-model'),
    process.platform === 'win32' ? 'junction' : 'dir',
  );
  assert.throws(
    () => ensureModelCacheDirectory(path.join(linkedLeafRoot, 'embed-model')),
    /refusing linked or non-directory model cache path/,
  );
});

test('terminal contract: concurrent model transfers never share a part path', () => {
  const destination = path.join(os.tmpdir(), 'umadev-model-cache', 'model.safetensors');
  const first = modelDownloadTempPath(destination);
  const second = modelDownloadTempPath(destination);
  assert.notEqual(first, second);
  assert.ok(first.startsWith(`${destination}.part.${process.pid}.`));
  assert.ok(second.startsWith(`${destination}.part.${process.pid}.`));
});

function fakeHttpsRequest(responseFactory, callback) {
  const request = new EventEmitter();
  request.setTimeout = () => request;
  request.destroy = (error) => {
    if (error) queueMicrotask(() => request.emit('error', error));
  };
  queueMicrotask(() => callback(responseFactory()));
  return request;
}

test('terminal contract: model download rejects a declared oversized body', async (t) => {
  const original = https.get;
  https.get = (_url, _options, callback) =>
    fakeHttpsRequest(() => {
      const response = new PassThrough();
      response.statusCode = 200;
      response.headers = { 'content-length': '9' };
      return response;
    }, callback);
  t.after(() => { https.get = original; });

  await assert.rejects(
    downloadTo(
      'https://github.com/umacloud/umadev/releases/download/v1/config.json',
      path.join(os.tmpdir(), `umadev-oversized-${process.pid}`),
      false,
      '',
      null,
      8,
    ),
    /exceeds 8 bytes/,
  );
});

test('terminal contract: model download bounds a chunked body without Content-Length', async (t) => {
  const original = https.get;
  const destination = path.join(os.tmpdir(), `umadev-chunked-${process.pid}-${Date.now()}`);
  https.get = (_url, _options, callback) =>
    fakeHttpsRequest(() => {
      const response = new PassThrough();
      response.statusCode = 200;
      response.headers = {};
      queueMicrotask(() => response.end(Buffer.alloc(9)));
      return response;
    }, callback);
  t.after(() => {
    https.get = original;
    fs.rmSync(destination, { force: true });
    fs.rmSync(`${destination}.part`, { force: true });
  });

  await assert.rejects(
    downloadTo(
      'https://github.com/umacloud/umadev/releases/download/v1/config.json',
      destination,
      false,
      '',
      null,
      8,
    ),
    /exceeds 8 bytes/,
  );
});

test('terminal contract: model download refuses symlink temp and destination paths', async (t) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'umadev-model-symlink-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const protectedFile = path.join(root, 'protected.txt');
  const destination = path.join(root, 'config.json');
  const forcedTemp = path.join(root, 'forced.part');
  fs.writeFileSync(protectedFile, 'do-not-touch');
  try {
    fs.symlinkSync(protectedFile, forcedTemp, 'file');
  } catch (error) {
    if (process.platform === 'win32' && error && error.code === 'EPERM') {
      t.skip('Windows runner did not grant symlink creation');
      return;
    }
    throw error;
  }

  const original = https.get;
  https.get = (_url, _options, callback) =>
    fakeHttpsRequest(() => {
      const response = new PassThrough();
      response.statusCode = 200;
      response.headers = { 'content-length': '2' };
      queueMicrotask(() => response.end('{}'));
      return response;
    }, callback);
  t.after(() => { https.get = original; });

  await assert.rejects(
    downloadTo(
      'https://github.com/umacloud/umadev/releases/download/v1/config.json',
      destination,
      false,
      '',
      null,
      8,
      0,
      Date.now() + 1000,
      forcedTemp,
    ),
    /EEXIST/,
  );
  assert.equal(fs.readFileSync(protectedFile, 'utf8'), 'do-not-touch');

  fs.rmSync(forcedTemp, { force: true });
  fs.symlinkSync(protectedFile, destination, 'file');
  await assert.rejects(
    downloadTo(
      'https://github.com/umacloud/umadev/releases/download/v1/config.json',
      destination,
      false,
      '',
      null,
      8,
    ),
    /refusing to replace non-regular model cache entry/,
  );
  assert.equal(fs.readFileSync(protectedFile, 'utf8'), 'do-not-touch');
});

test('terminal contract: model download redirect chain is bounded', async (t) => {
  const original = https.get;
  let requests = 0;
  https.get = (url, _options, callback) =>
    fakeHttpsRequest(() => {
      requests += 1;
      const response = new PassThrough();
      response.statusCode = 302;
      response.headers = { location: url };
      return response;
    }, callback);
  t.after(() => { https.get = original; });

  await assert.rejects(
    downloadTo(
      'https://github.com/umacloud/umadev/releases/download/v1/model.safetensors',
      path.join(os.tmpdir(), `umadev-redirect-${process.pid}`),
      false,
      '',
    ),
    /too many model download redirects/,
  );
  assert.equal(requests, MAX_MODEL_REDIRECTS + 1);
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
    const manifest = JSON.stringify(trustedUpdateManifest(expected));
    fs.writeFileSync(
      manager,
      `@echo off\r\n` +
        `if "%1"=="--version" (echo 9.9.9& exit /b 0)\r\n` +
        `if "%1"=="view" (echo ${manifest}& exit /b 0)\r\n` +
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
    assert.match(
      text,
      /npm install -g umadev@999\.0\.0 --registry=https:\/\/registry\.npmjs\.org --force/,
    );
    assert.match(text, /where umadev/);
    assert.doesNotMatch(text, /upgraded and verified|repaired and verified/);
  },
);
