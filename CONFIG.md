# UmaDev configuration reference

UmaDev works with zero configuration. Everything below is optional — an override
for advanced users. Each variable is read from the process environment; unset,
empty, or malformed values fall back to the documented default (fail-open by
contract — a bad value never removes a safety ceiling or crashes the run).

This document lists the **user-facing** toggles. The workspace reads many more
`UMADEV_*` variables that are **internal or test-only** (process-tree fixtures,
IPC handoff tokens, parallel-test overrides); those are intentionally omitted and
are listed as a group at the end. If a variable is not documented here, treat it
as internal and unsupported.

---

## Run budgets & timeouts

### `UMADEV_RUN_BUDGET_SECS` — has TWO meanings, by run path

This one variable is consumed by two different runners with **different
semantics**. It is kept as a single variable for backward compatibility; raising
it loosens both ceilings, so a larger value is always "let the run go longer."

- **Director path (default `/run` and free-text builds)** — a **sliding idle
  window**, default **1800s (30 min)**. The build is bounded by this much
  wall-clock of *no base productivity*, not this much wall-clock in total. Each
  genuine tool-call / text event resets the window, so a slow-but-progressing
  build keeps going. A separate **absolute** hard ceiling of **4×** this value
  (default **7200s / 2 h**) bounds a base that emits forever without converging.
  When a ceiling is hit the loop stops scheduling new work, runs the final gate on
  what exists, and exits with an honest "budget reached" note — never a mid-write
  kill.
- **Legacy fixed-pipeline path (`UMADEV_LEGACY_PIPELINE=1`)** — a **whole-run
  soft budget**, default **3600s (1 h)**. When cumulative run time crosses it, the
  auto loop stops at the *current gate* (it does not force-kill the in-flight
  block) and asks the user to take over.

In short: **director = idle window (1800s, ×4 absolute); legacy = total soft
budget (3600s).** Same variable, different clocks.

### Other budgets

| Variable | Meaning | Default |
| --- | --- | --- |
| `UMADEV_PHASE_BUDGET_SECS` | Per-phase wall-clock ceiling in the legacy fixed pipeline. | `900` (15 min) |
| `UMADEV_DOCS_BUDGET_SECS` | Docs/planning-phase ceiling (tighter than a build phase). Falls back to `UMADEV_PHASE_BUDGET_SECS`, then the default. | `480` (8 min) |
| `UMADEV_VERIFY_TIMEOUT_SECS` | Global override applied to **every** verify step's budget (install/lint/test/build), lowering as well as raising it. When unset (or `0`), each step uses its own per-step default: 120 s, 600 s for install/test/build. | per-step |

> All budgets are graceful ceilings: they stop scheduling new work and finalize on
> what exists; they do not hard-kill an in-flight write.

---

## Base CLI selection

UmaDev drives an external base coding CLI. By default it finds each one on `PATH`;
set these to point at a specific executable (absolute path or a differently-named
binary).

| Variable | Base CLI |
| --- | --- |
| `UMADEV_CLAUDE_BIN` | Claude Code (`claude`) |
| `UMADEV_CODEX_BIN` | Codex (`codex`) |
| `UMADEV_OPENCODE_BIN` | OpenCode (`opencode`) |
| `UMADEV_KIMI_BIN` | Kimi Code (`kimi`) |
| `UMADEV_GROK_BIN` | Grok Build (`grok`) |

| Variable | Meaning | Default |
| --- | --- | --- |
| `UMADEV_BASE_CONCURRENCY` | Maximum concurrent base turns (positive integer). `1` mirrors a single live base session, so UmaDev "just works" anywhere. | `1` |

---

## Knowledge retrieval & embeddings

| Variable | Meaning | Default |
| --- | --- | --- |
| `UMADEV_EMBED_MODEL_DIR` | Directory holding the local embedding model (`config.json`, `tokenizer.json`, `model.safetensors`). The npm launcher sets this automatically; a `cargo install` user can point it at a hand-placed model to enable offline hybrid search. When you set it to a directory, the npm launcher uses that directory and never downloads the model. Absent → keyword-only (BM25). | npm-managed / `~/.umadev/embed-model` |
| `UMADEV_KNOWLEDGE_DIR` | Directory of the bundled curated knowledge corpus. The npm launcher sets this; the project's own `knowledge/` still wins. | npm-managed |
| `UMADEV_ALLOW_CLOUD_EMBED` | Opt in to sending text to a cloud embedding API (requires an OpenAI-compatible key). Default is **local-only**; leaving it is a loud, intentional act. | off (local-only) |

