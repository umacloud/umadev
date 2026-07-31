#!/usr/bin/env node

import crypto from "node:crypto";
import fs from "node:fs";
import https from "node:https";
import path from "node:path";
import { spawn, spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

export const MAX_GROK_ARTIFACT_BYTES = 256 * 1024 * 1024;
export const MAX_GROK_REDIRECTS = 10;
export const GROK_DOWNLOAD_IDLE_TIMEOUT_MS = 20000;
export const GROK_DOWNLOAD_TOTAL_TIMEOUT_MS = 5 * 60 * 1000;
export const GROK_VERSION_TIMEOUT_MS = 10000;
export const MAX_GROK_VERSION_OUTPUT_BYTES = 4096;
export const GROK_VERSION_TERM_GRACE_MS = 1000;
export const GROK_VERSION_KILL_GRACE_MS = 1000;

function trustedUrl(raw) {
  try {
    const url = new URL(raw);
    if (url.protocol !== "https:" || url.username || url.password) return false;
    return url.hostname === "x.ai" || url.hostname === "storage.googleapis.com";
  } catch {
    return false;
  }
}

function uniqueTemporary(destination) {
  return `${destination}.part.${process.pid}.${crypto.randomBytes(8).toString("hex")}`;
}

export function downloadGrokArtifact(
  url,
  destination,
  { deadline = Date.now() + GROK_DOWNLOAD_TOTAL_TIMEOUT_MS, redirects = 0, temporary = uniqueTemporary(destination) } = {},
) {
  return new Promise((resolve, reject) => {
    if (!trustedUrl(url)) return reject(new Error(`untrusted Grok artifact URL: ${url}`));
    if (redirects > MAX_GROK_REDIRECTS) return reject(new Error("too many Grok artifact redirects"));
    const remaining = deadline - Date.now();
    if (remaining <= 0) return reject(new Error("Grok artifact download timed out"));
    let settled = false;
    let totalTimer;
    const settle = (error) => {
      if (settled) return;
      settled = true;
      clearTimeout(totalTimer);
      if (error) {
        try { fs.rmSync(temporary, { force: true }); } catch { }
        reject(error);
      } else {
        resolve(destination);
      }
    };
    const request = https.get(
      url,
      { headers: { "User-Agent": "UmaDev-release-contract", Accept: "application/octet-stream" } },
      (response) => {
        const status = response.statusCode || 0;
        if (status >= 300 && status < 400 && response.headers.location) {
          response.resume();
          clearTimeout(totalTimer);
          const next = new URL(response.headers.location, url).href;
          downloadGrokArtifact(next, destination, {
            deadline,
            redirects: redirects + 1,
            temporary,
          }).then(() => settle(), settle);
          return;
        }
        if (status !== 200) {
          response.resume();
          settle(new Error(`Grok artifact HTTP ${status}`));
          return;
        }
        const declared = response.headers["content-length"];
        if (declared !== undefined) {
          const length = Number(declared);
          if (!Number.isSafeInteger(length) || length < 0 || length > MAX_GROK_ARTIFACT_BYTES) {
            response.resume();
            settle(new Error("Grok artifact Content-Length is invalid or oversized"));
            return;
          }
        }
        let received = 0;
        const output = fs.createWriteStream(temporary, { flags: "wx", mode: 0o600 });
        response.on("data", (chunk) => {
          received += chunk.length;
          if (received > MAX_GROK_ARTIFACT_BYTES) {
            const error = new Error("Grok artifact exceeded its byte limit");
            response.destroy(error);
            output.destroy(error);
            settle(error);
          }
        });
        response.on("error", settle);
        output.on("error", settle);
        output.on("finish", () => output.close((closeError) => {
          if (closeError) return settle(closeError);
          try {
            try {
              const existing = fs.lstatSync(destination);
              if (!existing.isFile() || existing.isSymbolicLink()) {
                throw new Error(`refusing non-regular Grok artifact destination: ${destination}`);
              }
              fs.unlinkSync(destination);
            } catch (error) {
              if (!error || error.code !== "ENOENT") throw error;
            }
            fs.renameSync(temporary, destination);
          } catch (error) {
            return settle(error);
          }
          settle();
        }));
        response.pipe(output);
      },
    );
    request.on("error", settle);
    request.setTimeout(GROK_DOWNLOAD_IDLE_TIMEOUT_MS, () => {
      request.destroy(new Error("Grok artifact download idle timeout"));
    });
    totalTimer = setTimeout(() => {
      request.destroy(new Error("Grok artifact download total timeout"));
    }, remaining);
  });
}

export function probeGrokVersion(
  binary,
  { timeoutMs = GROK_VERSION_TIMEOUT_MS, maxOutputBytes = MAX_GROK_VERSION_OUTPUT_BYTES } = {},
) {
  return new Promise((resolve, reject) => {
    const child = spawn(binary, ["--version"], {
      stdio: ["ignore", "pipe", "pipe"],
      windowsHide: true,
      // A separate Unix process group lets the timeout reap descendants that
      // inherited stdout/stderr. On Windows, taskkill /T below is the matching
      // tree primitive.
      detached: process.platform !== "win32",
    });
    const chunks = [];
    let bytes = 0;
    let settled = false;
    let terminationError = null;
    let termTimer;
    let killTimer;
    let timer;
    const finish = (error, value) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      clearTimeout(termTimer);
      clearTimeout(killTimer);
      if (error) reject(error);
      else resolve(value);
    };
    const signalTree = (force) => {
      if (!child.pid) return;
      if (process.platform === "win32") {
        const args = ["/PID", String(child.pid), "/T"];
        if (force) args.push("/F");
        spawnSync("taskkill", args, {
          windowsHide: true,
          stdio: "ignore",
          timeout: GROK_VERSION_KILL_GRACE_MS,
        });
        return;
      }
      try {
        process.kill(-child.pid, force ? "SIGKILL" : "SIGTERM");
      } catch {
        try { child.kill(force ? "SIGKILL" : "SIGTERM"); } catch { }
      }
    };
    const terminate = (error) => {
      if (settled || terminationError) return;
      terminationError = error;
      signalTree(false);
      // A grandchild can keep inherited pipes open after the direct child
      // exits. Destroy our pipe ends and unref the process handle so an
      // unkillable external process cannot keep the CI Node runner alive.
      child.stdout.destroy();
      child.stderr.destroy();
      child.unref();
      termTimer = setTimeout(() => {
        signalTree(true);
        killTimer = setTimeout(() => finish(terminationError), GROK_VERSION_KILL_GRACE_MS);
      }, GROK_VERSION_TERM_GRACE_MS);
    };
    const collect = (chunk) => {
      bytes += chunk.length;
      if (bytes > maxOutputBytes) {
        terminate(new Error("Grok --version output exceeded its byte limit"));
        return;
      }
      chunks.push(chunk);
    };
    child.stdout.on("data", collect);
    child.stderr.on("data", collect);
    child.once("error", (error) => {
      // Once termination begins, the fixed TERM -> KILL schedule owns
      // settlement. A direct-child error must not cancel the later tree kill.
      if (terminationError) return;
      finish(error);
    });
    child.once("exit", (code, signal) => {
      // The process-group leader can exit on TERM while a descendant ignores
      // it. Keep the force-kill timer alive until the absolute bound instead
      // of treating the leader's exit as proof that the tree is gone.
      if (terminationError) return;
      if (code !== 0) return finish(new Error(`Grok --version failed (${code ?? signal})`));
      finish(null, Buffer.concat(chunks).toString("utf8").trim());
    });
    timer = setTimeout(() => {
      terminate(new Error("Grok --version timed out"));
    }, timeoutMs);
  });
}

