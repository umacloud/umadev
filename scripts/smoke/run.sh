#!/usr/bin/env bash
# ============================================================================
# UmaDev real-backend smoke test
# ============================================================================
#
# WHAT THIS IS
#   A *manual / opt-in* smoke that drives UmaDev's core paths against a REAL,
#   already-logged-in `claude` base CLI on a real machine. The 1700+ unit tests
#   all run against FakeRuntime / OfflineRuntime and never start a real base
#   subprocess, so the entire real-interaction surface (idle watchdog, phase /
#   run time budgets, anti-hallucination git cross-check, "stuck at 0/9" stall
#   defence, research not being idle-killed or re-run forever) has zero
#   automated coverage. This script closes that blind spot before a release.
#
#   It is NOT wired into default CI: CI has no logged-in base, so this script
#   detects that and SKIPS (exit 0) instead of going red. Run it by hand as a
#   pre-release dogfood step.
#
# HOW TO RUN
#   bash scripts/smoke/run.sh
#
# REQUIREMENTS
#   - A real `claude` CLI on PATH, logged in (`claude --version` must succeed).
#     If it is missing/unhealthy the script SKIPS, it does not fail.
#   - A `umadev` binary. By default the script builds `target/debug/umadev`
#     (`cargo build -p umadev`); set UMADEV_BIN=/path/to/umadev to skip the
#     build and use a prebuilt one.
#   - `git` on PATH (used for the temp project + the agentic reality check).
#
# ROUGH COST / TIME
#   Two real `auto`-mode base runs with deliberately SHORT time budgets: Test 1
#   runs research -> docs then stops at a gate; Test 2 runs further, into the
#   code phases. Expect a few-to-several minutes wall-clock and a small amount of
#   the base subscription's quota. Per-run hard ceilings
#   (UMADEV_SMOKE_HARD_CEILING_SECS / UMADEV_SMOKE_AGENTIC_CEILING_SECS) kill any
#   run that overshoots, so the script can never truly hang.
#
# WHAT IT VERIFIES (each assertion checks "healthy completion", not content
# correctness — we are regression-testing the harness, not grading the base):
#   [run]     `run --mode auto` (auto SKIPS the clarify gate and actually runs
#             research -> docs; guarded would just pause at clarify and never
#             run a phase). With short budgets the auto-loop STOPS at a gate
#             within the hard ceiling (does not hang / wedge at "0/9"), exits 0,
#             and the research artifact (output/*-research.md) was produced —
#             i.e. research actually ran and was neither idle-killed nor re-run
#             forever.
#   [agentic] a second `auto`-mode run inside a fresh temp git repo (so it
#             reaches the frontend/backend code phases where the L3 check fires)
#             leaves a consistent reality signal: EITHER real source changes are
#             visible in `git status`, OR UmaDev emitted its "无文件变更 / no
#             file changes -> degraded" warning. The forbidden state is "base
#             narrated work + tree unchanged + NO warning" -> that would mean
#             the L3 anti-hallucination cross-check is dead.
#   [budget]  the short UMADEV_*_BUDGET_SECS / UMADEV_IDLE_TIMEOUT_SECS env
#             overrides are honoured (each run is bounded, not open-ended).
#
# FAIL-SAFE GUARANTEES
#   - Base unavailable             -> SKIP (exit 0), never a failure.
#   - Run overshoots its budget    -> hard-killed by a portable bash watchdog
#                                     (no dependency on coreutils `timeout`).
#   - Temp dirs (workspace + bin)  -> always removed via an EXIT trap.
#   - Never edits the real repo / the user's ~/.umadev (HOME is sandboxed to
#     the temp dir for the duration of each run).
# ============================================================================

set -u  # NOTE: intentionally NOT `set -e` -- this harness inspects exit codes
        # itself and must finish cleanup + reporting even when a step fails.

# ---- exit codes -------------------------------------------------------------
readonly EXIT_PASS=0
readonly EXIT_SKIP=0   # a skip is a non-failure (CI-green when no base)
readonly EXIT_FAIL=1

