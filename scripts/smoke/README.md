# Real-backend smoke test

A **manual, opt-in** smoke that drives UmaDev's core paths against a **real,
already-logged-in `claude` base CLI** on a real machine.

## Why this exists

UmaDev has 1700+ unit tests, but **nearly all of them run against
`FakeRuntime` / `OfflineRuntime`** and never start a real base subprocess.
That leaves the entire *real-interaction* surface uncovered — and that is
exactly where the recent bugs lived:

- research getting idle-killed mid-thought,
- agentic runs reporting changes that never hit disk (hallucinated edits),
- a run wedging at `0/9` and "looking frozen".

This stabilization sprint hardened the host idle watchdog, the per-phase and
whole-run time budgets, the anti-hallucination git cross-check, and the
`is_stalled` fallbacks. This script is the **regression harness** for those
fixes: it runs UmaDev against a real base with deliberately tiny time budgets
and asserts **healthy completion** (not content correctness).

It is **not part of default CI** — CI has no logged-in base, so the script
detects that and **skips (exit 0)** instead of going red.

## How to run

```bash
bash scripts/smoke/run.sh
```

## Requirements

- A real `claude` CLI on `PATH`, **logged in** (`claude --version` must
  succeed). If it is missing or unhealthy, the script **skips**, it does not
  fail.
- A `umadev` binary. By default the script builds `target/debug/umadev`
  (`cargo build -p umadev`). To use a prebuilt one:
  ```bash
  UMADEV_BIN=./target/release/umadev bash scripts/smoke/run.sh
  ```
- `git` on `PATH` (for the temp project + the agentic reality check).

## Rough cost / time

Two real `auto`-mode base runs with **short** time budgets: Test 1 runs
research → docs then stops at a gate; Test 2 runs further to reach the code
phases. Expect a few-to-several minutes wall-clock and a small amount of
base-subscription quota. Per-run hard ceilings
(`UMADEV_SMOKE_HARD_CEILING_SECS` for Test 1,
`UMADEV_SMOKE_AGENTIC_CEILING_SECS` for Test 2) hard-kill any run that
overshoots via a portable bash watchdog, so **the script can never truly
hang** — even though macOS ships no `timeout`/`gtimeout`.

## What it verifies

Each assertion checks **healthy completion**, not output quality — we are
regression-testing the harness, not grading the base.

| Path | Assertions |
|---|---|
| **hidden CLI `run`** (`--mode auto`, short budgets) | `auto` skips the clarify gate and actually runs research → docs (guarded would just pause at clarify and run no phase); the auto-loop then **stops at a gate** within the hard ceiling (no wedge / no `0/9` hang); exit code `0`; **research artifact** `output/*-research.md` produced (research ran, was not idle-killed, was not re-run forever — may be the budget-cut placeholder, which is fine, the assertion is "it exists + bounded"); pipeline progressed past research (≥1 planning doc); elapsed stayed under the hard ceiling (budgets honoured). |
| **agentic reality check** (a second `auto`-mode run, inside a fresh temp **git repo**) | reaches the frontend/backend code phases where the L3 cross-check lives; bounded, no wedge; **consistent reality signal** — EITHER real *source* changes are visible in `git status` (excluding the pipeline's own `output/` / `.umadev/` / `.claude/` writes), OR UmaDev emitted its `无文件变更 / no file changes → degraded` warning. The forbidden state is *base narrated work + tree unchanged + no warning* → that would mean the L3 anti-hallucination cross-check is dead. |
| **chat** (optional) | **Skipped by design**: chat is an interactive TUI with no headless one-shot verb. The same real-base subprocess + idle watchdog + git reality-check code it relies on is already exercised by the two `run` paths above; shipping a flaky pty test would add no real coverage. |

## How it skips when no base is available

The script exits `0` (a non-failure) at the first of these it hits:

- `git` not on `PATH`,
- no `claude` on `PATH`,
- `claude --version` produces no output (not logged in / unhealthy),
- `cargo build -p umadev` fails,
- `umadev` itself reports the backend as *not installed / unhealthy* mid-run
  (detected from its own message).

So on a machine without a logged-in base (CI, a fresh checkout) it prints a
clear `[skip]` explanation and returns green.

## Fail-safe guarantees

- Base unavailable → **skip** (exit 0), never a failure.
- A run overshooting its budget → **hard-killed** by a portable bash watchdog
  (no dependency on coreutils `timeout`).
- Temp dirs (workspace + binary) → **always removed** via an `EXIT` trap,
  even on Ctrl-C.
- It never edits the real repo's `output/` or the user's `~/.umadev`; every
  run writes into a throwaway `--project-root` temp dir.

## Tunables (environment variables)

| Var | Default | Meaning |
|---|---|---|
| `UMADEV_PHASE_BUDGET_SECS` | `90` | per-phase wall-clock budget |
| `UMADEV_RUN_BUDGET_SECS` | `120` | Test-1 run budget — stops the auto-loop at the docs gate (research + docs done) |
| `UMADEV_SMOKE_AGENTIC_RUN_BUDGET_SECS` | `600` | Test-2 run budget — larger so the auto-loop drives past the docs/preview gates into the **backend** phase, where the L3 git reality-check fires |
| `UMADEV_ADVISORY_TIMEOUT_SECS` | `30` | caps each post-docs critic/judge/consult call (keeps the auto tail bounded) |
| `UMADEV_IDLE_TIMEOUT_SECS` | `120` | base stdout-silence watchdog |
| `UMADEV_SMOKE_HARD_CEILING_SECS` | `900` | last-resort hard kill for the Test-1 run (a measured healthy run self-terminated at ~592s; this leaves headroom) |
| `UMADEV_SMOKE_AGENTIC_CEILING_SECS` | `1200` | last-resort hard kill for the Test-2 run (runs further, into code phases) |
| `UMADEV_BIN` | *(unset)* | use this prebuilt binary instead of building |

> **Why `auto` and not `guarded`?** Both tests run `--mode auto` on purpose.
> `guarded` (the product default) pauses at the **first gate — which is the
> pre-research `clarify` gate** — so a guarded run stops in ~10s having
> generated only clarify questions; the research / docs / code phases *never
> run*. That is useless for a smoke whose entire point is to exercise research
> (not idle-killed, not re-run forever) and the agentic git check. `auto` skips
> clarify and drives the real phases; the short budgets + the advisory-timeout
> cap + the hard ceiling keep it bounded so it stops cleanly at a gate instead
> of churning the whole pipeline.

> The budgets are short on purpose (a real default run takes 10-15 min). A tight
> phase budget may budget-*cut* a real research/docs phase down to its offline
> placeholder — that is acceptable here: the assertions check that the artifact
> **exists** and the run stayed **bounded** with no wedge, not that the research
> *content* is real. The artifact is still written either way, which is what
> proves research ran and was not stuck re-running.
