const test = require('node:test');
const assert = require('node:assert/strict');
const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawn, spawnSync } = require('node:child_process');

const repoRoot = path.resolve(__dirname, '..', '..');
const unixInstaller = path.join(repoRoot, 'umadev-website', 'public', 'install.sh');
const windowsInstaller = path.join(repoRoot, 'umadev-website', 'public', 'install.ps1');

function executable(file, body) {
  fs.writeFileSync(file, body, { mode: 0o755 });
}

function fixture({ candidate, existing, version = '1.2.3', latest = false }) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'umadev-installer-contract-'));
  const mockBin = path.join(root, 'mock-bin');
  const installDir = path.join(root, 'install 路径');
  const asset = path.join(root, 'release-asset');
  const checksum = path.join(root, 'release-asset.sha256');
  const curlLog = path.join(root, 'curl.log');
  fs.mkdirSync(mockBin);
  fs.mkdirSync(installDir);
  executable(asset, candidate);
  const digest = crypto.createHash('sha256').update(fs.readFileSync(asset)).digest('hex');
  fs.writeFileSync(checksum, `${digest}  umadev-aarch64-apple-darwin\n`);

  executable(
    path.join(mockBin, 'uname'),
    `#!/bin/sh
case "$1" in
  -s) printf 'Darwin\\n' ;;
  -m) printf 'arm64\\n' ;;
  *) exit 2 ;;
esac
`,
  );
  executable(
    path.join(mockBin, 'curl'),
    `#!/bin/sh
out=''
write_out=''
url=''
head_only=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o|--output) out="$2"; shift 2 ;;
    --write-out) write_out="$2"; shift 2 ;;
    -I|--head|-*I*) head_only=1; shift ;;
    http://*|https://*) url="$1"; shift ;;
    *) shift ;;
  esac
done
printf '%s\\n' "$url" >> "$MOCK_CURL_LOG"
if [ "$head_only" -eq 1 ]; then
  printf 'HTTP/2 302\\r\\nlocation: %s\\r\\n\\r\\n' "$MOCK_EFFECTIVE_URL"
  exit 0
fi
case "$url" in
  *.sha256) cp "$MOCK_CHECKSUM" "$out" ;;
  *) cp "$MOCK_ASSET" "$out" ;;
esac
if [ -n "$write_out" ]; then
  printf '%s' "$MOCK_EFFECTIVE_URL"
fi
`,
  );

  if (existing !== undefined) {
    executable(path.join(installDir, 'umadev'), existing);
  }

  const env = {
    ...process.env,
    PATH: `${mockBin}${path.delimiter}${process.env.PATH}`,
    HOME: root,
    UMADEV_INSTALL_DIR: installDir,
    MOCK_ASSET: asset,
    MOCK_CHECKSUM: checksum,
    MOCK_CURL_LOG: curlLog,
    MOCK_EFFECTIVE_URL: `https://github.com/umacloud/umadev/releases/download/v${version}/umadev-aarch64-apple-darwin`,
  };
  if (!latest) env.UMADEV_VERSION = version;
  else delete env.UMADEV_VERSION;

  return {
    root,
    asset,
    checksum,
    installDir,
    dest: path.join(installDir, 'umadev'),
    curlLog,
    env,
    run() {
      return spawnSync('bash', [unixInstaller], { env, encoding: 'utf8' });
    },
    cleanup() {
      fs.rmSync(root, { recursive: true, force: true });
    },
  };
}

function runInstallerAsync(env) {
  return new Promise((resolve, reject) => {
    const child = spawn('bash', [unixInstaller], { env, encoding: 'utf8' });
    let stdout = '';
    let stderr = '';
    child.stdout.on('data', (chunk) => { stdout += chunk; });
    child.stderr.on('data', (chunk) => { stderr += chunk; });
    child.once('error', reject);
    child.once('close', (status, signal) => resolve({ status, signal, stdout, stderr }));
  });
}

