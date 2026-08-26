#!/usr/bin/env node

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(scriptDir, '..', '..');

function fail(message) {
  console.error(`version-lock: ${message}`);
  process.exit(1);
}

const cargo = fs.readFileSync(path.join(repoRoot, 'Cargo.toml'), 'utf8');
const workspacePackage = cargo.match(/\[workspace\.package\]([\s\S]*?)(?=\n\[|$)/);
const cargoVersion = workspacePackage?.[1].match(/^version\s*=\s*"([^"]+)"/m)?.[1];
if (!cargoVersion) fail('could not read [workspace.package].version from Cargo.toml');
if (!/^\d+\.\d+\.\d+$/.test(cargoVersion)) {
  fail(`release version must be stable x.y.z, got ${cargoVersion}`);
}

// Cargo.lock is part of the release input, not a cache. A hand-edited
// Cargo.toml can otherwise agree with npm while the committed lockfile still
// names the previous workspace version; an unlocked Cargo command would then
// silently rewrite it in CI and build bytes that were never committed.
const cargoLock = fs.readFileSync(path.join(repoRoot, 'Cargo.lock'), 'utf8');
const localCargoPackages = cargoLock
  .split(/^\[\[package\]\]\s*$/m)
  .slice(1)
  .filter((block) => !/^source\s*=\s*/m.test(block))
  .map((block) => ({
    name: block.match(/^name\s*=\s*"([^"]+)"/m)?.[1],
    version: block.match(/^version\s*=\s*"([^"]+)"/m)?.[1],
  }))
  .filter(({ name, version }) => name && version);
if (localCargoPackages.length === 0) fail('Cargo.lock contains no local workspace packages');
for (const { name, version } of localCargoPackages) {
  if (version !== cargoVersion) {
    fail(`Cargo.lock workspace package ${name}@${version} != Cargo ${cargoVersion}`);
  }
}

const npmRoot = path.join(repoRoot, 'npm');
const manifests = fs
  .readdirSync(npmRoot, { withFileTypes: true })
  .filter((entry) => entry.isDirectory())
  .map((entry) => path.join(npmRoot, entry.name, 'package.json'))
  .filter((file) => fs.existsSync(file));
if (manifests.length === 0) fail('no npm package manifests were found');

const packages = manifests.map((file) => ({
  file,
  manifest: JSON.parse(fs.readFileSync(file, 'utf8')),
}));
const packagesByName = new Map();
for (const { file, manifest } of packages) {
  if (!manifest.name) fail(`${path.relative(repoRoot, file)} has no package name`);
  if (packagesByName.has(manifest.name)) fail(`duplicate npm package name: ${manifest.name}`);
  packagesByName.set(manifest.name, manifest);
  if (manifest.version !== cargoVersion) {
    fail(`${manifest.name} version ${manifest.version} != Cargo ${cargoVersion}`);
  }
}

const publishedDependencies = [
  '@umatech/cli-darwin-arm64',
  '@umatech/cli-darwin-x64',
  '@umatech/cli-linux-arm64',
  '@umatech/cli-linux-musl-arm64',
  '@umatech/cli-linux-musl-x64',
  '@umatech/cli-linux-x64',
  '@umatech/cli-win32-x64',
  '@umatech/knowledge',
].sort();
const expectedManifests = [
  ...publishedDependencies,
  '@umatech/model-e5-small', // archived manifest; the model ships on GitHub Releases
  '@umatech/umadev',
].sort();
const actualManifests = [...packagesByName.keys()].sort();
if (JSON.stringify(actualManifests) !== JSON.stringify(expectedManifests)) {
  fail(
    `npm manifest set changed: expected ${expectedManifests.join(', ')}, got ${actualManifests.join(', ')}`,
  );
}

const main = packagesByName.get('@umatech/umadev');
if (!main) fail('the main umadev npm package is missing');
if (main.bin?.umadev !== 'bin/cli.js') {
  fail(`umadev bin mapping must be umadev=bin/cli.js, got ${JSON.stringify(main.bin)}`);
}
if (JSON.stringify([...(main.files ?? [])].sort()) !== JSON.stringify(['README.md', 'bin/'])) {
  fail(`umadev files must be README.md and bin/, got ${JSON.stringify(main.files)}`);
}
if (main.engines?.node !== '>=18') {
  fail(`umadev Node engine must remain >=18, got ${JSON.stringify(main.engines?.node)}`);
}

// Lock the platform metadata as tightly as the artifact matrix. A swapped os,
// cpu, libc, or files declaration can make a correct binary impossible for npm
// to select (or allow it onto an incompatible machine) while publish succeeds.
const platformContracts = new Map([
  ['@umatech/cli-darwin-arm64', { os: ['darwin'], cpu: ['arm64'] }],
  ['@umatech/cli-darwin-x64', { os: ['darwin'], cpu: ['x64'] }],
  ['@umatech/cli-linux-arm64', { os: ['linux'], cpu: ['arm64'], libc: 'glibc' }],
  ['@umatech/cli-linux-musl-arm64', { os: ['linux'], cpu: ['arm64'], libc: 'musl' }],
  ['@umatech/cli-linux-musl-x64', { os: ['linux'], cpu: ['x64'], libc: 'musl' }],
  ['@umatech/cli-linux-x64', { os: ['linux'], cpu: ['x64'], libc: 'glibc' }],
  // Windows on ARM runs the x64 build through the OS compatibility layer.
  ['@umatech/cli-win32-x64', { os: ['win32'], cpu: ['x64', 'arm64'] }],
]);
for (const [name, expected] of platformContracts) {
  const manifest = packagesByName.get(name);
  if (!manifest) fail(`platform contract has no manifest: ${name}`);
  for (const field of ['os', 'cpu']) {
    if (JSON.stringify(manifest[field]) !== JSON.stringify(expected[field])) {
      fail(`${name} ${field} ${JSON.stringify(manifest[field])} != ${JSON.stringify(expected[field])}`);
    }
  }
  if ((manifest.libc ?? undefined) !== expected.libc) {
    fail(`${name} libc ${JSON.stringify(manifest.libc)} != ${JSON.stringify(expected.libc)}`);
  }
  if (JSON.stringify(manifest.files) !== JSON.stringify(['bin/'])) {
    fail(`${name} files must be ["bin/"], got ${JSON.stringify(manifest.files)}`);
  }
  if (manifest.preferUnplugged !== true) {
    fail(`${name} must set preferUnplugged=true so the native executable is materialized`);
  }
}