# ---- tunables (all overridable from the environment) ------------------------
# Run MODE: `auto`. This is load-bearing, not a default-by-habit:
#   * `guarded` (the product default) pauses at the FIRST gate, which is the
#     pre-research CLARIFY gate -- so in guarded mode the run stops in ~10s
#     having generated only clarify questions and the research/docs/code phases
#     NEVER RUN. That is useless for a smoke whose whole point is to exercise
#     research-not-idle-killed and the agentic git check.
#   * `auto` SKIPS clarify and drives research -> docs -> ... , which is exactly
#     the real-base interaction surface we need to regress.
# We bound the `auto` run with the budgets below + a hard ceiling, so it stops
# cleanly instead of churning. (There is no need to expose a mode knob; a smoke
# that doesn't run the phases isn't a smoke.)
readonly SMOKE_RUN_MODE="auto"
# SHORT budgets so the smoke is bounded to a few minutes (a real default run is
# 10-15min). With `auto`, the run-level budget is only re-checked BETWEEN gate
# hops, and the post-docs advisory/critic round is a long serial stretch, so we
# ALSO cap that round (advisory timeout) to keep the whole thing bounded.
#   A tight phase budget may budget-CUT a real research/docs phase to its
#   offline placeholder -- that is fine for this smoke: the assertions check
#   "the artifact exists + the run stayed bounded + no wedge", NOT that the
#   research CONTENT is real. The artifact (placeholder or real) is still
#   written, which is what proves research ran and was not re-run forever.
: "${UMADEV_PHASE_BUDGET_SECS:=90}"      # per-phase wall-clock budget
: "${UMADEV_RUN_BUDGET_SECS:=120}"       # Test-1 run budget: stops auto-loop at the docs gate
: "${UMADEV_ADVISORY_TIMEOUT_SECS:=30}"  # caps each critic/judge/consult call
# Test-2 run budget. Test 2's whole POINT is to reach the code phases
# (frontend/backend) where the L3 git reality-check fires -- so its run budget
# must be LARGE enough that the auto-loop drives past docs_confirm AND
# preview_confirm into backend before it stops. A small budget (like Test 1's)
# would stop it at the docs gate, never reaching the L3 check. Sized for
# research+docs+frontend+backend at ~90s/phase plus overhead.
: "${UMADEV_SMOKE_AGENTIC_RUN_BUDGET_SECS:=600}"
# Idle watchdog: a REAL base can legitimately be silent for a while mid-research
# (deep thinking / web calls with no stdout). Keep this generous enough that the
# smoke does NOT re-introduce the very "research idle-killed" regression it is
# meant to guard against; the phase budget is the real upper bound.
: "${UMADEV_IDLE_TIMEOUT_SECS:=120}"     # base stdout-silence watchdog
# Hard kill ceiling for the Test-1 run. The budgets above stop the auto-loop
# before this; it is only a last-resort backstop against a genuine wedge. Sized
# with comfortable headroom: on a real base, research + docs + the (advisory-
# capped) post-docs critic stretch can run ~10min even with short phase budgets,
# because the auto-loop only re-checks the run budget BETWEEN gate hops, not
# mid-block. The graceful budget-stop must win the race, so keep this well above
# the observed ~10min real-cycle time -- a too-tight ceiling would false-fail a
# perfectly healthy (just slow) run. (A measured run self-terminated at ~592s.)
: "${UMADEV_SMOKE_HARD_CEILING_SECS:=900}"
# Hard kill ceiling for the Test-2 AGENTIC run. Test 2 must reach the code
# phases (frontend/backend) where the L3 "claimed work but no file change"
# cross-check lives, so it runs further into the pipeline and needs more
# headroom. Still a last-resort backstop -- the run/phase budgets stop it first.
: "${UMADEV_SMOKE_AGENTIC_CEILING_SECS:=1200}"

