import assert from "node:assert/strict";
import test from "node:test";

import {
  RELEASE_PACKAGE_NAMES,
  validatePackageRecord,
  validateReleaseRecords,
} from "./release-package-contract.mjs";

const version = "1.2.3";
const repository = { type: "git", url: "git+https://github.com/umacloud/umadev.git" };
const platformBinary = new Map([
  ["@umatech/cli-darwin-arm64", "bin/umadev"],
  ["@umatech/cli-darwin-x64", "bin/umadev"],
  ["@umatech/cli-linux-x64", "bin/umadev"],
  ["@umatech/cli-linux-arm64", "bin/umadev"],
  ["@umatech/cli-linux-musl-x64", "bin/umadev"],
  ["@umatech/cli-linux-musl-arm64", "bin/umadev"],
  ["@umatech/cli-win32-x64", "bin/umadev.exe"],
]);

function record(name) {
  if (name === "@umatech/umadev") {
    return {
      manifest: {
        name,
        version,
        repository: { ...repository },
        bin: { umadev: "bin/cli.js" },
        optionalDependencies: Object.fromEntries(
          RELEASE_PACKAGE_NAMES.filter((candidate) => candidate !== "@umatech/umadev").map((candidate) => [candidate, version]),
        ),
      },
      entries: [
        "package/LICENSE",
        "package/README.md",
        "package/bin/cli.js",
        "package/bin/cli-main.js",
        "package/package.json",
      ],
    };
  }
  if (name === "@umatech/knowledge") {
    return {
      manifest: { name, version, repository: { ...repository } },
      entries: ["package/LICENSE", "package/package.json", "package/agent/guide.md"],
    };
  }
  return {
    manifest: { name, version, repository: { ...repository } },
    entries: [
      "package/LICENSE",
      `package/${platformBinary.get(name)}`,
      "package/bin/umadev.provenance.json",
      "package/package.json",
    ],
  };
}

test("the complete inert nine-package release set is accepted", () => {
  assert.doesNotThrow(() => validateReleaseRecords(RELEASE_PACKAGE_NAMES.map(record), version));
});

test("the ChainDrop lifecycle-script shape is rejected", () => {
  const compromised = record("@umatech/umadev");
  compromised.manifest.scripts = { preinstall: "node setup.mjs" };
  compromised.entries.push("package/setup.mjs", "package/math_init.js");
  assert.throws(
    () => validatePackageRecord(compromised, version),
    /must not declare package scripts/,
  );
});

test("an unexpected executable payload in a platform package is rejected", () => {
  const compromised = record("@umatech/cli-linux-x64");
  compromised.entries.push("package/router_runtime.js");
  assert.throws(
    () => validatePackageRecord(compromised, version),
    /known payload filename is forbidden|files changed/,
  );
});

test("a missing release package is rejected", () => {
  assert.throws(
    () => validateReleaseRecords(RELEASE_PACKAGE_NAMES.slice(1).map(record), version),
    /each of the nine packages exactly once/,
  );
});

test("a package cannot detach provenance from the audited repository", () => {
  const detached = record("@umatech/umadev");
  detached.manifest.repository.url = "https://example.invalid/other.git";
  assert.throws(() => validatePackageRecord(detached, version), /audited GitHub repository/);

  const provenanceDisabled = record("@umatech/knowledge");
  provenanceDisabled.manifest.publishConfig = { provenance: false };
  assert.throws(() => validatePackageRecord(provenanceDisabled, version), /must not disable npm provenance/);
});