const knowledge = packagesByName.get('@umatech/knowledge');
if (JSON.stringify(knowledge?.files) !== JSON.stringify(['**/*.md'])) {
  fail(`@umatech/knowledge files must be ["**/*.md"], got ${JSON.stringify(knowledge?.files)}`);
}
const archivedModel = packagesByName.get('@umatech/model-e5-small');
const expectedModelFiles = ['README.md', 'config.json', 'model.safetensors', 'tokenizer.json'];
const actualModelFiles = [...(archivedModel?.files ?? [])].sort();
if (JSON.stringify(actualModelFiles) !== JSON.stringify(expectedModelFiles)) {
  fail(
    `archived model files ${JSON.stringify(archivedModel?.files)} != ${JSON.stringify(expectedModelFiles)}`,
  );
}

const actualDependencies = Object.keys(main.optionalDependencies ?? {}).sort();
if (JSON.stringify(actualDependencies) !== JSON.stringify(publishedDependencies)) {
  fail(
    `umadev optionalDependencies changed: expected ${publishedDependencies.join(', ')}, got ${actualDependencies.join(', ')}`,
  );
}
for (const [name, version] of Object.entries(main.optionalDependencies ?? {})) {
  if (version !== cargoVersion) fail(`${name} pin ${version} != Cargo ${cargoVersion}`);
  if (!packagesByName.has(name)) fail(`${name} is pinned but has no local release manifest`);
}

const website = fs.readFileSync(
  path.join(repoRoot, 'umadev-website', 'src', 'app', 'content.ts'),
  'utf8',
);
const changelog = fs.readFileSync(path.join(repoRoot, 'CHANGELOG.md'), 'utf8');
const changelogVersion = changelog.match(/^## \[(\d+\.\d+\.\d+)\]/m)?.[1];
if (!changelogVersion) fail('could not read the latest stable CHANGELOG.md version');
if (changelogVersion !== cargoVersion) {
  fail(`CHANGELOG ${changelogVersion} != Cargo ${cargoVersion}`);
}

// Kimi compatibility is runtime capability-driven, never exact-version pinned.
// Keep current install surfaces on the unversioned package so routine upgrades do
// not contradict the driver's all-official-versions contract.
const kimiInstall = '@moonshot-ai/kimi-code';
for (const relative of [
  'README.md',
  'README.zh-CN.md',
  'README.zh-TW.md',
  'umadev-website/src/app/content.ts',
]) {
  const body = fs.readFileSync(path.join(repoRoot, relative), 'utf8');
  if (!body.includes(kimiInstall)) {
    fail(`${relative} does not advertise the version-agnostic Kimi install ${kimiInstall}`);
  }
  if (/@moonshot-ai\/kimi-code@(?:v?\d|=|\^|~)/.test(body)) {
    fail(`${relative} reintroduced a fixed Kimi CLI install version`);
  }
}
// An unreleased entry may lead each locale while a version is being prepared.
// Lock the first stable changelog entry instead of pretending in-flight work
// has already shipped under the current Cargo/npm version.
const zhVersion = website.match(
  /export const releases\s*=\s*\{\s*zh:\s*\[[\s\S]*?\bver:\s*"(\d+\.\d+\.\d+)"/,
)?.[1];
const enVersion = website.match(
  /\n\s*en:\s*\[[\s\S]*?\bver:\s*"(\d+\.\d+\.\d+)"/,
)?.[1];
if (!zhVersion || !enVersion) fail('could not read both stable website changelog versions');
if (zhVersion !== cargoVersion || enVersion !== cargoVersion) {
  fail(`website zh=${zhVersion}, en=${enVersion} != Cargo ${cargoVersion}`);
}
const currentWebsiteVersions = [...website.matchAll(/\bver:\s*"(\d+\.\d+\.\d+)"[^\n]*\bcurrent:\s*true/g)]
  .map((match) => match[1]);
if (
  currentWebsiteVersions.length !== 2
  || currentWebsiteVersions.some((version) => version !== cargoVersion)
) {
  fail(
    `website must mark exactly the zh/en ${cargoVersion} entries current, got ${currentWebsiteVersions.join(', ') || '<none>'}`,
  );
}

if (process.env.GITHUB_REF?.startsWith('refs/tags/v')) {
  const expectedTag = `v${cargoVersion}`;
  if (process.env.GITHUB_REF_NAME !== expectedTag) {
    fail(`tag ${process.env.GITHUB_REF_NAME || '<missing>'} != ${expectedTag}`);
  }
}

console.log(
  `version-lock: Cargo.toml, Cargo.lock, CHANGELOG, website, tag, nine release packages, archived model manifest, and version-agnostic Kimi install surfaces agree on ${cargoVersion}`,
);
