#!/usr/bin/env node

import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const SCRIPT_DIR = path.dirname(fileURLToPath(import.meta.url));
const DEFAULT_NPM_ROOT = path.resolve(SCRIPT_DIR, "..");

export const PROVENANCE_FILE = "umadev.provenance.json";
export const PROVENANCE_KIND = "umadev-npm-binary-provenance";
export const PROVENANCE_SCHEMA_VERSION = 1;

export const PLATFORM_CONTRACTS = Object.freeze({
  "darwin-arm64": Object.freeze({
    directory: "cli-darwin-arm64",
    package: "@umatech/cli-darwin-arm64",
    target: "aarch64-apple-darwin",
    binary: "bin/umadev",
  }),
  "darwin-x64": Object.freeze({
    directory: "cli-darwin-x64",
    package: "@umatech/cli-darwin-x64",
    target: "x86_64-apple-darwin",
    binary: "bin/umadev",
  }),
  "linux-x64": Object.freeze({
    directory: "cli-linux-x64",
    package: "@umatech/cli-linux-x64",
    target: "x86_64-unknown-linux-gnu",
    binary: "bin/umadev",
  }),
  "linux-arm64": Object.freeze({
    directory: "cli-linux-arm64",
    package: "@umatech/cli-linux-arm64",
    target: "aarch64-unknown-linux-gnu",
    binary: "bin/umadev",
  }),
  "linux-musl-x64": Object.freeze({
    directory: "cli-linux-musl-x64",
    package: "@umatech/cli-linux-musl-x64",
    target: "x86_64-unknown-linux-musl",
    binary: "bin/umadev",
  }),
  "linux-musl-arm64": Object.freeze({
    directory: "cli-linux-musl-arm64",
    package: "@umatech/cli-linux-musl-arm64",
    target: "aarch64-unknown-linux-musl",
    binary: "bin/umadev",
  }),
  "win32-x64": Object.freeze({
    directory: "cli-win32-x64",
    package: "@umatech/cli-win32-x64",
    target: "x86_64-pc-windows-msvc",
    binary: "bin/umadev.exe",
  }),
});

const REQUIRED_KEYS = Object.freeze([
  "binary",
  "commit",
  "kind",
  "package",
  "platform",
  "schemaVersion",
  "sha256",
  "tag",
  "target",
  "version",
]);

function fail(message) {
  throw new Error(message);
}

function readJson(file, label) {
  let parsed;
  try {
    parsed = JSON.parse(fs.readFileSync(file, "utf8"));
  } catch (error) {
    fail(`${label}: cannot read valid JSON (${error.message})`);
  }
  return parsed;
}

function normalizeCommit(commit, label = "commit") {
  const value = String(commit ?? "").toLowerCase();
  if (!/^(?:[0-9a-f]{40}|[0-9a-f]{64})$/.test(value)) {
    fail(`${label}: expected a full 40- or 64-character Git object id`);
  }
  return value;
}

function validateVersion(version, label = "version") {
  const value = String(version ?? "");
  if (!/^\d+\.\d+\.\d+$/.test(value)) {
    fail(`${label}: expected a stable x.y.z version`);
  }
  return value;
}

function digest(file) {
  return crypto.createHash("sha256").update(fs.readFileSync(file)).digest("hex");
}

function contractFor(platform) {
  const contract = PLATFORM_CONTRACTS[platform];
  if (!contract) fail(`unsupported platform ${platform}`);
  return contract;
}

function packageState(root, platform) {
  const contract = contractFor(platform);
  const packageRoot = path.join(root, contract.directory);
  const packageJson = path.join(packageRoot, "package.json");
  const manifest = readJson(packageJson, `${platform} package.json`);
  if (manifest.name !== contract.package) {
    fail(`${platform}: package name ${JSON.stringify(manifest.name)} != ${contract.package}`);
  }
  const version = validateVersion(manifest.version, `${platform} package version`);
  const binaryFile = path.join(packageRoot, contract.binary);
  if (!fs.existsSync(binaryFile)) fail(`${platform}: missing ${contract.binary}`);
  const binaryStat = fs.lstatSync(binaryFile);
  if (!binaryStat.isFile() || binaryStat.isSymbolicLink()) {
    fail(`${platform}: ${contract.binary} must be a regular file, not a link`);
  }
  return { contract, packageRoot, version, binaryFile };
}

function validateManifestShape(manifest, platform) {
  if (!manifest || typeof manifest !== "object" || Array.isArray(manifest)) {
    fail(`${platform}: provenance must be a JSON object`);
  }
  const actual = Object.keys(manifest).sort();
  const expected = [...REQUIRED_KEYS].sort();
  if (JSON.stringify(actual) !== JSON.stringify(expected)) {
    fail(`${platform}: provenance keys changed; expected ${expected.join(", ")}`);
  }
}

