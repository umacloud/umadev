import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { spawnSync } from "node:child_process";
import test from "node:test";

import {
  GROK_DOWNLOAD_IDLE_TIMEOUT_MS,
  GROK_DOWNLOAD_TOTAL_TIMEOUT_MS,
  GROK_VERSION_KILL_GRACE_MS,
  GROK_VERSION_TERM_GRACE_MS,
  GROK_VERSION_TIMEOUT_MS,
  MAX_GROK_ARTIFACT_BYTES,
  MAX_GROK_REDIRECTS,
  MAX_GROK_VERSION_OUTPUT_BYTES,
  probeGrokVersion,
} from "./grok-published-contract.mjs";

function nodeExecutable(t, body) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "umadev-grok-probe-"));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const file = path.join(root, "probe");
  fs.writeFileSync(file, `#!/usr/bin/env node\n${body}\n`, { mode: 0o755 });
  return file;
}

function shellExecutable(t, body) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "umadev-grok-probe-shell-"));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const file = path.join(root, "probe");
  fs.writeFileSync(file, `#!/bin/sh\n${body}\n`, { mode: 0o755 });
  return file;
}

test("Grok published contract has finite transport and process bounds", () => {
  assert.equal(MAX_GROK_ARTIFACT_BYTES, 256 * 1024 * 1024);
  assert.equal(MAX_GROK_REDIRECTS, 10);
  assert.equal(GROK_DOWNLOAD_IDLE_TIMEOUT_MS, 20000);
  assert.equal(GROK_DOWNLOAD_TOTAL_TIMEOUT_MS, 5 * 60 * 1000);
  assert.equal(GROK_VERSION_TIMEOUT_MS, 10000);
  assert.equal(MAX_GROK_VERSION_OUTPUT_BYTES, 4096);
  assert.equal(GROK_VERSION_TERM_GRACE_MS, 1000);
  assert.equal(GROK_VERSION_KILL_GRACE_MS, 1000);
});

test("Grok version probe returns exact bounded output", async (t) => {
  if (process.platform === "win32") return t.skip("shebang fixture is Unix-only");
  const binary = nodeExecutable(t, `process.stdout.write("grok 0.2.112 (abc123)\\n");`);
  assert.equal(await probeGrokVersion(binary, { timeoutMs: 2000, maxOutputBytes: 64 }), "grok 0.2.112 (abc123)");
});

test("Grok version probe kills hangs and excessive output", async (t) => {
  if (process.platform === "win32") return t.skip("shebang fixture is Unix-only");
  const hanging = nodeExecutable(t, "setInterval(() => {}, 1000);");
  await assert.rejects(
    probeGrokVersion(hanging, { timeoutMs: 100, maxOutputBytes: 64 }),
    /timed out/,
  );
  const noisy = nodeExecutable(t, `process.stdout.write("x".repeat(1024));`);
  await assert.rejects(
    probeGrokVersion(noisy, { timeoutMs: 2000, maxOutputBytes: 64 }),
    /output exceeded/,
  );
});

test("Grok version probe escalates past ignored TERM without retaining descendant pipes", async (t) => {
  if (process.platform === "win32") return t.skip("Unix signal fixture; Windows uses taskkill /T /F");
  const stubbornTree = shellExecutable(
    t,
    `trap '' TERM
     sh -c 'trap "" TERM; while :; do sleep 1; done' &
     while :; do sleep 1; done`,
  );
  const started = Date.now();
  await assert.rejects(
    probeGrokVersion(stubbornTree, { timeoutMs: 500, maxOutputBytes: 64 }),
    /timed out/,
  );
  const elapsed = Date.now() - started;
  assert.ok(elapsed >= GROK_VERSION_TERM_GRACE_MS, "fixture exited before forced-kill escalation");
  assert.ok(elapsed < 3500, "TERM-to-KILL escalation exceeded its absolute bound");
});

test("Grok version probe still force-kills descendants after the parent exits on TERM", { timeout: 30000 }, async (t) => {
  if (process.platform === "win32") return t.skip("Unix process-group fixture; Windows uses taskkill /T then /T /F");
  const fixtureStartupTimeoutMs = 5000;
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "umadev-grok-parent-exit-"));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const descendantPidFile = path.join(root, "descendant.pid");
  const parentTermFile = path.join(root, "parent.term");
  const parentExitsFirst = nodeExecutable(
    t,
    `const { spawn } = require("node:child_process");
     const fs = require("node:fs");
     process.on("SIGTERM", () => {
       fs.writeFileSync(${JSON.stringify(parentTermFile)}, "received");
       process.exit(0);
     });
     const descendant = spawn(process.execPath, ["-e", "process.on('SIGTERM', () => {}); setInterval(() => {}, 1000)"], { stdio: ["ignore", "inherit", "inherit"] });
     fs.writeFileSync(${JSON.stringify(descendantPidFile)}, String(descendant.pid));
     setInterval(() => {}, 1000);`,
  );
  const started = Date.now();
  await assert.rejects(
    probeGrokVersion(parentExitsFirst, { timeoutMs: fixtureStartupTimeoutMs, maxOutputBytes: 64 }),
    /timed out/,
  );
  const elapsed = Date.now() - started;
  assert.equal(fs.readFileSync(parentTermFile, "utf8"), "received", "parent did not exit in response to TERM");
  assert.ok(
    elapsed >= fixtureStartupTimeoutMs + GROK_VERSION_TERM_GRACE_MS,
    "parent exit canceled the force-kill escalation",
  );
  // JS timers cannot promise hard wall time while the host process is
  // descheduled or the machine sleeps. The test-level timeout catches a real
  // hang; the assertions below prove the TERM -> KILL semantics and tree state.

  const descendantPid = Number(fs.readFileSync(descendantPidFile, "utf8"));
  const ps = spawnSync("ps", ["-o", "stat=", "-p", String(descendantPid)], { encoding: "utf8" });
  const state = String(ps.stdout || "").trim();
  assert.ok(!state || state.startsWith("Z"), `descendant survived force-kill escalation with state ${state}`);
});