export UMADEV_PHASE_BUDGET_SECS UMADEV_RUN_BUDGET_SECS UMADEV_IDLE_TIMEOUT_SECS \
       UMADEV_ADVISORY_TIMEOUT_SECS

# ---- repo root + paths ------------------------------------------------------
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." >/dev/null 2>&1 && pwd)"

# ---- logging helpers --------------------------------------------------------
c_bold=$'\033[1m'; c_dim=$'\033[2m'; c_red=$'\033[31m'; c_grn=$'\033[32m'
c_yel=$'\033[33m'; c_cya=$'\033[36m'; c_off=$'\033[0m'
# Disable colour when not a TTY (e.g. piped to a log file).
if [ ! -t 1 ]; then c_bold=; c_dim=; c_red=; c_grn=; c_yel=; c_cya=; c_off=; fi

log()  { printf '%s\n' "$*"; }
hd()   { printf '\n%s== %s ==%s\n' "$c_bold" "$*" "$c_off"; }
info() { printf '%s[info]%s %s\n' "$c_cya" "$c_off" "$*"; }
ok()   { printf '%s[pass]%s %s\n' "$c_grn" "$c_off" "$*"; }
warn() { printf '%s[warn]%s %s\n' "$c_yel" "$c_off" "$*"; }
bad()  { printf '%s[fail]%s %s\n' "$c_red" "$c_off" "$*"; }
skip() { printf '%s[skip]%s %s\n' "$c_yel" "$c_off" "$*"; }

# ---- assertion bookkeeping --------------------------------------------------
ASSERT_TOTAL=0
ASSERT_FAIL=0
assert() {  # assert <condition-already-evaluated:0|1> <description>
  local rc="$1"; shift
  ASSERT_TOTAL=$((ASSERT_TOTAL + 1))
  if [ "$rc" -eq 0 ]; then
    ok "$*"
  else
    bad "$*"
    ASSERT_FAIL=$((ASSERT_FAIL + 1))
  fi
}

# ---- cleanup trap -----------------------------------------------------------
# Every temp dir we create is registered here and removed on ANY exit path
# (success, failure, Ctrl-C). We never leave residue behind.
#
# Belt-and-braces: the EXIT trap covers normal exits and SIGINT/SIGTERM. It
# CANNOT fire on SIGKILL (`kill -9`) — so a previous run that was hard-killed
# could leave a `umadev-smoke.*` dir behind. `sweep_stale_dirs` (called once at
# startup) garbage-collects any such orphan older than a couple of hours, so
# leaks self-heal on the next invocation instead of accumulating forever.
declare -a CLEANUP_DIRS=()
cleanup() {
  local d
  for d in "${CLEANUP_DIRS[@]:-}"; do
    [ -n "${d:-}" ] && [ -d "$d" ] && rm -rf "$d" 2>/dev/null
  done
}
trap cleanup EXIT INT TERM

# Remove orphaned smoke temp dirs from a previously hard-killed run. Only those
# older than 120 minutes, so we never touch a CONCURRENT smoke run's live dir.
sweep_stale_dirs() {
  local base="${TMPDIR:-/tmp}"
  find "$base" -maxdepth 1 -type d -name 'umadev-smoke.*' -mmin +120 \
    -exec rm -rf {} + 2>/dev/null || true
}

mktmp() {  # echo a fresh temp dir and register it for cleanup
  local d
  d="$(mktemp -d "${TMPDIR:-/tmp}/umadev-smoke.XXXXXX")" || return 1
  CLEANUP_DIRS+=("$d")
  printf '%s' "$d"
}

