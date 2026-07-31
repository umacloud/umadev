import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

import {
  PLATFORM_CONTRACTS,
  PROVENANCE_FILE,
  verifyPlatformProvenance,
  verifyReleaseProvenance,
  writePlatformProvenance,
} from "./release-provenance.mjs";

const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const VERSION = "9.8.7";
const TAG = `v${VERSION}`;
const COMMIT = "a".repeat(40);

function fixture(t, tag = TAG) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "umadev-provenance-"));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  for (const [platform, contract] of Object.entries(PLATFORM_CONTRACTS)) {
    const packageRoot = path.join(root, contract.directory);
    const binary = path.join(packageRoot, contract.binary);
    fs.mkdirSync(path.dirname(binary), { recursive: true });
    fs.writeFileSync(binary, `binary:${platform}:${VERSION}\n`);
    fs.writeFileSync(
      path.join(packageRoot, "package.json"),
      `${JSON.stringify({ name: contract.package, version: VERSION }, null, 2)}\n`,
    );
    writePlatformProvenance({ root, platform, tag, commit: COMMIT });
  }
  return root;
}

test("all seven manifests bind binary bytes to version, tag, target, and commit", (t) => {
  const root = fixture(t);
  const verified = verifyReleaseProvenance({
    root,
    expectedVersion: VERSION,
    expectedTag: TAG,
    expectedCommit: COMMIT,
  });
  assert.equal(verified.length, 7);
  assert.deepEqual(new Set(verified.map(({ platform }) => platform)), new Set(Object.keys(PLATFORM_CONTRACTS)));
});

test("stage.sh atomically stages a binary and writes its release manifest", (t) => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "umadev-stage-contract-"));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const scripts = path.join(root, "npm", "scripts");
  const packageRoot = path.join(root, "npm", "cli-linux-x64");
  fs.mkdirSync(scripts, { recursive: true });
  fs.mkdirSync(packageRoot, { recursive: true });
  fs.copyFileSync(path.join(scriptDir, "stage.sh"), path.join(scripts, "stage.sh"));
  fs.copyFileSync(
    path.join(scriptDir, "release-provenance.mjs"),
    path.join(scripts, "release-provenance.mjs"),
  );
  fs.writeFileSync(
    path.join(packageRoot, "package.json"),
    `${JSON.stringify({ name: "@umacloud/cli-linux-x64", version: VERSION })}\n`,
  );
  const candidate = path.join(root, "candidate");
  fs.writeFileSync(candidate, "staged release bytes\n");

  const result = spawnSync("bash", [path.join(scripts, "stage.sh"), "linux-x64", candidate], {
    encoding: "utf8",
    env: {
      ...process.env,
      UMADEV_RELEASE_TAG: TAG,
      UMADEV_RELEASE_COMMIT: COMMIT,
    },
  });
  assert.equal(result.status, 0, result.stderr);
  assert.equal(fs.readFileSync(path.join(packageRoot, "bin", "umadev"), "utf8"), "staged release bytes\n");
  assert.ok(
    fs.existsSync(path.join(packageRoot, "bin", PROVENANCE_FILE)),
    `stage output did not contain provenance; stdout=${result.stdout}; files=${fs.readdirSync(path.join(packageRoot, "bin"))}`,
  );
  verifyPlatformProvenance({
    root: path.join(root, "npm"),
    platform: "linux-x64",
    expectedVersion: VERSION,
    expectedTag: TAG,
    expectedCommit: COMMIT,
  });
});

test("verification rejects a binary replaced after staging", (t) => {
  const root = fixture(t);
  const contract = PLATFORM_CONTRACTS["linux-x64"];
  fs.appendFileSync(path.join(root, contract.directory, contract.binary), "tampered");
  assert.throws(
    () => verifyReleaseProvenance({
      root,
      expectedVersion: VERSION,
      expectedTag: TAG,
      expectedCommit: COMMIT,
    }),
    /binary SHA-256 .* != staged provenance/,
  );
});

test("verification rejects a manifest from another commit", (t) => {
  const root = fixture(t);
  assert.throws(
    () => verifyReleaseProvenance({
      root,
      expectedVersion: VERSION,
      expectedTag: TAG,
      expectedCommit: "b".repeat(40),
    }),
    /provenance commit/,
  );
});

test("local smoke manifests cannot pass a release verification", (t) => {
  const root = fixture(t, "local");
  assert.throws(
    () => verifyReleaseProvenance({
      root,
      expectedVersion: VERSION,
      expectedTag: TAG,
      expectedCommit: COMMIT,
    }),
    /provenance tag "local"/,
  );
});

test("verification rejects schema drift and unreviewed fields", (t) => {
  const root = fixture(t);
  const contract = PLATFORM_CONTRACTS["win32-x64"];
  const provenanceFile = path.join(root, contract.directory, "bin", PROVENANCE_FILE);
  const provenance = JSON.parse(fs.readFileSync(provenanceFile, "utf8"));
  provenance.unsignedNote = "ignore me";
  fs.writeFileSync(provenanceFile, `${JSON.stringify(provenance)}\n`);
  assert.throws(
    () => verifyReleaseProvenance({
      root,
      expectedVersion: VERSION,
      expectedTag: TAG,
      expectedCommit: COMMIT,
    }),
    /provenance keys changed/,
  );
});

test("verification rejects a linked binary even when its target has the expected bytes", (t) => {
  const root = fixture(t);
  const contract = PLATFORM_CONTRACTS["darwin-x64"];
  const binary = path.join(root, contract.directory, contract.binary);
  const target = `${binary}.target`;
  fs.renameSync(binary, target);
  fs.symlinkSync(target, binary);
  assert.throws(
    () => verifyReleaseProvenance({
      root,
      expectedVersion: VERSION,
      expectedTag: TAG,
      expectedCommit: COMMIT,
    }),
    /must be a regular file, not a link/,
  );
});
