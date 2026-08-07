import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(scriptDir, "..", "..");
const workflowDir = path.join(repoRoot, ".github", "workflows");
const allWorkflows = fs
  .readdirSync(workflowDir)
  .filter((name) => /\.ya?ml$/.test(name))
  .sort()
  .map((name) => [name, fs.readFileSync(path.join(workflowDir, name), "utf8")]);
const workflow = fs.readFileSync(path.join(repoRoot, ".github", "workflows", "release.yml"), "utf8");
const ciWorkflow = fs.readFileSync(path.join(repoRoot, ".github", "workflows", "ci.yml"), "utf8");
const pagesWorkflow = fs.readFileSync(
  path.join(repoRoot, ".github", "workflows", "website-pages.yml"),
  "utf8",
);
const productionGuard = "github.event_name == 'push' && startsWith(github.ref, 'refs/tags/v')";

function jobBlock(name) {
  const jobs = workflow.slice(workflow.indexOf("\njobs:\n") + 1);
  const startToken = `  ${name}:\n`;
  const start = jobs.indexOf(startToken);
  assert.notEqual(start, -1, `missing ${name} job`);
  const tail = jobs.slice(start + startToken.length);
  const next = tail.search(/^  [a-zA-Z0-9_-]+:\n/m);
  return next === -1 ? tail : tail.slice(0, next);
}