async function waitForFile(file, timeoutMs = 5000) {
  const deadline = Date.now() + timeoutMs;
  while (!fs.existsSync(file)) {
    if (Date.now() >= deadline) throw new Error(`timed out waiting for ${file}`);
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
}

const oldBinary = `#!/bin/sh
printf 'umadev 0.9.0\\n'
`;

test('native installer verifies and atomically replaces a previous Unix binary', () => {
  const f = fixture({
    candidate: `#!/bin/sh
printf 'umadev 1.2.3\\n'
`,
    existing: oldBinary,
  });
  try {
    const result = f.run();
    assert.equal(result.status, 0, result.stderr);
    assert.match(result.stdout, /Installed: .* \(v1\.2\.3\)/);
    assert.match(fs.readFileSync(f.dest, 'utf8'), /umadev 1\.2\.3/);
    assert.deepEqual(
      fs.readdirSync(f.installDir).filter((name) => name.startsWith('.umadev-')),
      [],
    );
  } finally {
    f.cleanup();
  }
});

test('wrong downloaded version never changes the existing Unix binary', () => {
  const f = fixture({
    candidate: `#!/bin/sh
printf 'umadev 1.2.4\\n'
`,
    existing: oldBinary,
  });
  try {
    const before = fs.readFileSync(f.dest);
    const result = f.run();
    assert.notEqual(result.status, 0);
    assert.doesNotMatch(result.stdout, /Installed:/);
    assert.deepEqual(fs.readFileSync(f.dest), before);
    assert.match(result.stderr, /expected UmaDev 1\.2\.3/);
  } finally {
    f.cleanup();
  }
});

test('a hanging Unix candidate version probe is terminated on time', () => {
  const f = fixture({
    candidate: `#!/bin/sh
sleep 60
printf 'umadev 1.2.3\\n'
`,
    existing: oldBinary,
  });
  try {
    const before = fs.readFileSync(f.dest);
    const started = Date.now();
    const result = f.run();
    const elapsed = Date.now() - started;
    assert.notEqual(result.status, 0);
    assert.ok(elapsed >= 9000 && elapsed < 15000, `probe ended after ${elapsed}ms`);
    assert.match(result.stderr, /candidate --version timed out after 10s/);
    assert.deepEqual(fs.readFileSync(f.dest), before);
  } finally {
    f.cleanup();
  }
});

test('an excessive Unix candidate version response is bounded', () => {
  const f = fixture({
    candidate: `#!/bin/sh
awk 'BEGIN { for (i = 0; i < 5000; i++) printf "x" }'
`,
    existing: oldBinary,
  });
  try {
    const before = fs.readFileSync(f.dest);
    const result = f.run();
    assert.notEqual(result.status, 0);
    // A completely descheduled probe can reach the total timeout before the
    // installer observes the file. Both outcomes are bounded and fail closed;
    // under ordinary scheduling the live byte poll reports the precise cap.
    assert.match(result.stderr, /candidate --version (?:output exceeded 4096 bytes|timed out after 10s)/);
    assert.deepEqual(fs.readFileSync(f.dest), before);
  } finally {
    f.cleanup();
  }
});

test('a successful Unix version probe reaps descendants before installation succeeds', () => {
  const f = fixture({
    candidate: `#!/bin/sh
(trap '' TERM; while :; do sleep 1; done) &
printf '%s\\n' "$!" > "$DESCENDANT_PID_FILE"
printf 'umadev 1.2.3\\n'
`,
    existing: oldBinary,
  });
  const descendantPidFile = path.join(f.root, 'descendant.pid');
  f.env.DESCENDANT_PID_FILE = descendantPidFile;
  let descendantPid = 0;
  try {
    const result = f.run();
    assert.equal(result.status, 0, result.stderr);
    descendantPid = Number(fs.readFileSync(descendantPidFile, 'utf8'));
    const ps = spawnSync('ps', ['-o', 'stat=', '-p', String(descendantPid)], { encoding: 'utf8' });
    const state = String(ps.stdout || '').trim();
    assert.ok(!state || state.startsWith('Z'), `version-probe descendant survived with state ${state}`);
  } finally {
    if (descendantPid > 0) {
      try { process.kill(descendantPid, 'SIGKILL'); } catch { /* already reaped */ }
    }
    f.cleanup();
  }
});

test('failed post-install verification restores the previous Unix binary', () => {
  const f = fixture({
    candidate: `#!/bin/sh
if [ "$0" = "$UMADEV_INSTALL_DIR/umadev" ]; then
  printf 'umadev 9.9.9\\n'
else
  printf 'umadev 1.2.3\\n'
fi
`,
    existing: oldBinary,
  });
  try {
    const before = fs.readFileSync(f.dest);
    const result = f.run();
    assert.notEqual(result.status, 0);
    assert.doesNotMatch(result.stdout, /Installed:/);
    assert.deepEqual(fs.readFileSync(f.dest), before);
    assert.match(result.stderr, /previous binary was restored/);
  } finally {
    f.cleanup();
  }
});

test('failed first-install verification leaves no Unix binary behind', () => {
  const f = fixture({
    candidate: `#!/bin/sh
if [ "$0" = "$UMADEV_INSTALL_DIR/umadev" ]; then
  exit 9
else
  printf 'umadev 1.2.3\\n'
fi
`,
  });
  try {
    const result = f.run();
    assert.notEqual(result.status, 0);
    assert.doesNotMatch(result.stdout, /Installed:/);
    assert.equal(fs.existsSync(f.dest), false);
    assert.match(result.stderr, /incomplete first install was removed/);
  } finally {
    f.cleanup();
  }
});

test('latest Unix install pins checksum and runtime checks to the resolved tag', () => {
  const f = fixture({
    candidate: `#!/bin/sh
printf 'umadev 1.2.3\\n'
`,
    latest: true,
  });
  try {
    const result = f.run();
    assert.equal(result.status, 0, result.stderr);
    const requests = fs.readFileSync(f.curlLog, 'utf8');
    assert.match(requests, /releases\/latest\/download\/umadev-aarch64-apple-darwin/);
    assert.match(
      requests,
      /releases\/download\/v1\.2\.3\/umadev-aarch64-apple-darwin(?:\n|$)/,
    );
    assert.match(
      requests,
      /releases\/download\/v1\.2\.3\/umadev-aarch64-apple-darwin\.sha256/,
    );
  } finally {
    f.cleanup();
  }
});

test('concurrent Unix installers serialize replacement and rollback', async () => {
  const marker = path.join(os.tmpdir(), `umadev-installer-hold-${process.pid}-${Date.now()}`);
  const first = fixture({
    candidate: `#!/bin/sh
if [ "$0" = "$UMADEV_INSTALL_DIR/umadev" ]; then
  : > "$MOCK_INSTALL_MARKER"
  sleep 1
fi
printf 'umadev 1.2.3\\n'
`,
    existing: oldBinary,
  });
  const secondAsset = path.join(first.root, 'release-asset-1.2.4');
  const secondChecksum = `${secondAsset}.sha256`;
  executable(secondAsset, `#!/bin/sh
printf 'umadev 1.2.4\\n'
`);
  const digest = crypto.createHash('sha256').update(fs.readFileSync(secondAsset)).digest('hex');
  fs.writeFileSync(secondChecksum, `${digest}  umadev-aarch64-apple-darwin\n`);

  try {
    const firstRun = runInstallerAsync({ ...first.env, MOCK_INSTALL_MARKER: marker });
    await waitForFile(marker);
    const secondRun = runInstallerAsync({
      ...first.env,
      UMADEV_VERSION: '1.2.4',
      MOCK_ASSET: secondAsset,
      MOCK_CHECKSUM: secondChecksum,
      MOCK_EFFECTIVE_URL: 'https://github.com/umacloud/umadev/releases/download/v1.2.4/umadev-aarch64-apple-darwin',
    });
    const [a, b] = await Promise.all([firstRun, secondRun]);
    assert.equal(a.status, 0, a.stderr);
    assert.equal(b.status, 0, b.stderr);
    assert.match(fs.readFileSync(first.dest, 'utf8'), /umadev 1\.2\.4/);
    assert.deepEqual(
      fs.readdirSync(first.installDir).filter((name) => name.startsWith('.umadev-')),
      [],
    );
  } finally {
    fs.rmSync(marker, { force: true });
    first.cleanup();
  }
});

test('Windows installer keeps replacement transactional and delays success output', () => {
  const source = fs.readFileSync(windowsInstaller, 'utf8');
  const downloadedCheck = source.indexOf("-Phase 'downloaded binary verification'");
  const stagedCheck = source.indexOf("-Phase 'staged binary verification'");
  const replace = source.indexOf('[System.IO.File]::Replace($stage, $dest, $backup, $true)');
  const installedCheck = source.indexOf("-Phase 'installed binary verification'");
  const success = source.indexOf('Write-Host "Installed: $dest (v$version)"');

  assert.ok(downloadedCheck > 0);
  assert.ok(stagedCheck > downloadedCheck);
  assert.ok(replace > stagedCheck);
  assert.ok(installedCheck > replace);
  assert.ok(success > installedCheck);
  assert.match(source, /previous binary was restored automatically/);
  assert.match(source, /incomplete first install was removed/);
  assert.match(source, /Close UmaDev, VS Code, Zcode, Codex/);
  assert.match(source, /FileShare\]::None/);
  assert.match(source, /another UmaDev installer is updating/);
});

