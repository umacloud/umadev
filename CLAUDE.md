# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working
with code in this repository.

## What this project is

UmaDev (1.0.0) is a Rust workspace that ships a **single binary**
(`umadev`) acting as a coach + orchestrator for AI coding hosts. It
embeds `UMADEV_HOST_SPEC_V1` (see `spec/`). It does **not** own a model
endpoint — it is a deterministic Rust shell that drives someone else's
brain and enforces a 9-phase commercial-delivery pipeline + governance
on top of it.

The binary has two execution modes:

- **base-CLI** (the product) — `run --backend <id>` drives an
  already-logged-in base CLI as a subprocess; **no API key of its own**, the
  customer's existing base subscription/config IS the brain. Exactly **three**
  first-class bases: `claude-code` (`claude --print`), `codex` (`codex exec`),
  `opencode` (`opencode run`). See `umadev_host::BACKEND_IDS`. UmaDev injects
  **NOTHING** into the base — whatever the base is already configured with
  (official login OR the customer's own third-party / local-model routing) is
  exactly what runs. UmaDev does not own, broker, or configure any model
  endpoint; connecting a third-party/local model is the base's job, not ours.
- **offline** (internal fallback only) — deterministic templates, no network
  (demo / CI / no base reachable). NOT offered to the customer as a choice;
  the first-run picker lists only the three bases.

Just typing `umadev` (no subcommand) launches a Claude Code-style chat
TUI over the same engine — first launch shows a base picker (language →
pick a base) that writes `~/.umadev/config.toml`; later launches drop
straight into the conversation. Slash commands (`/run` `/continue`
`/revise` `/backend` `/status` `/help` `/clear` `/quit`) live inside the
chat and mirror the hidden CLI subcommands.

3.0+ is a complete rebuild from a previous Python implementation; do not
look for `umadev/` or `pyproject.toml` — they are intentionally gone.

## Build & test

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings   # CI gate: pedantic, warnings = errors
cargo fmt --all

# Run a single test (tests live next to code as `mod tests`):
cargo test -p umadev-host backend_arg_ids_match_host
cargo test -p umadev-contract --test contract        # integration test file
cargo test -p umadev --test e2e
```

Clippy runs at `pedantic` level workspace-wide; new code must pass with
`-D warnings`. `cargo fmt --all` is enforced.

## Workspace layout

Ten crates. The binary depends on everything; the lib crates do not depend
on the binary.

| Crate | Purpose |
|---|---|
| `crates/umadev` | The `umadev` binary. Visible clap verbs: `init` / `install` / `mcp` / `ci` / `mcp-manage` / `skill` / `knowledge-manage` / `history`; hidden-but-scriptable verbs (mirror TUI slash commands): `run` / `continue` / `revise` / `rollback` / `spec` / `verify` / `report` / `doctor` / `examples` / `guide`; hidden internals: `hook` / `uninstall`. Bin-only modules: `ci`, `hook`, `mcp` (MCP stdio server), `mcp_manager`, `skill_manager`, `knowledge_manager`, `doctor`. No subcommand → TUI. |
| `crates/umadev-spec` | `UMADEV_HOST_SPEC_V1` as Rust data — clauses, phases, gates, runtime kinds. Normative prose mirror lives in `spec/` (see Spec sync contract). |
| `crates/umadev-governance` | The fail-open enforcement kernel. `rules` (block emoji / hardcoded colors / AI-slop), `policy` (configurable rule policy), `audit` (API + tool-call JSONL), `context` (session injection), `compliance` (UD-EVID-004 → SOC2 / ISO 27001 / EU AI Act), `tokenizer`. |
| `crates/umadev-agent` | 9-phase pipeline runner, gate semantics, workflow `state`, `events` stream, `manifest` (UD-META-001), `coach` / `experts` / `lessons` (prompt + methodology injection), `verify`, `scaffolding`, `tech_debt`, `planner` (complexity-tiered phase plan), `coverage` (FR→task spec-coverage check), `checkpoint` (file rewind), `run_lock` (single-writer lock), `acceptance`. |
| `crates/umadev-runtime` | `Runtime` trait + `OfflineRuntime` (deterministic templates) + `RuntimeKind` (wire-protocol tag used by the drivers). The three host drivers impl `Runtime`; UmaDev owns **no** HTTP/model endpoint of its own. |
| `crates/umadev-host` | `HostDriver` trait — drives a logged-in host CLI as a subprocess. Exactly three drivers: `claude` / `codex` / `opencode`. Each impls `umadev_runtime::Runtime` so `AgentRunner` drives it unchanged. `BACKEND_IDS` is the authoritative id list. |
| `crates/umadev-knowledge` | Structured retrieval over the curated `knowledge/` corpus: `chunker` (markdown-aware), `index` (pure-Rust BM25 + CJK-bigram tokeniser, cached to `.umadev/kb-index/`), optional `vector` (embeddings, only when `OPENAI_EMBED_KEY` is set), `retrieve` (single entry point, fail-open → empty result). Replaces the old keyword path matcher. |
| `crates/umadev-contract` | Machine-verifiable API contract for UD-CODE-003 (frontend↔backend alignment): parses the architecture-doc API table into a typed `ApiSpec`, renders `openapi.{json,yaml}` to `.umadev/contracts/`, extracts frontend `fetch`/`axios` calls, and cross-validates. Self-contained OpenAPI subset (no `oas3` dep). |
| `crates/umadev-tui` | ratatui terminal app over the engine event stream. |
| `crates/umadev-i18n` | Trilingual (zh-CN / zh-TW / en) string catalogs + system-locale detection for all user-facing text. |

## Conventions

- All `pub` items have docstrings.
- Every governance function is **fail-open**: an error path returns `Decision::pass()` or an empty record. The host MUST NEVER be blocked by a bug in the governor.
- Every clause in `umadev-spec::CLAUSES` is tagged with its `UD-LAYER-NNN` id (e.g. `UD-CODE-001`). When you write or modify a governance rule, reference the clause id in the docstring.
- Tests live next to code (`mod tests { ... }` at the bottom of each `.rs`).

## Spec sync contract

`spec/UMADEV_HOST_SPEC_V1.md` is the normative prose. Any change to
`umadev-spec::CLAUSES` MUST be accompanied by a change to the
matching section of the markdown, and vice versa. The unit tests in
`crates/umadev-spec/src/lib.rs` lock the data shape; add new clauses
there in `UD-LAYER-NNN` order.

## What lives outside the Rust workspace

- `knowledge/` — curated knowledge base (language-agnostic, used by the agent at runtime)
- `umadev-website/` — Next.js marketing site (independent build)
- `output/`, `.umadev/` — per-project user data (gitignored)
- `docs/assets/` — README images

## Anti-rules (do not undo these)

- Do not reintroduce Python packaging (`pyproject.toml`, `umadev/`).
- Only add adapters for hosts that have a documented non-interactive CLI
  form (`binary [flags] "<prompt>"` → stdout). The base-CLI surface has
  deliberately narrowed to **three** first-class drivers (`claude-code`,
  `codex`, `opencode`) — `umadev_host::BACKEND_IDS` is the authoritative
  list, and tests (`backend_arg_ids_match_host` in the binary,
  `BACKEND_IDS.len() == 3` in the host crate) lock it. If you add a fourth,
  update both `BACKEND_IDS` and `BackendArg`, or those tests fail. Broader
  model coverage belongs in the external-HTTP provider path, not new
  base-CLI drivers.
- Do not vendor any host SDK crate. UmaDev is pure-Rust by design.
  Driving the user's *installed* CLI as a subprocess — see
  `umadev-host` — is the intended architecture.
- Governance is **fail-open by contract**: never make a governance function
  return an error that could block the host. An exceptional input returns
  `Decision::pass()` / an empty record. Do not "harden" this into fail-closed.
- Keep `umadev-spec::CLAUSES` and `spec/UMADEV_HOST_SPEC_V1.md` in
  lockstep (see Spec sync contract). The dependency-light lib crates
  (`spec`, `governance`, `contract`) avoid heavy transitive deps on purpose
  — don't pull in large parser/ICU trees.