test("external Actions are immutable and deprecated Node 20 pins cannot return", () => {
  const externalRef = /^\s*(?:-\s+)?uses:\s+([^\s#]+)\s*(?:#.*)?$/gm;
  const approvedRefs = new Map([
    ["actions/attest", "actions/attest@a1948c3f048ba23858d222213b7c278aabede763"],
    [
      "actions/checkout",
      "actions/checkout@d23441a48e516b6c34aea4fa41551a30e30af803",
    ],
    [
      "actions/deploy-pages",
      "actions/deploy-pages@cd2ce8fcbc39b97be8ca5fce6e763baed58fa128",
    ],
    [
      "actions/download-artifact",
      "actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c",
    ],
    ["actions/setup-node", "actions/setup-node@249970729cb0ef3589644e2896645e5dc5ba9c38"],
    [
      "actions/upload-artifact",
      "actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a",
    ],
    [
      "actions/upload-pages-artifact",
      "actions/upload-pages-artifact@fc324d3547104276b827a68afc52ff2a11cc49c9",
    ],
    [
      "anchore/sbom-action",
      "anchore/sbom-action@e22c389904149dbc22b58101806040fa8d37a610",
    ],
    [
      "dtolnay/rust-toolchain",
      "dtolnay/rust-toolchain@4be7066ada62dd38de10e7b70166bc74ed198c30",
    ],
    [
      "rustsec/audit-check",
      "rustsec/audit-check@858dc40f52ca2b8570b7a997c1c4e35c6fc9a432",
    ],
    [
      "Swatinem/rust-cache",
      "Swatinem/rust-cache@6323deb102c322ba6fcbdcafc7e3dddab59af2b6",
    ],
  ]);
  const seen = new Set();

  for (const [name, source] of allWorkflows) {
    let parsedUses = 0;
    for (const match of source.matchAll(externalRef)) {
      const ref = match[1];
      parsedUses += 1;
      if (ref.startsWith("./")) continue;
      assert.match(
        ref,
        /^[^/@\s]+\/[^@\s]+@[0-9a-f]{40}$/,
        `${name} has mutable Action ref ${ref}`,
      );
      const repository = ref.slice(0, ref.indexOf("@"));
      assert.ok(
        approvedRefs.has(repository),
        `${name} added unaudited Action repository ${repository}`,
      );
      assert.equal(
        ref,
        approvedRefs.get(repository),
        `${name} changed audited Action pin ${repository}`,
      );
      seen.add(ref);
    }
    const declaredUses = source.match(/\buses\s*:/g)?.length ?? 0;
    assert.equal(
      parsedUses,
      declaredUses,
      `${name} contains a uses declaration outside the audited line form`,
    );
  }

  for (const ref of approvedRefs.values()) {
    assert.ok(seen.has(ref), `missing approved Node 24 Action pin ${ref}`);
  }
});

test("workflow_dispatch is validation-only even when dispatched on a tag", () => {
  assert.match(workflow, /^  workflow_dispatch:\s*$/m);
  assert.match(jobBlock("dispatch-validation"), /^    if: github\.event_name == 'workflow_dispatch'$/m);
  for (const name of ["publish-github", "publish-npm", "deploy-website", "verify-publication"]) {
    assert.match(jobBlock(name), new RegExp(`^    if: ${productionGuard.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}$`, "m"));
  }
});

test("credential, signing, attestation, and npm artifact steps require a tag push", () => {
  const protectedSteps = [
    "Require GitHub Pages to allow this release tag",
    "Require npm Trusted Publishing for tags",
    "Require native signing credentials for tags",
    "Developer ID sign and notarize (macOS)",
    "Authenticode sign and timestamp (Windows)",
    "Stage tag-bound npm sub-package",
    "Verify tag-bound npm binary provenance",
    "Attest binary provenance",
    "Upload staged npm sub-package",
  ];
  for (const step of protectedSteps) {
    const escaped = step.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
    const block = new RegExp(`- name: ${escaped}\\n\\s+if: ([^\\n]+)`).exec(workflow);
    assert.ok(block, `missing explicit guard for ${step}`);
    assert.ok(block[1].includes(productionGuard), `${step} is not tag-push guarded: ${block[1]}`);
  }
});

test("RustSec jobs can maintain advisory findings without turning clean audits red", () => {
  const security = fs.readFileSync(
    path.join(repoRoot, ".github", "workflows", "security.yml"),
    "utf8",
  );
  for (const [name, content] of [
    ["security", security],
    ["release", workflow],
  ]) {
    const rustsecJob = content.match(/\n  rustsec:\n[\s\S]*?(?=\n  [a-zA-Z0-9_-]+:\n|$)/)?.[0];
    assert.ok(rustsecJob, `${name} workflow must define a rustsec job`);
    assert.match(rustsecJob, /permissions:\s*\n(?:\s+[a-z-]+:\s+\w+\s*\n)*\s+issues:\s+write\b/);
  }
});

test("Pages tag policy gate consumes every paginated policy", () => {
  const gate = jobBlock("release-credentials");
  assert.match(gate, /deployment-branch-policies\?per_page=100&page=\$page/);
  assert.match(gate, /seen=\$\(\(seen \+ returned\)\)/);
  assert.match(gate, /seen >= total/);
  assert.match(gate, /pagination ended after \$seen of \$total/);
});

test("npm publication re-verifies tag and commit provenance before publish", () => {
  const block = jobBlock("publish-npm");
  const verifyIndex = block.indexOf("release-provenance.mjs verify");
  const publishIndex = block.indexOf("./npm/scripts/publish.sh");
  assert.ok(verifyIndex >= 0, "publish-npm does not verify release provenance");
  assert.ok(publishIndex > verifyIndex, "npm publish can run before provenance verification");
  assert.match(block, /"\$GITHUB_REF_NAME" "\$GITHUB_SHA"/);
});

test("npm publication is OIDC-only and rejects lifecycle payloads", () => {
  const credentials = jobBlock("release-credentials");
  const publish = jobBlock("publish-npm");
  assert.match(publish, /^    environment: npm-production$/m);
  assert.match(publish, /^      id-token: write$/m);
  assert.match(publish, /package-manager-cache: false/);
  assert.match(publish, /UMADEV_TRUSTED_PUBLISHING: "1"/);
  assert.match(publish, /NPM_CONFIG_PROVENANCE: "true"/);
  assert.match(workflow, /^  NPM_CONFIG_REGISTRY: "https:\/\/registry\.npmjs\.org"$/m);
  assert.match(pagesWorkflow, /^  NPM_CONFIG_REGISTRY: "https:\/\/registry\.npmjs\.org"$/m);
  for (const [name, source] of allWorkflows) {
    assert.doesNotMatch(
      source,
      /registry-url:/i,
      `${name}: setup-node registry-url injects a dummy auth configuration`,
    );
    assert.doesNotMatch(source, /^\s+(?:NODE_AUTH_TOKEN|NPM_TOKEN):\s*/im);
    assert.doesNotMatch(source, /_authToken|NPM_CONFIG_USERCONFIG/i);
  }
  assert.match(credentials, /long-lived npm credentials are forbidden/);
  const script = fs.readFileSync(path.join(repoRoot, "npm", "scripts", "publish.sh"), "utf8");
  assert.match(script, /release-package-contract\.mjs/);
  assert.match(script, /UMADEV_TRUSTED_PUBLISHING/);
  assert.match(script, /--tag latest/);
  assert.doesNotMatch(script, /--tag staging|npm dist-tag (?:add|rm)|^\s*npm whoami/m);
});

test("release concurrency preserves every tag without mixing different tags", () => {
  assert.match(workflow, /^concurrency:\n  group: release-\$\{\{ github\.ref \}\}\n  cancel-in-progress: false\n  queue: max$/m);
});

test("publication waits for the immediately preceding GitHub and npm release", () => {
  const block = jobBlock("publish-github");
  const orderGate = block.indexOf("Wait for the immediately preceding public release");
  const createRelease = block.indexOf('gh release create "$GITHUB_REF_NAME"');
  assert.ok(orderGate >= 0, "missing predecessor publication gate");
  assert.ok(createRelease > orderGate, "GitHub Release can become visible before the order gate");
  assert.match(block, /stable_versions\[1\]/);
  assert.match(block, /npm view umadev@latest version/);
  assert.match(block, /github_latest.*v\$previous/);
  assert.match(block, /npm_latest.*\$previous/);
  assert.match(block, /timeout --signal=TERM --kill-after=5s 30s[\s\\]+gh release view/);
  assert.match(block, /timeout --signal=TERM --kill-after=5s 30s[\s\\]+npm view umadev@latest version/);
});

test("Pages cannot be manually or concurrently downgraded from the current public release", () => {
  assert.match(
    pagesWorkflow,
    /^concurrency:\n  group: github-pages\n  cancel-in-progress: false\n  queue: max$/m,
  );
  const gate = pagesWorkflow.indexOf("Require the current public release identity");
  const upload = pagesWorkflow.indexOf("actions/upload-pages-artifact@");
  const deploy = pagesWorkflow.indexOf("actions/deploy-pages@");
  assert.ok(gate >= 0 && upload > gate && deploy > upload);
  assert.match(pagesWorkflow, /"\$GITHUB_REF" == "refs\/tags\/\$expected_tag"/);
  assert.match(pagesWorkflow, /"\$sha" == "\$GITHUB_SHA"/);
  assert.match(pagesWorkflow, /gh release view "\$expected_tag"[\s\S]*--json isDraft,tagName/);
  assert.match(pagesWorkflow, /gh release view --repo "\$GITHUB_REPOSITORY"[\s\S]*--json tagName/);
  assert.match(pagesWorkflow, /npm view umadev@latest version/);
  assert.match(pagesWorkflow, /"\$github_latest" == "\$expected_tag"/);
  assert.match(pagesWorkflow, /"\$npm_latest" == "\$version"/);
});

test("a half-published retry stages npm from the immutable public release", () => {
  const github = jobBlock("publish-github");
  assert.match(github, /assert_remote_tag_commit\(\)/);
  assert.match(github, /"\$sha" == "\$GITHUB_SHA"/);
  assert.match(github, /--target "\$GITHUB_SHA"/);
  assert.ok(
    github.lastIndexOf("assert_remote_tag_commit") < github.indexOf('gh release edit "$GITHUB_REF_NAME"'),
    "tag identity is not rechecked immediately before publication",
  );
  assert.match(github, /if \[\[ "\$state" == "draft" \]\]/);
  assert.match(github, /for checksum in \*\.sha256/);
  assert.match(github, /published Linux binary reports/);
  assert.match(github, /timeout --signal=TERM --kill-after=2s 10s/);
  assert.match(
    github,
    /release-manifest\.mjs verify[\s\S]*"\$GITHUB_REF_NAME" "\$GITHUB_SHA" "\$verify_dir"/,
  );
  const exactNames = github.indexOf('if [[ "$actual_names" != "$expected_names" ]]');
  const remoteChecksum = github.indexOf('for checksum in *.sha256');
  assert.ok(exactNames >= 0, "public retry does not reject a missing or extra remote asset");
  assert.ok(remoteChecksum > exactNames, "remote checksums run before the exact asset manifest is accepted");

  const npm = jobBlock("publish-npm");
  const download = npm.indexOf('gh release download "$GITHUB_REF_NAME"');
  const releaseManifest = npm.indexOf("release-manifest.mjs verify");
  const stage = npm.indexOf('./npm/scripts/stage.sh "$platform" "release-assets/$asset"');
  const verify = npm.indexOf("release-provenance.mjs verify");
  assert.ok(download >= 0, "npm retry does not read canonical public assets");
  assert.ok(releaseManifest > download, "npm does not verify the public tag/commit manifest");
  assert.ok(stage > releaseManifest, "npm stages binaries before the public commit binding is verified");
  assert.ok(stage > download, "npm stages before downloading the public assets");
  assert.ok(verify > stage, "npm provenance is not regenerated and checked after canonical staging");
});

test("release model transport is bounded before immutable hash verification", () => {
  const github = jobBlock("publish-github");
  const transport = github.indexOf("download_model() {");
  const hashes = github.indexOf("Verify the immutable upstream bytes");
  assert.ok(transport >= 0 && hashes > transport);
  for (const bound of [
    "--connect-timeout 20",
    '--max-time "$max_time"',
    '--retry-max-time "$max_time"',
    "--max-redirs 10",
    '--max-filesize "$max_bytes"',
    "--proto '=https' --proto-redir '=https'",
    'model/config.json 1048576 60',
    'model/tokenizer.json 67108864 300',
    'model/model.f32.safetensors 536870912 900',
  ]) {
    assert.ok(github.includes(bound), `missing model download bound: ${bound}`);
  }
});

test("CI and release use the same bounded Grok artifact contract", () => {
  for (const [name, source] of [["CI", ciWorkflow], ["release", workflow]]) {
    assert.match(
      source,
      /node npm\/scripts\/grok-published-contract\.mjs\s+"\$\{\{ matrix\.artifact \}\}"\s+"\$\{\{ matrix\.sha256 \}\}"\s+"\$\{\{ matrix\.binary \}\}"/,
      `${name} does not invoke the bounded Grok contract helper`,
    );
    assert.doesNotMatch(source, /Invoke-WebRequest[^\n]*https:\/\/x\.ai\/cli\//);
    assert.doesNotMatch(source, /curl[^\n]*https:\/\/x\.ai\/cli\//);
  }
});

test("CI and release both gate launcher classification and Windows GNU", () => {
  for (const [name, source] of [["CI", ciWorkflow], ["release", workflow]]) {
    assert.match(
      source,
      /node --test npm\/scripts\/cli-classification\.test\.cjs/,
      `${name} omits the launcher command-classification regression`,
    );
    assert.match(
      source,
      /targets: x86_64-pc-windows-gnu/,
      `${name} does not install the Windows GNU target`,
    );
    assert.match(
      source,
      /- name: Install Windows GNU C toolchain\n\s+timeout-minutes: 10[\s\S]*?Acquire::Retries=3[\s\S]*?DPkg::Lock::Timeout=60/,
      `${name} can wait indefinitely for the Windows GNU apt toolchain`,
    );
    assert.match(
      source,
      /cargo clippy --workspace --all-(?:features --all-targets|targets --all-features) --locked --target x86_64-pc-windows-gnu -- -D warnings/,
      `${name} does not run the full Windows GNU strict-Clippy gate`,
    );
  }
  assert.match(
    ciWorkflow,
    /needs: \[[^\]]*windows-gnu-cross[^\]]*\]/,
    "release artifact builds can bypass the Windows GNU gate",
  );
});

test("cross release builds never restore host-compiled target artifacts", () => {
  const build = jobBlock("build");
  assert.match(
    build,
    /uses: Swatinem\/rust-cache@[^\n]+\n\s+with:\n\s+cache-targets: false/,
  );
});

test("public release verification bounds every external read", () => {
  const block = jobBlock("verify-publication");
  assert.match(block, /^    timeout-minutes: 60$/m);
  assert.match(block, /timeout --signal=TERM --kill-after=5s 30s[\s\\]+gh release view/);
  assert.match(block, /timeout --signal=TERM --kill-after=5s 30s[\s\\]+npm view "\$\{package\}@latest"/);
  assert.match(block, /timeout --signal=TERM --kill-after=5s 30s[\s\\]+gh api/);
  for (const bound of [
    "--connect-timeout 20",
    "--max-time 60",
    "--retry-max-time 60",
    "--max-redirs 5",
    "--proto '=https' --proto-redir '=https'",
  ]) {
    assert.ok(block.includes(bound), `public website fetch is missing ${bound}`);
  }
  assert.match(block, /--max-filesize 5242880/);
  assert.match(block, /--max-filesize 1048576/);
});
