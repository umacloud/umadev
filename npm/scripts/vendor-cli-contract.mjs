#!/usr/bin/env node

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const versions = {
  claude: "2.1.216",
  codex: "0.144.6",
  opencode: "1.18.4",
};

const cleanHome = mkdtempSync(join(tmpdir(), "umadev-base-contract-"));
const cleanEnv = {
  ...process.env,
  HOME: cleanHome,
  USERPROFILE: cleanHome,
  CLAUDE_CONFIG_DIR: join(cleanHome, ".claude"),
  CODEX_HOME: join(cleanHome, ".codex"),
  XDG_CACHE_HOME: join(cleanHome, ".cache"),
  XDG_CONFIG_HOME: join(cleanHome, ".config"),
  XDG_DATA_HOME: join(cleanHome, ".local", "share"),
  NO_COLOR: "1",
};

function run(command, args, input = "") {
  const executable = process.platform === "win32" ? `${command}.cmd` : command;
  const result = spawnSync(executable, args, {
    encoding: "utf8",
    env: cleanEnv,
    input,
    shell: process.platform === "win32",
    timeout: 20_000,
    windowsHide: true,
  });
  assert.equal(result.error, undefined, `${command} failed to launch: ${result.error}`);
  const output = `${result.stdout ?? ""}\n${result.stderr ?? ""}`.replace(
    /\x1b\[[0-?]*[ -/]*[@-~]/g,
    "",
  );
  return { ...result, output };
}

function requireText(output, required, surface) {
  for (const text of required) {
    assert.ok(output.includes(text), `${surface} no longer exposes ${text}`);
  }
}

function exactVersion(command, expected) {
  const result = run(command, ["--version"]);
  assert.equal(result.status, 0, `${command} --version failed:\n${result.output}`);
  assert.match(
    result.output,
    new RegExp(`(^|\\D)${expected.replaceAll(".", "\\.")}($|\\D)`),
    `${command} is not the audited ${expected} package`,
  );
}

exactVersion("claude", versions.claude);
exactVersion("codex", versions.codex);
exactVersion("opencode", versions.opencode);

const claudeHelp = run("claude", ["--help"]);
assert.equal(claudeHelp.status, 0, `claude --help failed:\n${claudeHelp.output}`);
requireText(
  claudeHelp.output,
  [
    "--print",
    "--input-format",
    "--output-format",
    "--replay-user-messages",
    "--include-partial-messages",
    "--permission-mode",
    "--allowedTools",
    "--dangerously-skip-permissions",
    "--resume",
    "--session-id",
    "--model",
    "--verbose",
    "--setting-sources",
    "--strict-mcp-config",
    "--settings",
  ],
  "Claude Code",
);

const claudeLegacyManual = run("claude", [
  "--print",
  "--output-format",
  "json",
  "--permission-mode",
  "default",
  "--allowedTools",
  "Read,Grep,Glob,WebSearch,WebFetch",
]);
assert.ok(
  !claudeLegacyManual.output.includes("argument 'default' is invalid"),
  `Claude Code rejected its documented default/manual compatibility alias:\n${claudeLegacyManual.output}`,
);
assert.ok(
  claudeLegacyManual.output.includes("Input must be provided"),
  `Claude Code did not reach the expected no-input boundary:\n${claudeLegacyManual.output}`,
);

// `--settings` takes a JSON string or a file path. Pass a file: on Windows this
// script reaches `claude.cmd` through `shell: true`, which does not quote the
// arguments, so cmd.exe would strip the JSON's double quotes.
const emptySettings = join(cleanHome, "umadev-contract-settings.json");
writeFileSync(emptySettings, '{"hooks":{}}');

const claudeStream = run("claude", [
  "--print",
  "--input-format",
  "stream-json",
  "--output-format",
  "stream-json",
  "--replay-user-messages",
  "--include-partial-messages",
  "--verbose",
  "--session-id",
  "550e8400-e29b-41d4-a716-446655440000",
  "--permission-mode",
  "plan",
  "--allowedTools",
  "Read,Grep,Glob",
  "--max-turns",
  "1",
  "--setting-sources",
  "user",
  "--strict-mcp-config",
  "--settings",
  emptySettings,
]);
assert.equal(
  claudeStream.status,
  0,
  `Claude Code rejected UmaDev's resident stream shape:\n${claudeStream.output}`,
);

const codexExec = run("codex", ["exec", "--help"]);
assert.equal(codexExec.status, 0, `codex exec --help failed:\n${codexExec.output}`);
requireText(
  codexExec.output,
  [
    "--skip-git-repo-check",
    "--sandbox",
    "--dangerously-bypass-approvals-and-sandbox",
    "--config",
    "--color",
    "--json",
    "--model",
    "resume",
  ],
  "Codex exec",
);

for (const args of [
  [
    "exec",
    "--skip-git-repo-check",
    "--sandbox",
    "read-only",
    "--config",
    'approval_policy="never"',
    "--color",
    "never",
    "--json",
    "--help",
  ],
  [
    "exec",
    "--skip-git-repo-check",
    "--dangerously-bypass-approvals-and-sandbox",
    "--config",
    'approval_policy="never"',
    "--color",
    "never",
    "--json",
    "--help",
  ],
  [
    "exec",
    "--skip-git-repo-check",
    "--sandbox",
    "read-only",
    "--config",
    'projects={"/tmp/untrusted.project"={trust_level="untrusted"}}',
    "--json",
    "--help",
  ],
]) {
  const result = run("codex", args);
  assert.equal(result.status, 0, `Codex rejected UmaDev's exec flags:\n${result.output}`);
}

const codexServer = run("codex", ["app-server", "--help"]);
assert.equal(
  codexServer.status,
  0,
  `codex app-server --help failed:\n${codexServer.output}`,
);
requireText(codexServer.output, ["stdio://", "--listen"], "Codex app-server");

const opencodeRun = run("opencode", ["run", "--help"]);
assert.equal(opencodeRun.status, 0, `opencode run --help failed:\n${opencodeRun.output}`);
requireText(
  opencodeRun.output,
  [
    "--agent",
    "--auto",
    "--continue",
    "--session",
    "--model",
    "--format",
    "--dir",
  ],
  "OpenCode run",
);

for (const args of [
  ["run", "--agent", "plan", "--format", "json", "--help"],
  ["run", "--agent", "build", "--auto", "--format", "json", "--help"],
]) {
  const result = run("opencode", args);
  assert.equal(result.status, 0, `OpenCode rejected UmaDev's run flags:\n${result.output}`);
}

const opencodeServe = run("opencode", ["serve", "--help"]);
assert.equal(
  opencodeServe.status,
  0,
  `opencode serve --help failed:\n${opencodeServe.output}`,
);
requireText(opencodeServe.output, ["--hostname", "--port"], "OpenCode serve");

console.log(
  `verified published vendor CLIs: claude ${versions.claude}, codex ${versions.codex}, opencode ${versions.opencode}`,
);
