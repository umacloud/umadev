import assert from "node:assert/strict";
import crypto from "node:crypto";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import {
  RELEASE_MANIFEST_NAME,
  createReleaseManifest,
  verifyReleaseManifest,
} from "./release-manifest.mjs";

const TAG = "v9.8.7";
const COMMIT = "a".repeat(40);

function fixture(t) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "umadev-release-manifest-"));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const payload = path.join(root, "umadev-x86_64-unknown-linux-gnu");
  const payloadSidecar = `${payload}.sha256`;
  fs.writeFileSync(payload, "release binary bytes\n");
  const payloadHash = crypto.createHash("sha256").update(fs.readFileSync(payload)).digest("hex");
  fs.writeFileSync(payloadSidecar, `${payloadHash}  ${path.basename(payload)}\n`);
  const manifest = path.join(root, RELEASE_MANIFEST_NAME);
  createReleaseManifest({ output: manifest, tag: TAG, commit: COMMIT, files: [payload, payloadSidecar] });
  const manifestHash = crypto.createHash("sha256").update(fs.readFileSync(manifest)).digest("hex");
  fs.writeFileSync(`${manifest}.sha256`, `${manifestHash}  ${RELEASE_MANIFEST_NAME}\n`);
  return { root, payload, manifest };
}

test("release manifest binds tag, commit, exact asset set, and bytes", (t) => {
  const f = fixture(t);
  const manifest = verifyReleaseManifest({
    manifestFile: f.manifest,
    expectedTag: TAG,
    expectedCommit: COMMIT,
    assetDirectory: f.root,
  });
  assert.equal(manifest.tag, TAG);
  assert.equal(manifest.commit, COMMIT);
  assert.equal(Object.keys(manifest.assets).length, 2);
});

test("release manifest rejects a force-moved tag commit", (t) => {
  const f = fixture(t);
  assert.throws(
    () => verifyReleaseManifest({
      manifestFile: f.manifest,
      expectedTag: TAG,
      expectedCommit: "b".repeat(40),
      assetDirectory: f.root,
    }),
    /release manifest commit .* !=/,
  );
});

test("release manifest rejects changed, missing, or extra public assets", (t) => {
  const changed = fixture(t);
  fs.appendFileSync(changed.payload, "tampered");
  assert.throws(
    () => verifyReleaseManifest({
      manifestFile: changed.manifest,
      expectedTag: TAG,
      expectedCommit: COMMIT,
      assetDirectory: changed.root,
    }),
    /SHA-256 mismatch/,
  );

  const extra = fixture(t);
  fs.writeFileSync(path.join(extra.root, "unexpected"), "extra");
  assert.throws(
    () => verifyReleaseManifest({
      manifestFile: extra.manifest,
      expectedTag: TAG,
      expectedCommit: COMMIT,
      assetDirectory: extra.root,
    }),
    /release asset set mismatch/,
  );
});

test("release manifest bounds and refuses linked checksum sidecars", (t) => {
  const oversized = fixture(t);
  fs.writeFileSync(`${oversized.manifest}.sha256`, "0".repeat(4097));
  assert.throws(
    () => verifyReleaseManifest({
      manifestFile: oversized.manifest,
      expectedTag: TAG,
      expectedCommit: COMMIT,
      assetDirectory: oversized.root,
    }),
    /checksum sidecar is too large/,
  );

  const linked = fixture(t);
  const sidecar = `${linked.manifest}.sha256`;
  const target = `${linked.root}.sidecar-target`;
  t.after(() => fs.rmSync(target, { force: true }));
  fs.renameSync(sidecar, target);
  fs.symlinkSync(target, sidecar);
  assert.throws(
    () => verifyReleaseManifest({
      manifestFile: linked.manifest,
      expectedTag: TAG,
      expectedCommit: COMMIT,
      assetDirectory: linked.root,
    }),
    /checksum sidecar must be a regular file, not a link/,
  );
});
