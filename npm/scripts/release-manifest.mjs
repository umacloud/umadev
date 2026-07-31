#!/usr/bin/env node

import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

export const RELEASE_MANIFEST_NAME = "umadev-release-manifest.json";
const SCHEMA_VERSION = 1;
const MAX_MANIFEST_BYTES = 1024 * 1024;
const MAX_CHECKSUM_SIDECAR_BYTES = 4096;
const TAG_RE = /^v(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/;
const COMMIT_RE = /^(?:[0-9a-f]{40}|[0-9a-f]{64})$/;
const SHA256_RE = /^[0-9a-f]{64}$/;

function assertRegularFile(file, label = file) {
  const stat = fs.lstatSync(file);
  if (!stat.isFile() || stat.isSymbolicLink()) {
    throw new Error(`${label} must be a regular file, not a link`);
  }
  return stat;
}

function sha256File(file) {
  assertRegularFile(file);
  const hash = crypto.createHash("sha256");
  const descriptor = fs.openSync(file, "r");
  const buffer = Buffer.allocUnsafe(1024 * 1024);
  try {
    for (;;) {
      const count = fs.readSync(descriptor, buffer, 0, buffer.length, null);
      if (count === 0) break;
      hash.update(count === buffer.length ? buffer : buffer.subarray(0, count));
    }
  } finally {
    fs.closeSync(descriptor);
  }
  return hash.digest("hex");
}

function validateIdentity(tag, commit) {
  if (!TAG_RE.test(tag)) throw new Error(`invalid release tag: ${tag}`);
  if (!COMMIT_RE.test(commit)) throw new Error(`invalid release commit: ${commit}`);
}

function assertManifestShape(manifest) {
  if (!manifest || typeof manifest !== "object" || Array.isArray(manifest)) {
    throw new Error("release manifest must be an object");
  }
  const keys = Object.keys(manifest).sort();
  const expected = ["assets", "commit", "schemaVersion", "tag", "version"];
  if (JSON.stringify(keys) !== JSON.stringify(expected)) {
    throw new Error(`release manifest keys changed: ${keys.join(", ")}`);
  }
  if (manifest.schemaVersion !== SCHEMA_VERSION) {
    throw new Error(`unsupported release manifest schema: ${manifest.schemaVersion}`);
  }
  validateIdentity(manifest.tag, manifest.commit);
  if (manifest.version !== manifest.tag.slice(1)) {
    throw new Error("release manifest version does not match tag");
  }
  if (!manifest.assets || typeof manifest.assets !== "object" || Array.isArray(manifest.assets)) {
    throw new Error("release manifest assets must be an object");
  }
  const names = Object.keys(manifest.assets);
  if (names.length === 0 || JSON.stringify(names) !== JSON.stringify([...names].sort())) {
    throw new Error("release manifest asset names must be non-empty and sorted");
  }
  for (const name of names) {
    if (path.basename(name) !== name || name === RELEASE_MANIFEST_NAME || name === `${RELEASE_MANIFEST_NAME}.sha256`) {
      throw new Error(`invalid release manifest asset name: ${name}`);
    }
    if (!SHA256_RE.test(manifest.assets[name])) {
      throw new Error(`invalid release manifest SHA-256 for ${name}`);
    }
  }
  return names;
}

export function createReleaseManifest({ output, tag, commit, files }) {
  validateIdentity(tag, commit);
  if (!Array.isArray(files) || files.length === 0) throw new Error("no release assets supplied");
  const entries = [];
  const seen = new Set();
  for (const file of files) {
    assertRegularFile(file, `release asset ${file}`);
    const name = path.basename(file);
    if (seen.has(name)) throw new Error(`duplicate release asset basename: ${name}`);
    if (name === RELEASE_MANIFEST_NAME || name === `${RELEASE_MANIFEST_NAME}.sha256`) {
      throw new Error(`manifest cannot hash itself: ${name}`);
    }
    seen.add(name);
    entries.push([name, sha256File(file)]);
  }
  entries.sort(([left], [right]) => (left < right ? -1 : left > right ? 1 : 0));
  const manifest = {
    schemaVersion: SCHEMA_VERSION,
    tag,
    version: tag.slice(1),
    commit,
    assets: Object.fromEntries(entries),
  };
  assertManifestShape(manifest);
  fs.mkdirSync(path.dirname(output), { recursive: true });
  const temporary = `${output}.tmp.${process.pid}.${crypto.randomBytes(8).toString("hex")}`;
  fs.writeFileSync(temporary, `${JSON.stringify(manifest, null, 2)}\n`, { flag: "wx", mode: 0o600 });
  try {
    try {
      const destination = fs.lstatSync(output);
      if (!destination.isFile() || destination.isSymbolicLink()) {
        throw new Error(`refusing to replace non-regular release manifest: ${output}`);
      }
    } catch (error) {
      if (!error || error.code !== "ENOENT") throw error;
    }
    fs.renameSync(temporary, output);
  } catch (error) {
    fs.rmSync(temporary, { force: true });
    throw error;
  }
  return manifest;
}

export function verifyReleaseManifest({ manifestFile, expectedTag, expectedCommit, assetDirectory }) {
  validateIdentity(expectedTag, expectedCommit);
  const stat = assertRegularFile(manifestFile, "release manifest");
  if (stat.size > MAX_MANIFEST_BYTES) throw new Error("release manifest is too large");
  const manifest = JSON.parse(fs.readFileSync(manifestFile, "utf8"));
  const names = assertManifestShape(manifest);
  if (manifest.tag !== expectedTag) {
    throw new Error(`release manifest tag ${manifest.tag} != ${expectedTag}`);
  }
  if (manifest.commit !== expectedCommit) {
    throw new Error(`release manifest commit ${manifest.commit} != ${expectedCommit}`);
  }

  const root = assetDirectory || path.dirname(manifestFile);
  const manifestName = path.basename(manifestFile);
  const sidecarName = `${manifestName}.sha256`;
  const expectedNames = [...names, manifestName, sidecarName].sort();
  const actualNames = fs.readdirSync(root).sort();
  if (JSON.stringify(actualNames) !== JSON.stringify(expectedNames)) {
    throw new Error(`release asset set mismatch: ${actualNames.join(", ")}`);
  }
  for (const name of names) {
    const file = path.join(root, name);
    const actual = sha256File(file);
    if (actual !== manifest.assets[name]) {
      throw new Error(`release asset SHA-256 mismatch for ${name}`);
    }
  }
  const sidecarFile = path.join(root, sidecarName);
  const sidecarStat = assertRegularFile(sidecarFile, "release manifest checksum sidecar");
  if (sidecarStat.size > MAX_CHECKSUM_SIDECAR_BYTES) {
    throw new Error("release manifest checksum sidecar is too large");
  }
  const sidecar = fs.readFileSync(sidecarFile, "utf8").trim().split(/\s+/);
  if (sidecar.length !== 2 || sidecar[1].replace(/^\*/, "") !== manifestName || !SHA256_RE.test(sidecar[0])) {
    throw new Error("invalid release manifest checksum sidecar");
  }
  const manifestHash = sha256File(manifestFile);
  if (manifestHash !== sidecar[0]) throw new Error("release manifest checksum mismatch");
  return manifest;
}

function usage() {
  console.error("usage: release-manifest.mjs create <output> <tag> <commit> <asset>... | verify <manifest> <tag> <commit> [asset-dir]");
  process.exit(2);
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const [command, manifestFile, tag, commit, ...rest] = process.argv.slice(2);
  try {
    if (command === "create" && rest.length > 0) {
      createReleaseManifest({ output: manifestFile, tag, commit, files: rest });
    } else if (command === "verify" && rest.length <= 1) {
      verifyReleaseManifest({
        manifestFile,
        expectedTag: tag,
        expectedCommit: commit,
        assetDirectory: rest[0],
      });
    } else {
      usage();
    }
  } catch (error) {
    console.error(`release-manifest: ${error.message}`);
    process.exit(1);
  }
}