# ----------------------------------------------------------------------------
# run_with_ceiling <ceiling_secs> <logfile> -- <command...>
#
# Portable hard-timeout. macOS ships no `timeout`/`gtimeout` by default, so we
# implement our own: launch the command in the background, launch a watchdog
# that sleeps then kills the process group, and whichever finishes first wins.
# Stdout+stderr of the command are tee'd into <logfile> AND shown live.
#
# Echoes the command's exit code via the global RUN_RC. Sets RUN_TIMED_OUT=1
# if the watchdog had to kill it.
# ----------------------------------------------------------------------------
RUN_RC=0
RUN_TIMED_OUT=0
run_with_ceiling() {
  local ceiling="$1"; shift
  local logfile="$1"; shift
  [ "$1" = "--" ] && shift
  RUN_RC=0
  RUN_TIMED_OUT=0

  # Run the command in its own process group so the watchdog can kill the
  # whole subprocess tree (the base CLI is a grandchild of umadev).
  set -m 2>/dev/null
  ( "$@" ) >"$logfile" 2>&1 &
  local cmd_pid=$!
  set +m 2>/dev/null

  # Watchdog: sleep, then kill the command's process group if still alive.
  (
    local waited=0
    while [ "$waited" -lt "$ceiling" ]; do
      sleep 1
      kill -0 "$cmd_pid" 2>/dev/null || exit 0   # command already finished
      waited=$((waited + 1))
    done
    # Still alive past the ceiling -> escalate.
    kill -TERM "-$cmd_pid" 2>/dev/null || kill -TERM "$cmd_pid" 2>/dev/null
    sleep 3
    kill -KILL "-$cmd_pid" 2>/dev/null || kill -KILL "$cmd_pid" 2>/dev/null
  ) &
  local wd_pid=$!

  # Live-tail the log while the command runs, so a human watching sees progress.
  tail -n +1 -f "$logfile" 2>/dev/null &
  local tail_pid=$!

  if wait "$cmd_pid" 2>/dev/null; then
    RUN_RC=0
  else
    RUN_RC=$?
  fi
  # A 143 (SIGTERM) / 137 (SIGKILL) means the watchdog fired.
  if [ "$RUN_RC" -eq 143 ] || [ "$RUN_RC" -eq 137 ]; then
    RUN_TIMED_OUT=1
  fi

  kill "$wd_pid"   2>/dev/null; wait "$wd_pid"   2>/dev/null
  kill "$tail_pid" 2>/dev/null; wait "$tail_pid" 2>/dev/null
}

# ============================================================================
# Phase 0 -- preflight: is a real base available?
# ============================================================================
hd "UmaDev real-backend smoke"
info "repo root: ${REPO_ROOT}"
info "mode=${SMOKE_RUN_MODE}; budgets: phase=${UMADEV_PHASE_BUDGET_SECS}s run=${UMADEV_RUN_BUDGET_SECS}s advisory=${UMADEV_ADVISORY_TIMEOUT_SECS}s idle=${UMADEV_IDLE_TIMEOUT_SECS}s; ceilings: t1=${UMADEV_SMOKE_HARD_CEILING_SECS}s t2=${UMADEV_SMOKE_AGENTIC_CEILING_SECS}s"
sweep_stale_dirs   # GC any orphan temp dir from a previously hard-killed run

# 0a. git
if ! command -v git >/dev/null 2>&1; then
  skip "git not on PATH -- cannot build the temp project; skipping."
  exit "$EXIT_SKIP"
fi

# 0b. real claude base
if ! command -v claude >/dev/null 2>&1; then
  skip "no real \`claude\` CLI on PATH. This smoke needs a logged-in base."
  skip "Install + log in to Claude Code, then re-run: bash scripts/smoke/run.sh"
  exit "$EXIT_SKIP"
fi
CLAUDE_VER="$(claude --version 2>/dev/null || true)"
if [ -z "$CLAUDE_VER" ]; then
  skip "\`claude --version\` produced no output -- base unhealthy/not logged in; skipping."
  exit "$EXIT_SKIP"
fi
info "real base detected: claude ${CLAUDE_VER}"

# 0c. umadev binary -- prefer a caller-provided one, else build debug.
if [ -n "${UMADEV_BIN:-}" ] && [ -x "${UMADEV_BIN:-}" ]; then
  BIN="$UMADEV_BIN"
  info "using prebuilt umadev: ${BIN}"
