# AGENTS.md

Guidance for any AI coding agent (Codex CLI / Codex Desktop / Antigravity
CLI / Claude Code / …) when operating inside this repository.

## What this repo is

A Rust workspace that ships a single binary `umadev` — a coach for
AI coding hosts. The product is the specification
[`UMADEV_HOST_SPEC_V1`](spec/UMADEV_HOST_SPEC_V1.md); the binary
is one of its delivery surfaces.

## Build / test / lint

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

## Workspace layout

See [README.md](README.md). Short version:

- `crates/umadev` — binary (clap CLI + hook + doctor + CI + MCP server + MCP/Skill/Knowledge managers)
- `crates/umadev-spec` — spec as Rust data
- `crates/umadev-governance` — rules (113 clauses × 20 languages) + audit + compliance kernel + policy engine
- `crates/umadev-agent` — 9-phase runner + gates + workflow state + scaffolding + coach prompts
- `crates/umadev-runtime` — Runtime trait + OfflineRuntime + OpenAI/Anthropic HTTP runtime
- `crates/umadev-host` — subprocess drivers for Claude Code / Codex (2 backends)
- `crates/umadev-contract` — typed OpenAPI 3.1 contract layer
- `crates/umadev-knowledge` — BM25 + optional vector (hybrid) knowledge retrieval
- `crates/umadev-tui` — ratatui real-time terminal UI (dark/light adaptive theme)

## Hard rules

- **Pure Rust.** No Python, no Node, no subprocess shims to vendor SDKs.
- **Fail-open governance.** Any governance function must return a pass
  decision (or empty record) on unexpected input — the host MUST NEVER
  be blocked by a bug in the governor.
- **Spec is the source of truth.** When data and prose diverge, fix
  both.
- **Subprocess-driver scope.** Hosts are driven as non-interactive
  subprocesses (no API key, no SDK vendoring). The authoritative list is
  `umadev_host::BACKEND_IDS` (23 as of 4.6.0). Adding a host means a
  `SimpleHostDriver` factory + a `BackendArg` variant, not an HTTP client.

## Recommended sequence for new contributors

1. `cargo test --workspace` — green baseline.
2. Read `spec/UMADEV_HOST_SPEC_V1.md`.
3. Skim `crates/umadev-spec/src/lib.rs` (clauses + phases as data).
4. Open `crates/umadev-governance/src/rules.rs` to see how a clause
   is enforced end-to-end.