> With no local model and no cloud opt-in, retrieval runs on keyword-only BM25 —
> fully functional, just without vector recall. `umadev doctor` reports whether the
> local embedding model is present, and the runtime logs a one-time notice on first
> knowledge use when it degrades to keyword-only.

---

## TUI & interaction

| Variable | Meaning | Default |
| --- | --- | --- |
| `UMADEV_THEME` | Force the color theme: `light` or `dark`. Any other/unset value auto-detects from the terminal. | auto-detect |
| `UMADEV_BELL` | Terminal bell on attention events. Set `0`/`false`/`off`/`no` to silence. | on |
| `UMADEV_SCROLLBACK_HANDOFF` | On clean exit, print the full session summary into terminal scrollback. Enable with `1`/`true`/`yes`. | off (compact footer) |
| `UMADEV_REACTIVE_QC` | Enable reactive quality-check surfacing in the TUI. Presence of the variable (any value) turns it on. | off |

---

## Pipeline mode

| Variable | Meaning | Default |
| --- | --- | --- |
| `UMADEV_CONTINUOUS` | Long-session ("continuous") driver. On by default; set an off value (or `UMADEV_LEGACY_RUN`) to opt out to the single-shot driver. | on |
| `UMADEV_LEGACY_PIPELINE` | Route `/run` through the legacy fixed 9-phase pipeline instead of the default director-driven agentic path. Enable with `1`/`true`/`on`. Also selects the `UMADEV_RUN_BUDGET_SECS` "whole-run soft budget" semantics above. | off (director path) |
| `UMADEV_STRICT_COVERAGE` | Treat coverage as a strict gate. Enable with `1`. | off |

---

## Paths

| Variable | Meaning | Default |
| --- | --- | --- |
| `UMADEV_HOME` | Override the UmaDev home directory (state, caches, markers). | `~/.umadev` |
| `UMADEV_PROJECT_DIR` | Project root for governance hooks invoked outside the workspace (e.g. a base CLI's PreToolUse hook). The CLI itself uses the current directory. | cwd |

---

## npm launcher (`bin/cli.js`) only

These are read by the Node launch shim, not the Rust binary, so they apply to
npm/pnpm/yarn/bun installs.

| Variable | Meaning | Default |
| --- | --- | --- |
| `UMADEV_REGISTRY_URL` | Registry queried by `umadev update`'s "already latest?" check. Also honors npm's own `npm_config_registry`. | `https://registry.npmjs.org` |
| `UMADEV_MODEL_BASE_URL` | HTTPS base URL for the one-time embedding-model download (an admin-controlled mirror). Redirects are then confined to that mirror's own origin. | the versioned official GitHub Release |
| `UMADEV_NO_MODEL_DOWNLOAD` | Set to `1` (or `true`/`yes`/`on`) to never download the embedding model; retrieval then uses BM25. Without it, a download that fails, times out or is skipped with Ctrl+C is not retried for 24 hours (the marker is `~/.umadev/embed-model/.umadev-model-download-failed`; delete it to retry sooner), and upgrading UmaDev retries at once. | off |

---

## Internal / test-only (not user-facing)

The workspace reads many additional `UMADEV_*` variables that are **not** supported
configuration. They exist for the test harness and internal IPC and may change or
disappear without notice. Do not set them. They fall into these families:

- **Test fixtures / harness:** `UMADEV_*_FIXTURE_*`, `UMADEV_PROCESS_JOB_FIXTURE_*`,
  `UMADEV_*_PUBLISHED_CONTRACT`, `UMADEV_*_SOURCE_DIR`, `UMADEV_PORT_OK`,
  `UMADEV_QUEUE_*`, `UMADEV_RESUME_*`, `UMADEV_LIVE_*`, `UMADEV_USAGE_CHILD_*`,
  `UMADEV_TYPED_USER_INPUT_*`, `UMADEV_BACKGROUND_STARTED_`, `UMADEV_FULL_ACCESS_`.
- **Fine-grained internal tuning** (handshake/idle/retry/route/fork timeouts, context
  and token caps, embedding dims, lessons-marketplace wiring, sandbox/permission
  internals): e.g. `UMADEV_ACP_HANDSHAKE_TIMEOUT_SECS`, `UMADEV_IDLE_TIMEOUT_SECS`,
  `UMADEV_RETRY_BASE_MS`, `UMADEV_EMBED_DIM`, `UMADEV_CONTEXT_MAX_CHARS`,
  `UMADEV_LESSONS_MP_*`, `UMADEV_CLAUDE_PERMISSION_MODE`. These are implementation
  details, not a stable interface.