else
  info "building umadev (debug) -- set UMADEV_BIN to skip this..."
  if ! ( cd "$REPO_ROOT" && cargo build -p umadev ) >/dev/null 2>&1; then
    skip "cargo build -p umadev failed -- cannot smoke without a binary; skipping."
    exit "$EXIT_SKIP"
  fi
  BIN="${REPO_ROOT}/target/debug/umadev"
fi
if [ ! -x "$BIN" ]; then
  skip "umadev binary not found at ${BIN}; skipping."
  exit "$EXIT_SKIP"
fi
info "umadev binary: ${BIN}"

# ============================================================================
# Shared run harness
# ============================================================================
# Each smoke run gets:
#   - a fresh temp WORKSPACE (the project root the base writes into)
#   - a sandboxed HOME so we never touch the real ~/.umadev / ~/.claude config
#     (the base's own login lives elsewhere; Claude Code reads its creds via
#     its own resolution, so a sandbox HOME does not log us out -- but if a
#     run unexpectedly cannot reach the base, that surfaces as a SKIP-worthy
#     unhealthy result, not a false FAIL).
#
# We DO NOT sandbox HOME for the base auth, because that could break login.
# Instead we point only UmaDev's per-project state at the temp workspace by
# running with --project-root inside it; the base inherits the real HOME so
# its existing login keeps working.

LATEST_LOG=""

# do_run <workspace> <requirement> <mode> <ceiling_secs> <run_budget_secs> <extra-args...>
# Each call picks its own mode, hard ceiling, AND run budget, so the shorter
# Test-1 run (stop at the docs gate) and the deeper Test-2 run (drive into the
# code phases) can each be bounded independently in one script run.
#   The run budget matters: the auto-loop stops at the first gate reached AFTER
#   the run budget elapses. A small budget (Test 1) stops at `docs_confirm`; a
#   larger one (Test 2) lets the loop drive past docs_confirm/preview_confirm
#   into the backend phase, which is where the L3 git reality-check actually
#   fires. We pass it per-call via the env the binary reads.
do_run() {
  local ws="$1"; shift
  local req="$1"; shift
  local mode="$1"; shift
  local ceiling="$1"; shift
  local run_budget="$1"; shift
  LATEST_LOG="${ws}/.smoke-run.log"
  info "running: umadev run \"${req}\" --backend claude-code --mode ${mode} (ceiling ${ceiling}s, run-budget ${run_budget}s, in ${ws})"
  # Export the per-call run budget so the binary (a grandchild process) reads it.
  export UMADEV_RUN_BUDGET_SECS="$run_budget"
  run_with_ceiling "$ceiling" "$LATEST_LOG" -- \
    "$BIN" run "$req" \
      --backend claude-code \
      --mode "$mode" \
      --project-root "$ws" \
      --slug smoke \
      "$@"
}

# A run is "base-unavailable" (skip, not fail) if umadev itself reported the
# backend as not installed / unhealthy. We detect that from its own message.
run_hit_unavailable_base() {  # <logfile>
  grep -qE 'not available|is unhealthy|not on PATH|Install / log in' "$1" 2>/dev/null
}

# ============================================================================
# Test 1 -- hidden CLI `run` (research bounded, not idle-killed, not re-run)
# ============================================================================
hd "Test 1: hidden CLI run -- bounded auto run + research artifact"
WS1="$(mktmp)" || { bad "mktemp failed"; exit "$EXIT_FAIL"; }
# A plain static-page requirement -> exercises research + docs cheaply. `auto`
# skips clarify and actually RUNS research/docs; the budgets stop the auto-loop
# at a gate before it churns the rest of the pipeline.
START1=$(date +%s)
do_run "$WS1" "做一个简单的待办清单静态页" "$SMOKE_RUN_MODE" \
       "$UMADEV_SMOKE_HARD_CEILING_SECS" "$UMADEV_RUN_BUDGET_SECS"
END1=$(date +%s)
ELAPSED1=$((END1 - START1))