function sha256File(file) {
  const hash = crypto.createHash("sha256");
  const buffer = Buffer.allocUnsafe(1024 * 1024);
  const descriptor = fs.openSync(file, "r");
  try {
    for (;;) {
      const bytes = fs.readSync(descriptor, buffer, 0, buffer.length, null);
      if (bytes === 0) break;
      hash.update(buffer.subarray(0, bytes));
    }
  } finally {
    fs.closeSync(descriptor);
  }
  return hash.digest("hex");
}

async function main() {
  const [artifact, expectedSha256, binaryName] = process.argv.slice(2);
  if (
    !/^[0-9A-Za-z._-]+$/.test(artifact || "") ||
    !/^[0-9a-f]{64}$/.test(expectedSha256 || "") ||
    !/^[0-9A-Za-z._-]+$/.test(binaryName || "") ||
    !process.env.RUNNER_TEMP
  ) {
    throw new Error("usage: grok-published-contract.mjs <artifact> <sha256> <binary-name> (RUNNER_TEMP required)");
  }
  const destination = path.join(process.env.RUNNER_TEMP, "umadev-grok-contract", binaryName);
  fs.mkdirSync(path.dirname(destination), { recursive: true });
  const sources = [
    `https://x.ai/cli/${artifact}`,
    `https://storage.googleapis.com/grok-build-public-artifacts/cli/${artifact}`,
  ];
  let lastError;
  for (const source of sources) {
    try {
      await downloadGrokArtifact(source, destination);
      lastError = null;
      break;
    } catch (error) {
      lastError = error;
    }
  }
  if (lastError) throw lastError;
  const actual = sha256File(destination);
  if (actual !== expectedSha256) {
    try { fs.rmSync(destination, { force: true }); } catch { }
    throw new Error(`Grok artifact SHA-256 mismatch: ${actual}`);
  }
  if (process.platform !== "win32") fs.chmodSync(destination, 0o755);
  const version = await probeGrokVersion(destination);
  if (!/^grok 0\.2\.112 \([0-9a-f]+\)$/.test(version)) {
    throw new Error(`unexpected Grok version: ${version}`);
  }
  if (process.env.GITHUB_ENV) {
    if (/\r|\n/.test(destination)) throw new Error("invalid Grok binary path");
    fs.appendFileSync(process.env.GITHUB_ENV, `UMADEV_GROK_BIN=${destination}\n`);
  }
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().catch((error) => {
    console.error(`grok-published-contract: ${error.message}`);
    process.exit(1);
  });
}
