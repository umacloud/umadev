#!/usr/bin/env node

import { execFileSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

const PLATFORM_BINARIES = new Map([
  ["@umatech/cli-darwin-arm64", "bin/umadev"],
  ["@umatech/cli-darwin-x64", "bin/umadev"],
  ["@umatech/cli-linux-x64", "bin/umadev"],
  ["@umatech/cli-linux-arm64", "bin/umadev"],
  ["@umatech/cli-linux-musl-x64", "bin/umadev"],
  ["@umatech/cli-linux-musl-arm64", "bin/umadev"],
  ["@umatech/cli-win32-x64", "bin/umadev.exe"],
]);

export const RELEASE_PACKAGE_NAMES = Object.freeze([
  ...PLATFORM_BINARIES.keys(),
  "@umatech/knowledge",
  "@umatech/umadev",
]);

const MAIN_OPTIONAL_DEPENDENCIES = Object.freeze([
  ...PLATFORM_BINARIES.keys(),
  "@umatech/knowledge",
]);

const MAIN_FILES = new Set([
  "LICENSE",
  "README.md",
  "bin/cli.js",
  "bin/cli-main.js",
  "package.json",
]);

const BANNED_PAYLOAD_NAMES = /^(?:setup\.mjs|math_init\.js|math\.js|router_runtime\.js)$/i;
const RELEASE_REPOSITORY = "git+https://github.com/umacloud/umadev.git";

function fail(message) {
  throw new Error(`release-package-contract: ${message}`);
}

function sorted(values) {
  return [...values].sort((left, right) => left.localeCompare(right));
}

function sameMembers(actual, expected) {
  const left = sorted(actual);
  const right = sorted(expected);
  return left.length === right.length && left.every((value, index) => value === right[index]);
}

function normalizedEntries(rawEntries) {
  const entries = [];
  for (const raw of rawEntries) {
    if (!raw || raw.endsWith("/")) continue;
    if (!raw.startsWith("package/")) fail(`tar entry escapes package root: ${raw}`);
    const entry = raw.slice("package/".length);
    if (
      !entry ||
      entry.startsWith("/") ||
      entry.includes("\\") ||
      entry.split("/").some((part) => part === "" || part === "." || part === "..")
    ) {
      fail(`unsafe tar entry: ${raw}`);
    }
    if (BANNED_PAYLOAD_NAMES.test(path.posix.basename(entry))) {
      fail(`known payload filename is forbidden: ${entry}`);
    }
    entries.push(entry);
  }
  return entries;
}

function validateManifest(manifest, expectedVersion) {
  if (!manifest || typeof manifest !== "object" || Array.isArray(manifest)) {
    fail("package.json is not an object");
  }
  if (!RELEASE_PACKAGE_NAMES.includes(manifest.name)) {
    fail(`unexpected package name ${JSON.stringify(manifest.name)}`);
  }
  if (manifest.version !== expectedVersion) {
    fail(`${manifest.name} version ${JSON.stringify(manifest.version)} != ${expectedVersion}`);
  }
  if (manifest.repository?.url !== RELEASE_REPOSITORY) {
    fail(`${manifest.name} must identify the audited GitHub repository`);
  }
  // UmaDev's npm packages are inert launchers, native binaries, or Markdown.
  // There is no legitimate install-time code path. Requiring the field to be
  // absent turns the exact 1.0.74 `preinstall: node setup.mjs` compromise into
  // a hard failure instead of relying on a blacklist of hook names.
  if (Object.hasOwn(manifest, "scripts")) {
    fail(`${manifest.name} must not declare package scripts`);
  }
  if (manifest.publishConfig?.provenance === false) {
    fail(`${manifest.name} must not disable npm provenance`);
  }
}

function validateMain(manifest, entries, expectedVersion) {
  if (!sameMembers(entries, MAIN_FILES)) {
    fail(`umadev files changed; expected ${sorted(MAIN_FILES).join(", ")}, got ${sorted(entries).join(", ")}`);
  }
  if (!manifest.bin || Object.keys(manifest.bin).length !== 1 || manifest.bin.umadev !== "bin/cli.js") {
    fail("umadev must expose only bin/cli.js as the umadev launcher");
  }
  const optional = manifest.optionalDependencies;
  if (!optional || typeof optional !== "object" || Array.isArray(optional)) {
    fail("umadev optionalDependencies are missing");
  }
  if (!sameMembers(Object.keys(optional), MAIN_OPTIONAL_DEPENDENCIES)) {
    fail("umadev optionalDependencies no longer name exactly the eight release dependencies");
  }
  for (const name of MAIN_OPTIONAL_DEPENDENCIES) {
    if (optional[name] !== expectedVersion) {
      fail(`umadev optional dependency ${name} is ${JSON.stringify(optional[name])}, expected ${expectedVersion}`);
    }
  }
}

function validatePlatform(name, entries) {
  const binary = PLATFORM_BINARIES.get(name);
  const expected = new Set([
    "LICENSE",
    binary,
    "bin/umadev.provenance.json",
    "package.json",
  ]);
  if (!sameMembers(entries, expected)) {
    fail(`${name} files changed; expected ${sorted(expected).join(", ")}, got ${sorted(entries).join(", ")}`);
  }
}

function validateKnowledge(entries) {
  if (!entries.includes("LICENSE") || !entries.includes("package.json")) {
    fail("@umatech/knowledge is missing LICENSE or package.json");
  }
  const documents = entries.filter((entry) => entry !== "LICENSE" && entry !== "package.json");
  if (documents.length === 0 || documents.some((entry) => !entry.endsWith(".md"))) {
    fail("@umatech/knowledge may contain only Markdown documents, LICENSE, and package.json");
  }
}

export function validatePackageRecord(record, expectedVersion) {
  validateManifest(record.manifest, expectedVersion);
  const entries = normalizedEntries(record.entries);
  if (new Set(entries).size !== entries.length) {
    fail(`${record.manifest.name} contains duplicate tar entries`);
  }
  if (record.manifest.name === "@umatech/umadev") {
    validateMain(record.manifest, entries, expectedVersion);
  } else if (record.manifest.name === "@umatech/knowledge") {
    validateKnowledge(entries);
  } else {
    validatePlatform(record.manifest.name, entries);
  }
  return record.manifest.name;
}

export function validateReleaseRecords(records, expectedVersion) {
  const names = records.map((record) => validatePackageRecord(record, expectedVersion));
  if (!sameMembers(names, RELEASE_PACKAGE_NAMES)) {
    fail(`release set must contain each of the nine packages exactly once; got ${sorted(names).join(", ")}`);
  }
}

function readTarball(tarball) {
  const options = { encoding: "utf8", maxBuffer: 16 * 1024 * 1024 };
  const manifestText = execFileSync("tar", ["-xOf", tarball, "package/package.json"], options);
  const listing = execFileSync("tar", ["-tzf", tarball], options);
  return {
    manifest: JSON.parse(manifestText),
    entries: listing.split(/\r?\n/).filter(Boolean),
  };
}

function main(argv) {
  const [expectedVersion, ...tarballs] = argv;
  if (!/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/.test(expectedVersion || "") || tarballs.length === 0) {
    fail("usage: release-package-contract.mjs <version> <package.tgz>...");
  }
  validateReleaseRecords(tarballs.map(readTarball), expectedVersion);
  console.log(`release-package-contract: nine inert packages match ${expectedVersion}`);
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    main(process.argv.slice(2));
  } catch (error) {
    console.error(error instanceof Error ? error.message : String(error));
    process.exitCode = 1;
  }
}