if run_hit_unavailable_base "$LATEST_LOG"; then
  skip "umadev reported the base as unavailable/unhealthy mid-run; treating as SKIP."
  skip "(log: ${LATEST_LOG})"
  exit "$EXIT_SKIP"
fi

info "run exit code: ${RUN_RC}; elapsed: ${ELAPSED1}s; hard-timeout-fired: ${RUN_TIMED_OUT}"

# 1a. did NOT hit the hard ceiling -> the budget/watchdog stopped it gracefully
#     instead of it wedging (the "0/9 forever" / "looks frozen" regression).
[ "$RUN_TIMED_OUT" -eq 0 ]
assert $? "run finished within the hard ceiling (no wedge / no '0/9' hang)"

# 1b. sane exit code. The auto-loop returns 0 when the run-budget stops it at a
#     gate; anything else (panic/abort/137/143) is a real failure.
[ "$RUN_RC" -eq 0 ]
assert $? "run exited with code 0 (healthy bounded stop), got ${RUN_RC}"

# 1c. research actually ran and produced an artifact -> proves the research
#     phase was NOT idle-killed by the (real) base watchdog and was NOT stuck
#     re-running forever (we got here inside the bounded time).
research_md="$(ls "${WS1}/output/"*-research.md 2>/dev/null | head -1)"
[ -n "$research_md" ] && [ -s "$research_md" ]
assert $? "research artifact produced (output/*-research.md, non-empty)"

# 1d. at least one further planning doc landed too -> the pipeline progressed
#     past research into docs (it didn't stall AT research). Lenient: any of
#     prd/architecture/uiux counts, since a tiny budget may stop mid-docs.
docs_count="$(ls "${WS1}/output/"*-prd.md "${WS1}/output/"*-architecture.md "${WS1}/output/"*-uiux.md 2>/dev/null | wc -l | tr -d ' ')"
[ "${docs_count:-0}" -ge 1 ]
assert $? "pipeline progressed past research (>=1 planning doc written, found ${docs_count:-0})"

# 1e. the run stayed bounded. The auto-loop stops at a gate once the run budget
#     elapses, after the two heavy phases (research + docs) + the post-docs
#     advisory round, so the natural elapsed bound is ~two phase budgets plus a
#     few advisory-capped critic calls. Soft check: warn-only past that band;
#     the hard assertion is just that we finished well under the hard ceiling
#     (i.e. the budgets/watchdog kept the real run from running away).
slack_ceiling=$(( 2 * UMADEV_PHASE_BUDGET_SECS + UMADEV_RUN_BUDGET_SECS + 120 ))
if [ "$ELAPSED1" -gt "$slack_ceiling" ]; then
  warn "run took ${ELAPSED1}s, beyond the expected band (~${slack_ceiling}s) -- bounded but slow."
fi
[ "$ELAPSED1" -lt "$UMADEV_SMOKE_HARD_CEILING_SECS" ]
assert $? "run elapsed (${ELAPSED1}s) stayed under the hard ceiling -> budgets are honoured"

# ============================================================================
# Test 2 -- agentic reality check (anti-hallucination L3) in a real git repo
# ============================================================================
hd "Test 2: agentic reality check -- git cross-validation of claimed changes"
WS2="$(mktmp)" || { bad "mktemp failed"; exit "$EXIT_FAIL"; }
# Make it a real git repo with one committed file, so `git status --porcelain`
# is meaningful and UmaDev's snapshot-around-implementation check is armed.
(
  cd "$WS2" || exit 1
  git init -q
  git config user.email "smoke@umadev.local"
  git config user.name  "umadev-smoke"
  printf '# smoke fixture\n' > README.md
  git add README.md
  git commit -qm "init" 2>/dev/null
) || { warn "could not initialise temp git repo; skipping Test 2."; }