test('native installers bound redirects, duration, and actual response bytes', () => {
  const unix = fs.readFileSync(unixInstaller, 'utf8');
  for (const contract of [
    'MAX_BINARY_BYTES=536870912',
    'MAX_CHECKSUM_BYTES=4096',
    '--connect-timeout',
    '--max-time',
    '--retry-max-time',
    '--max-redirs',
    '--max-filesize',
    "%{url_effective}",
    'release-assets.githubusercontent.com',
    'BINARY_VERSION_TIMEOUT=10',
    'MAX_BINARY_VERSION_OUTPUT_BYTES=4096',
    'candidate --version timed out',
  ]) {
    assert.match(unix, new RegExp(contract.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')));
  }
  assert.match(unix, /wc -c < "\$destination"/);
  assert.match(unix, /\.umadev-install\.lock/);
  assert.match(unix, /kill -0 "\$owner_pid"/);
  assert.match(unix, /output_exceeded=1/);
  assert.match(unix, /kill -KILL -- "-\$version_probe_pid"/);

  const windows = fs.readFileSync(windowsInstaller, 'utf8');
  assert.match(windows, /AllowAutoRedirect = \$false/);
  assert.match(windows, /ResponseHeadersRead/);
  assert.match(windows, /\$total -gt \$MaxBytes/);
  assert.match(windows, /CancellationTokenSource/);
  assert.match(windows, /WaitForExit\(\$binaryVersionTimeoutMilliseconds\)/);
  assert.match(windows, /maxBinaryVersionOutputChars = 4096/);
  assert.match(windows, /candidate --version timed out/);
  assert.match(windows, /too many redirects/);
  assert.match(windows, /release-assets\.githubusercontent\.com/);
});