export function writePlatformProvenance({
  root = DEFAULT_NPM_ROOT,
  platform,
  tag,
  commit,
}) {
  const absoluteRoot = path.resolve(root);
  const { contract, packageRoot, version, binaryFile } = packageState(absoluteRoot, platform);
  const normalizedCommit = normalizeCommit(commit);
  const normalizedTag = String(tag ?? "");
  if (normalizedTag !== "local" && normalizedTag !== `v${version}`) {
    fail(`${platform}: tag ${JSON.stringify(normalizedTag)} must be local or v${version}`);
  }

  const provenance = {
    kind: PROVENANCE_KIND,
    schemaVersion: PROVENANCE_SCHEMA_VERSION,
    package: contract.package,
    platform,
    target: contract.target,
    binary: contract.binary,
    version,
    tag: normalizedTag,
    commit: normalizedCommit,
    sha256: digest(binaryFile),
  };
  const output = path.join(packageRoot, "bin", PROVENANCE_FILE);
  const temporary = `${output}.tmp-${process.pid}-${crypto.randomBytes(6).toString("hex")}`;
  try {
    fs.writeFileSync(temporary, `${JSON.stringify(provenance, null, 2)}\n`, {
      encoding: "utf8",
      mode: 0o644,
      flag: "wx",
    });
    fs.renameSync(temporary, output);
  } finally {
    fs.rmSync(temporary, { force: true });
  }
  return { output, provenance };
}

export function verifyPlatformProvenance({
  root = DEFAULT_NPM_ROOT,
  platform,
  expectedVersion,
  expectedTag,
  expectedCommit,
}) {
  const absoluteRoot = path.resolve(root);
  const { contract, packageRoot, version, binaryFile } = packageState(absoluteRoot, platform);
  const wantedVersion = validateVersion(expectedVersion, "expected version");
  const wantedCommit = normalizeCommit(expectedCommit, "expected commit");
  const wantedTag = String(expectedTag ?? "");
  if (wantedTag !== `v${wantedVersion}`) {
    fail(`expected tag ${JSON.stringify(wantedTag)} != v${wantedVersion}`);
  }
  if (version !== wantedVersion) {
    fail(`${platform}: package version ${version} != ${wantedVersion}`);
  }

  const provenanceFile = path.join(packageRoot, "bin", PROVENANCE_FILE);
  if (!fs.existsSync(provenanceFile)) fail(`${platform}: missing bin/${PROVENANCE_FILE}`);
  const provenanceStat = fs.lstatSync(provenanceFile);
  if (!provenanceStat.isFile() || provenanceStat.isSymbolicLink()) {
    fail(`${platform}: bin/${PROVENANCE_FILE} must be a regular file, not a link`);
  }
  const provenance = readJson(provenanceFile, `${platform} provenance`);
  validateManifestShape(provenance, platform);
  const expectedFields = {
    kind: PROVENANCE_KIND,
    schemaVersion: PROVENANCE_SCHEMA_VERSION,
    package: contract.package,
    platform,
    target: contract.target,
    binary: contract.binary,
    version: wantedVersion,
    tag: wantedTag,
    commit: wantedCommit,
  };
  for (const [field, expected] of Object.entries(expectedFields)) {
    if (provenance[field] !== expected) {
      fail(`${platform}: provenance ${field} ${JSON.stringify(provenance[field])} != ${JSON.stringify(expected)}`);
    }
  }
  if (!/^[0-9a-f]{64}$/.test(provenance.sha256)) {
    fail(`${platform}: provenance sha256 is not a lowercase SHA-256 digest`);
  }
  const actualDigest = digest(binaryFile);
  if (provenance.sha256 !== actualDigest) {
    fail(`${platform}: binary SHA-256 ${actualDigest} != staged provenance ${provenance.sha256}`);
  }
  return provenance;
}

export function verifyReleaseProvenance({
  root = DEFAULT_NPM_ROOT,
  expectedVersion,
  expectedTag,
  expectedCommit,
}) {
  const verified = [];
  for (const platform of Object.keys(PLATFORM_CONTRACTS)) {
    verified.push(verifyPlatformProvenance({
      root,
      platform,
      expectedVersion,
      expectedTag,
      expectedCommit,
    }));
  }
  return verified;
}

function usage() {
  console.error(
    "usage:\n"
      + "  release-provenance.mjs write <npm-root> <platform> <tag|local> <commit>\n"
      + "  release-provenance.mjs verify-one <npm-root> <platform> <version> <tag> <commit>\n"
      + "  release-provenance.mjs verify <npm-root> <version> <tag> <commit>",
  );
}

function main(argv) {
  const [command, root, ...args] = argv;
  if (command === "write" && args.length === 3) {
    const [platform, tag, commit] = args;
    const { output } = writePlatformProvenance({ root, platform, tag, commit });
    console.log(`release-provenance: wrote ${output}`);
    return;
  }
  if (command === "verify-one" && args.length === 4) {
    const [platform, expectedVersion, expectedTag, expectedCommit] = args;
    verifyPlatformProvenance({ root, platform, expectedVersion, expectedTag, expectedCommit });
    console.log(`release-provenance: ${platform} matches ${expectedTag}@${expectedCommit}`);
    return;
  }
  if (command === "verify" && args.length === 3) {
    const [expectedVersion, expectedTag, expectedCommit] = args;
    verifyReleaseProvenance({ root, expectedVersion, expectedTag, expectedCommit });
    console.log(`release-provenance: all seven binaries match ${expectedTag}@${expectedCommit}`);
    return;
  }
  usage();
  process.exitCode = 2;
}

const invokedAsMain = process.argv[1]
  && fs.realpathSync(process.argv[1]) === fs.realpathSync(fileURLToPath(import.meta.url));
if (invokedAsMain) {
  try {
    main(process.argv.slice(2));
  } catch (error) {
    console.error(`release-provenance: ${error.message}`);
    process.exitCode = 1;
  }
}