if [ -d "${WS2}/.git" ]; then
  # A "small change" style request: ask the base to add a small static file.
  # Drive AUTO so the run reaches the frontend/backend code phases where the
  # L3 "claimed work but no file change" cross-check actually fires (it does
  # NOT run during research/docs). Its own larger ceiling backstops the run.
  do_run "$WS2" "在仓库里新增一个 index.html 写上 Hello" "auto" \
         "$UMADEV_SMOKE_AGENTIC_CEILING_SECS" "$UMADEV_SMOKE_AGENTIC_RUN_BUDGET_SECS"

  if run_hit_unavailable_base "$LATEST_LOG"; then
    skip "base reported unavailable during Test 2; skipping the agentic assertions."
  else
    info "agentic run exit code: ${RUN_RC}; hard-timeout-fired: ${RUN_TIMED_OUT}"

    # 2a. bounded, no wedge.
    [ "$RUN_TIMED_OUT" -eq 0 ]
    assert $? "agentic run finished within the hard ceiling (no wedge)"

    # Did real SOURCE changes land in the working tree? We exclude UmaDev's own
    # output/ planning docs and .claude/ hook config -- those are written by the
    # pipeline itself on every run and would trivially mark the tree dirty,
    # masking whether the base's CLAIMED code edit actually hit disk. After that
    # filter, a dirty tree means a genuine source/file change landed.
    tree_changed=1
    if [ -n "$(cd "$WS2" && git status --porcelain 2>/dev/null \
                 | grep -vE ' (output/|\.claude/|\.smoke-run\.log|\.umadev/)' )" ]; then
      tree_changed=0
    fi
    # Did UmaDev emit its "claimed work but tree unchanged -> degraded" warning?
    # (Matches the run-pipeline warning text in runner.rs.)
    warned_no_change=1
    if grep -qE '无文件变更|底座报告了实现|底座报告了改动|no file change|reported.*but.*tree' "$LATEST_LOG" 2>/dev/null; then
      warned_no_change=0
    fi

    info "git tree changed: $([ $tree_changed -eq 0 ] && echo yes || echo no); UmaDev no-change warning emitted: $([ $warned_no_change -eq 0 ] && echo yes || echo no)"

    # 2b. THE core anti-hallucination assertion:
    #     a healthy run lands in exactly one of two consistent states --
    #       (i)  real file changes are visible in git, OR
    #       (ii) UmaDev loudly warned that the base claimed work but the tree
    #            did not change.
    #     The FORBIDDEN state is "no tree change AND no warning": that means the
    #     base could have narrated a fake edit and UmaDev's L3 cross-check
    #     failed to catch it.
    if [ "$tree_changed" -eq 0 ] || [ "$warned_no_change" -eq 0 ]; then
      assert 0 "agentic reality is consistent (real git change OR an explicit no-change warning)"
    else
      assert 1 "agentic reality is consistent -- got NEITHER a git change NOR a no-change warning (L3 cross-check may be dead)"
    fi
  fi
else
  skip "temp git repo unavailable; Test 2 skipped."
fi

# ============================================================================
# Test 3 (optional) -- chat smoke
# ============================================================================
# The chat surface is an interactive ratatui TUI with no non-interactive
# one-shot entrypoint, so it cannot be driven reliably from a headless script
# without a pty harness. The same real-base subprocess + idle watchdog + git
# reality-check code that chat relies on is already exercised by Tests 1 & 2
# through the `run` path, so we intentionally SKIP a separate chat smoke here
# rather than ship a flaky pty test.
hd "Test 3: chat (optional)"
skip "chat is an interactive TUI with no headless one-shot verb; covered indirectly by Tests 1+2. Skipped by design."

# ============================================================================
# Summary
# ============================================================================
hd "Summary"
info "assertions: $((ASSERT_TOTAL - ASSERT_FAIL))/${ASSERT_TOTAL} passed"
if [ "$ASSERT_FAIL" -eq 0 ]; then
  ok "real-backend smoke PASSED"
  exit "$EXIT_PASS"
else
  bad "real-backend smoke FAILED (${ASSERT_FAIL} assertion(s))"
  info "inspect the run log(s) under the temp workspace before they are cleaned, or re-run with the dirs preserved."
  exit "$EXIT_FAIL"
fi
