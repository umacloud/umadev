//! The director build loop — the USB / smart-hardware model of
//! `docs/AGENT_WIELDS_BASE_ARCHITECTURE.md` (simplified: NO marker protocol).
//!
//! ## The model: UmaDev is firmware; the base is the brain + hands
//!
//! UmaDev is a smart device with its own firmware — a senior team-director
//! identity, engineering taste, accumulated knowledge, governance, and memory —
//! but **no compute of its own**. Plugged into one of five bases (three native
//! plus Grok Build/Kimi Code over ACP) over the continuous session, it **borrows the base's
//! intelligence and hands**
//! to get work done, the way a smart peripheral borrows a host computer's CPU and
//! storage. The firmware is injected into the base (via the directive + system
//! prompt the caller built — `experts::director_build_directive` /
//! `experts::director_with_team_tools`); the base then thinks, plans, and writes
//! files with its OWN internal agentic tool loop.
//!
//! **The key insight that retired the old marker protocol:** the base is already a
//! whole brain. Once UmaDev's firmware is injected, the base ITSELF plays PM /
//! architect / frontend / QA internally and builds the goal end to end. It does
//! **not** need to emit `<<<umadev:summon …>>>` markers for UmaDev to "summon a
//! team" from the outside — real-machine testing showed the base writes good,
//! multi-role code with ZERO markers, because the team lives inside the base's
//! head, steered by the firmware. So this loop no longer asks the base to speak a
//! scheduling protocol, and no longer parses one.
//!
//! ## The boundary: UmaDev grows NO "operating" machinery of its own
//!
//! The base is a complete Agent already — its model is the brain, its CLI tools are
//! the body that builds code, writes files, runs commands, runs tests, and fixes
//! bugs. UmaDev is the EXTERNAL agent that plugs into that body and shares that
//! brain. So **UmaDev never grows its own build/write/run/test/fix capability** —
//! all of that work is the base's body using the base's own tools. The only two
//! tiny things UmaDev does for itself (firmware business, not "operating"):
//!
//! - **Governance** — the background safety net riding on the base's file writes
//!   (the existing PreToolUse hook). Untouched here.
//! - **A read-only honesty check** — when the base says "built it", UmaDev reads the
//!   disk to confirm real code actually exists ([`crate::acceptance::source_files`]
//!   via [`crate::director::verify`] / [`VerifyKind::SourcePresent`]). Tiny,
//!   deterministic, read-only — it just stops a hallucinated "done".
//!
//! Everything else — building, and FIXING what QC surfaces — the base's body does
//! with its own tools, steered by a fix directive UmaDev feeds back. UmaDev reads
//! objective facts and judges; it does not operate.
//!
//! ## The loop: end-to-end build → UmaDev honesty/QC read → bounded feedback-fix
//!
//! 1. Drive the base end to end on the goal (one firmware-injected turn — its own
//!    agentic tool loop runs PM→…→QA internally and writes real files).
//! 2. When the turn settles, **UmaDev reads reality ITSELF** (it does not wait for
//!    the base to ask, and it does not operate):
//!    - **honesty hard floor (deterministic, read-only)** — did real source files
//!      actually land ([`crate::acceptance::source_files`])? This is UmaDev's own
//!      tiny check.
//!    - **optional fork review (read-only, borrows the brain)** — for a build that
//!      produced real code, fork the review team on isolated read-only sessions and
//!      collect blocking findings ([`crate::director::review`]). UmaDev judges with
//!      the borrowed brain; it writes nothing.
//! 3. If QC found blocking problems, fold them into ONE fix directive and feed it
//!    back to the base over the same session (the USB channel) — and the directive
//!    tells the base's body to **run its own build/test and fix the cause with its
//!    own tools**. Then re-read. **Bounded** by the internal QC-round limit.
//! 4. Clean mechanical QC → done; report honestly. A quiet/tool-only reply is
//!    never completion evidence by itself.
//!
//! Note on build/test: UmaDev does NOT run the build itself as its gate — the base's
//! body runs build/test (it has the tools), and the fix directive explicitly asks it
//! to. The optional [`VerifyKind::BuildTest`] read is retained only as a cheap
//! reuse of an EXISTING reader to surface an objective failure fact when a manifest
//! is present; it is positioned as "read a fact", never as "UmaDev operates".
//!
//! ## Floor preserved (every invariant still holds)
//!
//! 1. **Single-writer.** Only the MAIN base turn mutates the workspace, under the
//!    run-lock the caller holds. UmaDev's checks are read-only: the source floor
//!    reads disk; the QC review runs on isolated read-only forks
//!    ([`crate::director::review`]). Nothing UmaDev does writes the workspace.
//! 2. **Objective floor untouched.** The source-present floor remains a
//!    deterministic, read-only check. Reviewer blockers seed bounded fixes and
//!    must clear before a clean claim; Rust-side budgets still own termination.
//! 3. **Governance + audit.** Every base turn (the first build and every fix pass)
//!    drives the SAME governed/audited session; the PreToolUse hook still fires
//!    under every write.
//! 4. **No new endpoint.** Every QC review reads over the SAME borrowed brain + its
//!    `fork()`; no extra model endpoint, no API key.
//! 5. **Bounded failure.** A required review that cannot run is explicitly
//!    unavailable, never a pass. It may leave the build incomplete, but cannot
//!    wedge the loop; a dead session is an honest `Failed`, never a panic.
//! 6. **Reversible.** This loop is the DEFAULT `/run` path; the legacy fixed
//!    pipeline (`UMADEV_LEGACY_PIPELINE=1`) is untouched.

use std::{sync::Arc, time::Duration};

use umadev_runtime::{
    ApprovalDecision, BaseSession, HostApprovalOption, HostApprovalOptionKind, HostRequest,
    HostResponse, SessionEvent, StreamEvent, ToolActivity, TurnStatus, Usage,
};

use crate::critics::ReviewStatus;
use crate::director::{self, ReviewResult, VerifyKind, VerifyResult};
use crate::events::{EngineEvent, EventSink};
use crate::knowledge_feedback::{commit_sent_memories, SentReceiptGuard, TurnOutcome};
use crate::phases::KnowledgeDigest;
use crate::plan_state::{self, Plan, StepStatus};
use crate::router::RoutePlan;
use crate::runner::RunOptions;
use crate::trust::requires_confirmation_with_ledger;
use umadev_spec::Phase;

mod quality_evidence;
mod resume;

use quality_evidence::{has_reproduction_test, runtime_proof_blocking};
pub use resume::{
    has_resumable_director_plan, has_resumable_run, is_budget_pause_reason, transient_resume_hint,
};
use resume::{load_resumable_plan, record_artifact_versions};

/// The hard ceiling on auto-QC feedback-fix rounds in one `/run`. One round is: the
/// base builds (or fixes) end to end, then UmaDev runs its objective QC pass. A
/// clean pass ends the loop immediately, so a simple goal that builds correctly the
/// first time spends ZERO fix rounds. The cap only bounds a goal that keeps failing
/// QC — after it, the loop terminates as [`DirectorLoopOutcome::Failed`] with the
/// residual objective evidence. Mirrors the proven bounded-rework shape (`continuous::MAX_REWORK_ROUNDS`)
/// at the build level: small + decisive, not an open-ended grind.
const MAX_QC_ROUNDS: usize = 3;

/// Default idle watchdog window, in seconds, for the director loop's per-event
/// wait. A base that hangs (stops emitting stdout but never exits) would
/// otherwise leave [`drive_one_turn`] blocked on `next_event().await` FOREVER —
/// no `TurnDone`, no settle, the TUI's `thinking` stuck and the queued input
/// never drained. This is the regression the USB → continuous-session move
/// introduced: the old single-shot `complete_streaming` path had exactly this
/// watchdog (`umadev-host` keys the same env).
///
/// 600s (10 min) is the floor for a base that is NOT mid-tool. The old 300s
/// default falsely killed legitimate long work: a base emits ONE tool-use event
/// then the tool (a `docker build` / a compile / `npm install` / a long test run)
/// runs SILENTLY for minutes, so the 300s window elapsed and the watchdog killed a
/// base that was busy, not hung. The real protection for that case is the
/// tool-aware LIVENESS POLL ([`tool_idle_timeout`] re-checked while a tool is in
/// flight — see [`IdleBudget`] / [`next_event_idle`]): a tool of ANY duration with a
/// LIVE base keeps waiting. This base default is the backstop for a TRULY silent base
/// (no tool running), bumped to 1200s so an ordinary slow turn - or a base pointed at a
/// rate-limited third-party model doing its OWN internal retry backoff - is never mis-killed
/// before its retry can land (raise UMADEV_IDLE_TIMEOUT_SECS for even more patience).
/// Note this continuous-session watchdog has NO per-event hard ceiling (only the
/// run budget), unlike the single-shot host path whose idle watchdog keeps its own
/// 300s default below its 600s hard `timeout` ceiling.
const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 1200;

/// Default LIVENESS-POLL interval, in seconds, used while the base is plausibly
/// executing a tool (see [`IdleBudget`] / [`tool_phase_transition`] / [`next_event_idle`]).
/// This is NOT a grace cap on how long a tool may run: a long-running tool is
/// legitimately silent for a long time — a clean release build, a cold `docker build`,
/// a big `npm install`, or a full integration-test run can each emit NOTHING for tens
/// of minutes, and a dev server / data job can run for hours — so capping the silence
/// is the wrong model (the user rejected a fixed 30-min cap). Instead, every time this
/// interval elapses with no event while a tool is in flight, the watchdog RE-CHECKS
/// that the base process is still alive: a live base means the tool is genuinely
/// running, so UmaDev keeps waiting; only a DEAD base (or the overall run-budget
/// deadline) settles the turn. 5 min is a calm re-check cadence — short enough to
/// notice a dead base promptly, long enough to add no measurable overhead. Overridable
/// via `UMADEV_TOOL_IDLE_TIMEOUT_SECS`.
const DEFAULT_TOOL_IDLE_TIMEOUT_SECS: u64 = 300;

/// The idle watchdog window for one `next_event().await`, from
/// `UMADEV_IDLE_TIMEOUT_SECS` (the SAME env the single-shot host watchdog reads —
/// `umadev_host`), falling back to the internal default idle timeout. A non-positive /
/// unparseable value falls back to the default (fail-open: a bad env never
/// disables the watchdog, which would re-introduce the permanent hang). Read once
/// per turn at the app boundary, not per wait, so a mid-turn env flip can't race.
///
/// `pub` so every main-session event pump (this loop, plus [`crate::continuous`]'s
/// `drive_phase` / `drive_rework_turn`, AND the TUI chat path) reads the SAME
/// window from ONE place — the consistency the P1-11 fix depends on.
pub fn idle_timeout() -> Duration {
    let secs = std::env::var("UMADEV_IDLE_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_IDLE_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

/// The LIVENESS-POLL interval used while the base is plausibly mid-tool, from
/// `UMADEV_TOOL_IDLE_TIMEOUT_SECS`, falling back to the internal default tool-idle interval
/// (5 min). It is how OFTEN the watchdog re-checks the base is alive while a tool runs,
/// NOT a cap on how long the tool may take — a tool of any duration with a live base
/// keeps waiting (see the internal idle-event pump). A non-positive / unparseable value falls
/// back to the default (fail-open: a bad env never disables the liveness re-check).
/// Read once per turn at the pump boundary (folded into [`IdleBudget::from_env`]), not
/// per wait, so a mid-turn env flip can't race. `pub` so the TUI chat path shares the
/// exact same source.
pub fn tool_idle_timeout() -> Duration {
    let secs = std::env::var("UMADEV_TOOL_IDLE_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_TOOL_IDLE_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

/// The two idle-watchdog windows for a base turn, read ONCE at the run / pump
/// boundary (not per wait, so a mid-turn env flip can't race): the `base` window for a
/// quiet-or-hung base that is NOT running a tool, and the `tool` LIVENESS-POLL interval
/// used while the base is plausibly mid-tool.
///
/// The mechanism that fixes "a real build went silent and got killed": a base emits a
/// tool-use event, then the tool itself (a `docker build` / a compile / `npm install` /
/// a long test / a dev server / a data job) runs SILENTLY — for minutes or hours. While
/// such a tool is in flight the watchdog does NOT cap the silence; it polls on the
/// internal `tool` interval and, each time the interval elapses with no event,
/// re-checks the base is alive — a live base keeps waiting, only a DEAD base (or the
/// overall run-budget deadline) settles (see the internal idle-event pump). When NO tool is in
/// flight the `base` window applies, so a TRULY hung base (no tool running) STILL
/// settles at the base window and the watchdog never becomes unbounded for a genuine
/// hang. The caller flips the in-tool-call state with [`tool_phase_transition`] as it
/// observes each event.
#[derive(Debug, Clone, Copy)]
pub struct IdleBudget {
    /// Window for a base that is NOT mid-tool (quiet / hung) —
    /// `UMADEV_IDLE_TIMEOUT_SECS`.
    base: Duration,
    /// Liveness-poll interval while a tool is plausibly executing —
    /// `UMADEV_TOOL_IDLE_TIMEOUT_SECS`.
    tool: Duration,
}

impl IdleBudget {
    /// Read both windows from the environment once, fail-open to the defaults.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            base: idle_timeout(),
            tool: tool_idle_timeout(),
        }
    }

    /// Build an explicit budget (used by tests and the chat path, which want a
    /// deterministic window without touching the process environment).
    #[must_use]
    pub fn new(base: Duration, tool: Duration) -> Self {
        Self { base, tool }
    }

    /// The idle window for the NEXT `next_event_idle` wait, given whether a tool is
    /// plausibly mid-flight: the `tool` liveness-poll interval while one is executing,
    /// else the `base` window. Pure.
    #[must_use]
    pub fn window(self, in_tool_call: bool) -> Duration {
        if in_tool_call {
            self.tool
        } else {
            self.base
        }
    }
}

/// Whether an observed [`SessionEvent`] flips the "a tool is plausibly executing"
/// state used to pick the idle window. A [`SessionEvent::ToolCall`] is the base
/// kicking off a tool (claude's `tool_use` block, opencode's `running` frame, codex's
/// completed command item) — the tool may then run SILENTLY for minutes or hours, so
/// the next wait should switch to the liveness-poll window ⇒ `Some(true)`. A
/// [`SessionEvent::ToolResult`] is a tool finishing ⇒ back to the base window
/// (`Some(false)`). Any other event leaves the flag unchanged ⇒ `None`.
///
/// This gives TRUE mid-tool tracking on bases that mark tool start and finish
/// distinctly (claude: `tool_use` then a later `tool_result`; opencode: a `running`
/// then a `completed` frame), and the documented HEURISTIC on a base that only
/// surfaces COMPLETED tool calls (codex emits the `ToolCall` + `ToolResult` together
/// at item completion): there, a `ToolCall` still arms the liveness-poll window for the
/// NEXT wait, on the assumption the base likely just kicked off something slow. Pure.
#[must_use]
pub fn tool_phase_transition(ev: &SessionEvent) -> Option<bool> {
    match ev {
        SessionEvent::ToolCall { .. } | SessionEvent::ToolCallCorrelated { .. } => Some(true),
        SessionEvent::ToolResult { .. } | SessionEvent::ToolResultCorrelated { .. } => Some(false),
        _ => None,
    }
}

/// Default wall-clock budget for a build loop — a generous 30 min, enough for a
/// thorough deliberate build (plan → step scheduling → QC → acceptance rework) yet
/// a hard ceiling so a build can never run unbounded.
const DEFAULT_RUN_BUDGET_SECS: u64 = 1_800;

/// The wall-clock budget for ONE build loop, read from `UMADEV_RUN_BUDGET_SECS`
/// (falling back to [`DEFAULT_RUN_BUDGET_SECS`]). This is a GRACEFUL ceiling, not a
/// kill switch: when it is reached the loop stops scheduling NEW work, runs the
/// final gate on what's already built, and exits with an honest "budget reached"
/// note — never a hard abort mid-write. A non-positive / unparseable value falls
/// back to the default (fail-open: a bad env never removes the ceiling).
///
/// This is the SLIDING idle window (see [`sliding_deadline`]): a build is bounded by
/// this much wall-clock of NO base productivity, not this much wall-clock TOTAL. A
/// build that keeps producing (real tool-calls / text) resets the window and runs on
/// up to the ABSOLUTE ceiling [`run_budget_absolute`], so a slow-but-progressing
/// build is never guillotined mid-progress.
pub(crate) fn run_budget() -> Duration {
    let secs = std::env::var("UMADEV_RUN_BUDGET_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_RUN_BUDGET_SECS);
    Duration::from_secs(secs)
}

/// How many times the sliding idle window ([`run_budget`]) a build may run in TOTAL
/// wall-clock — even while CONTINUOUSLY productive — before the ABSOLUTE hard ceiling
/// stops it. The idle window slides on base productivity so a slow-but-progressing
/// build is not cut mid-progress; this multiplier is the pathology backstop that
/// still bounds a base that emits forever without ever converging. 4× keeps the
/// absolute ceiling generous (a full deliberate build) yet finite.
pub(crate) const RUN_BUDGET_ABSOLUTE_MULT: u32 = 4;

/// The ABSOLUTE wall-clock ceiling for ONE build loop: [`run_budget`] ×
/// [`RUN_BUDGET_ABSOLUTE_MULT`]. Unlike the sliding idle window this NEVER moves —
/// it is the hard cap a continuously-emitting base can never run past. Saturating so
/// an enormous `UMADEV_RUN_BUDGET_SECS` can never overflow the multiply.
pub(crate) fn run_budget_absolute() -> Duration {
    run_budget().saturating_mul(RUN_BUDGET_ABSOLUTE_MULT)
}

/// Whether the run budget is EXHAUSTED at `now`, given the ABSOLUTE hard cap
/// `hard_cap`, the SLIDING idle window `idle`, and the instant of the last genuine
/// base productivity `last_progress`. Exhausted iff EITHER the absolute ceiling is
/// reached OR the base has been unproductive for a full idle window. Pure + total:
/// the sliding term saturates so a huge `idle` can never overflow the add. This is
/// the ONE predicate every build-turn/step budget check keys off, so a
/// slow-but-progressing build (each productive event resets `last_progress`) is
/// bounded only by `hard_cap`, while a stalled one still stops after `idle`.
#[must_use]
pub(crate) fn budget_exhausted(
    now: std::time::Instant,
    hard_cap: std::time::Instant,
    last_progress: std::time::Instant,
    idle: Duration,
) -> bool {
    now >= hard_cap || now.saturating_duration_since(last_progress) >= idle
}

/// The EFFECTIVE deadline instant for the next idle wait: the sliding idle window
/// (`last_progress + idle`) clamped to never exceed the ABSOLUTE `hard_cap`. Threaded
/// as the run-budget `deadline` into [`next_event_idle`] so a live-but-silent tool is
/// allowed a FULL idle window from when it last produced (not from run start), while
/// the absolute cap remains the hard ceiling. Total: an `idle` add overflow saturates
/// to `hard_cap` (via the clamp), never panics.
#[must_use]
pub(crate) fn sliding_deadline(
    hard_cap: std::time::Instant,
    last_progress: std::time::Instant,
    idle: Duration,
) -> std::time::Instant {
    last_progress
        .checked_add(idle)
        .map_or(hard_cap, |dl| dl.min(hard_cap))
}

/// H6 — the run-budget deadline to hand [`next_event_idle`] for the NEXT idle wait.
///
/// While a tool is in flight (`in_tool_call`) the bound MUST be the ABSOLUTE cap
/// `deadline`, NOT the sliding `eff` window: a live-but-silent tool (a long
/// build / test / install / dev server that legitimately emits nothing for minutes)
/// is liveness-checked by [`next_event_idle`] and must survive to the absolute cap,
/// exactly as that watchdog's docstring promises. Passing the sliding `eff` there
/// would settle `IdleTimedOut` after ONE idle window and guillotine a healthy tool.
/// With NO tool in flight the bound is the sliding `eff` window, so a genuinely hung
/// (silent, non-tool) base is still caught after one idle window. Pure.
#[must_use]
pub(crate) fn idle_wait_deadline(
    in_tool_call: bool,
    deadline: std::time::Instant,
    eff: std::time::Instant,
) -> std::time::Instant {
    if in_tool_call {
        deadline
    } else {
        eff
    }
}

/// Whether `ev` is GENUINE base productivity — a real content-bearing frame (text,
/// reasoning, a tool call / result / output) as opposed to a pure control frame
/// (session/model/state/queue housekeeping). Only a productive event slides the run
/// budget's idle window (resets `last_progress`), so a base that merely emits control
/// noise cannot hold the budget open forever, while one that is actually writing code
/// / running tools keeps the window fresh. Pure.
#[must_use]
pub(crate) fn is_productive_event(ev: &SessionEvent) -> bool {
    matches!(
        ev,
        SessionEvent::TextDelta(_)
            | SessionEvent::ThinkingDelta(_)
            | SessionEvent::ToolCall { .. }
            | SessionEvent::ToolCallCorrelated { .. }
            | SessionEvent::ToolProgressCorrelated { .. }
            | SessionEvent::ToolOutputDelta(_)
            | SessionEvent::ToolOutputDeltaCorrelated { .. }
            | SessionEvent::ToolOutputSnapshot(_)
            | SessionEvent::ToolOutputSnapshotCorrelated { .. }
            | SessionEvent::ToolResult { .. }
            | SessionEvent::ToolResultCorrelated { .. }
    )
}

/// A short, fixed ceiling on the best-effort `interrupt()` issued when the idle
/// watchdog fires. A base that has wedged its event stream can also wedge the
/// interrupt path (the same dead pipe), so the interrupt is ITSELF bounded —
/// otherwise the watchdog would just move the permanent hang from `next_event`
/// to `interrupt`. 5s is ample for a live child to acknowledge a signal; a dead
/// one simply times out and the pump settles regardless. `pub(crate)` so the
/// shared rework pump (`continuous::drive_rework_turn_with_idle`) reuses the SAME
/// bounded-interrupt grace on its mid-turn budget settle (no second constant).
pub(crate) const INTERRUPT_TIMEOUT_SECS: u64 = 5;

/// The hard ceiling on bounded, VISIBLE retries of a TRANSIENT base failure (a 429
/// rate limit, an overloaded base, a network blip — [`crate::base_error::is_transient`])
/// within ONE turn before it fails honestly. Small + decisive, mirroring the
/// bounded-rework philosophy: a transient hiccup earns a few backoff-and-retry
/// attempts, but a base that is genuinely down still fails promptly rather than
/// grinding. A HARD failure (auth / context / a non-zero exit) is NOT retried at all.
pub(crate) const MAX_TRANSIENT_RETRIES: u32 = 3;

/// The base unit of the exponential transient backoff (attempt 1 → 1×, 2 → 2×, 3 →
/// 4× this, capped at [`TRANSIENT_BACKOFF_CAP`]). 2s keeps a single retry quick yet
/// gives a rate limit room to clear before the next attempt.
pub(crate) const TRANSIENT_BACKOFF_BASE: Duration = Duration::from_secs(2);

/// The cap on any single transient backoff wait, so the schedule stays bounded even
/// if [`MAX_TRANSIENT_RETRIES`] grows. 30s is the longest a transient retry ever waits.
pub(crate) const TRANSIENT_BACKOFF_CAP: Duration = Duration::from_secs(30);

/// The backoff wait before transient-retry `attempt` (1-based): exponential off `base`,
/// capped at `cap` — attempt 1 → `base`, 2 → 2×`base`, 3 → 4×`base`, … never exceeding
/// `cap`. Pure + total + bounded: the shift is clamped (so it can never overflow) and a
/// multiply overflow saturates at `cap`, so the schedule is deterministic and can never
/// balloon. `base`/`cap` are parameters so the test drives a tiny, fast window.
pub(crate) fn transient_backoff_wait(base: Duration, cap: Duration, attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(16);
    let mult = 1u32 << shift; // shift ≤ 16 ⇒ ≤ 65 536, never overflows a u32
    base.checked_mul(mult).map_or(cap, |d| d.min(cap))
}

/// The result of ONE idle-guarded `next_event()` wait — the shared primitive
/// every main-session event pump uses so the "stops emitting but never exits"
/// hang can NEVER wedge a pump (the P0-3 / P1-11 zero-stall fix).
#[derive(Debug)]
pub(crate) enum IdleEvent {
    /// The base emitted an event (the idle timer is reset by the caller looping).
    Event(SessionEvent),
    /// `next_event()` returned `None` — the session ended (process dead / EOF).
    /// The pump treats this as a failed turn (fail-open, no panic). Carries the
    /// base's OWN diagnosis captured at the settle (`try_exit_status` /
    /// `stderr_tail`) so the caller can fold it into the user-visible reason via
    /// [`enrich_idle_reason`] — the same WHY the chat path surfaces.
    SessionEnded {
        /// The base child's exit status if it had already exited (else `None`).
        exit: Option<std::process::ExitStatus>,
        /// A bounded tail of the base's stderr (else `None`) — the real cause a
        /// broken base writes to stderr, never stdout.
        stderr_tail: Option<String>,
    },
    /// The watchdog settled the turn without a real event — either a NON-tool hang
    /// (no event for the base window with no tool in flight → genuinely hung, so the
    /// watchdog issued a best-effort, bounded `interrupt()` before settling), OR a base
    /// that was still mid-tool when the overall run-budget `deadline` was reached (the
    /// liveness backstop: a live base running a tool keeps waiting only until the run
    /// budget is exhausted, then settles WITHOUT an interrupt — the run finalization /
    /// `session.end()` releases it). The pump settles the turn as a failure so
    /// `thinking` clears rather than blocking forever. Carries the base's diagnosis
    /// captured at the settle (for the hang case, AFTER the bounded interrupt — an
    /// interrupt may have made a hung child exit, surfacing its status/stderr).
    IdleTimedOut {
        /// The base child's exit status if it had already exited (else `None`).
        exit: Option<std::process::ExitStatus>,
        /// A bounded tail of the base's stderr (else `None`).
        stderr_tail: Option<String>,
    },
}

/// Wait for the next session event under the idle watchdog — the ONE place the
/// "base hung holding the pipe open" failure is converted into a settle instead
/// of a permanent block. Used by EVERY main-session event pump (this loop's
/// `drive_one_turn`, and [`crate::continuous`]'s `drive_phase` /
/// `drive_rework_turn`) so the protection can't be "fixed in A, forgotten in B".
///
/// The watchdog is LIVENESS-based while a tool runs, NOT a fixed kill timeout —
/// legitimate tasks (big builds, long test suites, dev servers, data jobs) can run for
/// hours, so a fixed cap is the wrong model. The window is picked from `budget` by
/// `in_tool_call`:
///
/// - **A tool is in flight** (`in_tool_call == true`): the `budget.tool` window is a
///   liveness-POLL interval, not a deadline. Each time it elapses with no event the
///   watchdog re-checks the base process: a DEAD base (`try_exit_status` is `Some`)
///   settles as [`IdleEvent::SessionEnded`] within one poll window; a LIVE base means
///   the tool is genuinely running, so it keeps waiting — indefinitely, bounded ONLY by
///   the optional run-budget `deadline` (when that is reached it settles as
///   [`IdleEvent::IdleTimedOut`]). A tool of ANY duration with a live base survives.
/// - **No tool in flight** (`in_tool_call == false`): the `budget.base` window IS the
///   hang deadline — pure silence past it means the base is genuinely hung, so the
///   watchdog issues a best-effort `interrupt()` (itself bounded by
///   [`INTERRUPT_TIMEOUT_SECS`] so a wedged interrupt path can't re-introduce the hang)
///   and settles as [`IdleEvent::IdleTimedOut`]. The non-tool case is NEVER unbounded.
///
/// ANY real event returns immediately ([`IdleEvent::Event`]); the caller loops, calling
/// this again, so the next wait re-reads the window for the (possibly changed)
/// in-tool-call state. `deadline` is the run's wall-clock ceiling (`Some` on the /run
/// pumps, `None` on the interactive chat path where the user controls via Esc and a
/// dead base still settles via [`IdleEvent::SessionEnded`]). Fail-open by contract: a
/// bad/dead session always resolves to a settle, never a wedge.
pub(crate) async fn next_event_idle(
    session: &mut dyn BaseSession,
    budget: IdleBudget,
    in_tool_call: bool,
    deadline: Option<std::time::Instant>,
) -> IdleEvent {
    let window = budget.window(in_tool_call);
    loop {
        match tokio::time::timeout(window, session.next_event()).await {
            Ok(Some(ev)) => return IdleEvent::Event(ev),
            Ok(None) => {
                // The session ended — capture the base's OWN diagnosis NOW (we hold
                // `&mut session` right here) so the caller can tell the user WHY
                // instead of a bare "ended mid-turn". Fail-open: both default to
                // `None`, never block (the run path mirrors the chat path's enrich).
                return IdleEvent::SessionEnded {
                    exit: session.try_exit_status(),
                    stderr_tail: session.stderr_tail(),
                };
            }
            Err(_) => {
                // The window elapsed with no event.
                if in_tool_call {
                    // A tool is plausibly mid-flight: the window is a liveness POLL, not
                    // a kill deadline. Re-check whether the base is still alive instead
                    // of killing a busy build. The base process exited under the tool
                    // (the tool's child, or the base itself, died, surfacing a status) →
                    // settle as SessionEnded promptly (within one poll window), carrying
                    // the base's own diagnosis.
                    if let Some(status) = session.try_exit_status() {
                        return IdleEvent::SessionEnded {
                            exit: Some(status),
                            stderr_tail: session.stderr_tail(),
                        };
                    }
                    // Base still ALIVE → the tool is genuinely running (a build /
                    // compile / install / long test / dev server / data job that is
                    // legitimately silent). Keep waiting, bounded ONLY by the overall run
                    // budget: if the run deadline is exhausted, settle as IdleTimedOut
                    // (the run finalization / `session.end()` releases the still-live
                    // base — no interrupt issued here, the caller's graceful budget path
                    // owns that). Otherwise poll again.
                    if let Some(dl) = deadline {
                        if std::time::Instant::now() >= dl {
                            return IdleEvent::IdleTimedOut {
                                exit: session.try_exit_status(),
                                stderr_tail: session.stderr_tail(),
                            };
                        }
                    }
                    continue;
                }
                // NOT in a tool → pure silence past the base window means the base is
                // genuinely hung. Best-effort interrupt to release the child, bounded
                // so a dead pipe can't wedge it (the watchdog must always make
                // progress), then settle. Capture AFTER the interrupt — a hung child
                // the interrupt just killed now has an exit status / final stderr line.
                let _ = tokio::time::timeout(
                    Duration::from_secs(INTERRUPT_TIMEOUT_SECS),
                    session.interrupt(),
                )
                .await;
                return IdleEvent::IdleTimedOut {
                    exit: session.try_exit_status(),
                    stderr_tail: session.stderr_tail(),
                };
            }
        }
    }
}

/// The user-facing reason for an idle-watchdog [`IdleEvent::IdleTimedOut`] settle —
/// shared so every pump reports it identically. Trilingual (`base.fail.idle`). With the
/// liveness model an IdleTimedOut means one of two things, and the message covers both:
/// the base went silent with NO tool running (it looks genuinely hung — raise
/// `UMADEV_IDLE_TIMEOUT_SECS` for that non-tool idle window, or retry / switch base), OR
/// the overall run budget was reached while a tool was still running. It explicitly does
/// NOT frame this as a login/config problem (the auth/network classification is folded
/// in by [`enrich_idle_reason`] only when the base's own stderr actually indicates one).
/// `idle` is the base idle window (the knob the user would raise), so the reported
/// seconds match `UMADEV_IDLE_TIMEOUT_SECS`. Note a base that genuinely DIED mid-tool
/// settles as [`IdleEvent::SessionEnded`] with its own "ended mid-turn" reason, NOT this
/// one. The stable, locale-independent marker `UMADEV_IDLE_TIMEOUT_SECS` is present in
/// every language (tests key off it).
pub(crate) fn idle_reason(idle: Duration) -> String {
    umadev_i18n::tlf("base.fail.idle", &[&idle.as_secs().to_string()])
}

/// Fold the base's OWN diagnosis (exit status + stderr tail, captured at the idle /
/// ended settle by [`next_event_idle`]) into the user-visible reason — so the RUN
/// path tells the user WHY a base went idle / ended, exactly as the chat path's
/// `enrich_base_failure` does (a broken model id / "not logged in" / a config error
/// the base prints to STDERR, never stdout). Without this the run path settled with
/// a bare "base went idle — …" and no cause — the original symptom on the path that
/// matters most for a hung build.
///
/// Fail-open + bounded, mirroring the chat path: it first [`classify`]es the
/// base's own captured evidence (exit + stderr tail) and
/// PREPENDS the per-base [`actionable_message`] (D1: turn "base session idle" into
/// "底座未登录 — 运行 claude auth login …"); then a non-success exit appends
/// `(base 进程已退出: <status>)` and a present stderr tail appends
/// ` — base stderr: …` using its last 3 non-empty lines, ≤280 chars, so power
/// users still see the verbatim base error as the technical detail. A failure
/// that classifies as [`BaseFailure::Unknown`] prepends nothing → today's bare
/// `base_reason` behaviour. Never panics, never blocks.
///
/// [`classify`]: crate::base_error::classify
/// [`actionable_message`]: crate::base_error::actionable_message
/// [`BaseFailure::Unknown`]: crate::base_error::BaseFailure::Unknown
pub(crate) fn enrich_idle_reason(
    base_reason: &str,
    exit: Option<std::process::ExitStatus>,
    stderr_tail: Option<String>,
    backend: &str,
) -> String {
    // Classify FIRST on the captured evidence — the BASE's own exit + stderr (the
    // `base_reason` is UmaDev's OWN synthetic label, NOT base output, so it is
    // never fed to the classifier). Pass the exit string only when it is a real
    // non-success exit.
    let exit_str = exit.filter(|s| !s.success()).map(|s| s.to_string());
    let failure = crate::base_error::classify(exit_str.as_deref(), stderr_tail.as_deref(), None);

    let mut msg = match exit {
        Some(s) if !s.success() => format!("{base_reason}(base 进程已退出: {s})"),
        _ => base_reason.to_string(),
    };
    if let Some(tail) = stderr_tail {
        // Strip ANSI color/control sequences first — a base writes COLORED errors
        // to stderr, so the raw tail carries `\x1b[…m` runs that would surface as
        // garble inside the failure message.
        let tail = crate::base_error::strip_ansi(&tail);
        let lines: Vec<&str> = tail
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        let start = lines.len().saturating_sub(3);
        let snippet: String = lines[start..].join(" | ").chars().take(280).collect();
        if !snippet.is_empty() {
            msg = format!("{msg} — base stderr: {snippet}");
        }
    }

    // PREPEND the actionable diagnosis (empty for Unknown → unchanged behaviour).
    let prefix = crate::base_error::actionable_message(&failure, backend);
    if prefix.is_empty() {
        msg
    } else {
        format!("{prefix} — {msg}")
    }
}

/// How the director loop settled. Mirrors the caller's existing director outcome but
/// lives in the agent crate so both the CLI and the TUI share ONE loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirectorLoopOutcome {
    /// The caller selected [`crate::TrustMode::Plan`], so the requested build was
    /// deliberately **not executed**. This is neither a successful build nor a
    /// failure: callers must render the read-only notice without showing build
    /// completion UI or applying source-present checks.
    Planned {
        /// Localized explanation of the read-only boundary and how to execute.
        reply: String,
    },
    /// The build finished — the base built end to end and UmaDev's auto-QC passed.
    /// Bounded work that still has blocking evidence is [`Self::Failed`], never
    /// disguised as a successful settle for the caller to celebrate.
    Done {
        /// The base's final assistant text — the caller reads it for a "claimed a
        /// build" check against the real source files.
        reply: String,
    },
    /// The session died, a turn failed, or bounded verification settled with
    /// residual blocking evidence — an honest hard stop, never disguised as
    /// success. Carries a machine-true reason/evidence summary.
    Failed(String),
    /// The run PAUSED at a spec-MUST human confirmation gate (`UD-FLOW-002`
    /// `docs_confirm` / `UD-FLOW-003` `preview_confirm`) awaiting the user.
    ///
    /// Produced ONLY on a HOSTED, non-auto run (the hosting UI declared it can
    /// render + resume gates — [`crate::interaction::RunInteraction::confirm_gates`]
    /// — and the trust tier is not `auto`): the plan was persisted with the
    /// remaining steps `Pending`, the open door was written to
    /// `workflow-state.json#active_gate`, and an [`EngineEvent::GateOpened`] was
    /// emitted. The caller resumes via [`drive_director_loop_resume`] once the
    /// user approves (the already-`Done` doc steps are never re-driven). Headless
    /// runs never produce this — they keep today's drive-through behaviour.
    PausedAtGate {
        /// The gate now awaiting the user's confirmation.
        gate: crate::gates::Gate,
    },
    /// The run's wall-clock budget was exhausted mid-build WHILE resumable work
    /// remained — a first-class RESUMABLE PAUSE, never a hard failure. The plan was
    /// checkpointed (the completed steps `Done`, the rest still `Pending`) exactly as
    /// [`Self::PausedAtGate`], so `/continue` ([`drive_director_loop_resume`]) drives
    /// ONLY the remaining steps (readiness scheduling never re-runs a `Done` step).
    ///
    /// This is the alternative to guillotining a legitimately-progressing build into
    /// an unrecoverable ABORT: a run that stopped only because the clock ran out keeps
    /// its in-progress plan and points the user at the resume door, rather than forcing
    /// them to re-enter the whole requirement. Produced ONLY when ≥1 `Pending`/`Active`
    /// step remains (a fully-terminal plan settles [`Self::Done`]/[`Self::Failed`]
    /// instead — a pause with nothing left would strand the run). Unlike a gate pause
    /// this is NOT gated on a hosting UI: the plan is on disk and `/continue` resumes it
    /// from any surface (TUI or headless CLI).
    PausedAtBudget {
        /// Completed steps at the pause (`plan.progress().0`).
        done: usize,
        /// Total steps in the plan (`plan.progress().1`).
        total: usize,
    },
}

/// Persist this run's derived governance context to `.umadev/governance-context.json`,
/// at the point the requirement is known and BEFORE any code is written.
///
/// **One rule book for the run, the write hook, and the commit gate.** The context records
/// what the run has established and the user has already decided — is this a proven static
/// frontend (so the server/security-surface rules have nothing to guard), and did the
/// requirement ask for a purple/violet brand (the ONE stand-down of the banned-hue
/// default-reject). Two of the three surfaces that need it are OTHER PROCESSES with no
/// access to this run's memory: `umadev hook pre-write` (spawned per base tool call) and
/// `umadev ci` (spawned from `.git/hooks/pre-commit`, and the one that actually fails the
/// commit).
///
/// The legacy gated walk (`continuous::run_block`) and the single-shot `AgentRunner` both
/// wrote it. The DEFAULT path — this one — did not. So a user who asked for a purple brand
/// landing page watched the run honour it, and then could not commit it: `ci` read no
/// context, judged with `ProjectContext::unknown()` (purple forbidden), blocked UD-CODE-002
/// on the very color they had specified, and exited 1 with nothing they could edit to
/// converge. The same hole stood the `static_frontend_only` leniency down for the whole
/// director path.
///
/// Called from the two public doors into this engine ([`drive_director_loop_routed`] and
/// [`drive_director_loop_resume`]) — every code-writing path passes through one of them.
///
/// ## The colour permission is asked ONCE, here, and only here
///
/// The ONE stand-down of the banned-hue default-reject is "the user authorized this hue",
/// and that is an INTENT question — the same class as "is this turn chat, an edit, or a
/// build", which UmaDev answers by asking the brain, never with a keyword table. So this is
/// where the brain is asked ([`crate::color_permission::consult_color_permission`]): one
/// short structured consult on a read-only fork, at the door, before the first file is
/// written, and the verdict is PERSISTED. The three readers that come after — the PreToolUse
/// hook, `umadev ci`, the design floor — are other processes with no brain, and they read the
/// stored decision rather than re-deriving one. A model call per write is not an option.
///
/// STRICT on failure: no brain, no fork, a garbled reply → permission withheld, rule armed
/// (see the `color_permission` module docs for why this is not a fail-open violation — it
/// never blocks the host, it only declines to disarm a check).
///
/// Fail-open: a write failure is swallowed, readers then default to strict. `pub` so the resident chat path can persist the SAME floor before its first write (Stage A / A1).
pub async fn persist_run_governance_context(session: &mut dyn BaseSession, options: &RunOptions) {
    let permission =
        crate::color_permission::consult_color_permission(session, &options.requirement).await;
    let _ = crate::planner::persist_project_context_with_color(
        &options.requirement,
        &options.project_root,
        &options.effective_slug(),
        permission.purple_allowed,
    );
}

/// Drive an explicit `/run` (full product build) through the **director build loop**
/// — the USB-model engine. ONE live [`BaseSession`] is the director's brain; the
/// firmware (team identity + craft + knowledge) is already injected by the caller
/// into `first_directive` (and, on the TUI path, the system prompt). The base builds
/// the goal end to end with its OWN internal agentic loop; then UmaDev runs an
/// objective QC pass ITSELF (through the internal auto-QC routine) and, if QC found blocking problems,
/// feeds ONE fix directive back over the same session for another pass — bounded by
/// the internal QC-round limit.
///
/// `first_directive` is the goal framing the caller already built (e.g.
/// [`crate::experts::director_build_directive`]). The caller owns the session
/// lifetime (and the run-lock) and `end()`s it after this returns.
///
/// Floor preserved (see the module docs): single-writer, governance + audit,
/// bounded typed review, objective verify, and no endpoint. Every failure mode
/// settles without hanging the host.
pub async fn drive_director_loop(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    first_directive: String,
) -> DirectorLoopOutcome {
    // Backward-compatible entry: no route, no owned plan → today's behaviour
    // unchanged (fail-open). New callers pass a route via
    // [`drive_director_loop_routed`] to get the Wave 1 visible intent + plan.
    drive_director_loop_routed(session, options, events, first_directive, None).await
}

/// Drive the director loop with a Wave 1 [`RoutePlan`] in hand — the routed entry.
///
/// When `route` is `Some`, this:
/// 1. emits [`EngineEvent::IntentDecided`] so the user SEES the decision (chat vs
///    build, depth, team, budget, one-line reason), and
/// 2. before the build loop, synthesises an owned [`Plan`] over a read-only fork
///    ([`plan_state::synthesize_plan`]), persists it to `.umadev/plan.json`, and
///    emits [`EngineEvent::PlanPosted`] — the live checklist that replaces the
///    frozen phase bar. As the build runs, [`EngineEvent::PlanStepStatus`] events
///    tick the steps.
///
/// **Everything is fail-open and additive:** `route == None`, an offline brain, a
/// fork that won't open, or an unparseable plan ALL leave the existing single-turn
/// build behaviour exactly as it was. The QC feedback loop, hard floor, idle
/// watchdog, and `MAX_QC_ROUNDS` are untouched. In Wave 1 the plan is *synthesised
/// and shown*; the existing build loop still EXECUTES (driving the plan step-by-step
/// via `summon` is Wave 2).
pub async fn drive_director_loop_routed(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    first_directive: String,
    route: Option<&RoutePlan>,
) -> DirectorLoopOutcome {
    // Plan is a mechanical no-write ceiling, not a late approval decision. Check
    // it before governance-context persistence (which consults the base and writes
    // `.umadev/governance-context.json`) or any other run setup, and return a typed
    // non-build outcome so callers cannot celebrate this as a completed build.
    if !options.mode.executes() {
        events.emit(EngineEvent::Note(
            umadev_i18n::tl("continuous.plan_mode_skip").to_string(),
        ));
        events.emit(EngineEvent::Note(
            umadev_i18n::tl("mode.plan.gate").to_string(),
        ));
        return DirectorLoopOutcome::Planned {
            reply: umadev_i18n::tl("continuous.plan_mode_skip").to_string(),
        };
    }

    // 0. ONE RULE BOOK. Persist the derived governance context BEFORE anything is written.
    //    See `persist_run_governance_context` — the default `/run` path drives the base
    //    from HERE, and it used to be the one path that never wrote the context: the run
    //    honoured "a purple brand landing page" in-process, then the user's `git commit`
    //    ran `umadev ci`, which read no context, judged with `ProjectContext::unknown()`,
    //    and BLOCKED the exact color they had asked for — unconvergeable, because the gate
    //    and the run were reading different rule books.
    persist_run_governance_context(session, options).await;

    // 1. Surface the routing decision so the user sees the intent (fail-open: no
    //    route → no card, current behaviour).
    if let Some(r) = route {
        events.emit(EngineEvent::intent_decided(r));
        // ADVISORY self-evolution consult (fail-open): if THIS route class has a
        // measured, trustworthy-LOW first-pass acceptance rate (the cheap path has
        // historically been unreliable here), surface a nudge toward more consult /
        // lower autonomy. Pure advisory — it changes nothing about the deterministic
        // floor, the gates, or loop termination; it only informs the user + the
        // default. No signal (too few samples / a healthy rate / a fresh project) →
        // nothing is emitted and behaviour is byte-for-byte unchanged.
        if let Some(nudge) = crate::first_pass::low_confidence_nudge(
            &options.project_root,
            &crate::first_pass::class_kind(r.class.as_str()),
        ) {
            events.emit(EngineEvent::Note(nudge));
        }
        // ADVISORY sizing-calibration consult (fail-open): if THIS route class has a
        // measured, systematic SIZING miss (it historically under- or over-sizes the
        // turn), surface a nudge toward a heavier / lighter DEFAULT. Pure advisory — it
        // changes NOTHING about this run's already-decided route, plan, deterministic
        // floor, gates, or termination; it only informs the user + a future default. No
        // signal (too few runs / no systematic miss / a fresh project) → nothing emitted
        // and behaviour is byte-for-byte unchanged.
        if let Some(nudge) =
            crate::sizing_calibration::advisory_nudge(&options.project_root, r.class.as_str())
        {
            events.emit(EngineEvent::Note(nudge));
        }
    }

    // Read the idle watchdog window + the wall-clock build budget ONCE at the
    // boundary (not per-wait), so a mid-run env flip can't race the in-flight turns.
    // The deadline is a GRACEFUL ceiling — the loop winds down (final gate on what's
    // built) rather than aborting mid-write.
    //
    // LOW #5: compute the `deadline` BEFORE plan synthesis so the WHOLE deliberate
    // build — including the planning turn — shares ONE clock. Plan synthesis used to
    // run before the deadline existed and bounded its own drain by a fixed 180s
    // per-event timeout, so planning time was unattributed to the run budget (a slow
    // plan could eat minutes the build then didn't account for). The drain is now
    // bounded by this same deadline.
    //
    // The `deadline` is the ABSOLUTE hard ceiling ([`run_budget_absolute`]); the
    // SLIDING idle window ([`run_budget`]) is applied INSIDE each turn/step pump
    // (see [`sliding_deadline`] / [`budget_exhausted`]) so a slow-but-progressing
    // build is not cut mid-progress — only a stalled one (a full idle window with no
    // base productivity) or one that runs past the absolute cap winds down.
    let deadline = std::time::Instant::now() + run_budget_absolute();

    // 2. Synthesise + persist + post the owned plan. Fail-open at every step: a
    //    `None` plan (offline / no fork / unparseable) simply means no checklist —
    //    the build loop below runs exactly as it does today. Plan synthesis runs on
    //    a READ-ONLY fork (single-writer preserved); it never writes the workspace.
    let posted = match route {
        Some(r) => synthesize_and_post_plan(session, options, events, r, deadline).await,
        None => PostedPlan {
            plan: None,
            recipe_receipt: None,
        },
    };

    let outcome = drive_director_loop_with_idle(
        session,
        options,
        events,
        first_directive,
        posted.plan,
        route,
        IdleBudget::from_env(),
        deadline,
    )
    .await;
    settle_recipe_for_outcome(posted.recipe_receipt.as_ref(), &outcome);
    outcome
}

// ───────────────────────────────────────────────────────────────────────────
// Cross-session RESUME — re-attach to a persisted director-loop run.
//
// A `/run` persists its owned plan (`.umadev/plan.json`, each step's status) +
// the 9-phase workflow state "for resume". When the user closes the TUI mid-build
// and reopens it, there is NO in-memory gate and NO live base subprocess — the old
// session is gone. But the plan + the on-disk artifacts ARE the continuity: a FRESH
// session can re-attach to the persisted plan, skip the already-`Done` steps, and
// drive only what's left. These helpers + [`drive_director_loop_resume`] make that
// reattachment possible. Every path is fail-open: a corrupt / absent / fully-done
// plan simply yields "nothing to resume" and the caller falls back to a fresh run.
// ───────────────────────────────────────────────────────────────────────────

/// **Cross-session resume** — re-attach to a persisted director-loop run on a FRESH
/// base session instead of synthesising a new plan.
///
/// Loads `.umadev/plan.json`; when a RESUMABLE plan exists (≥1 incomplete step) it
/// re-emits [`EngineEvent::IntentDecided`] + [`EngineEvent::PlanPosted`] so the TUI
/// re-renders the checklist with the already-`Done` steps checked, then drives ONLY
/// the remaining steps via the internal plan-step driver — which walks [`Plan::ready_steps`],
/// so a `Done` step is never ready and is never re-run. The base session is fresh
/// (the old subprocess is gone): the persisted plan + the on-disk artifacts ARE the
/// continuity, exactly as a `/run` opens a new session.
///
/// Returns `Some(outcome)` when a resume actually ran, or `None` when there was
/// nothing resumable (absent / corrupt / fully-terminal plan) OR the first remaining
/// step could not drive on the fresh session — in BOTH cases the caller falls back to
/// a fresh [`drive_director_loop_routed`], so a resume never loses the build.
/// Fail-open by contract: never panics, never wedges.
pub async fn drive_director_loop_resume(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    route: &RoutePlan,
) -> Option<DirectorLoopOutcome> {
    // A resume is still an executing build. Enforce Plan before refreshing the
    // persisted governance context or touching plan/workflow state, and keep the
    // terminal meaning distinct from a genuinely completed run.
    if !options.mode.executes() {
        events.emit(EngineEvent::Note(
            umadev_i18n::tl("continuous.plan_mode_skip").to_string(),
        ));
        events.emit(EngineEvent::Note(
            umadev_i18n::tl("mode.plan.gate").to_string(),
        ));
        return Some(DirectorLoopOutcome::Planned {
            reply: umadev_i18n::tl("continuous.plan_mode_skip").to_string(),
        });
    }

    // ONE RULE BOOK, on the resume door too (see `persist_run_governance_context`): a
    // `/continue` drives the remaining steps and writes real code, so it must leave the
    // same context on disk that the PreToolUse hook and `umadev ci` will judge by. A
    // resumed run is still a run.
    persist_run_governance_context(session, options).await;

    let mut plan = load_resumable_plan(&options.project_root)?;

    // The resume CLOSES any door the previous pause left open: re-sync the phase
    // state (its writes always clear `active_gate`), so `/status` stops reporting
    // "paused at gate" the moment the run is driving again. Fail-open.
    sync_phase_from_plan(&plan, options);

    // Surface the routing decision (the same visible intent card a fresh run shows).
    events.emit(EngineEvent::intent_decided(route));
    // Re-render the checklist so the user SEES the already-Done steps checked and the
    // remaining ones pending — the visible proof the run resumed, not restarted.
    events.emit(EngineEvent::plan_posted(&plan));
    let (done, total) = plan.progress();
    events.emit(EngineEvent::Note(umadev_i18n::tlf(
        "continue.resuming_plan",
        &[&done.to_string(), &total.to_string()],
    )));

    // One shared clock for the resumed build (same as a fresh routed run): the
    // ABSOLUTE hard ceiling, with the sliding idle window applied inside the pumps.
    let deadline = std::time::Instant::now() + run_budget_absolute();
    let idle = IdleBudget::from_env();
    // Drive ONLY the remaining steps. `drive_plan_steps` schedules by readiness, so
    // the already-Done steps are skipped and only the Pending ones drive; it persists
    // the plan + finalizes exactly as a fresh deliberate build does. A first-step
    // drive failure returns `None` (the caller fails open to a fresh run).
    let receipt = crate::recipes::project_recipes_dir(&options.project_root).and_then(|dir| {
        crate::recipes::active_recipe_receipt_for_plan(&dir, &plan).map(|receipt| (dir, receipt))
    });
    let outcome =
        drive_plan_steps(session, options, events, route, &mut plan, idle, deadline).await;
    match outcome.as_ref() {
        Some(outcome) => settle_recipe_for_outcome(receipt.as_ref(), outcome),
        // The resume could not start and the caller will create a fresh run. Its
        // prior's result is unknowable, so close it instead of leaving fake pending
        // evidence forever.
        None => {
            if let Some((dir, receipt)) = &receipt {
                let _ = crate::recipes::settle_recipe_receipt(
                    dir,
                    receipt,
                    crate::recipes::RecipeOutcome::Unknown,
                );
            }
        }
    }
    outcome
}

/// Synthesise the owned plan, persist it best-effort, and emit [`EngineEvent::PlanPosted`].
/// Returns the plan when one was produced, else `None` (the caller then runs the
/// existing single-turn build). Fully fail-open: synthesis / persistence failures
/// degrade to `None` / a skipped write, never an error.
struct PostedPlan {
    plan: Option<Plan>,
    recipe_receipt: Option<(std::path::PathBuf, crate::recipes::RecipeReceipt)>,
}

async fn synthesize_and_post_plan(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    route: &RoutePlan,
    deadline: std::time::Instant,
) -> PostedPlan {
    // A plan is warranted whenever there's a BUILD to make visible — every Build
    // route, even a lean single-page one, gets a (proportionally short) plan so the
    // user SEES the director think, not just a deliberate/deep one. A fast chat /
    // explain / quick-edit needs no DAG (and would just pay a fork round-trip for
    // nothing).
    if !(matches!(route.class, crate::router::RouteClass::Build) || route.depth.is_deliberate()) {
        return PostedPlan {
            plan: None,
            recipe_receipt: None,
        };
    }
    // Tell the user the director is PLANNING before the synthesis fork. That fork
    // collects the plan SILENTLY with up to a 180s window, so a complex requirement
    // otherwise shows a bare "正在思考" spinner for minutes with no hint of what's
    // happening (user-reported: "到这里没有进度显示了" under a multi-minute build).
    events.emit(EngineEvent::Note(
        umadev_i18n::tl("plan.synthesizing").to_string(),
    ));
    // LOW #5: bound the planning drain by the SHARED run deadline so planning is
    // attributed to the run budget (no separate fixed 180s clock).
    let synthesis =
        plan_state::synthesize_plan_traced(session, options, &options.requirement, route, deadline)
            .await;
    let Some(plan) = synthesis.plan else {
        return PostedPlan {
            plan: None,
            recipe_receipt: synthesis.recipe_receipt,
        };
    };
    // A FRESH plan synthesis == a NEW deliberate run: rotate the previous run's
    // notes (`.umadev/run-notes.md` → `.umadev/run-notes.prev.md`) so the notes
    // file stays run-scoped. A RESUME re-attaches the persisted plan (it never
    // reaches this synthesis), so its notes survive — exactly the memory it wants
    // back. Best-effort + fail-open (see `context::rotate_run_notes`).
    crate::context::rotate_run_notes(&options.project_root);
    // Persist best-effort; a failed write is ignored (fail-open — never blocks).
    let _ = plan_state::save(&plan, &options.project_root);
    // Sync the 9-phase workflow state off its initial `research` value the moment a
    // plan exists — so `/status` stops reporting "research / all pending" while the
    // build is actually planning + working. Fail-open (swallows write errors).
    sync_phase_from_plan(&plan, options);
    events.emit(EngineEvent::plan_posted(&plan));
    PostedPlan {
        plan: Some(plan),
        recipe_receipt: synthesis.recipe_receipt,
    }
}

fn settle_recipe_for_outcome(
    receipt: Option<&(std::path::PathBuf, crate::recipes::RecipeReceipt)>,
    outcome: &DirectorLoopOutcome,
) {
    let Some((dir, receipt)) = receipt else {
        return;
    };
    let outcome = match outcome {
        DirectorLoopOutcome::Done { .. } => crate::recipes::RecipeOutcome::Pass,
        DirectorLoopOutcome::Failed(_) => crate::recipes::RecipeOutcome::Fail,
        DirectorLoopOutcome::Planned { .. } => crate::recipes::RecipeOutcome::Unknown,
        // A gate OR budget pause is not terminal. The active marker carries this exact
        // receipt into `drive_director_loop_resume`, where it will settle once.
        DirectorLoopOutcome::PausedAtGate { .. } | DirectorLoopOutcome::PausedAtBudget { .. } => {
            return
        }
    };
    let _ = crate::recipes::settle_recipe_receipt(dir, receipt, outcome);
}

/// Internal escape hatch (Wave A safety valve): when `UMADEV_NO_SEAT_BUILD` is
/// truthy (`1` / `true` / `yes`, case-insensitive) force-disable seat-by-seat
/// building so even a deliberate build runs the single end-to-end turn. This is NOT a
/// user-facing flag/mode — there is no CLI surface for it and the DEFAULT is always
/// router-driven (a deliberate build builds seat-by-seat). It exists only so an
/// operator can fall back to the cheaper single-turn path in the field if the seat
/// scheduler ever misbehaves. Read once at the run boundary (not per step), fail-open:
/// an unset / unparseable value leaves the router-driven default intact.
fn seat_build_force_disabled() -> bool {
    std::env::var("UMADEV_NO_SEAT_BUILD")
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            v == "1" || v == "true" || v == "yes"
        })
        .unwrap_or(false)
}

/// Whether to drive THIS build SEAT-BY-SEAT (the team builds its own steps via
/// [`drive_plan_steps`]) versus the single end-to-end turn — the Wave A build-path
/// decision. It is AUTOMATIC from the existing [`RoutePlan`]: a deliberate build
/// ([`crate::router::Depth::is_deliberate`] — the router already sized the turn
/// `Standard` / `Deep`, i.e. a Greenfield product, a high-complexity feature, the full
/// team convened) warrants seat-by-seat building, while a lean / `Fast` / quick-edit /
/// docs route stays the cheap single turn so token cost stays proportional to the task.
/// It REUSES the router's own `depth` signal — no new classifier, no user flag. The
/// `force_single_turn` escape hatch (see [`seat_build_force_disabled`]) can only
/// DISABLE seat-driving, never force it on.
fn seat_driven_build_warranted(route: &RoutePlan, force_single_turn: bool) -> bool {
    !force_single_turn && route.depth.is_deliberate()
}

/// [`drive_director_loop`] with an explicit idle window — the env read is hoisted
/// to the public wrapper so this core is deterministic (the test drives it with a
/// tiny window, no process-env mutation / race).
///
/// **Wave A / Wave 2 — depth-tiered scheduling (the "drive the plan" change):**
///
/// - **Deliberate build (`Standard` / `Deep`) WITH an owned plan** → drive the plan
///   STEP-BY-STEP via [`director::summon`] ([`drive_plan_steps`]): each ready Build
///   step gets a focused directive on the MAIN session (single-writer), is verified
///   against its own `acceptance` on the deterministic floor, and only then ticks
///   `Done`; Review steps fork the cross-review team. This is the real "schedule a
///   team" path. Fail-open: if step-driving can't even start its first step, it
///   degrades to the single-turn loop below (so a wedged base never loses the build).
/// - **Lean / Fast build, or no plan** → the EXISTING single-turn loop (one
///   end-to-end base turn + bounded auto-QC). Unchanged — a simple page stays ONE
///   fast turn; we never pay the per-step round-trips for a lean goal (Wave 1 speed
///   invariant).
#[allow(clippy::too_many_arguments)]
async fn drive_director_loop_with_idle(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    first_directive: String,
    mut plan: Option<Plan>,
    route: Option<&RoutePlan>,
    idle: IdleBudget,
    deadline: std::time::Instant,
) -> DirectorLoopOutcome {
    // Wave A: the build-path decision is AUTOMATIC from the route (no user flag). A
    // DELIBERATE build with an owned plan is driven step-by-step — each ready step is
    // `summon`ed on its seat (single-writer doer), verified against its own acceptance
    // on the deterministic floor, and only then ticked Done, so the TEAM visibly
    // BUILDS rather than the base doing it all in one turn the team merely reviews
    // after. A lean/Fast build — or any path with no plan — keeps the single-turn fast
    // loop below, untouched (token cost proportional to the task). The escape hatch
    // (`UMADEV_NO_SEAT_BUILD`) can only force the cheaper single turn, never force
    // seat-driving on. Fail-open: `drive_plan_steps` returns `None` if it couldn't
    // drive even the first step (the caller then runs the single-turn loop, never
    // losing the build).
    if let (Some(r), Some(p)) = (route, plan.as_mut()) {
        if seat_driven_build_warranted(r, seat_build_force_disabled()) {
            if let Some(outcome) =
                drive_plan_steps(session, options, events, r, p, idle, deadline).await
            {
                return outcome;
            }
            // Step-driving could not start — fall through to the single-turn loop.
            events.emit(EngineEvent::Note(
                "team · step scheduling unavailable — falling back to a single end-to-end turn"
                    .to_string(),
            ));
        }
    }

    let mut next_directive = first_directive;
    // The last objective findings that a fix turn was meant to clear. They survive
    // a wall-clock settle so the terminal outcome can name WHY the build is not done.
    let mut residual_qc: Vec<String> = Vec::new();
    let mut budget_reached = false;
    let mut qc_blockers = crate::blocker::BlockerSetTracker::default();
    let mut last_qc_snapshot: Option<SourceTreeSnapshot> = None;
    // Change 2 (single-turn twin): did this build go through blocking-item rework? A round
    // past 0 is a fix turn, so the build was reworked → the ONE integrated final report
    // supersedes report A at the settle. A clean round-0 build keeps its reply untouched.
    let mut reworked = false;

    for round in 0..MAX_QC_ROUNDS {
        // The single-turn twin of the scheduler's halt (see `halt_if_workspace_in_past`).
        // This path had NO check at all: a workspace known to be stranded at an earlier
        // checkpoint would still be handed the build turn, and then a fix turn, and then
        // another — every one of them writing new code on top of files that are not the
        // user's. The flag is raised at process start by a heal that stood down, and every
        // fix round is another turn's worth of writes, so it is read on each round, not
        // once. A pure mutex lookup — it costs nothing on the (overwhelming) happy path.
        if let Some(halt) = halt_if_workspace_in_past(options, events) {
            return halt;
        }
        // Wall-clock ceiling (graceful): a fix round past the budget is abandoned —
        // the build so far stands on its own deterministic floor (the source-present
        // hard-gate the caller runs). Round 0 (the build itself) always runs.
        if round > 0 && std::time::Instant::now() >= deadline {
            events.emit(EngineEvent::Note(
                "team · time budget reached — stopping incomplete with the current build (raise \
                 UMADEV_RUN_BUDGET_SECS for more fix rounds)"
                    .to_string(),
            ));
            budget_reached = true;
            break;
        }
        // Plan visibility (Wave 1): mark the ready BUILD steps Active before this
        // turn drives the base over them, so the checklist shows live progress. The
        // base still executes the whole goal in one turn this wave (step-by-step
        // `summon` driving is Wave 2); here we surface the plan's motion. Fail-open:
        // no plan → nothing emitted, current behaviour.
        if round == 0 {
            mark_ready_steps(&mut plan, events, StepStatus::Active);
        } else {
            // Change 2: a round past 0 is a FIX turn — the build went through rework.
            reworked = true;
        }

        // 1. Drive ONE end-to-end base turn (build, or fix-the-QC-findings). The
        //    base runs its own agentic tool loop (PM→…→QA internally) and writes
        //    real files under the run-lock the caller holds (single-writer).
        // Change 1 (scoped to FIX rounds): defer the base's premature project-level
        // wrap-up on a fix turn — narration stays, only the "## Next steps" conclusion
        // is held back; the integrated final report then supersedes it at convergence.
        // ROUND 0 gets NO suppression: a single-turn build that settles clean keeps its
        // round-0 reply as the final word (no extra report turn), so suppressing the
        // wrap-up there would deliberately strip the ONLY final report the user gets
        // (the clean single-turn build's one turn IS its wrap-up).
        let directive = if round == 0 {
            next_directive.clone()
        } else {
            format!("{next_directive}{}", wrapup_suppression_note())
        };
        let turn = match drive_one_turn(session, options, events, directive, idle, deadline).await {
            Ok(t) => t,
            // A base-reported turn failure (an API error like a 429 rate limit)
            // carries the base's OWN error text. Run it through the actionable
            // classifier so the run's terminal failure NAMES the fix while keeping
            // the raw error — never an anonymous stop. Fail-open: an
            // unclassifiable reason surfaces verbatim (never swallowed).
            Err(reason) => {
                return DirectorLoopOutcome::Failed(crate::base_error::diagnose_turn_failure(
                    &reason,
                    &options.backend,
                ))
            }
        };
        let last_reply = turn.text.clone();

        // 2. UmaDev ALWAYS runs its own objective QC pass — hard floor + verify +
        //    optional fork review. Reply wording is narration, never acceptance
        //    evidence: a tool-only turn, a terse "OK", or a base that simply omits a
        //    change verb may still have written files, left an owned plan Active, or
        //    skipped verification. The old first-round prose shortcut returned Done
        //    before any of those facts were checked. Keeping every round on this one
        //    mechanical boundary also means an uncorroborated green claim reaches the
        //    build/test fact read (`ran_build_tool == false`) instead of bypassing it.
        //
        //    A true Chat/Explain request never enters the Director in the first place;
        //    once this Build engine owns the turn, only objective QC may settle it.
        //
        // 3. UmaDev runs its OWN objective QC pass — hard floor + verify + optional
        //    fork review. NOTHING here is the base summoning a team; it is UmaDev
        //    inspecting reality over the borrowed brain. When a route is in hand, the
        //    review team is sized from the ROUTE's seats (deliverable 3 on the
        //    single-turn path too); else the kind-derived team (the legacy entry).
        let qc = run_auto_qc(
            session,
            options,
            events,
            route,
            Some(turn.text.as_str()),
            turn.ran_build_tool,
        )
        .await;

        // 4. Clean QC → the build is genuinely done. Settle and report honestly.
        if qc.is_clean() {
            // Plan visibility (Wave 1): a clean pass means the work the plan
            // describes landed — tick its steps Done + persist the final plan.
            complete_plan(&mut plan, options, events);
            // Sync the 9-phase workflow state at a CLEAN finalize: every step ticked
            // Done above, so this is a genuine clean finish → `delivery`. With no plan
            // (single-turn fallback) a clean build is still `delivery`. Fail-open.
            finalize_phase_from_plan_opt(&plan, options, true);
            // Wave 4 (§L4 / G8): restore the shareable delivery on the DEFAULT
            // path — depth-gated, fail-open. A clean Build leaves a PRD /
            // architecture / UI-UX doc (+ a proof-pack on the deliberate path). This
            // arm is reached only inside `qc.is_clean()`, so the build is clean.
            director::finalize(options, events, route, true);
            // SIZING calibration: a clean settle on round 0 (no rework) was a LIGHT
            // actual outcome; a clean settle only AFTER bounded QC fix rounds means the
            // cheap single turn under-sized the work → HEAVY. Advisory, fail-open.
            record_run_sizing(
                options,
                route,
                if round == 0 {
                    crate::sizing_calibration::SizeRank::Light
                } else {
                    crate::sizing_calibration::SizeRank::Heavy
                },
            );
            // ACTIVE FACT-RECORDING BACKSTOP (single-turn path): this turn passed the
            // objective build QC, so it is a real completed work turn regardless of
            // whether its final prose happened to use a change verb — extract its
            // durable facts ourselves and persist
            // them to `.umadev/memory/facts.jsonl` so the store reliably populates
            // without depending on the base writing it. Once per clean single-turn
            // build (count 1 → the throttle always fires the first work turn); a
            // pure-chat/explain route is skipped inside; fully fail-open.
            crate::fact_extract::maybe_extract_facts(
                session,
                &options.project_root,
                route,
                1,
                events,
            )
            .await;
            // Change 2: on a REWORKED build, supersede the (now-stale) report A with ONE
            // integrated final report; a clean round-0 build keeps its reply (fail-open).
            return DirectorLoopOutcome::Done {
                reply: if reworked {
                    integrated_final_report(session, options, events, last_reply, deadline).await
                } else {
                    last_reply
                },
            };
        }

        residual_qc = qc.blocking.clone();
        let current_qc_snapshot = source_tree_snapshot(&options.project_root);
        let workspace_progress = last_qc_snapshot
            .as_ref()
            .is_some_and(|previous| previous != &current_qc_snapshot);
        last_qc_snapshot = Some(current_qc_snapshot);
        let assessments = qc.assess_blockers(&mut qc_blockers, workspace_progress);
        emit_blocker_assessments(events, &assessments);
        if let Some(stuck) = assessments
            .iter()
            .find(|item| item.disposition == crate::blocker::BlockerDisposition::Escalate)
        {
            residual_qc.push(format!(
                "stuck detector: blocker `{}` repeated {} times without a source-tree change",
                stuck.diagnosis.fingerprint, stuck.repeat_count
            ));
            persist_plan(&plan, options);
            finalize_phase_from_plan_opt(&plan, options, false);
            return DirectorLoopOutcome::Failed(qc_incomplete_reason(
                "auto-QC stopped an unchanged repair loop",
                &residual_qc,
            ));
        }

        // 5. QC found blocking problems. Out of fix budget → terminate honestly.
        //    The caller's source-present check is only ONE floor and cannot clear
        //    governance/build/review findings, so returning Done here would overwrite
        //    the real incomplete state with a completion card.
        if round + 1 >= MAX_QC_ROUNDS {
            events.emit(EngineEvent::Note(
                "team · auto-QC reached its fix-round budget — stopping incomplete with residual evidence"
                    .to_string(),
            ));
            // The plan steps stay where they are (Active), honestly reflecting that
            // QC didn't fully clear; the terminal Failed reason carries the findings.
            // Persist the final state for resume.
            persist_plan(&plan, options);
            // Sync the 9-phase state at a NON-clean settle: never claim `delivery` —
            // advance only to the furthest phase that actually completed (no plan / no
            // Done step → keep the in-progress anchor). Fail-open.
            finalize_phase_from_plan_opt(&plan, options, false);
            // SIZING calibration: the cheap path burned its whole QC fix budget without
            // clearing → the work was HEAVIER than the single-turn sizing assumed.
            record_run_sizing(options, route, crate::sizing_calibration::SizeRank::Heavy);
            return DirectorLoopOutcome::Failed(qc_incomplete_reason(
                "auto-QC fix-round budget exhausted before the blocking findings cleared",
                &residual_qc,
            ));
        }

        // 6. Fold the QC findings into ONE fix directive and feed it back over the
        //    USB channel for another build pass → re-QC.
        next_directive = qc.fix_directive_with_assessments("", &assessments);
    }

    // Loop fell through (exhausted the bounded rounds). H7: on a BUDGET break the
    // single-turn loop already drove the whole goal in one turn but left its steps
    // Active/Pending (it never ticks Done outside the clean branch). Reconcile Done
    // from the workspace BEFORE persisting, so the checkpoint the resume reloads
    // reports honest done/total and `drive_director_loop_resume` skips the steps whose
    // outputs the mega-turn already wrote (redoing only what's genuinely missing). A
    // NON-budget fall-through (a real QC failure) keeps its statuses — reconciling
    // there would flatter an unfinished build, so it is gated on `budget_reached`.
    if budget_reached {
        if let Some(p) = plan.as_mut() {
            reconcile_single_turn_plan_from_workspace(p, &options.project_root);
        }
    }
    // Persist the plan's final state for resume; reality is the caller's hard-gate.
    persist_plan(&plan, options);
    // Non-clean settle (the bounded rounds didn't fully clear): honest phase only.
    finalize_phase_from_plan_opt(&plan, options, false);
    // SIZING calibration: exhausting the bounded rounds means the work outran the
    // single-turn sizing → HEAVY actual outcome. Advisory, fail-open.
    record_run_sizing(options, route, crate::sizing_calibration::SizeRank::Heavy);
    // RESUMABLE BUDGET PAUSE (Stage 2, single-turn twin): if the ONLY reason this
    // loop fell through was the wall-clock budget AND the persisted plan still has
    // resumable steps, offer `/continue` (`PausedAtBudget`) instead of a hard
    // `Failed` — the plan was just persisted above, so a resume re-drives only what's
    // left. A run with no plan, or one with nothing `Pending`/`Active`, is not
    // resumable and settles Failed as before. `budget_reached` is set only by the
    // wall-clock round check, never by a QC/verification failure.
    if budget_reached {
        if let Some(p) = plan.as_ref() {
            if p.steps
                .iter()
                .any(|s| matches!(s.status, StepStatus::Pending | StepStatus::Active))
            {
                let (done, total) = p.progress();
                return DirectorLoopOutcome::PausedAtBudget { done, total };
            }
        }
    }
    let reason = if budget_reached {
        "run time budget exhausted before auto-QC cleared"
    } else {
        "auto-QC settled without a clean verdict"
    };
    DirectorLoopOutcome::Failed(qc_incomplete_reason(reason, &residual_qc))
}

// ───────────────────────────────────────────────────────────────────────────
// Wave 2 — DRIVE the plan step-by-step (the "schedule a team" path).
//
// For a DELIBERATE build with an owned plan, the director no longer fires ONE
// mega-turn — it walks the DAG: each ready Build step is `summon`ed serially on
// the main session (single-writer) with a FOCUSED directive, verified against
// THAT step's `acceptance` on the deterministic floor, and only ticks `Done` when
// the floor passes; a failing step folds its blocking findings into a bounded fix
// loop (reusing MAX_QC_ROUNDS + a stall guard, mirroring `review_and_rework`).
// Review steps fork the cross-review team. The lean/Fast path never reaches here.
//
// INVARIANTS (identical to the single-turn loop): single-writer (only `summon`'s
// Serial main turn writes; reviews run on read-only forks), idle watchdog (each
// summon's turn pump is `drive_rework_turn`, idle-guarded), hard floor (the SAME
// content-governance scan + source-present floor run as the final QC gate), bounded
// (a per-step fix budget + an overall step ceiling), fail-open (any summon / verify
// that can't run degrades to "advance the step" — never a wedge, never a false
// failure — and a first-step failure degrades the WHOLE path to the single turn).
// ───────────────────────────────────────────────────────────────────────────

/// The hard ceiling on per-step fix rounds while driving one plan step — the same
/// small, decisive bound as [`MAX_QC_ROUNDS`], applied at the step level. A step
/// that builds correctly the first time spends ZERO fix rounds; only a step that
/// keeps failing its acceptance pays the (bounded) re-drive cost.
const MAX_STEP_FIX_ROUNDS: usize = 2;

/// A step whose **blast radius** (the count of steps that transitively depend on it,
/// [`Plan::blast_radius`]) reaches this is treated as HIGH-impact and earns ONE extra
/// bounded fix round in [`drive_build_step`] — verification RIGOR weighted by blast
/// radius. An upstream node many later steps build on is expensive to leave wrong, so
/// the director tries harder to make it actually PASS its deterministic acceptance
/// before giving up and marking it Blocked. The acceptance check itself is unchanged
/// (the same deterministic floor; no critic verdict ever drives control), and the extra
/// round is still finite + deadline-bounded — never an open grind. A leaf / low-impact
/// step keeps the base [`MAX_STEP_FIX_ROUNDS`] budget.
const HIGH_BLAST_RADIUS: usize = 2;

/// A safety ceiling on total step transitions so a pathological plan (e.g. a brain
/// that emitted a huge DAG, or a flapping readiness set) can never spin — generous
/// (real plans are 3-8 steps) but finite. Mirrors the bounded-loop discipline.
const MAX_STEP_TRANSITIONS: usize = 32;

/// Drive a DELIBERATE plan step-by-step via [`director::summon`] + per-step
/// acceptance. Returns `Some(outcome)` when the schedule ran (clean or settled at a
/// bound), or `None` when it could not drive even its FIRST step — the signal for
/// the caller to fail open to the single end-to-end turn (so a wedged base never
/// loses the whole build to a scheduling failure).
/// STOP the schedule when this run's workspace is known to be stuck at an earlier
/// checkpoint — a temporary evidence rewind that could not be undone (see
/// [`crate::checkpoint::workspace_is_in_past`]).
///
/// `Some(Failed)` ends the run immediately: no further steps, no final gate, no finalize
/// — every one of those WRITES, and writing onto a tree that is in the past compounds the
/// damage (new code layered over reverted files, a proof-pack attesting to a state that
/// never existed). The user gets the loud note on the surface they are watching, plus the
/// workspace notice the next start drains.
///
/// **The note tells the truth about which branch they are in** — see
/// [`crate::checkpoint::InPastReason`]. It used to say "restart UmaDev and it will put the
/// tree back" in BOTH branches, and in one of them that is false: a marker whose head this
/// workspace cannot name is refused by the heal on every start, forever, so the restart it
/// advises can only reproduce the halt. That branch gets the note that names the real escape.
///
/// `None` (the overwhelming default) changes nothing.
fn halt_if_workspace_in_past(
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
) -> Option<DirectorLoopOutcome> {
    // ONE definition of the note, shared with the TUI chat write path — see
    // `checkpoint::workspace_in_past_note`. A halt worded differently on two surfaces drifts.
    let note = crate::checkpoint::workspace_in_past_note(&options.project_root)?;
    events.emit(EngineEvent::Note(note.clone()));
    crate::checkpoint::record_workspace_notice(note.clone());
    Some(DirectorLoopOutcome::Failed(note))
}

async fn drive_plan_steps(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    route: &RoutePlan,
    plan: &mut Plan,
    idle: IdleBudget,
    deadline: std::time::Instant,
) -> Option<DirectorLoopOutcome> {
    let _ = idle; // each summon turn reads the idle window itself (drive_rework_turn)
                  // BEFORE THE FIRST STEP, NOT ONLY AFTER IT. The halt was read only once a step had
                  // already driven — so a process that STARTS with the flag up drove a FULL STEP's worth
                  // of writes onto a tree that is in the past before it ever looked. And it can start
                  // that way: the workspace heal raises the flag at process start whenever it stood down
                  // (it could not snapshot the present, it could not reset, or the marker names a head it
                  // cannot identify — see `checkpoint::recover_abandoned_temp_rewind`). The first swing is
                  // as damaging as the fifth.
    if let Some(halt) = halt_if_workspace_in_past(options, events) {
        return Some(halt);
    }
    // P0 EXECUTION CONTRACT — intent classification is not a write boundary. Every
    // mutating plan step must name its create/modify surface before the first doer
    // runs; otherwise the scope denominator is unknowable and the old scope floor
    // silently stood down. Planning already performs one bounded repair re-ask. If
    // it is still incomplete, fail explicitly and keep the workspace untouched —
    // never widen to the legacy end-to-end mega-turn.
    let contract =
        crate::execution_contract::ExecutionContract::from_plan(route, &options.requirement, plan);
    if let Some(violation) = contract.preflight_violations().into_iter().next() {
        for step in plan
            .steps
            .iter_mut()
            .filter(|step| step.kind == plan_state::StepKind::Build && step.files.is_empty())
        {
            step.status = StepStatus::Blocked;
            events.emit(EngineEvent::plan_step_status(
                step.id.clone(),
                step.title.clone(),
                StepStatus::Blocked,
            ));
        }
        events.emit(EngineEvent::Note(format!(
            "floor · {}: {}",
            violation.code, violation.message
        )));
        persist_plan_ref(plan, options);
        finalize_phase_from_plan(plan, options, false);
        emit_plan_completion_summary(plan, events, &[]);
        return Some(DirectorLoopOutcome::Failed(violation.message));
    }
    let mut task_tracker = match crate::plan_tasks::PlanTaskTracker::open(
        &options.project_root,
        &options.backend,
        &options.requirement,
        plan,
    ) {
        Ok(tracker) => tracker,
        Err(error) => {
            let reason = format!("agent task ledger unavailable: {error}");
            events.emit(EngineEvent::Note(format!("team · {reason}")));
            return Some(DirectorLoopOutcome::Failed(reason));
        }
    };
    events.emit(EngineEvent::Note(format!(
        "team · agent run {} · {} durable task(s)",
        task_tracker.run_id(),
        task_tracker.tasks().count()
    )));
    events.emit(EngineEvent::Note(format!(
        "team · scheduling {} step(s) over the team ({} build · {} review)",
        plan.steps.len(),
        plan.steps
            .iter()
            .filter(|s| s.kind == plan_state::StepKind::Build)
            .count(),
        plan.steps
            .iter()
            .filter(|s| s.kind == plan_state::StepKind::Review)
            .count(),
    )));

    let mut last_reply = String::new();
    // Every clean plan-driven build gets ONE integrated report: per-step doer replies
    // are wrap-up-suppressed. A non-clean build gets no success-shaped report turn.
    let mut transitions = 0usize;
    // The running count of completed BUILD steps — the "work turn" tally that the
    // active fact-extraction backstop throttles on (see `crate::fact_extract`).
    let mut work_turns = 0usize;
    // CIRCUIT BREAKER (UD-FLOW-008): trip after CONSECUTIVE_FAILURE_THRESHOLD
    // consecutive same-class step-verification failures with NO intervening progress,
    // so a build where the base keeps failing the same way STOPS with a diagnosis
    // instead of grinding to MAX_STEP_TRANSITIONS burning effort. A Done step resets it.
    let mut failure_breaker = crate::trust::ConsecutiveFailureBreaker::new();
    // Machine-true evidence accumulated when a step settles Blocked. The plan stores
    // the terminal status, but not each verifier's gap text; keep a bounded run-local
    // copy so a non-clean terminal outcome can explain the failure to the caller.
    let mut incomplete_evidence: Vec<(String, String)> = Vec::new();
    let mut budget_reached = false;
    // SLIDING run-budget clock (Stage 3): the instant of the last genuine base
    // productivity at the SCHEDULING level — reset whenever a step actually made
    // progress. The between-steps ceiling below keys off it via [`budget_exhausted`]
    // so a build that keeps completing steps is never cut between them (bounded only
    // by the ABSOLUTE cap `deadline`), while a run that stalls a full idle window
    // still winds down. `run_budget()` is the sliding idle window (`deadline` is the
    // absolute ceiling). Read once so a mid-run env flip can't race the checks.
    let idle_window = run_budget();
    let mut last_progress = std::time::Instant::now();
    // SELF-EVOLUTION: run-scoped set of recurring-pitfall signatures a reflection has
    // already been attempted for, so `drive_build_step` fires the (forked, fail-open)
    // reflection consult AT MOST ONCE per signature per run. Bounded by construction.
    let mut reflected: std::collections::HashSet<String> = std::collections::HashSet::new();
    // BOUNDED RE-PLAN guard (mirrors the `reflected` bound): a run gets AT MOST ONE
    // re-plan of a blocked subtree. When a step blocks and strands dependents, the
    // director asks the brain (read-only fork, fail-open) for a replacement sub-DAG that
    // routes around the blocker; this flag is consumed on the single attempt (whether or
    // not it helps) so re-planning can NEVER loop — after one try, honesty wins.
    let mut replanned = false;

    // Walk the DAG by readiness: drive each ready step, mark it, repeat. A step that
    // can't be accepted (after its bounded fix budget) is marked Blocked so it stops
    // gating its dependents — readiness then drains the remaining independent steps
    // rather than deadlocking. The transition ceiling is the final hard stop.
    while transitions < MAX_STEP_TRANSITIONS {
        // Snapshot the next ready step's id (ready_steps borrows the plan immutably;
        // we drop the borrow before mutating). Drive ONE step per outer iteration so
        // dependents become ready only after their prerequisite actually accepted.
        // Wall-clock ceiling (graceful): once the build budget is spent, stop
        // scheduling NEW steps and fall through to the final gate on what's already
        // built — a thorough deliberate build can never run unbounded, but we never
        // abort mid-write. The first step always runs (a budget can't be so small it
        // starves the very first doer turn). SLIDING (Stage 3): `budget_exhausted`
        // stops only when the ABSOLUTE cap `deadline` is reached OR a full idle window
        // elapsed since the last step made progress — so a build that keeps completing
        // steps is never guillotined between them. A budget stop here becomes a
        // RESUMABLE `PausedAtBudget` (not `Failed`) at the terminal return below.
        if transitions > 0
            && budget_exhausted(
                std::time::Instant::now(),
                deadline,
                last_progress,
                idle_window,
            )
        {
            events.emit(EngineEvent::Note(
                "team · time budget reached — finalizing what's complete (raise \
                 UMADEV_RUN_BUDGET_SECS for a longer run)"
                    .to_string(),
            ));
            budget_reached = true;
            break;
        }
        // Blast-radius-weighted scheduling: among the currently-ready PEERS, drive the
        // highest-blast-radius step FIRST (upstream-before-downstream when both are
        // ready). This never breaks the DAG order — `ready_steps_prioritized` only
        // reorders steps whose dependencies are ALL already Done — but it does the
        // expensive-to-unwind work (a schema / contract / scaffold many steps depend on)
        // earliest, so it is verified soonest AND, if it fails, reworked first: handling
        // the high-impact upstream step before its peers can obviate the downstream
        // rework (a Blocked upstream step strands its dependents, pruned below).
        let Some(step_id) = plan.ready_steps_prioritized().first().map(|s| s.id.clone()) else {
            break; // nothing ready (all Done / Blocked, or a satisfied DAG) → finish
        };
        transitions += 1;

        // Mark the step Active + surface it on the checklist BEFORE driving it.
        // LOW #4: resolve the step fail-open — this is a fail-open module, so a
        // ready id that no longer resolves (a concurrently-mutated plan) breaks the
        // schedule cleanly to the final gate rather than panicking the host.
        let Some(step) = plan.steps.iter().find(|s| s.id == step_id).cloned() else {
            break;
        };
        if let Err(error) = task_tracker.start_step(plan, &step) {
            let reason = format!("agent task `{}` could not start: {error}", step.id);
            plan.mark(&step_id, StepStatus::Blocked);
            events.emit(EngineEvent::plan_step_status(
                step_id,
                step.title,
                StepStatus::Blocked,
            ));
            events.emit(EngineEvent::Note(format!("team · {reason}")));
            persist_plan_ref(plan, options);
            let _ = task_tracker.finish(false, &reason, vec![reason.clone()]);
            return Some(DirectorLoopOutcome::Failed(reason));
        }
        plan.mark(&step_id, StepStatus::Active);
        events.emit(EngineEvent::plan_step_status(
            step_id.clone(),
            step.title.clone(),
            StepStatus::Active,
        ));
        persist_plan_ref(plan, options);

        // Blast radius of THIS step (downstream-dependent count) — weights its rework
        // rigor: a high-blast-radius upstream step earns one extra bounded fix round.
        let blast_radius = plan.blast_radius(&step_id);
        // Capture the title + kind for the circuit-breaker diagnosis BEFORE the step's
        // `title` is moved into the status event below.
        let step_title = step.title.clone();
        let step_kind = step.kind;
        // PLAN RECITATION (bounded): a compact "where we are in the plan" line so the
        // base stays anchored to the whole plan over a long step-by-step run.
        let plan_progress = plan_progress_recitation(plan, &step_id);
        let outcome = match step.kind {
            plan_state::StepKind::Build => {
                drive_build_step(
                    session,
                    options,
                    events,
                    route,
                    &step,
                    &plan_progress,
                    blast_radius,
                    deadline,
                    &mut reflected,
                )
                .await
            }
            plan_state::StepKind::Review => {
                drive_review_step(session, options, events, route, &step, deadline).await
            }
        };
        // A WORKSPACE IN THE PAST MUST NOT ACCUMULATE MORE WRITES. A step's evidence check
        // can rewind the tree to an earlier checkpoint to replay a test (the red half of a
        // red→green contract) and then put it back. When that restore FAILS, the user's
        // tracked source is left reverted — and every step we drive from here writes new
        // code on top of files that are not theirs, on a base reading a codebase that no
        // longer exists. Stop, honestly, right here: the plan is persisted, the notice is
        // raised, and the next start's heal puts the tree back before anything else runs.
        if let Some(halt) = halt_if_workspace_in_past(options, events) {
            let reason = "workspace recovery interrupted the active agent task";
            let blockers = vec![reason.to_string()];
            let _ = task_tracker.settle_base_agents(
                &step,
                &outcome.base_agents,
                StepStatus::Blocked,
                false,
                reason,
                &blockers,
            );
            let _ = task_tracker.settle_step(
                &step,
                StepStatus::Blocked,
                false,
                reason,
                blockers.clone(),
            );
            let _ = task_tracker.finish(false, reason, blockers);
            return Some(halt);
        }

        let StepOutcome {
            accepted,
            reply,
            drove,
            made_progress,
            unavailable,
            base_agents,
            gap_evidence,
        } = outcome;
        if !reply.is_empty() {
            last_reply = reply;
        }

        // MEDIUM #2 (first-step bail) — FIX: reset the just-marked Active step BEFORE
        // bailing. If the FIRST step is a Build that could not drive a single turn (a
        // dead session on the very first doer turn), the base can't be scheduled —
        // return None so the caller runs the single end-to-end turn rather than
        // silently marking a plan "done" over an empty build. But step 1 was already
        // marked Active above; left as-is it strands the plan with a wedged `Active`
        // step that a resume reads as in-flight. Reset it to Pending (+ emit + persist)
        // so the fallback single-turn build starts from a clean plan. A first Review
        // step that no-ops (an empty team) does NOT bail: there's simply nothing to
        // review yet, and the next (Build) step still gets its chance.
        if transitions == 1 && step.kind == plan_state::StepKind::Build && !drove {
            let reason = "the first scheduled doer turn could not run";
            let blockers = vec![reason.to_string()];
            let _ = task_tracker.settle_base_agents(
                &step,
                &base_agents,
                StepStatus::Blocked,
                true,
                reason,
                &blockers,
            );
            let _ = task_tracker.settle_step(
                &step,
                StepStatus::Blocked,
                true,
                reason,
                blockers.clone(),
            );
            let _ = task_tracker.finish(false, reason, blockers);
            plan.mark(&step_id, StepStatus::Pending);
            events.emit(EngineEvent::plan_step_status(
                step_id,
                step.title,
                StepStatus::Pending,
            ));
            persist_plan_ref(plan, options);
            return None;
        }

        // MEDIUM #3 / HIGH #1 — a step that was "accepted" but made NO real progress
        // (an empty-team ReviewClean neutral skip, or a Build step over a dead turn
        // that only cleared a neutral-skip acceptance) must NOT advance the done
        // count. Mark it Blocked (honest: nothing verifiable happened) rather than
        // Done, so the checklist + conclusion don't overstate completion. A genuinely
        // accepted step (real evidence) ticks Done as before.
        let status = if accepted && made_progress {
            StepStatus::Done
        } else if accepted && step.kind == plan_state::StepKind::Review && gap_evidence.is_empty() {
            // An empty-team REVIEW is a NEUTRAL skip: there was no seat to convene, so
            // there are no blocking findings — a clean pass for a review step, NOT a
            // block. Marking it Blocked made `all Done` false → `clean=false` → the whole
            // finalize withheld the proof-pack, so a fully-successful build was reported
            // INCOMPLETE purely because a review step had nobody to convene. (A BUILD
            // step over a dead turn is still Blocked below — that IS an honest gap.)
            // GUARD `gap_evidence.is_empty()` (M3): a review step whose residual finding
            // was CORROBORATED by the deterministic floor returns accepted+!made_progress
            // WITH non-empty gap_evidence — it must stay Blocked (its own intent), so the
            // final `clean` fails + a bounded re-plan can route around it. Only the TRUE
            // empty-team skip (no gap_evidence) ticks Done.
            StepStatus::Done
        } else {
            // Bounded: a step that exhausted its fix budget — OR a BUILD step that cleared
            // only a neutral skip with no real work — is Blocked (honest), so it no longer
            // gates dependents but the plan records the gap. The final QC gate + the
            // final QC gate still decides overall reality.
            StepStatus::Blocked
        };
        let task_summary = if status == StepStatus::Done {
            format!("step `{}` passed its deterministic acceptance", step.title)
        } else if unavailable {
            format!(
                "step `{}` could not obtain a required host/review",
                step.title
            )
        } else {
            format!(
                "step `{}` did not pass its deterministic acceptance",
                step.title
            )
        };
        let mut task_blockers = gap_evidence.clone();
        if status == StepStatus::Blocked && task_blockers.is_empty() {
            task_blockers.push(task_summary.clone());
        }
        if let Err(error) = task_tracker.settle_base_agents(
            &step,
            &base_agents,
            status,
            unavailable,
            &task_summary,
            &task_blockers,
        ) {
            let reason = format!(
                "base-native tasks under `{}` could not settle: {error}",
                step.id
            );
            events.emit(EngineEvent::Note(format!("team · {reason}")));
            plan.mark(&step_id, StepStatus::Blocked);
            persist_plan_ref(plan, options);
            let _ = task_tracker.finish(false, &reason, vec![reason.clone()]);
            return Some(DirectorLoopOutcome::Failed(reason));
        }
        if let Err(error) =
            task_tracker.settle_step(&step, status, unavailable, &task_summary, task_blockers)
        {
            let reason = format!("agent task `{}` could not settle: {error}", step.id);
            events.emit(EngineEvent::Note(format!("team · {reason}")));
            plan.mark(&step_id, StepStatus::Blocked);
            persist_plan_ref(plan, options);
            let _ = task_tracker.finish(false, &reason, vec![reason.clone()]);
            return Some(DirectorLoopOutcome::Failed(reason));
        }
        plan.mark(&step_id, status);
        events.emit(EngineEvent::plan_step_status(
            step_id,
            step.title.clone(),
            status,
        ));
        persist_plan_ref(plan, options);
        // Advance the 9-phase workflow state to the furthest phase the plan's Done
        // steps now imply (monotonic — `persist_phase` clamps, never regresses). Only
        // a step that actually ticked Done moves the phase; a Blocked step leaves it.
        // Fail-open. This is what keeps `/status` honest as the build progresses.
        sync_phase_from_plan(plan, options);

        // SLIDING run-budget reset (Stage 3): a step that made genuine forward
        // progress is base productivity at the scheduling level — slide the between-
        // steps idle window so a build that keeps completing (even slow) steps is
        // never guillotined between them (bounded only by the ABSOLUTE cap). A step
        // that drove NOTHING forward does not reset it, so a truly stalled run still
        // winds down to a resumable budget pause.
        if made_progress {
            last_progress = std::time::Instant::now();
        }

        if status == StepStatus::Done && made_progress {
            let kind = match step.kind {
                plan_state::StepKind::Build => "build",
                plan_state::StepKind::Review => "review",
            };
            let _ = crate::context::record_run_note(
                &options.project_root,
                &format!("Verified {kind} step completed: {}", step.title.trim()),
            );
        }

        // CIRCUIT BREAKER (UD-FLOW-008). Feed this step's outcome into the
        // consecutive-same-class-failure breaker: a Done step is real progress (reset);
        // a step that actually DROVE a turn but could not pass its acceptance (a real
        // verification/rework failure) records a same-class failure. A neutral skip (an
        // empty-team review, or a dead-but-accepted step — `!drove`, or accepted with no
        // progress) is NEITHER a failure nor progress, so it never trips and never resets.
        // When the breaker trips we STOP scheduling new steps and fall through to the
        // honest final gate + finalize(clean=false) with a typed diagnosis surfaced,
        // instead of looping to MAX_STEP_TRANSITIONS burning the base's effort.
        if status == StepStatus::Done {
            failure_breaker.record_success();
        } else if drove && !accepted {
            let class = match step_kind {
                plan_state::StepKind::Build => "build-verify",
                plan_state::StepKind::Review => "review-verify",
            };
            if failure_breaker.record_failure(class) {
                events.emit(EngineEvent::Note(format!(
                    "team · {} — stopping the schedule early (last failing step: {}). \
                     The base could not make the plan steps pass their acceptance; fix \
                     the blocker or raise the plan quality, then /continue.",
                    failure_breaker
                        .diagnosis()
                        .unwrap_or_else(|| "circuit breaker tripped".to_string()),
                    step_title,
                )));
                break;
            }
        }

        // BOUNDED RE-PLAN (≤1 per run) — a step just BLOCKED. If it strands a dependent
        // subtree (those steps can never become ready), ask the brain ONCE for a
        // replacement sub-DAG that routes around / resolves the blocker, validate it
        // through the SAME `normalized()` machinery, and merge it into the live plan
        // replacing the blocked subtree — then keep driving. Every failure mode (no
        // dependents, budget spent, consult failed, unparseable, no genuine change)
        // falls back to today's EXACT behaviour: the strand is left for
        // `mark_unreachable_pending_blocked` below and reported honestly. The merged
        // sub-DAG still faces the identical acceptance floor; no gate is weakened. The
        // `replanned` flag is consumed on the single attempt so this can never loop.
        if status == StepStatus::Blocked {
            if gap_evidence.is_empty() {
                incomplete_evidence.push((
                    step.id.clone(),
                    format!(
                        "step `{step_title}` did not satisfy acceptance or produce verifiable progress"
                    ),
                ));
            } else {
                incomplete_evidence.extend(
                    gap_evidence
                        .iter()
                        .map(|gap| (step.id.clone(), format!("step `{step_title}`: {gap}"))),
                );
            }
            attempt_replan_blocked_subtree(
                session,
                options,
                events,
                plan,
                &step.id,
                &step_title,
                &gap_evidence,
                &mut replanned,
                deadline,
            )
            .await;
        }

        // ACTIVE FACT-RECORDING BACKSTOP — after a Build step that did REAL work,
        // extract this turn's durable facts ourselves (a read-only fork asking the
        // brain for `key: value` lines) and persist them to `.umadev/memory/facts.jsonl`,
        // so the store reliably populates instead of relying on the base voluntarily
        // writing it (the user-reported gap). A step's completion is the natural hook
        // point; the call is THROTTLED (only a bounded subset of build steps) and
        // fully fail-open — a failed fork / `none` reply / unwritable store records
        // nothing and never affects the schedule. Only a step that actually ticked
        // Done counts as a work turn (a Blocked/empty step never extracts).
        if step.kind == plan_state::StepKind::Build && status == StepStatus::Done {
            work_turns += 1;
            crate::fact_extract::maybe_extract_facts(
                session,
                &options.project_root,
                Some(route),
                work_turns,
                events,
            )
            .await;
        }

        // ── Spec-MUST human confirmation gates on the DEFAULT path (UD-FLOW-002 /
        // UD-FLOW-003 — A1-GAP1). When the just-settled step completed the core-doc
        // family (PM / architect / UIUX all Done) → pause at `docs_confirm`; when it
        // completed the frontend family → `preview_confirm`. Hosted + non-auto runs
        // only (headless / auto keep today's drive-through). The pause is REAL: the
        // plan persists with the remaining steps Pending, the open door lands in
        // `workflow-state.json`, `GateOpened` renders the gate card, and the caller
        // resumes via `drive_director_loop_resume` (Done steps never re-drive).
        // FAIL-OPEN: if the plan can't persist, do NOT pause — a pause that can't
        // re-load would lose the build on resume, so the run keeps driving instead.
        if let Some(gate) = confirm_gate_after_step(&step, status, plan, options) {
            if plan_state::save(plan, &options.project_root).is_ok() {
                if let Err(error) = task_tracker.wait_for_user(gate.id_str()) {
                    let reason = format!("agent task ledger could not pause at gate: {error}");
                    events.emit(EngineEvent::Note(format!("team · {reason}")));
                    let _ = task_tracker.finish(false, &reason, vec![reason.clone()]);
                    return Some(DirectorLoopOutcome::Failed(reason));
                }
                // Checkpoint the doc versions the user is confirming, so a resume
                // re-opens doc steps ONLY if the user actually edits a doc while
                // the run is parked (the staleness store, same as persist_plan_ref).
                record_artifact_versions(&options.project_root);
                persist_gate_open(options, gate);
                events.emit(EngineEvent::gate_opened(gate));
                return Some(DirectorLoopOutcome::PausedAtGate { gate });
            }
        }
    }

    // RESUMABLE BUDGET PAUSE (park, do NOT finish/finalize) — mirror the GATE-pause
    // exit above. If the schedule loop stopped ONLY because the wall-clock budget was
    // exhausted AND resumable work remains (≥1 `Pending`/`Active` step), PARK the
    // ledger (root → `Waiting`, exactly like `wait_for_user` at the gate pause) and
    // return a first-class `PausedAtBudget` BEFORE the finish/finalize block below.
    //
    // That block calls `task_tracker.finish(false, …)`, which CANCELS the unfinished
    // child seats and marks the ledger ROOT terminally `Failed`; on `/continue`,
    // `PlanTaskTracker::open` → `ensure_root` then REFUSES that terminal root
    // ("director task is already terminal") and the whole resume dies `Failed`. It
    // also runs `finalize_phase_from_plan(false)` / `director::finalize(false)`, which
    // record a resumable pause as a finalized FAILURE (disagreeing with `/status`).
    // Parking keeps the seats + root resumable and skips the finalize entirely — a
    // budget pause is a resumable CHECKPOINT, never a terminal settle. The plan is
    // checkpointed here (Done steps stay Done, the rest Pending — this runs BEFORE
    // `mark_unreachable_pending_blocked`, so no remaining step is stranded Blocked) and
    // a per-step summary is emitted so the parked state survives in the transcript.
    //
    // FAIL-OPEN: if the plan can't persist OR the ledger can't park, fall THROUGH to
    // the normal finish/finalize path rather than returning an unresumable pause (a
    // pause that can't reload would lose the build). `budget_reached` is set ONLY at
    // the between-steps budget break (never by the circuit breaker or a verification
    // failure), so a real failure is never disguised as a pause.
    let budget_resumable = budget_reached
        && plan
            .steps
            .iter()
            .any(|s| matches!(s.status, StepStatus::Pending | StepStatus::Active));
    if budget_resumable && plan_state::save(plan, &options.project_root).is_ok() {
        match task_tracker.wait_for_user("run time budget exhausted") {
            Ok(()) => {
                // Same checkpoint the gate pause records: the artifact versions the
                // parked plan was built against (so a resume only re-opens a doc step
                // if the user actually edited a doc while parked).
                record_artifact_versions(&options.project_root);
                emit_plan_completion_summary(plan, events, &incomplete_evidence);
                let (done, total) = plan.progress();
                return Some(DirectorLoopOutcome::PausedAtBudget { done, total });
            }
            Err(error) => {
                // Parking failed — do NOT return an unresumable pause; fall through to
                // the finish/finalize path so the run settles honestly instead.
                events.emit(EngineEvent::Note(format!(
                    "team · could not park the agent ledger at the budget pause: {error}"
                )));
            }
        }
    }

    // MEDIUM #2 — honest scope: a Blocked step permanently strands its dependents as
    // Pending (they never become ready, since readiness needs every dep Done). The
    // scheduling loop above just leaves them; without this they'd sit Pending forever
    // while the run still reports Done — a SILENT loss of scope. Mark every Pending
    // step that is unreachable because a dependency (transitively) Blocked as Blocked
    // too, surface a one-line Note, and persist — so the checklist and the conclusion
    // are honest about what was actually skipped. Fail-open: an empty/clean plan
    // strands nothing → no Note.
    let stranded = mark_unreachable_pending_blocked(plan, events);
    if stranded > 0 {
        events.emit(EngineEvent::Note(format!(
            "team · {stranded} 个计划步骤因前置被阻塞而跳过(标记为已阻塞,未执行)"
        )));
        persist_plan_ref(plan, options);
    }

    // Final whole-build QC gate — the SAME objective pass the single-turn loop runs
    // as its last word (source-present hard floor + content governance + optional
    // review), so a step-driven build is held to the identical floor. Advisory: it
    // does not re-drive here (each step was already verified); it folds any residual
    // finding into ONE last fix turn, bounded, then settles. This guarantees a
    // step-driven build is never held to a WEAKER bar than the single-turn build.
    // Seed corroboration `false`: the step-driver doesn't observe per-step tool calls
    // here, so the final whole-build gate's round 0 runs UmaDev's OWN build/test read
    // rather than trusting the last step's prose (a safe tightening — each step was
    // already verified; this only re-checks once). Fix rounds re-derive it per turn.
    let no_fix_context = KnowledgeDigest::default();
    let final_gate = run_final_gate(
        session,
        options,
        events,
        route,
        &last_reply,
        deadline,
        &no_fix_context,
        false,
    )
    .await;
    if !final_gate.reply.is_empty() {
        last_reply = final_gate.reply;
    }

    // HONEST clean signal (used for the report below AND finalize): every step
    // reached Done (none Blocked / stranded) AND the final whole-build gate settled
    // clean — only then may the build claim `delivery`. H1: the final gate runs the
    // cross-cutting checks (coverage / contract / runtime-proof / governance / fork
    // review); its clean-ness was previously DISCARDED, so a build with every step
    // Done but a DIRTY final gate (a dropped FR / contract drift / unverified
    // runtime-proof) finalized as success. AND the gate's clean signal in, so an
    // incomplete build can never be disguised as a clean delivery. This makes the
    // step path's gate never weaker than the single-turn loop (which already gates
    // finalize INSIDE `qc.is_clean()`). Fail-open: a dirty gate just means "not clean".
    let mut clean = plan.steps.iter().all(|s| s.status == StepStatus::Done) && final_gate.clean;
    let mut task_failure_reason = None;
    let task_finish_summary = if clean {
        "all agent tasks passed their deterministic acceptance"
    } else {
        "one or more agent tasks did not reach verified success"
    };
    match task_tracker.finish(
        clean,
        task_finish_summary,
        incomplete_evidence
            .iter()
            .map(|(step_id, evidence)| format!("{step_id}: {evidence}"))
            .chain(final_gate.blocking.iter().cloned())
            .collect(),
    ) {
        Ok(crate::task_lifecycle::RunReadiness::Succeeded) if clean => {}
        Ok(readiness) => {
            clean = false;
            task_failure_reason = Some(summarize_ledger_readiness(&readiness));
        }
        Err(error) => {
            clean = false;
            task_failure_reason = Some(format!("agent task ledger could not settle: {error}"));
        }
    }

    // Only a CLEAN plan-driven convergence earns the integrated final report. Every
    // per-step directive suppressed its project-level wrap-up, so this bounded turn is
    // the one user-facing conclusion. A Blocked/budget/dirty-gate settle deliberately
    // skips it: streaming a success-shaped report before returning Failed would recreate
    // the false completion this terminal check prevents. Fail-open on a dead report turn:
    // `integrated_final_report` preserves the previous reply.
    if clean {
        last_reply = integrated_final_report(session, options, events, last_reply, deadline).await;
    }

    let incomplete_reason = (!clean).then(|| {
        let mut reason = plan_incomplete_reason(
            plan,
            budget_reached || std::time::Instant::now() >= deadline,
            transitions >= MAX_STEP_TRANSITIONS,
            &incomplete_evidence,
            &final_gate.blocking,
        );
        if let Some(task_reason) = task_failure_reason {
            reason.push_str("; ");
            reason.push_str(&task_reason);
        }
        reason
    });

    // Persist the plan's terminal state for resume.
    persist_plan_ref(plan, options);
    finalize_phase_from_plan(plan, options, clean);
    // A PERSISTENT per-step completion summary, emitted on EVERY deliberate-build exit
    // (clean, budget-reached, or blocked). The live plan panel is ephemeral — it vanishes
    // when the run settles — so on an incomplete finish the user could no longer see WHICH
    // steps completed, which stalled, which never ran (reported feedback). This writes the
    // breakdown into the transcript so the final task state always survives. The
    // per-step evidence is threaded in so each Blocked step shows its own WHY.
    emit_plan_completion_summary(plan, events, &incomplete_evidence);
    // Wave 4 (§L4 / G8): a step-driven (always deliberate) build leaves the FULL
    // shareable delivery — core docs + proof-pack + scorecard — but ONLY when the
    // build settled clean (every step Done). MEDIUM M2: passing `clean` here stops
    // finalize from emitting a proof-pack + delivery scorecard for an INCOMPLETE build
    // (blocked / stranded steps), which would disguise it as success. Fail-open inside.
    director::finalize(options, events, Some(route), clean);
    // SELF-EVOLUTION at delivery (a SIDE EFFECT of a clean deliberate delivery, never
    // a driver): reconcile the lesson library — ask the brain (read-only fork,
    // fail-open) to judge each fresh lesson against its similar priors (ADD / UPDATE /
    // INVALIDATE) so memory is CURATED, not just appended. Gated to a clean deliberate
    // delivery (finalize already ran the plain append-sediment); offline / no-fork
    // degrades to a no-op. Runs next to finalize where the session is still live.
    if clean && route.depth.is_deliberate() {
        crate::self_evolve::reconcile_at_delivery(session, &options.project_root, events).await;
        // SUCCESS-RECIPE CAPTURE (the WIN sibling of the pitfall pipeline): distil the
        // plan shape this CLEAN deliberate build actually executed — the ordered
        // step titles/seats that reached Done, the scaffold its evidence named, the
        // detected stack + requirement shape — into a reusable project-private recipe,
        // so the next similar build gets it as a plan-time PRIOR. One optional
        // read-only fork enriches patterns; everything is best-effort + fail-open, so
        // a capture error NEVER affects the just-finished delivery.
        crate::recipes::capture_at_delivery(
            session,
            &options.project_root,
            route,
            plan,
            &options.requirement,
            events,
        )
        .await;
    }
    // SIZING calibration: a step-driven build is ALWAYS a deliberate route (predicted
    // HEAVY). Measure the ACTUAL heaviness by how many Build steps did real work — a
    // deliberate route that finished in <=1 real build step OVER-sized the turn (the
    // "Greenfield that finished in one trivial step" case). Advisory + fail-open: this
    // never altered any step's status, the floor, or the gate above.
    crate::sizing_calibration::record_route(
        &options.project_root,
        route,
        run_actual_size_from_plan(plan),
    );
    // TERMINAL SETTLE. The RESUMABLE budget pause is handled EARLIER, at the
    // park-not-finish exit above (which returns `PausedAtBudget` BEFORE this
    // finish/finalize block so the ledger is parked `Waiting`, never finalized
    // `Failed`). Reaching here with `budget_reached` set therefore means the pause
    // could NOT be checkpointed/parked (the plan failed to persist or the ledger
    // could not park) — the run has already been finalized `Failed` above, so
    // returning an unresumable `PausedAtBudget` here would recreate the "resume dies
    // already-terminal" bug. Settle honestly: `Done` on a clean convergence, else
    // `Failed` with the incomplete reason.
    Some(match incomplete_reason {
        Some(reason) => DirectorLoopOutcome::Failed(reason),
        None => DirectorLoopOutcome::Done { reply: last_reply },
    })
}

/// The confirmation gate to OPEN after a plan step settled, or `None` (the
/// overwhelmingly common case) — the DEFAULT path's revival of the two spec-MUST
/// human gates (`UD-FLOW-002` docs_confirm / `UD-FLOW-003` preview_confirm;
/// A1-GAP1: `drive_plan_steps` never emitted `GateOpened` and never paused, so
/// the gates only existed on the legacy engine).
///
/// **Transition-triggered by design** (no persisted "gate passed" state needed):
/// a gate fires exactly when the JUST-SETTLED step is a `Done` BUILD step of the
/// gate's producing seat family AND that family is now fully `Done`. A resumed
/// run never re-drives a `Done` step, so a resume can never re-fire the gate it
/// paused at; a doc step re-opened by artifact staleness (the user revised the
/// doc) that re-settles `Done` legitimately re-confirms.
///
/// Fires ONLY when every condition holds:
/// - the run is HOSTED by a UI that renders + resumes gates
///   ([`crate::interaction::gates_hosted`]) — a headless CLI / CI run could never
///   resume a pause, so it keeps today's drive-through behaviour (fail-open);
/// - the trust tier is NOT `auto` ([`crate::trust::TrustMode::gates_auto_approve`]
///   — auto runs end-to-end, exactly as today);
/// - real work remains (≥1 `Pending`/`Active` step) — a pause with nothing left
///   would strand the run (a fully-terminal plan is not resumable).
///
/// The designer's **design-tokens deliverable step**
/// ([`plan_state::is_design_tokens_step`]) is UIUX-seated but is CODE-PHASE PREP,
/// not doc authoring: it is excluded from the docs family AND from the trigger, so
/// (1) the docs gate fires as soon as the actual PM/architect/UIUX *docs* are Done
/// (the tokens step runs after the gate resumes), and (2) the tokens step settling
/// Done post-resume can never RE-fire `docs_confirm` (gate-opens-once).
fn confirm_gate_after_step(
    step: &plan_state::PlanStep,
    status: StepStatus,
    plan: &Plan,
    options: &RunOptions,
) -> Option<crate::gates::Gate> {
    use crate::critics::Seat;
    // Only a BUILD step that genuinely settled Done completes a gate's producing
    // work (a Blocked doc/frontend step is an honest gap — nothing to confirm).
    if step.kind != plan_state::StepKind::Build || status != StepStatus::Done {
        return None;
    }
    if options.mode.gates_auto_approve() || !crate::interaction::gates_hosted() {
        return None;
    }
    // Never pause a run that has nothing left to drive — the pause would not be
    // resumable and the final gate / finalize below own the ending instead.
    if !plan
        .steps
        .iter()
        .any(|s| matches!(s.status, StepStatus::Pending | StepStatus::Active))
    {
        return None;
    }
    // "The whole family is Done" — true only when the family is non-empty AND
    // every member reached Done (a Blocked member keeps the gate closed: the
    // docs/preview are incomplete, so there is nothing coherent to confirm).
    let family_done = |member: &dyn Fn(&plan_state::PlanStep) -> bool| -> bool {
        let mut any = false;
        for s in plan.steps.iter().filter(|s| member(s)) {
            any = true;
            if s.status != StepStatus::Done {
                return false;
            }
        }
        any
    };
    let doc_seat = |s: Seat| {
        matches!(
            s,
            Seat::ProductManager | Seat::Architect | Seat::UiuxDesigner
        )
    };
    // A DOC-family member is a doc-seat BUILD step that is NOT designer code-phase
    // PREP — neither the visual-direction step nor the design-tokens deliverable
    // (both run AFTER the docs are confirmed, and neither may re-fire the gate).
    let doc_member = |s: &plan_state::PlanStep| {
        s.kind == plan_state::StepKind::Build
            && doc_seat(s.seat)
            && !plan_state::is_design_prep_step(s)
    };
    if doc_member(step) && family_done(&doc_member) {
        return Some(crate::gates::Gate::DocsConfirm);
    }
    if step.seat == Seat::FrontendEngineer
        && family_done(&|s: &plan_state::PlanStep| {
            s.kind == plan_state::StepKind::Build && s.seat == Seat::FrontendEngineer
        })
    {
        return Some(crate::gates::Gate::PreviewConfirm);
    }
    None
}

/// Persist the OPEN-GATE workflow state for a director-loop pause — the P0-A twin
/// of [`crate::continuous::run_block`]'s gate write, so `/status`, `umadev
/// continue`, and a fresh TUI session all read the REAL door this run parked at.
/// Phase is clamped monotonic exactly like [`persist_phase_impl`]; the base
/// session id (the cross-session resume pointer) is carried forward. **Fail-open
/// by contract:** a failed write is swallowed — the pause itself is already
/// guarded by a successful plan persist at the call site.
fn persist_gate_open(options: &RunOptions, gate: crate::gates::Gate) {
    let current_state = crate::state::read_workflow_state(&options.project_root);
    let current = current_state
        .as_ref()
        .and_then(|s| {
            umadev_spec::PHASE_CHAIN
                .iter()
                .copied()
                .find(|p| p.id() == s.phase)
        })
        .unwrap_or(Phase::Research);
    let gate_phase = match gate {
        crate::gates::Gate::DocsConfirm => Phase::DocsConfirm,
        crate::gates::Gate::PreviewConfirm => Phase::PreviewConfirm,
        // Never produced by the director loop; anchor defensively to the head.
        crate::gates::Gate::ClarifyGate => Phase::Research,
    };
    // Clamp: never regress below what's already on disk (the doc steps may have
    // already advanced the phase past the gate's own anchor).
    let phase = if phase_rank(gate_phase) >= phase_rank(current) {
        gate_phase
    } else {
        current
    };
    let state = crate::state::WorkflowState {
        phase: phase.id().to_string(),
        active_gate: gate.id_str().to_string(),
        slug: options.effective_slug(),
        requirement: options.requirement.clone(),
        last_transition_at: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        note: format!("Paused at {} (director loop)", gate.id_str()),
        backend: options.backend.clone(),
        base_session_id: current_state
            .as_ref()
            .and_then(|s| s.base_session_id.clone()),
        base_resume_identity: current_state
            .as_ref()
            .and_then(|s| s.base_resume_identity.clone()),
        permission_profile: Some(options.mode.base_permissions()),
        spec_version: umadev_spec::SPEC_VERSION.to_string(),
    };
    let _ = crate::state::write_workflow_state(&options.project_root, &state);
}

/// The ACTUAL [`crate::sizing_calibration::SizeRank`] a step-driven build settled at,
/// read deterministically from the terminal plan: how many Build steps actually reached
/// `Done`. Zero real build steps → `Trivial`; exactly one → `Light` (finished in a
/// single trivial step); two or more → `Heavy` (a genuine multi-step build). Review
/// steps don't count — only doer-seat work moves the dial. Pure; advisory telemetry.
fn run_actual_size_from_plan(plan: &Plan) -> crate::sizing_calibration::SizeRank {
    use crate::sizing_calibration::SizeRank;
    let done_build = plan
        .steps
        .iter()
        .filter(|s| s.kind == plan_state::StepKind::Build && s.status == StepStatus::Done)
        .count();
    match done_build {
        0 => SizeRank::Trivial,
        1 => SizeRank::Light,
        _ => SizeRank::Heavy,
    }
}

/// The observable result of driving one plan step — what the scheduler reads to set
/// the step's terminal status. `made_progress` is the MEDIUM #3 honesty signal: an
/// "accepted" step that did NO real verifiable work (a dead Build turn that only
/// cleared a neutral skip, or an empty-team ReviewClean) is accepted-but-not-progress,
/// so the scheduler marks it Blocked rather than falsely ticking it Done.
// The three flags are INDEPENDENT honest observations of one step's outcome (accepted /
// drove-a-turn / made-real-progress), not a state machine — collapsing
// them into an enum would lose the orthogonal signals the scheduler reads separately.
#[allow(clippy::struct_excessive_bools)]
struct StepOutcome {
    /// Whether the step's acceptance is satisfied (passed or fail-open neutral skip).
    accepted: bool,
    /// The step's last assistant reply text (empty when nothing ran).
    reply: String,
    /// Whether at least one real work turn actually ran (a live doer turn settled /
    /// a review team convened). `false` = a dead/hung session or an empty team.
    drove: bool,
    /// Whether the step made REAL, verifiable progress — `accepted` resting on either
    /// a turn that actually ran OR positive deterministic evidence (real source / a
    /// green build / a seat that actually reviewed). `false` = a neutral skip that
    /// must not count toward `Done`.
    made_progress: bool,
    /// The required host/reviewer was unavailable, rather than returning a
    /// semantic verification failure.
    unavailable: bool,
    /// Base-native child agents observed while producing this step. The plan
    /// ledger hashes vendor ids and settles these children with the same
    /// deterministic result as their parent step.
    base_agents: crate::bg_agents::BaseAgentObservation,
    /// The TYPED gap evidence from the step's LAST failing acceptance check (the
    /// diagnosed "declared X but Y" lines the deterministic floor produced) — carried
    /// out so a BOUNDED RE-PLAN of a blocked subtree can feed the brain WHY the step
    /// blocked, not just that it did. Empty on an accepted step or a neutral skip that
    /// produced no verifiable failure.
    gap_evidence: Vec<String>,
}

/// The overall-goal preamble prepended to every plan-step directive — the directive
/// half of full-context cross-session resume. It restates the ORIGINAL requirement
/// so the base knows the product it is building, not just an isolated step title.
///
/// On a real base-session resume (`--resume` / `thread/resume`) the base already
/// re-supplies its own transcript, so this is belt-and-suspenders; when the resume
/// degraded to a fresh session (no persisted id / a resume error), this preamble is
/// the LOAD-BEARING context that stops the brain "forgetting the task" and acting on
/// a bare step title. Fail-open: an empty requirement yields an empty frame (the
/// directive is byte-for-byte the old step-title form).
fn step_goal_frame(options: &RunOptions) -> String {
    let req = options.requirement.trim();
    if req.is_empty() {
        return String::new();
    }
    format!(
        "## Overall goal (the product being delivered)\n{req}\n\n\
         You are continuing the delivery plan for that goal; complete the current \
         step below in service of it.\n\n## Current step\n"
    )
}

/// A TARGETED directive addendum (Change 1) that DEFERS the base's premature final
/// wrap-up while a step is still running. The base streams its own "here's what I did
/// — ## Next steps …" conclusion the instant a doer step finishes — BEFORE the team
/// review + blocking-item rework runs — so when there ARE blockers that conclusion is
/// stale (it can't reflect fixes or the plan/dependency changes they caused). This
/// note tells the base to keep NARRATING its concrete working actions (unchanged) but
/// to hold back only the project-level DONE / next-steps CONCLUSION this turn; the
/// single integrated final report is written once the whole build converges (after
/// review + any rework — see [`integrated_final_report_directive`]). Appended to the
/// mid-run doer / review-fix / single-turn FIX-round directives — NEVER to the
/// single-turn ROUND-0 build directive (a clean single-turn build settles on that one
/// reply, so its only turn IS the wrap-up) and NEVER to the integrated-summary
/// directive (that one WANTS the conclusion).
pub(crate) fn wrapup_suppression_note() -> &'static str {
    "\n\n## Stay mid-run — do NOT write the final wrap-up yet\n\
     This is one step of a build that is still being reviewed and hardened, not the \
     end. Keep narrating the concrete actions you take as you work, but do NOT close \
     this turn with a project-level conclusion — no final \"summary\", \"## Next \
     steps\", \"## 下一步\", or \"完成汇报\" block. The single, integrated final report is \
     written only after the whole build converges (after team review and any \
     blocking-item rework)."
}

/// Whether a turn's directive is a RESEARCH / PLANNING / core-doc-authoring turn —
/// the advisory seam where a base legitimately reaches for live web research (and so
/// where a gateway's refusal of a hosted `web_search` may be DEGRADED past instead of
/// hard-failing the whole build). Pure substring scan over the lowercased directive.
///
/// This is the SECONDARY gate the capability degrade is bound to (the primary being
/// the tight [`crate::base_error::is_capability_degradable`] class): the two together
/// are what confine the degrade to the research/planning seam and keep a genuine code
/// failure — never this class, and never on one of these directives — on its honest
/// hard-fail path.
///
/// The markers are curated to fire on UmaDev's OWN research/planning/doc-authoring
/// directives (the `research_prompt` / coach research framing, the "produce the three
/// core documents" docs framing, and the `output/<slug>-{research,prd,architecture,
/// uiux}.md` deliverable paths a doc-authoring seat step names) while staying ABSENT
/// from the universal build preamble ([`crate::experts::agentic_engineering_rules`]),
/// the per-step goal/route frames, and a pure frontend/backend code step's directive —
/// so a code turn does not read as a research turn.
#[must_use]
pub(crate) fn is_research_or_planning_directive(directive: &str) -> bool {
    let hay = directive.to_ascii_lowercase();
    const MARKERS: &[&str] = &[
        "research",             // research brief / research phase / product research
        "web search",           // the research method's live-web instruction
        "web_search",           // the hosted tool the gateway refused, named in-directive
        "search wide",          // research_prompt's "SEARCH WIDE first"
        "product researcher",   // the research persona
        "design strategist",    // the research persona
        "competitive analysis", // a research-brief section
        "similar products",     // a research-brief section
        "market positioning",   // a research-brief section
        "discovery section",    // the research Discovery section
        "product requirements", // the PRD authoring frame
        "core document",        // "produce ALL THREE core documents"
        "-research.md",         // the research deliverable path
        "-prd.md",              // the PRD deliverable path (doc authoring)
        "-architecture.md",     // the architecture deliverable path (doc authoring)
        "-uiux.md",             // the UIUX deliverable path (doc authoring)
    ];
    MARKERS.iter().any(|m| hay.contains(m))
}

/// The directive addendum appended for a ONE-SHOT capability degrade re-drive: it
/// tells the base its hosted web tool was refused by the gateway and to PROCEED on
/// LOCAL KNOWLEDGE WITHOUT web research rather than stalling. Deliberately harmless to
/// append to any turn — a turn that was not going to web-search is unaffected — so the
/// degrade can only ever help, never corrupt a real code turn.
#[must_use]
pub(crate) fn local_knowledge_without_web_directive() -> &'static str {
    "\n\n## Web research is unavailable — proceed on LOCAL KNOWLEDGE\n\
     The gateway for this model REJECTED your hosted web-search tool, so it cannot be \
     used on this run. Do NOT attempt `web_search` or any hosted web tool again. \
     Complete this step NOW using only the local knowledge provided, the requirement, \
     and your own domain reasoning — where you would have cited live competitor / \
     market data, mark it as an assumption to verify rather than stopping. Do not ask \
     me and do not abandon the step; produce the deliverable and end your turn."
}

/// The user-facing Note emitted when a research/planning turn degrades past a refused
/// hosted web tool. Bilingual literal (no i18n key needed for a single operator line),
/// matching the `team ·`-style operator notes this loop already emits.
#[must_use]
pub(crate) fn capability_degrade_note() -> &'static str {
    "team · 底座的 web_search 被网关拒绝 — 研究阶段降级为本地知识,继续构建 \
     (base web_search rejected by the gateway — research degraded to local knowledge, continuing)"
}

/// The directive (Change 2) that drives the ONE integrated final report at convergence,
/// AFTER the review → rework loop has settled. Because the base session is CONTINUOUS
/// (it accumulated the full build + every rework across steps), the base can produce a
/// holistic report from its OWN context — UmaDev keeps no critic verdicts agent-side.
/// This is the wrap-up deferred by [`wrapup_suppression_note`], so it explicitly WANTS
/// the conclusion (no suppression appended). Emitted when the build went through
/// blocking-item rework (any path) AND at the CLEAN convergence of a plan-driven
/// build (whose per-step doer replies were all suppression-noted, so without this
/// turn a clean build would end on a conclusion-free step narration). A clean
/// single-turn build keeps its round-0 reply as-is — that turn ran un-suppressed.
fn integrated_final_report_directive(options: &RunOptions) -> String {
    let req = options.requirement.trim();
    let goal = if req.is_empty() {
        String::new()
    } else {
        format!("## The product being delivered\n{req}\n\n")
    };
    format!(
        "{goal}The build has now reached a clean convergence: no unresolved review \
         blocker or unavailable required reviewer remains. Write the SINGLE, \
         integrated final report for this build now — this is the wrap-up you deferred \
         while the steps were still running. Ground it entirely in what actually \
         happened in this session (you hold the full history). Do NOT open new work or \
         edit files — just report, concisely:\n\
         - What was ultimately delivered, and its real, current state.\n\
         - Which blocking / must-fix items the review actually surfaced and how each was resolved; if none were raised, say none.\n\
         - Any changes to the plan or task dependencies those fixes caused.\n\
         - Anything the user should know or do next.\n\
         Write the report in the same language you have been working in this session."
    )
}

/// Drive the ONE integrated final report ([`integrated_final_report_directive`]) on the
/// MAIN continuous session and return it as the reply the user sees as the build's final
/// word. FAIL-OPEN by contract: a turn that errors (dead/hung session) or comes back
/// empty degrades to `current` (the existing `last_reply`), so the reply is never lost —
/// the extra turn can only IMPROVE the reported result, never crash or blank it. Bounded
/// by the same `deadline` as every other governed turn.
async fn integrated_final_report(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    current: String,
    deadline: std::time::Instant,
) -> String {
    match drive_one_turn(
        session,
        options,
        events,
        integrated_final_report_directive(options),
        IdleBudget::from_env(),
        deadline,
    )
    .await
    {
        Ok(t) if !t.text.trim().is_empty() => t.text,
        _ => current,
    }
}

/// A COMPACT plan-progress recitation appended to each step directive — the
/// "next-steps" half of plan recitation ([`step_goal_frame`] is the goal half).
///
/// Periodically RE-STATING where the build is in the overall plan — how many steps
/// are done and what still lies ahead — over a long multi-step run keeps the base
/// anchored to the whole plan instead of drifting on a long sequence of isolated
/// step turns (a known long-horizon failure mode). It is recited on EVERY step (the
/// strongest "every N" bound, N=1) but is itself BOUNDED — at most the next two
/// upcoming titles, each head-clipped — so it stays one compact line and never
/// bloats the directive or the base's input budget. Fail-open: a trivial (≤1-step)
/// plan yields nothing, and the last step recites only its position.
fn plan_progress_recitation(plan: &Plan, current_step_id: &str) -> String {
    let total = plan.steps.len();
    // A single-step plan has no broader plan to keep in view — skip the recitation
    // entirely (the goal frame already states the objective).
    if total <= 1 {
        return String::new();
    }
    let done = plan
        .steps
        .iter()
        .filter(|s| s.status == StepStatus::Done)
        .count();
    // The next still-to-do steps that come AFTER this one in plan order — bounded to
    // two and each title head-clipped, so the recitation stays a single compact line.
    // (A finished step — Done or Blocked — is skipped so "ahead" is honestly ahead.)
    let upcoming: Vec<String> = plan
        .steps
        .iter()
        .skip_while(|s| s.id != current_step_id)
        .skip(1)
        .filter(|s| !matches!(s.status, StepStatus::Done | StepStatus::Blocked))
        .take(2)
        .map(|s| crate::experts::excerpt(&s.title, 60))
        .collect();
    let ahead = if upcoming.is_empty() {
        "this is the final step — finishing it completes the plan.".to_string()
    } else {
        format!("still ahead after this step: {}.", upcoming.join("; "))
    };
    format!(
        "## Plan progress (keep the whole plan in view)\n\
         {done} of {total} plan steps complete; {ahead}"
    )
}

fn render_project_learned_reference(
    kind: umadev_knowledge::PromptReferenceKind,
    source: &str,
    section: &str,
    content: &str,
) -> String {
    umadev_knowledge::render_prompt_reference(umadev_knowledge::PromptReference {
        kind,
        corpus_origin: umadev_knowledge::CorpusOrigin::ProjectLearned,
        corpus_scope: umadev_knowledge::CorpusScope::Project,
        source,
        section: Some(section),
        content,
    })
}

/// Drive ONE Build step: `summon` the step's seat serially on the main session with
/// a focused directive (recalled pitfalls injected), then verify against the step's
/// `acceptance` on the deterministic floor. A failing acceptance folds its evidence
/// into a bounded fix re-drive ([`MAX_STEP_FIX_ROUNDS`], plus one extra round for a
/// high-blast-radius upstream step — rigor weighted by `blast_radius`, see
/// [`HIGH_BLAST_RADIUS`]). Returns a [`StepOutcome`].
///
/// Wall-clock ceiling (graceful): the `deadline` bounds the EXTRA fix rounds, not the
/// real work — round 0 (the step's actual doer turn) ALWAYS runs, so a budget already
/// spent before this step never starves the step itself; only the re-drives past the
/// budget are skipped (the doc'd "hard ceiling" is honoured inside the step too).
#[allow(clippy::too_many_arguments)]
async fn drive_build_step(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    route: &RoutePlan,
    step: &plan_state::PlanStep,
    // A compact plan-progress recitation ([`plan_progress_recitation`]) appended to
    // this step's directive so the base stays anchored to the overall plan over a
    // long run. Empty for a trivial plan; bounded by construction.
    plan_progress: &str,
    // The step's blast radius (transitive downstream-dependent count). A HIGH value
    // (≥ [`HIGH_BLAST_RADIUS`]) is an upstream node many steps build on — it earns one
    // extra bounded fix round (rigor weighted by blast radius); a leaf keeps the base
    // budget. See [`HIGH_BLAST_RADIUS`].
    blast_radius: usize,
    deadline: std::time::Instant,
    // Run-scoped set of recurring-pitfall signatures a reflection has already been
    // ATTEMPTED for this run — threaded from the scheduler so self-evolution's
    // reflection consult fires AT MOST ONCE per signature per run (see
    // [`crate::self_evolve::reflect_on_recurring_failure`]).
    reflected: &mut std::collections::HashSet<String>,
) -> StepOutcome {
    let seat_id = step.seat.role_id();
    // Verification RIGOR weighted by blast radius: an expensive-to-unwind upstream step
    // is tried ONE extra bounded round before it's given up as Blocked. Still finite
    // and deadline-bounded; the deterministic acceptance is unchanged.
    let max_fix_rounds = MAX_STEP_FIX_ROUNDS + usize::from(blast_radius >= HIGH_BLAST_RADIUS);
    // The step's focused instruction + (fail-open) recalled stack pitfalls so the
    // doer pre-empts a known trap. relevant_lessons_for_prompt is empty on first
    // runs / a miss, so the directive is unchanged then.
    let pitfalls =
        crate::lessons::relevant_lessons_for_prompt(&options.project_root, &options.requirement);
    // PREPEND the ORIGINAL requirement (the directive half of full-context resume):
    // a per-step directive must restate the overall goal so the base — even a FRESH
    // brain on a cross-session resume that could not re-attach the base transcript —
    // builds the right product instead of acting on a bare step title and "forgetting
    // the task". On a real base-session resume this is belt-and-suspenders; without
    // one it is the load-bearing context. Fail-open: an empty requirement → no frame.
    let mut instruction = format!(
        "{}{} — {}",
        step_goal_frame(options),
        step.title,
        route_focus_line(route)
    );
    // PLAN RECITATION: re-state where this step sits in the whole plan so the base
    // does not drift on a long step sequence. Bounded + fail-open (empty for a
    // trivial plan).
    if !plan_progress.trim().is_empty() {
        instruction.push_str("\n\n");
        instruction.push_str(plan_progress.trim());
    }
    // RUN-NOTES RECALL (B1#6, bounded): UmaDev's persisted verified work notes
    // from earlier in this run (`.umadev/run-notes.md`). Re-injected as a bounded
    // tail at each step so the working memory SURVIVES session resets, compaction,
    // and cross-session resumes on a fresh brain. Fail-open: an absent / empty /
    // unreadable file injects nothing (directive unchanged).
    let run_notes = crate::context::run_notes_tail_block(
        &options.project_root,
        crate::context::RUN_NOTES_TAIL_LINES,
    );
    if !run_notes.trim().is_empty() {
        instruction.push_str("\n\n");
        instruction.push_str(run_notes.trim());
    }
    if !pitfalls.trim().is_empty() {
        instruction.push_str("\n\n## Known pitfalls to avoid (from past runs)\n");
        instruction.push_str(&render_project_learned_reference(
            umadev_knowledge::PromptReferenceKind::Lesson,
            ".umadev/learned/_raw",
            "requirement_recall",
            pitfalls.trim(),
        ));
    }
    // MID-RUN USER STEERING (A2#4/#5): drain the hosting UI's queued directives
    // (`/plan skip|veto|add`, text typed while the build ran, a gate revision) at
    // this STEP BOUNDARY and fold them into the doer's directive — so steering
    // applies at the next step instead of evaporating (the director path never
    // emitted the GateOpened/BlockCompleted gaps the legacy queue drained at).
    // Fail-open: headless / no intake / empty queue → the directive is unchanged.
    let steering = crate::interaction::take_steer();
    if !steering.is_empty() {
        events.emit(EngineEvent::Note(umadev_i18n::tlf(
            "plan.steer.folded",
            &[&steering.len().to_string()],
        )));
        instruction
            .push_str("\n\n## User steering (queued mid-run — honour it from this step onward)\n");
        for s in &steering {
            instruction.push_str(&format!("- {s}\n"));
        }
    }
    // REQUIRED DELIVERABLE PATH(S): a step's acceptance verifies a SPECIFIC file exists
    // (a doc-first PRD/architecture/UIUX at `output/<slug>-*.md`, a named source file).
    // A weak base often writes the deliverable to a plausible-but-wrong path (e.g. under
    // `docs/`), which fails the file-exists/contains check and stalls the step in rework.
    // Naming the exact path(s) the acceptance checks removes that ambiguity up front.
    let required_paths: Vec<&str> = step
        .evidence
        .iter()
        .filter_map(|e| match e {
            crate::plan_state::EvidenceContract::FileExists { path }
            | crate::plan_state::EvidenceContract::FileContains { path, .. } => Some(path.as_str()),
            _ => None,
        })
        .collect();
    if !required_paths.is_empty() {
        instruction.push_str(
            "\n\n## Required deliverable path(s)\nWrite this step's deliverable to EXACTLY \
             the path(s) below — the acceptance verifies the file exists there, so writing \
             it anywhere else (e.g. under docs/) fails the step:\n",
        );
        for p in &required_paths {
            instruction.push_str(&format!("- `{p}`\n"));
        }
    }
    // EXECUTION CONTRACT (model-facing belt). The deterministic post-condition
    // checks the run diff, but the doer should know the boundary before it reaches
    // for a tool: only this step's declared files, its concrete deliverables, and a
    // proportional change budget. The plan preflight above guarantees this block is
    // never the "no surface" form on a writer step.
    let step_contract =
        crate::execution_contract::ExecutionContract::from_step(route, &options.requirement, step);
    instruction.push_str("\n\n");
    instruction.push_str(&step_contract.prompt_block());

    // TEST-INTEGRITY BASELINE (UD-QA-001). Snapshot the project's TEST surface
    // BEFORE this step's doer turn(s) so the deterministic floor can detect
    // test-gaming across the step — a deleted test, removed test case, stripped
    // assertion, new skip/xfail/ignore marker, a hard-coded impl-output literal,
    // or a weakened test harness/command. Captured once at entry (the pre-step
    // state), so a deletion in round 0 that a later round restores clears itself.
    // Fail-open: a tree that can't be read yields an empty baseline (additions are
    // never flagged), and the whole guard is bounded by the SAME `max_fix_rounds`
    // as every other step finding — never an open grind.
    let test_baseline = crate::test_integrity::snapshot(&options.project_root);

    // ARCHITECTURE-FITNESS BASELINE (UD-CODE-006, spec §3.6). Snapshot
    // the source-shape surface (per-file line counts + content hashes + clone
    // windows) BEFORE this step's doer turn(s) so the deterministic floor can
    // judge what the step CHANGED: a new god file / a file grown past the
    // ceiling blocks with a split directive, a duplicated added block is
    // advisory (see `crate::arch_fitness`). DELIBERATE builds only — a chat /
    // quick-edit turn never pays this scan. Fail-open: a huge repo (>5k source
    // files) or an unreadable tree yields a disabled baseline that reports
    // nothing.
    let arch_baseline = route
        .depth
        .is_deliberate()
        .then(|| crate::arch_fitness::baseline(&options.project_root));

    // STEP-ATTRIBUTABLE-EVIDENCE BASELINE (the dead-summon fix). Snapshot the source
    // tree (path → size/mtime, bounded like `acceptance::source_files`) BEFORE this
    // step's doer turn(s), so a step whose turn NEVER ran (`!drove`) can only accept
    // on evidence attributable to THIS step — its own declared FileExists/FileContains
    // paths, or a source-tree delta since this snapshot — never on workspace-global
    // "any source exists" positives that an EARLIER step left on disk. Without this,
    // a base that died after step 1 wrote real source let steps 2..N fake-tick Done
    // over turns that never ran, converging into a fake clean delivery.
    let step_tree_baseline = source_tree_snapshot(&options.project_root);

    // RED→GREEN PRE-STATE CHECKPOINT. A step that declares
    // [`plan_state::EvidenceContract::TestFailsThenPasses`] is claiming its named test
    // could NOT have passed before it ran — a claim that is only falsifiable if we can
    // GO BACK and try. So snapshot the pre-state now, while it still exists.
    //
    // Taken ONLY for a step that declared the contract: no other step pays a shadow
    // commit for a question it never asked. Fail-open — `None` (no `git`, no shadow
    // repo) simply means the red half cannot be checked, and the contract degrades to
    // the ordinary `TestPasses` bar (see `test_red_green_outcome`).
    let red_green_pre = step
        .evidence
        .iter()
        .any(|e| matches!(e, plan_state::EvidenceContract::TestFailsThenPasses { .. }))
        .then(|| {
            crate::checkpoint::create_checkpoint(
                &options.project_root,
                &format!("{}{}", crate::checkpoint::RED_GREEN_PRE_PREFIX, step.id),
            )
        })
        .flatten();

    let mut drove = false;
    let mut last_reply = String::new();
    let mut last_fail_errors: Vec<String> = Vec::new();
    let mut base_agents = crate::bg_agents::BaseAgentObservation::default();
    // Failure whose recalled fix is embedded in the NEXT re-drive. It is
    // committed only after that directive was actually accepted by the host.
    let mut pending_fix_error: Option<String> = None;
    let mut pending_fix_verifiers: Vec<String> = Vec::new();
    let mut blocker_detector = crate::blocker::StuckDetector::default();
    let mut last_failure_snapshot: Option<SourceTreeSnapshot> = None;
    for round in 0..=max_fix_rounds {
        let mut committed_attempt: Option<String> = None;
        let mut committed_attempt_verifiers: Vec<String> = Vec::new();
        // Wall-clock ceiling (graceful): an EXTRA fix round past the budget is
        // abandoned — round 0 (the actual work) always runs, only the re-drives are
        // skipped, so a build can't keep grinding minute-long summon turns past its
        // deadline (the doc'd hard ceiling). The step stays unaccepted → the caller
        // marks it Blocked + the final gate / hard-gate still own reality.
        if round > 0 && std::time::Instant::now() >= deadline {
            events.emit(EngineEvent::Note(
                "team · time budget reached — skipping further fix rounds on this step \
                 (raise UMADEV_RUN_BUDGET_SECS for more)"
                    .to_string(),
            ));
            break;
        }
        // `instruction` carries the focused task on round 0 and is rewritten with
        // the failing acceptance evidence on each re-drive (see the loop tail).
        // FIX #7: emit a periodic in-turn heartbeat note while this (possibly
        // minute-long) doer turn runs, so a long ACTIVE step visibly progresses on
        // the TUI instead of a static spinner. The heartbeat races the summon future
        // via `tokio::select!` — no refactor of the summon internals; it only adds a
        // Note every HEARTBEAT_SECS until the turn settles.
        let summoned = with_step_heartbeat(
            events,
            &step.title,
            director::summon(
                session,
                options,
                events,
                seat_id,
                &instruction,
                director::SummonMode::Serial,
                deadline,
            ),
        )
        .await;
        base_agents.merge(summoned.base_agents.clone());
        if summoned.done {
            drove = true;
        }
        // DEFINITE no-turn (the dead-summon fix, primary guard): the doer directive
        // could not even be SENT — the base process is gone, so NO turn ran for this
        // round and no re-drive can reach the brain either. Stop the step immediately
        // and honestly: the caller marks it Blocked (for ANY step, not just the
        // first). Verifying acceptance here would read workspace-global evidence an
        // EARLIER step left on disk and fake-tick this step Done over work that never
        // ran. Distinct from a hung-but-productive turn (`done == false` after a real
        // send), which still gets verified below — its work may be real.
        if summoned.send_failed {
            events.emit(EngineEvent::Note(format!(
                "team · step '{}' — base session unreachable (the doer directive could \
                 not be sent); marking the step blocked",
                step.title
            )));
            return StepOutcome {
                accepted: false,
                reply: last_reply,
                drove,
                made_progress: false,
                unavailable: true,
                base_agents,
                gap_evidence: vec![
                    "base session unreachable — the step's doer directive could not be \
                     sent, so no turn ever ran for this step"
                        .to_string(),
                ],
            };
        }
        // The exact knowledge receipt exists only when the doer directive really
        // reached the host. Keep it armed across every mechanical verifier and
        // floor amendment below; cancellation/error before settlement drops it as
        // Unknown rather than guessing reward or penalty.
        let memory_receipt = summoned.memory_receipt;
        let skill_receipt = summoned.skill_receipt;
        if let Some(failure) = pending_fix_error.take() {
            committed_attempt_verifiers = std::mem::take(&mut pending_fix_verifiers);
            committed_attempt =
                crate::lessons::commit_pitfall_fix_attempt(&options.project_root, &failure);
        }
        if !summoned.text.trim().is_empty() {
            last_reply = summoned.text.clone();
        }
        // Verify against THIS step's acceptance on the deterministic floor.
        let mut verdict = verify_step_acceptance(
            session,
            options,
            events,
            route,
            step,
            red_green_pre.as_deref(),
        )
        .await;
        // TEST-INTEGRITY FLOOR (UD-QA-001). Compare the test surface to the
        // pre-step baseline. If the doer gamed the tests to fake a pass (deleted a
        // test, stripped assertions, added a skip/xfail/ignore marker, baked the
        // impl's output into an assertion, or weakened the harness/test command),
        // the step's passing test signal is NOT trusted: fold the typed,
        // file-naming findings into the verdict as blocking evidence so the SAME
        // bounded re-drive that handles any failing acceptance fixes the cause.
        // Deterministic + part of the floor (not an advisory critic); fail-open
        // (no baseline / unreadable tree → no findings); bounded by `max_fix_rounds`.
        let integrity = crate::test_integrity::check(&options.project_root, Some(&test_baseline));
        if !integrity.is_empty() {
            verdict.accepted = false;
            for finding in &integrity {
                events.emit(EngineEvent::Note(format!("floor · {finding}")));
            }
            verdict.evidence.extend(integrity);
        }
        // ARCHITECTURE-FITNESS FLOOR (UD-CODE-006, spec §3.6). Compare
        // the tree to the pre-step baseline: a NEW source file over the line
        // ceiling / a touched file GROWN past it (god-file, UD-CODE-006a) and an
        // import edge violating the architecture doc's declared layering
        // (UD-CODE-006b) are BLOCKING — folded into the verdict so the SAME
        // bounded re-drive that handles any failing acceptance fixes the cause
        // (split the file / invert the dependency). Duplicated added code
        // (UD-CODE-006c) and comment narration (UD-CODE-006d) are ADVISORY —
        // the floor has no advisory channel, so they surface as Notes and never
        // touch the verdict. Deterministic
        // + fail-open (no arch doc / huge repo / unreadable tree → no findings);
        // bounded by the same `max_fix_rounds` as every other step finding.
        if let Some(arch_before) = arch_baseline.as_ref() {
            let arch = crate::arch_fitness::arch_fitness_findings_since(
                &options.project_root,
                &options.effective_slug(),
                arch_before,
            );
            for f in arch {
                if f.blocking {
                    verdict.accepted = false;
                    events.emit(EngineEvent::Note(format!("floor · {}", f.message)));
                    verdict.evidence.push(f.message);
                } else {
                    events.emit(EngineEvent::Note(format!("advisory · {}", f.message)));
                }
            }
        }
        // Preserve observable event boundaries: every failed tool execution and
        // every deterministic acceptance finding is an independent episode.
        // `capture_turn_pitfalls` feeds them one by one; duplicate lines inside a
        // single event are still collapsed by the capture layer. Positive
        // evidence is not a pitfall feed.
        let mut round_pitfalls = summoned.pitfalls.clone();
        if !verdict.accepted {
            if let Some(raw_log) = verdict
                .raw_log
                .as_ref()
                .filter(|log| !log.trim().is_empty())
            {
                // `evidence` is often only "test: FAILED (exit 101)", too thin
                // to classify. The bounded raw log carries the compiler/test root
                // cause and represents this one deterministic verification event.
                round_pitfalls.push(raw_log.clone());
            } else {
                round_pitfalls.extend(verdict.evidence.iter().cloned());
            }
        }
        capture_turn_pitfalls(options, events, &round_pitfalls);
        if let Some(receipt) = memory_receipt {
            let _ = receipt.settle(memory_outcome_for_step_verdict(&verdict));
        }
        if let Some(receipt) = skill_receipt {
            let _ = receipt.settle(skill_outcome_for_step_verdict(&verdict));
        }
        // MEDIUM #3 — a dead/hung summon turn that never actually ran (`!drove`) must
        // not "complete" a Build step on a NEUTRAL-SKIP acceptance (an unavailable
        // check / a TurnSettled free pass). Require REAL evidence: either the doer
        // turn actually ran, OR — belt+suspenders for the dead-summon fix — evidence
        // ATTRIBUTABLE TO THIS STEP: its own declared FileExists/FileContains paths on
        // disk, or a source-tree delta since this step's start. A workspace-global
        // positive (`verdict.has_positive_evidence` via "any source exists") is NOT
        // enough when no turn ran: after an earlier step wrote source and the base
        // died, that global signal held for every later step and fake-ticked them
        // Done. Without step-attributable evidence, a build step over a dead session
        // is honestly left unaccepted (→ the caller marks it Blocked).
        if verdict.accepted
            && (drove
                || step_attributable_evidence(&options.project_root, step, &step_tree_baseline))
        {
            // SELF-EVOLUTION (a SIDE EFFECT of the PASS verdict; best-effort +
            // fail-open, never changes the outcome below): the recalled lessons were
            // in front of the doer and the step PASSED — reward their trust. If this
            // pass RECOVERED from a recorded failing round, that IS proof the pitfall's
            // recorded fix held: reward its dev-error trust and mark it resolved.
            if let Some(attempt) = committed_attempt.as_deref() {
                let _ = crate::lessons::settle_pitfall_fix_attempt(
                    &options.project_root,
                    attempt,
                    repair_attempt_result_for_verdict(&verdict, &committed_attempt_verifiers),
                );
            }
            // FIRST-PASS ACCEPTANCE signal (advisory self-evolution, fail-open):
            // this proposal PASSED verification — record whether it did so on the
            // FIRST attempt (round 0, no rework) or only after one or more fix
            // rounds. Telemetry only; it never affects this step's outcome below.
            record_step_first_pass(options, events, route, step, round == 0);
            return StepOutcome {
                accepted: true,
                reply: last_reply,
                drove,
                // A Build step makes real progress when its turn actually ran or the
                // floor positively confirmed real work — exactly the (drove ||
                // has_positive_evidence) condition that let it accept here.
                made_progress: true,
                unavailable: false,
                base_agents,
                gap_evidence: Vec::new(), // accepted → no gap to re-plan around
            };
        }
        // SELF-EVOLUTION (a SIDE EFFECT of this FAILING verdict — never a driver of
        // it, never touches loop control or the verdict): the recalled lessons were in
        // front of the doer and the step did NOT pass. Penalise their trust + the
        // dev-error pitfall that matches this failure, and — ONLY on a TRUE recurrence
        // — ask the brain (read-only fork, fail-open, at most once per signature per
        // run) for a higher-level corrective strategy. All best-effort: a store or
        // consult error NEVER fails the step.
        let evidence_line = verdict.evidence_line();
        let failure_detail = verdict.failure_detail();
        last_fail_errors = verdict.evidence.clone();
        let current_failure_snapshot = source_tree_snapshot(&options.project_root);
        let workspace_progress = last_failure_snapshot
            .as_ref()
            .is_some_and(|previous| previous != &current_failure_snapshot);
        last_failure_snapshot = Some(current_failure_snapshot);
        let assessment = blocker_detector.assess(
            &failure_detail,
            &step.criterion_label(),
            true,
            workspace_progress,
        );
        events.emit(EngineEvent::Note(format!(
            "team · blocker {}/{} · {} · unchanged repeat {}",
            assessment.diagnosis.class.as_str(),
            assessment.diagnosis.fingerprint,
            assessment.disposition.as_str(),
            assessment.repeat_count,
        )));
        if let Some(attempt) = committed_attempt.as_deref() {
            let _ = crate::lessons::settle_pitfall_fix_attempt(
                &options.project_root,
                attempt,
                repair_attempt_result_for_verdict(&verdict, &committed_attempt_verifiers),
            );
        }
        crate::self_evolve::reflect_on_recurring_failure(
            session,
            &options.project_root,
            events,
            &failure_detail,
            reflected,
        )
        .await;
        if assessment.disposition == crate::blocker::BlockerDisposition::Escalate {
            last_fail_errors.push(format!(
                "stuck detector: blocker `{}` repeated {} times without a source-tree change",
                assessment.diagnosis.fingerprint, assessment.repeat_count
            ));
            break;
        }
        // Remember this round's failing evidence so a recovery on a LATER round can
        // reward + mark-resolved the pitfall whose recorded fix then holds.
        // Out of fix budget → leave the step unaccepted (the caller marks it Blocked
        // and the final gate still has the last word). Bounded — never an open grind.
        if round >= max_fix_rounds {
            break;
        }
        // Highest-precision FAILURE-TIME recall: prior lessons with the SAME error
        // signature ("you hit this N times before; here's what worked; it keeps
        // recurring") + any base-reflected strategy. Fingerprint-gated + abstaining, so
        // an unclassifiable failure injects nothing. Fail-open (empty string on a miss).
        let prior = crate::lessons::lessons_for_error(&options.project_root, &failure_detail);
        pending_fix_error = (!prior.is_empty()).then(|| failure_detail.clone());
        pending_fix_verifiers = if prior.is_empty() {
            Vec::new()
        } else {
            verdict.mechanical_build_test_failed_steps.clone()
        };
        // Fold this step's failing acceptance into the NEXT re-drive's directive so
        // the same seat fixes the cause with raw evidence, in the same session. The
        // overall-goal frame is re-prepended so a fix turn keeps the product context.
        let prior_reference = if prior.is_empty() {
            String::new()
        } else {
            render_project_learned_reference(
                umadev_knowledge::PromptReferenceKind::Pitfall,
                ".umadev/learned/_raw/dev-errors.jsonl",
                "exact_error_match",
                &prior,
            )
        };
        instruction = format!(
            "{}{} — {}\n\n## This step did not pass its acceptance check yet — fix the cause\n{}\n\n{}{prior_reference}\n\
             Edit the real files, run any build/test you need, and make this step's \
             acceptance ({}) actually pass.",
            step_goal_frame(options),
            step.title,
            route_focus_line(route),
            assessment.prompt_block(),
            evidence_line,
            step.criterion_label(),
        );
        // B1#2: thread the failing build/test output's BOUNDED verbatim tail into the
        // rework directive when the floor captured one — the brain adapts from the raw
        // compiler/test evidence, not only UmaDev's one-line distillation. Skipped
        // cleanly when no raw log exists (a file-exists gap, contract drift, …).
        if let Some(raw) = verdict.raw_log.as_deref() {
            instruction.push_str("\n\n## Raw failing build/test output (verbatim tail)\n```text\n");
            instruction.push_str(raw.trim_end());
            instruction.push_str("\n```");
        }
        // Re-recite the plan position on a fix re-drive too, so a long fix sequence
        // on one step stays anchored to the overall plan.
        if !plan_progress.trim().is_empty() {
            instruction.push_str("\n\n");
            instruction.push_str(plan_progress.trim());
        }
    }
    // FIRST-PASS ACCEPTANCE signal (advisory, fail-open): the cheap path never
    // passed verification — definitively NOT a first-pass. Only record when a real
    // doer turn actually ran (`drove`): a dead/hung session that produced no
    // proposal is an infrastructure miss, not a measurable cheap-path failure, so
    // it must not poison the rate.
    if drove {
        record_step_first_pass(options, events, route, step, false);
    }
    StepOutcome {
        accepted: false,
        reply: last_reply,
        drove,
        made_progress: false,
        unavailable: false,
        base_agents,
        // The last failing round's typed evidence — WHY this step could not pass its
        // acceptance — so a bounded re-plan can route around the diagnosed blocker.
        gap_evidence: last_fail_errors,
    }
}

/// Record this build step's FIRST-PASS acceptance outcome into UmaDev's measured
/// engineering-doctrine signal ([`crate::first_pass`]) and surface the running
/// rate as a visible advisory [`EngineEvent::Note`].
///
/// `first_pass` is `true` iff the step's deterministic acceptance passed on the
/// FIRST attempt (round 0, ZERO rework rounds). The outcome is recorded under BOTH
/// the doer-seat kind (the step-kind dimension) AND the route-class kind (the
/// route-class dimension), so both accumulate from one call. ADVISORY + FAIL-OPEN:
/// recording never changes the step's pass/fail outcome, the loop, or any gate —
/// it only feeds the visible metric + later nudges.
fn record_step_first_pass(
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    route: &RoutePlan,
    step: &plan_state::PlanStep,
    first_pass: bool,
) {
    for kind in [
        crate::first_pass::seat_kind(step.seat.role_id()),
        crate::first_pass::class_kind(route.class.as_str()),
    ] {
        crate::first_pass::record(&options.project_root, &kind, first_pass);
        // Surface the running rate so the signal is visible (only once a kind has
        // crossed the trusted min-sample threshold). Pure observation.
        if let Some(rate) = crate::first_pass::first_pass_rate(&options.project_root, &kind) {
            events.emit(EngineEvent::Note(format!(
                "signal · first-pass acceptance {kind}: {:.0}% (advisory; the floor still governs)",
                rate * 100.0
            )));
        }
    }
}

/// Record this run's SIZING-calibration outcome (the single-turn loop's entry point):
/// the router's PREDICTED size for `route` vs. the `actual` size the run settled at,
/// keyed by route-class ([`crate::sizing_calibration`]). A `None` route (the
/// backward-compatible no-route entry) records nothing.
///
/// ADVISORY + FAIL-OPEN: recording never changes the run's route, plan, the
/// deterministic floor, loop termination, or any gate — by the time the actual size is
/// known the route is long-decided. It only feeds the per-class calibration that informs
/// a FUTURE default (see [`crate::sizing_calibration::calibrated_default`]).
fn record_run_sizing(
    options: &RunOptions,
    route: Option<&RoutePlan>,
    actual: crate::sizing_calibration::SizeRank,
) {
    if let Some(r) = route {
        crate::sizing_calibration::record_route(&options.project_root, r, actual);
    }
}

/// Drive ONE Review step: fork the cross-review team (read-only) over the current
/// blackboard. A review step is clean only when every convened seat returns pass;
/// blocking findings fold into ONE bounded fix turn on the MAIN session (the doer
/// repairs), then we re-read. Returns a [`StepOutcome`].
///
/// HIGH #1 / MEDIUM #3: an EMPTY-team review (the route convened no seats — 0 actually
/// reviewed) is a NEUTRAL SKIP, NOT real progress: `made_progress == false`, so the
/// scheduler does NOT tick it `Done` over a review that never happened. A team that
/// actually convened (`seats > 0`) and accepted is real progress.
///
/// Wall-clock ceiling (graceful): the read-only fork review ALWAYS runs (it's cheap
/// and surfaces honest findings), but the minute-level main-session FIX turn it would
/// trigger is skipped once the budget is spent — the findings are then surfaced as an
/// honest note and left for the final gate / hard-gate, never silently grinding past
/// the deadline.
async fn drive_review_step(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    route: &RoutePlan,
    step: &plan_state::PlanStep,
    deadline: std::time::Instant,
) -> StepOutcome {
    let _ = step;
    // Wave 2 deliverable 3: size the review team from the ROUTE's seats (the seats
    // the router already chose for this turn), not from a re-derived requirement
    // classification. An empty route team → no cross-review (the floor stands).
    let review = director::review_with_seats(session, options, events, &route.team).await;
    if review.status() == ReviewStatus::Unavailable {
        let mut gaps = review.blocking;
        gaps.extend(
            review
                .unavailable
                .iter()
                .map(|item| format!("review unavailable: {item}")),
        );
        events.emit(EngineEvent::Note(
            "team · required review unavailable — the step cannot be marked clean".to_string(),
        ));
        return StepOutcome {
            accepted: true,
            reply: String::new(),
            drove: review.seats > 0,
            made_progress: false,
            unavailable: true,
            base_agents: crate::bg_agents::BaseAgentObservation::default(),
            gap_evidence: gaps,
        };
    }
    if review.status() == ReviewStatus::Pass {
        // A team actually convened (seats > 0) ⇒ real review progress; an empty team
        // (seats == 0) is a neutral skip that must NOT advance the done count.
        let reviewed = review.seats > 0;
        return StepOutcome {
            accepted: true,
            reply: String::new(),
            drove: reviewed,
            made_progress: reviewed,
            unavailable: false,
            base_agents: crate::bg_agents::BaseAgentObservation::default(),
            gap_evidence: Vec::new(), // review accepted → no gap
        };
    }
    // The team found blockers after the fix budget ended. Preserve those blockers
    // on the step instead of accepting an empty result and hoping a later pass finds
    // them again.
    if std::time::Instant::now() >= deadline {
        events.emit(EngineEvent::Note(
            "team · time budget reached — review findings left for the final gate \
             (raise UMADEV_RUN_BUDGET_SECS to repair them in this run)"
                .to_string(),
        ));
        return StepOutcome {
            accepted: true,
            reply: String::new(),
            drove: review.seats > 0,
            made_progress: false,
            unavailable: false,
            base_agents: crate::bg_agents::BaseAgentObservation::default(),
            gap_evidence: review.blocking,
        };
    }
    // The team found blocking issues — fold them into ONE bounded fix turn on the
    // main session, then require a clean re-review before marking the step clean.
    let mut body = String::new();
    for b in &review.blocking {
        body.push_str("- ");
        body.push_str(b);
        body.push('\n');
    }
    let directive = format!(
        "The review team flagged MUST-FIX issues in what was built so far. Fix EVERY one \
         now by editing the files directly — do not narrate, just apply the fixes and \
         re-run any build/test you already ran. Issues:\n{body}\n{}\nWhen all are fixed, end \
         your turn.",
        diagnosed_blockers_for_prompt(&review.blocking, "team review")
    );
    let rework = crate::continuous::drive_rework_turn_capturing(
        session, options, events, directive, deadline,
    )
    .await;
    let drove = rework.done;
    let base_agents = rework.base_agents;
    // Re-run the same required review after the fix. A failed review transport is
    // unavailable, and a residual semantic blocker remains a blocker; neither may
    // be rewritten into a clean pass.
    let recheck = director::review_with_seats(session, options, events, &route.team).await;
    if recheck.status() == ReviewStatus::Unavailable {
        let mut gaps = review.blocking;
        gaps.extend(recheck.blocking);
        gaps.extend(
            recheck
                .unavailable
                .into_iter()
                .map(|item| format!("review unavailable after rework: {item}")),
        );
        return StepOutcome {
            accepted: true,
            reply: String::new(),
            drove,
            made_progress: false,
            unavailable: true,
            base_agents,
            gap_evidence: gaps,
        };
    }
    if recheck.status() == ReviewStatus::Fail {
        events.emit(EngineEvent::Note(format!(
            "team · review step still has {} must-fix finding(s) after rework — preserving them as blockers",
            recheck.blocking.len()
        )));
        return StepOutcome {
            accepted: true,
            reply: String::new(),
            drove,
            made_progress: false,
            unavailable: false,
            base_agents,
            gap_evidence: recheck.blocking,
        };
    }
    // A team convened, raised findings, and a repair turn ran — real review progress
    // regardless of whether the repair turn fully settled (`drove`).
    StepOutcome {
        accepted: true,
        reply: String::new(),
        drove,
        made_progress: true,
        unavailable: false,
        base_agents,
        gap_evidence: Vec::new(),
    }
}

/// A bounded snapshot of the project's source tree — path → (byte size, mtime) —
/// taken at a build step's START (see [`source_tree_snapshot`]). The comparison
/// basis for the step-attributable-evidence check ([`step_attributable_evidence`]).
type SourceTreeSnapshot =
    std::collections::BTreeMap<std::path::PathBuf, (u64, Option<std::time::SystemTime>)>;

/// Snapshot the project's SOURCE tree (path → size/mtime) — the same bounded file
/// set as [`crate::acceptance::source_files`] (depth 8, 600 files, vendor/VCS dirs
/// skipped), so it is cheap (metadata reads only, no content hashing). Taken at a
/// build step's start; [`step_attributable_evidence`] compares against it to decide
/// whether anything REAL changed during the step. Fail-open: an unreadable entry is
/// simply absent from the snapshot.
fn source_tree_snapshot(root: &std::path::Path) -> SourceTreeSnapshot {
    crate::acceptance::source_files(root)
        .into_iter()
        .filter_map(|p| {
            let meta = std::fs::metadata(&p).ok()?;
            Some((p, (meta.len(), meta.modified().ok())))
        })
        .collect()
}

/// Evidence ATTRIBUTABLE TO THIS STEP — the bar a Build step must meet to accept
/// when its doer turn NEVER ran (`!drove`, the dead-summon guard). Two accepted
/// forms, both step-specific by construction:
///
/// 1. the step's OWN declared `FileExists` / `FileContains` evidence paths verify
///    on disk (the plan named this step's concrete deliverable and it is there), or
/// 2. the source tree CHANGED since this step's start (`baseline`,
///    [`source_tree_snapshot`]) — a turn that died mid-way after writing real files
///    still shows a delta, so hung-but-productive work is honoured.
///
/// Deliberately NEVER the bare workspace-global source-present positive ("any
/// source exists"): after an earlier step wrote source and the base process died,
/// that global signal held for every remaining step and fake-ticked them Done over
/// turns that never ran — a fake clean delivery. Fail-open in the honest direction:
/// no declared evidence + no delta → `false` (the step is left to the bounded fix
/// loop / Blocked, and the final gate still owns reality).
fn step_attributable_evidence(
    root: &std::path::Path,
    step: &plan_state::PlanStep,
    baseline: &SourceTreeSnapshot,
) -> bool {
    let declared = step.evidence.iter().any(|e| match e {
        plan_state::EvidenceContract::FileExists { path } => step_path_exists(root, path),
        plan_state::EvidenceContract::FileContains { path, needle } => matches!(
            file_contains_outcome(root, path, needle),
            EvidenceOutcome::Pass
        ),
        _ => false,
    });
    if declared {
        return true;
    }
    source_tree_snapshot(root) != *baseline
}

/// The outcome of verifying one step against its declared acceptance.
struct StepVerdict {
    /// Whether the step's deterministic acceptance check passed (or was a neutral
    /// skip — an unavailable check is NOT a failure, fail-open).
    accepted: bool,
    /// Whether `accepted` rests on POSITIVE evidence (a check that actually ran and
    /// passed — e.g. real source on disk, a green build) rather than a NEUTRAL SKIP
    /// (an unavailable check / a `TurnSettled` free pass). MEDIUM #3 uses this to
    /// refuse to mark a Build step `Done` over a doer turn that never ran: a neutral
    /// skip is fine when the turn DID run (the work just isn't mechanically
    /// checkable), but a dead/hung session that produced no turn must not "complete"
    /// a step on a free pass.
    has_positive_evidence: bool,
    /// IDs of project build/test/lint commands that actually ran and passed.
    /// Source/file/review proof and unavailable/skipped commands never appear.
    /// Repair settlement compares this set with the original failed-step set.
    mechanical_build_test_passed_steps: Vec<String>,
    /// Mechanical verifier step IDs that ran and failed in this round. When a
    /// recalled repair is re-driven, these IDs are carried to the next round;
    /// a green aggregate only validates the repair if every original failed
    /// step exists and passes again (deleting/renaming the script is Unknown).
    mechanical_build_test_failed_steps: Vec<String>,
    /// Concrete evidence lines from the check (failed-step names / drift / count).
    evidence: Vec<String>,
    /// B1#2 — a BOUNDED verbatim tail of the failing build/test output (last ~60
    /// lines, char-capped; see [`director::verify_build_test_raw`]), captured only
    /// when the verdict REJECTED on a real build/test failure. Threaded into the
    /// step's rework directive so the brain fixes from the raw compiler/test
    /// evidence, not only UmaDev's one-line distillation. `None` when the failure
    /// has no raw log (a file-exists gap, contract drift, …) — skipped cleanly.
    raw_log: Option<String>,
}

impl StepVerdict {
    /// A one-line evidence string for the fix directive / the reply.
    fn evidence_line(&self) -> String {
        if self.evidence.is_empty() {
            String::new()
        } else {
            self.evidence.join("; ")
        }
    }

    /// Highest-fidelity failure evidence for classification and exact repair
    /// attribution. The one-line acceptance summary is useful for display, but
    /// it often omits the compiler/test identity; the bounded raw log owns that
    /// identity whenever present.
    fn failure_detail(&self) -> String {
        self.raw_log
            .as_ref()
            .filter(|raw| !raw.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| self.evidence.join("\n"))
    }
}

/// Causal memory outcome for one sent doer directive. A neutral/skipped
/// acceptance is not evidence that retrieved knowledge helped, and a rejection
/// without concrete mechanical evidence is not evidence that it hurt.
fn memory_outcome_for_step_verdict(verdict: &StepVerdict) -> TurnOutcome {
    if verdict.accepted && verdict.has_positive_evidence {
        TurnOutcome::Pass
    } else if !verdict.accepted
        && (!verdict.evidence.is_empty()
            || verdict
                .raw_log
                .as_deref()
                .is_some_and(|raw| !raw.trim().is_empty()))
    {
        TurnOutcome::Fail
    } else {
        TurnOutcome::Unknown
    }
}

fn skill_outcome_for_step_verdict(verdict: &StepVerdict) -> crate::skills::SkillUseOutcome {
    match memory_outcome_for_step_verdict(verdict) {
        TurnOutcome::Pass => crate::skills::SkillUseOutcome::Pass,
        TurnOutcome::Fail => crate::skills::SkillUseOutcome::Fail,
        TurnOutcome::Unknown => crate::skills::SkillUseOutcome::Unknown,
    }
}

fn repair_attempt_result_for_verdict(
    verdict: &StepVerdict,
    expected_verifiers: &[String],
) -> crate::lessons::PitfallFixAttemptResult {
    let same_verifiers_passed = !expected_verifiers.is_empty()
        && expected_verifiers.iter().all(|expected| {
            verdict
                .mechanical_build_test_passed_steps
                .iter()
                .any(|actual| actual == expected)
        });
    if verdict.accepted && same_verifiers_passed {
        crate::lessons::PitfallFixAttemptResult::Passed
    } else if verdict.accepted {
        // SourcePresent/FileExists/FileContains/TurnSettled/review acceptance,
        // plus an unavailable or all-skipped build, cannot causally prove that
        // the attempted repair fixed its original mechanical failure.
        crate::lessons::PitfallFixAttemptResult::Unknown
    } else {
        let detail = verdict.failure_detail();
        if detail.trim().is_empty() {
            crate::lessons::PitfallFixAttemptResult::Unknown
        } else {
            crate::lessons::PitfallFixAttemptResult::VerificationFailed(detail)
        }
    }
}

fn build_test_passed_steps(result: &VerifyResult) -> Vec<String> {
    if !result.available || !result.passed {
        return Vec::new();
    }
    result
        .evidence
        .iter()
        // `verify_build_test_raw` emits one `step: ok` line per command that
        // really ran and omits skipped commands.
        .filter_map(|line| line.strip_suffix(": ok"))
        .map(str::to_string)
        .collect()
}

fn build_test_failed_steps(result: &VerifyResult) -> Vec<String> {
    result
        .evidence
        .iter()
        .filter_map(|line| line.split_once(": FAILED").map(|(step, _)| step))
        .map(str::to_string)
        .collect()
}

/// Verify one step against its `acceptance` on the DETERMINISTIC floor — the SAME
/// objective checkers the single-turn QC uses, selected by the step's
/// [`plan_state::AcceptanceSpec`]. Never an opinion: a `ReviewClean` step forks the
/// read-only review team; everything else reads disk / runs the real build. An
/// unavailable check (no manifest / no contract) is a NEUTRAL skip (accepted), never
/// a false failure (fail-open invariant). A `Build` step ALSO always honours the
/// source-present honesty floor so a step that "claimed done" but wrote nothing is
/// caught even when its declared acceptance is weaker.
async fn verify_step_acceptance(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    route: &RoutePlan,
    step: &plan_state::PlanStep,
    // The checkpoint id of this step's PRE-state, captured in `drive_build_step` ONLY
    // when the step declared a red→green contract — the state its named test must have
    // FAILED at. `None` (the step asked no such question, or `git` is unavailable) makes
    // the red half unaskable, and the contract degrades to the ordinary `TestPasses`
    // bar (fail-open — see `test_red_green_outcome`).
    pre_checkpoint: Option<&str>,
) -> StepVerdict {
    use plan_state::AcceptanceSpec as A;
    // HIGH #1 — the source-present honesty floor binds ANY step that CLAIMED to build
    // (a Build step), regardless of how weak its declared acceptance is. A step that
    // builds but whose acceptance is `ReviewClean`/`TurnSettled` would otherwise
    // accept over a tree with zero source — "claimed done, wrote nothing" slipping
    // through. So for a Build step, run the source floor FIRST and reject on a real
    // empty-tree miss; a positive source pass becomes the verdict's positive evidence
    // for the weaker criteria (review/turn-settled don't add their own).
    let is_build = step.kind == plan_state::StepKind::Build;
    // EVIDENCE CONTRACT (per-step, the #1 falsifiability upgrade): if the step
    // declares a TYPED, deterministically-checkable evidence contract, verify THAT
    // specific evidence on the floor — UmaDev owns + checks it; the base never
    // self-grades. An empty contract (the brain named nothing checkable, or a
    // persisted plan predates the field) falls through to the acceptance check below
    // (fail-open — a missing/uncheckable contract never blocks).
    if !step.evidence.is_empty() {
        return verify_step_evidence(options, events, step, is_build, pre_checkpoint).await;
    }
    // A DOC-producing Build step (PM/architect/designer authoring output/*.md) has no
    // CODE deliverable, so the source-present CODE floor (which excludes output/) would
    // falsely reject it and strand the plan — the SAME exemption `verify_step_evidence`
    // already applies on its path. Reached here only when the step has NO typed evidence
    // (the evidence path handled it above); its doc is governed by the doc-evidence floor
    // + the critic team + coverage, not this floor.
    let is_doc_seat = matches!(
        step.seat,
        crate::critics::Seat::ProductManager
            | crate::critics::Seat::Architect
            | crate::critics::Seat::UiuxDesigner
    );
    match &step.acceptance {
        A::SourcePresent if is_build && is_doc_seat => StepVerdict {
            accepted: true,
            has_positive_evidence: false,
            mechanical_build_test_passed_steps: Vec::new(),
            mechanical_build_test_failed_steps: Vec::new(),
            evidence: Vec::new(),
            raw_log: None,
        },
        A::SourcePresent => acceptance_from_verify(
            director::verify(options, events, VerifyKind::SourcePresent).await,
        ),
        // The designer seat's anti-theatre floor: the design system must be a REAL
        // `design-tokens.{json,css}` file on the blackboard. This check is MORE
        // specific than the generic source floor (it names the exact deliverable),
        // so it stands on its own — a present tokens file is positive evidence, an
        // absent one is an honest reject the director folds into a rework directive.
        // (Not layered with source-present: a `design-tokens.json`-only deliverable
        // is a non-source ext, so the source floor would falsely sink it.)
        A::DesignTokensPresent => acceptance_from_verify(
            director::verify(options, events, VerifyKind::DesignTokensPresent).await,
        ),
        // The STRONGER design contract (UD-CODE-007, spec §3.7): the token file is
        // not merely present but CONFORMANT — schema floor, WCAG contrast measured
        // on every declared (surface, on-surface) pair, the UI actually drawing from
        // the token set, and no AI-purple brand hue. Layered over the existence
        // check so an ABSENT tokens file still reads as "the designer produced
        // nothing" (the honest reject), while a PRESENT one is held to the contract
        // it implicitly claimed. Fail-open at both layers.
        A::DesignTokensConform => {
            let present = director::verify(options, events, VerifyKind::DesignTokensPresent).await;
            if present.available && !present.passed {
                return acceptance_from_verify(present);
            }
            acceptance_from_verify(
                director::verify(options, events, VerifyKind::DesignSystemConform).await,
            )
        }
        A::BuildTest => {
            // Honesty floor first: no source ⇒ fail regardless of a skipped build.
            let src = director::verify(options, events, VerifyKind::SourcePresent).await;
            if src.available && !src.passed {
                return acceptance_from_verify(src);
            }
            // A positive source floor (real source on disk) IS real evidence — carry it
            // forward so a Build step whose build/test check is a neutral skip (no
            // manifest) still counts as positive progress for the dead-summon guard.
            let src_positive = src.available && src.passed;
            // B1#2: run the log-capturing variant so a REJECTING verdict carries the
            // failing build/test output's bounded verbatim tail for the rework
            // directive. Same check, same events; a pass/skip yields no raw log.
            events.emit(EngineEvent::Note("team · verify build-test".to_string()));
            let (bt, raw) = director::verify_build_test_raw(options).await;
            let mechanical_build_test_passed_steps = build_test_passed_steps(&bt);
            let mechanical_build_test_failed_steps = build_test_failed_steps(&bt);
            let mut v = with_source_evidence(acceptance_from_verify(bt), src_positive);
            v.mechanical_build_test_passed_steps = mechanical_build_test_passed_steps;
            v.mechanical_build_test_failed_steps = mechanical_build_test_failed_steps;
            if !v.accepted {
                v.raw_log = raw;
            }
            v
        }
        A::Contract => {
            let src = director::verify(options, events, VerifyKind::SourcePresent).await;
            if src.available && !src.passed {
                return acceptance_from_verify(src);
            }
            let src_positive = src.available && src.passed;
            with_source_evidence(
                acceptance_from_verify(
                    director::verify(options, events, VerifyKind::Contract).await,
                ),
                src_positive,
            )
        }
        A::ReviewClean => {
            // HIGH #1: a Build step that declares ReviewClean STILL honours the
            // source-present floor — a build with no source can't be "review-clean".
            let src_positive = if is_build {
                let src = director::verify(options, events, VerifyKind::SourcePresent).await;
                if src.available && !src.passed {
                    return acceptance_from_verify(src); // empty tree ⇒ honest reject
                }
                src.available && src.passed
            } else {
                false
            };
            // Route-team-aware (deliverable 3): the review seats come from the route.
            let review = director::review_with_seats(session, options, events, &route.team).await;
            StepVerdict {
                // Advisory: a review-clean step is accepted unless a seat blocks —
                // and even then the final deterministic gate, not this verdict, owns
                // overall termination. No team convened ⇒ accept (nothing to review).
                accepted: !review.has_blocking(),
                // HIGH #1: positive evidence only when a seat ACTUALLY reviewed and
                // accepted (seats > 0), or a Build step's source floor positively
                // passed. An EMPTY-team ReviewClean (0 seats) is a NEUTRAL SKIP — it
                // must not count as real progress that marks a step Done over no work.
                has_positive_evidence: src_positive || (review.seats > 0 && !review.has_blocking()),
                mechanical_build_test_passed_steps: Vec::new(),
                mechanical_build_test_failed_steps: Vec::new(),
                evidence: review.blocking.clone(),
                raw_log: None,
            }
        }
        A::TurnSettled => {
            // The weakest criterion: the work turn settled. Still honour the
            // source-present honesty floor for a Build step so "claimed done, wrote
            // nothing" never slips through — and surface its positive source pass as
            // the verdict's evidence (the doer's turn ran AND wrote real files).
            if is_build {
                return acceptance_from_verify(
                    director::verify(options, events, VerifyKind::SourcePresent).await,
                );
            }
            // A non-Build TurnSettled (a Review step whose brain named nothing
            // checkable) is a NEUTRAL SKIP, not positive progress.
            StepVerdict {
                accepted: true,
                has_positive_evidence: false,
                mechanical_build_test_passed_steps: Vec::new(),
                mechanical_build_test_failed_steps: Vec::new(),
                evidence: Vec::new(),
                raw_log: None,
            }
        }
    }
}

/// OR a positive source-floor pass into an already-computed verdict's positive
/// evidence — so a Build step whose own check (build/test, contract) was a neutral
/// skip still records POSITIVE evidence when real source landed on disk. Used by the
/// BuildTest / Contract Build paths so the dead-summon guard (MEDIUM #3) treats real
/// source as real progress even when the richer check couldn't run.
fn with_source_evidence(mut v: StepVerdict, src_positive: bool) -> StepVerdict {
    v.has_positive_evidence = v.has_positive_evidence || src_positive;
    v
}

/// Fold a [`VerifyResult`] into a [`StepVerdict`]: an unavailable (skipped) check is
/// a NEUTRAL accept (fail-open — never a false failure); a passed check accepts; a
/// real failure rejects, carrying the evidence for the fix directive. `accepted` via
/// an unavailable check is a NEUTRAL SKIP (`has_positive_evidence == false`); only an
/// available+passed check is POSITIVE evidence (MEDIUM #3).
fn acceptance_from_verify(r: VerifyResult) -> StepVerdict {
    StepVerdict {
        accepted: !r.available || r.passed,
        has_positive_evidence: r.available && r.passed,
        mechanical_build_test_passed_steps: Vec::new(),
        mechanical_build_test_failed_steps: Vec::new(),
        evidence: if r.available && !r.passed {
            r.evidence
        } else {
            Vec::new()
        },
        raw_log: None,
    }
}

/// The outcome of checking ONE [`plan_state::EvidenceContract`] on the deterministic
/// floor — the per-contract atom [`verify_step_evidence`] aggregates.
enum EvidenceOutcome {
    /// The declared evidence was found / the check ran and passed — POSITIVE,
    /// falsifiable proof the step did its specific job.
    Pass,
    /// The check genuinely could not run (no manifest to build, no dev server to
    /// boot, no contract to compare) — a NEUTRAL skip, fail-open: an uncheckable
    /// contract never blocks a step (it just adds no positive evidence).
    Skip,
    /// The declared evidence is ABSENT / the check ran and FAILED — a typed,
    /// diagnosed gap line ("step declared X but Y") the verifier folds into rework.
    Gap(String),
}

/// Verify a step's TYPED EVIDENCE CONTRACT(s) on the DETERMINISTIC floor — the per-
/// step falsifiability check. Each declared [`plan_state::EvidenceContract`] is
/// checked SPECIFICALLY (this file exists / contains X, this named test is present +
/// passing, this route answers, the contract matches) by REUSING UmaDev's existing
/// evidence producers ([`director::verify`] / [`crate::acceptance`] /
/// [`crate::runtime_proof`] / `umadev-contract`) — no new probing infra. ALL declared
/// contracts must be satisfied (or be a neutral skip) for the step to accept; ANY
/// unsatisfied one leaves the step not-done and surfaces a typed evidence-gap line.
///
/// A `Build` step ALSO always honours the source-present honesty floor FIRST (so a
/// "claimed done, wrote nothing" step is caught even if its declared evidence is a
/// route/contract that happens to skip). Fail-open throughout: an uncheckable
/// contract is a neutral skip, never a false failure. The expensive producers (the
/// build/test floor, the runtime boot, the contract floor) are each run AT MOST ONCE
/// per step regardless of how many contracts reference them.
async fn verify_step_evidence(
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    step: &plan_state::PlanStep,
    is_build: bool,
    // This step's PRE-state checkpoint (see [`verify_step_acceptance`]) — what the
    // red→green contract rewinds to. `None` ⇒ the red half is unaskable (fail-open).
    pre_checkpoint: Option<&str>,
) -> StepVerdict {
    use plan_state::EvidenceContract as E;
    let root = &options.project_root;

    // Build-step honesty floor first (same bar the acceptance path enforces): a real
    // empty tree is an honest reject regardless of the declared evidence. A positive
    // source pass becomes baseline positive evidence (so a Build step whose only
    // contract is an unrunnable route still counts the real source it wrote).
    // A DOC-producing Build step (PM authoring the PRD, architect the architecture
    // doc, designer the UIUX doc) delivers a DOCUMENT under `output/`, not source code
    // — and `acceptance::source_files` deliberately excludes `output/`. Gating those
    // steps on the source-present CODE floor falsely rejects them ("declared
    // source-present but 0 source files") and collapses a greenfield build at its very
    // first (docs) step. So apply the floor ONLY to code-writing seats; a doc step's
    // own FileContains evidence (checked below) verifies its real deliverable.
    let is_doc_seat = matches!(
        step.seat,
        crate::critics::Seat::ProductManager
            | crate::critics::Seat::Architect
            | crate::critics::Seat::UiuxDesigner
    );
    let src = director::verify(options, events, VerifyKind::SourcePresent).await;
    if is_build && !is_doc_seat && src.available && !src.passed {
        return acceptance_from_verify(src);
    }
    let mut any_positive = is_build && !is_doc_seat && src.available && src.passed;

    // Precompute the SHARED, expensive producers at most once — only when a declared
    // contract actually needs them — so multiple contracts of the same kind don't
    // re-run the build / re-boot the app / re-diff the contract.
    let needs_build = step
        .evidence
        .iter()
        .any(|c| matches!(c, E::BuildClean | E::TestPasses { .. }));
    // B1#2: the log-capturing variant — a failing build/test keeps a bounded verbatim
    // tail of its raw output so the rework directive carries the raw evidence too.
    let (build, build_raw) = if needs_build {
        events.emit(EngineEvent::Note("team · verify build-test".to_string()));
        let (r, raw) = director::verify_build_test_raw(options).await;
        (Some(r), raw)
    } else {
        (None, None)
    };
    let needs_contract = step
        .evidence
        .iter()
        .any(|c| matches!(c, E::ContractMatches));
    let contract = if needs_contract {
        Some(director::verify(options, events, VerifyKind::Contract).await)
    } else {
        None
    };
    let needs_runtime = step
        .evidence
        .iter()
        .any(|c| matches!(c, E::RouteResponds { .. }));
    let runtime = if needs_runtime {
        let proof = crate::runtime_proof::run_runtime_proof(root).await;
        // FRESHNESS: a proof is a statement about the tree it ran against. If the
        // source moved between the probe and this instant (a concurrent write, a base
        // still finishing a file), the proof no longer describes the code we are about
        // to accept — so it is NOT evidence, and every route contract falls to a
        // neutral skip rather than passing on a stale green. Fail-open where we truly
        // know nothing: an UNSTAMPED proof (we never recorded which tree it described)
        // reads as fresh. A STAMPED proof whose tree can no longer be fingerprinted reads
        // as STALE — we do not claim a freshness we cannot establish, and the cost is one
        // re-verification, never a block (see `freshness::is_stale`).
        if proof.is_stale(root) {
            events.emit(EngineEvent::Note(
                "team · runtime proof discarded — the source changed after the probe ran, so it \
                 no longer describes this code (evidence produced before the last change to the \
                 code it describes is not evidence)"
                    .to_string(),
            ));
            None
        } else {
            Some(proof)
        }
    } else {
        None
    };

    let mut gaps: Vec<String> = Vec::new();
    for c in &step.evidence {
        let outcome = match c {
            E::SourcePresent => source_present_outcome(&src),
            E::FileExists { path } => file_exists_outcome(root, path),
            E::FileContains { path, needle } => file_contains_outcome(root, path, needle),
            E::TestPasses { name } => test_passes_outcome(root, name.as_deref(), build.as_ref()),
            E::TestFailsThenPasses { test } => {
                test_red_green_outcome(root, events, test, pre_checkpoint, build.as_ref()).await
            }
            E::BuildClean => build_clean_outcome(build.as_ref()),
            E::ContractMatches => contract_outcome(contract.as_ref()),
            E::RouteResponds {
                method,
                path,
                status,
            } => route_responds_outcome(runtime.as_ref(), method, path, *status),
            // M6: an under-specified brain evidence entry is ALWAYS an unmet gap — it
            // never auto-passes, so the step is held to a falsifiable bar instead of
            // silently degrading to the coarse "any source exists" default.
            E::Malformed { detail } => EvidenceOutcome::Gap(format!(
                "declared evidence is under-specified ({detail}) — name the concrete \
                 file/route/needle so this step has a falsifiable acceptance bar"
            )),
        };
        match outcome {
            EvidenceOutcome::Pass => any_positive = true,
            EvidenceOutcome::Skip => {}
            EvidenceOutcome::Gap(line) => gaps.push(line),
        }
    }

    let accepted = gaps.is_empty();
    let mechanical_build_test_passed_steps = build
        .as_ref()
        .map(build_test_passed_steps)
        .unwrap_or_default();
    let mechanical_build_test_failed_steps = build
        .as_ref()
        .map(build_test_failed_steps)
        .unwrap_or_default();
    StepVerdict {
        // Accept iff NO declared contract is unsatisfied. (A neutral-skip-only step on
        // a Build path still has the honesty floor's positive source evidence; a
        // non-Build step with only skips is a neutral accept, matching the acceptance
        // path's fail-open posture.)
        accepted,
        has_positive_evidence: any_positive,
        mechanical_build_test_passed_steps,
        mechanical_build_test_failed_steps,
        evidence: gaps,
        // The raw build/test tail rides only a REJECTING verdict (a failed build that
        // produced gaps); a clean/neutral verdict carries no log.
        raw_log: if accepted { None } else { build_raw },
    }
}

/// `SourcePresent` contract → reuse the already-computed source floor: positive when
/// real source exists, a typed gap on a confirmed empty tree, neutral otherwise.
fn source_present_outcome(src: &VerifyResult) -> EvidenceOutcome {
    if src.available && src.passed {
        EvidenceOutcome::Pass
    } else if src.available && !src.passed {
        EvidenceOutcome::Gap(
            "declared source-present evidence but no real source files exist on disk".to_string(),
        )
    } else {
        EvidenceOutcome::Skip
    }
}

/// `FileExists` contract → the named repo-relative path exists on disk.
fn file_exists_outcome(root: &std::path::Path, path: &str) -> EvidenceOutcome {
    if step_path_exists(root, path) {
        EvidenceOutcome::Pass
    } else {
        EvidenceOutcome::Gap(format!(
            "declared file-exists `{path}` but that path is absent on disk"
        ))
    }
}

/// `FileContains` contract → the named path exists AND its contents hold `needle`.
fn file_contains_outcome(root: &std::path::Path, path: &str, needle: &str) -> EvidenceOutcome {
    let full = root.join(path);
    match std::fs::read_to_string(&full) {
        Ok(content)
            if content
                .to_ascii_lowercase()
                .contains(&needle.to_ascii_lowercase()) =>
        {
            // Case-INSENSITIVE contains: the doc-evidence needles ("FR-"/"API") are markers;
            // a doc that writes "api"/"Api" (or a lower-case FR id) is real work, not a gap.
            EvidenceOutcome::Pass
        }
        Ok(_) => EvidenceOutcome::Gap(format!(
            "declared `{path}` contains \"{needle}\" but the file does not contain it"
        )),
        Err(_) => EvidenceOutcome::Gap(format!(
            "declared `{path}` contains \"{needle}\" but the file is absent/unreadable"
        )),
    }
}

/// `TestPasses` contract → the named test is PRESENT in the codebase (a source file
/// mentions it — falsifiable: a test that doesn't exist can't pass) AND the project's
/// real test floor is green. A `None` name degrades to "the suite passes". The build
/// floor being unavailable (no manifest) is a neutral skip for that half; the name-
/// presence half is independent of the toolchain (a pure source scan), so a declared
/// but absent named test is a gap even with no manifest.
fn test_passes_outcome(
    root: &std::path::Path,
    name: Option<&str>,
    build: Option<&VerifyResult>,
) -> EvidenceOutcome {
    // Name-presence half: a named test that appears NOWHERE in the source is absent —
    // a falsifiable gap regardless of whether the suite can run.
    if let Some(n) = name {
        let needle = n.trim();
        if needle.len() >= 3 && !source_mentions(root, needle) {
            return EvidenceOutcome::Gap(format!(
                "declared test \"{n}\" passes but no test by that name is present in the codebase"
            ));
        }
    }
    // Suite half: the real build/test floor must be green when it can run.
    match build {
        Some(b) if b.available && !b.passed => EvidenceOutcome::Gap(match name {
            Some(n) => format!("declared test \"{n}\" passes but the test floor is failing"),
            None => "declared the test suite passes but the test floor is failing".to_string(),
        }),
        Some(b) if b.available && b.passed => EvidenceOutcome::Pass,
        // No manifest / nothing to build: the suite half is a neutral skip. If a name
        // was given and present, that presence is still positive evidence.
        _ => {
            if name.is_some() {
                EvidenceOutcome::Pass
            } else {
                EvidenceOutcome::Skip
            }
        }
    }
}

/// `TestFailsThenPasses` contract → **RED→GREEN**: the named test must FAIL at the
/// step's pre-state and PASS at head. The falsifiable form of "test-first".
///
/// ## Why this check exists
/// `TestPasses` is satisfied by a test written AFTER the code it "checks". Such a test
/// asserts whatever the implementation already happens to do; it passes on its first
/// run and has never once demonstrated that it can detect the behaviour's ABSENCE. It
/// is a rubber stamp with a green tick. The ONE mechanical property that separates a
/// real test from a rubber stamp is that a real test **failed once** — before the
/// change that made it pass. That property is checkable, and this is the check.
///
/// ## How
/// 1. **Head, green half.** The test must be PRESENT in the source (a test that does
///    not exist cannot pass — and this also closes the "a filter that matches nothing
///    exits 0" hole that would otherwise let a runner report success for a test that
///    isn't there) and must PASS when run by name. A failing/absent test at head is a
///    gap, exactly as `TestPasses` would report.
/// 2. **Pre-state, red half.** Rewind the workspace to the step's pre-state in a
///    SCOPED, REVERSIBLE way ([`crate::checkpoint::begin_temp_rewind`] — the present is
///    snapshotted and anchored first, and restored on the way out even through a panic;
///    dependency trees / build caches are outside the shadow repo, so the source goes
///    back in time and the toolchain does not, which is what makes the rewound tree
///    runnable). Run the SAME single test there. It must FAIL — or not exist at all,
///    which is the same fact stated more strongly.
/// 3. Restore head. (Unconditional: `TempRewind` restores on drop.)
///
/// A test that was **already green at the pre-state** rejects the step, and says so
/// plainly. That is not a bug in the check; it is the entire point of it.
///
/// ## Fail-open (never block on our own inability to verify)
/// Every inconclusive edge — no pre-state checkpoint (no `git`/shadow repo), the rewind
/// could not be taken, no test runner on PATH, an unrecognised project, a timeout —
/// degrades to exactly the existing [`test_passes_outcome`] semantics. We hold the step
/// to the bar we CAN check, never to a verdict we could not reach. Only a positively
/// observed "it was already passing" is a finding.
///
/// Bounded: at most two runs of ONE named test (never the whole suite), each capped by
/// [`crate::verify::run_named_test`]'s own timeout.
async fn test_red_green_outcome(
    root: &std::path::Path,
    events: &Arc<dyn EventSink>,
    test: &str,
    pre_checkpoint: Option<&str>,
    build: Option<&VerifyResult>,
) -> EvidenceOutcome {
    use crate::verify::NamedTestOutcome as T;

    // ── Head: the green half ────────────────────────────────────────────────────
    // Presence first — it is toolchain-independent and it is what makes a
    // matches-nothing filter (which some runners exit 0 on) unable to fake a pass.
    let needle = test.trim();
    if needle.len() >= 3 && !source_mentions(root, needle) {
        return EvidenceOutcome::Gap(format!(
            "declared red→green on test \"{test}\" but no test by that name is present in the \
             codebase — write the test, watch it fail, then make it pass"
        ));
    }
    match crate::verify::run_named_test(root, needle).await {
        T::Failed => {
            return EvidenceOutcome::Gap(format!(
                "declared red→green on test \"{test}\" but it does NOT pass at head — the step \
                 is not done until its own test is green"
            ));
        }
        // We could not run it at all (no runner / unrecognised project / timeout). Fall
        // open to the ordinary named-test bar — never block on our own blindness.
        T::Unavailable => return test_passes_outcome(root, Some(needle), build),
        T::Passed => {}
    }

    // ── Pre-state: the red half ─────────────────────────────────────────────────
    // Without a pre-state we cannot ask whether the test could ever have failed. The
    // green half already held, which is exactly the `TestPasses` bar — accept there.
    let Some(pre) = pre_checkpoint else {
        return test_passes_outcome(root, Some(needle), build);
    };

    // THE PAST DOES NOT MOVE. The red half asks one question about one immutable
    // commit — "did this test fail AT `pre`?" — and the answer cannot change while the
    // step's bounded fix rounds re-drive the PRESENT. Asking it again each round would
    // pay a full rewind (two `reset --hard`s over the user's tracked tree) and a test
    // run for an answer we already hold, and each rewind is a window in which a kill
    // leaves the tree in the past. So: ask once, remember the answer for (root, pre,
    // test), and let every later round read it.
    let red = if let Some(cached) = red_half_cached(root, pre, needle) {
        cached
    } else {
        let Some(rewind) = crate::checkpoint::begin_temp_rewind(root, pre) else {
            return test_passes_outcome(root, Some(needle), build);
        };
        events.emit(EngineEvent::Note(format!(
            "team · red→green — replaying test \"{needle}\" against this step's pre-state (a test \
             that never failed has never proven it can detect the bug)"
        )));
        // Inside the rewound window the user's tracked source is IN THE PAST. Keep this
        // SHORT (a dedicated, much smaller budget than the ordinary named-test one),
        // take no early return that skips the restore (`TempRewind` restores on drop
        // regardless — this is belt and braces), and let the crash marker cover the one
        // thing neither can: a kill.
        let observed = if source_mentions(root, needle) {
            crate::verify::run_named_test_bounded(
                root,
                needle,
                crate::verify::RED_TEST_TIMEOUT_SECS,
            )
            .await
        } else {
            // The test did not exist before this step — it could not possibly have
            // passed. The strongest possible red, and free.
            T::Failed
        };
        if !rewind.restore() {
            // THE TREE IS IN THE PAST AND WE COULD NOT PUT IT BACK. This used to be a
            // bare `tracing::warn!` — a line in a log FILE under the TUI — while the
            // schedule went right on driving further steps, writing new code on top of a
            // source tree reverted to an earlier state. `checkpoint::restore` has already
            // raised the workspace notice + the in-the-past signal for this root; SAY it
            // here too, on the surface the user is actually looking at. The scheduler
            // reads the same signal and stops (see `halt_if_workspace_in_past`).
            events.emit(EngineEvent::Note(
                umadev_i18n::tl("checkpoint.workspace_in_past_halt").to_string(),
            ));
        }
        red_half_remember(root, pre, needle, observed);
        observed
    };

    match red_half_verdict(test, red) {
        Some(outcome) => outcome,
        // Could not run it in the rewound tree (a toolchain that needs the new
        // dependencies, a timeout). Inconclusive → fall open to the green-half bar.
        None => test_passes_outcome(root, Some(needle), build),
    }
}

/// The memo of red-half observations, keyed by `(workspace, pre-state commit, test)`.
///
/// Sound because the key pins an IMMUTABLE fact: `pre` is a commit id (a specific past),
/// and the question is about that past alone. A different step takes a different
/// pre-state checkpoint, so it gets a different key and its own observation. Bounded —
/// past [`MAX_RED_HALF_MEMO`] entries the memo is dropped wholesale (a memo is an
/// optimisation; losing it costs a rewind, never a wrong answer).
static RED_HALF_MEMO: std::sync::Mutex<
    Option<std::collections::HashMap<String, crate::verify::NamedTestOutcome>>,
> = std::sync::Mutex::new(None);

/// Cap on the red-half memo before it is cleared wholesale.
const MAX_RED_HALF_MEMO: usize = 256;

/// The memo key — the workspace, the immutable pre-state, and the test.
fn red_half_key(root: &std::path::Path, pre: &str, test: &str) -> String {
    format!("{}\u{1f}{pre}\u{1f}{test}", root.display())
}

/// A previously observed red half for this exact `(root, pre, test)`, if any.
/// Fail-open: a poisoned lock simply misses (we re-run the rewind).
fn red_half_cached(
    root: &std::path::Path,
    pre: &str,
    test: &str,
) -> Option<crate::verify::NamedTestOutcome> {
    let guard = RED_HALF_MEMO.lock().ok()?;
    guard.as_ref()?.get(&red_half_key(root, pre, test)).copied()
}

/// Remember a red-half observation — but ONLY a conclusive one.
///
/// `Passed` / `Failed` are immutable FACTS ABOUT THE PAST: at commit `pre`, that test
/// did (or did not) fail. That past does not change, so memoizing it is sound and a
/// later fix round of the same step can reuse it.
///
/// `Unavailable` is not a fact about the past at all — it is a fact about OUR TOOLING at
/// one moment (a timeout, a toolchain that needed dependencies the pre-state did not
/// have, a transient runner failure). Memoizing it would freeze one transient miss into
/// the answer for `(root, pre, test)` for the whole process: every later fix round of
/// that step would skip the rewind, read the cached `Unavailable`, and fall open to the
/// plain `TestPasses` bar — permanently downgrading the red→green contract on the basis
/// of a single flake. So a non-verdict is never cached; we simply pay for the rewind
/// again and get a real answer.
///
/// Fail-open: a poisoned lock simply does not memoize.
fn red_half_remember(
    root: &std::path::Path,
    pre: &str,
    test: &str,
    outcome: crate::verify::NamedTestOutcome,
) {
    if matches!(outcome, crate::verify::NamedTestOutcome::Unavailable) {
        return;
    }
    let Ok(mut guard) = RED_HALF_MEMO.lock() else {
        return;
    };
    let memo = guard.get_or_insert_with(std::collections::HashMap::new);
    if memo.len() >= MAX_RED_HALF_MEMO {
        memo.clear();
    }
    memo.insert(red_half_key(root, pre, test), outcome);
}

/// The RED half's verdict, given how the named test behaved at the step's PRE-state.
/// Pure — the decision, separated from the IO that produces it (see
/// [`test_red_green_outcome`], which has already established that the test is present
/// and GREEN at head before asking this).
///
/// `None` ⇒ **inconclusive**: the caller must fall open to the ordinary `TestPasses`
/// bar rather than reach a verdict it could not support.
fn red_half_verdict(test: &str, red: crate::verify::NamedTestOutcome) -> Option<EvidenceOutcome> {
    use crate::verify::NamedTestOutcome as T;
    match red {
        // The step's test was RED before it ran and is GREEN now. That is a test.
        T::Failed => Some(EvidenceOutcome::Pass),
        // THE FINDING. The test passed BEFORE the step's work existed, so it cannot be
        // asserting that work — it was written to match code that was already there.
        T::Passed => Some(EvidenceOutcome::Gap(format!(
            "test \"{test}\" ALREADY PASSED at this step's pre-state — it was written after (or \
             around) the code, so it has never demonstrated that it can detect the behaviour's \
             absence. Make it a real test: assert the behaviour this step is supposed to add, \
             confirm it FAILS without that code, then make it pass"
        ))),
        // We could not run it in the rewound tree — inconclusive, never a verdict.
        T::Unavailable => None,
    }
}

/// `BuildClean` contract → reuse the already-run build/test floor: green = positive,
/// red = a typed gap, no-manifest = a neutral skip.
fn build_clean_outcome(build: Option<&VerifyResult>) -> EvidenceOutcome {
    match build {
        Some(b) if b.available && b.passed => EvidenceOutcome::Pass,
        Some(b) if b.available && !b.passed => {
            let detail = if b.evidence.is_empty() {
                String::new()
            } else {
                format!(": {}", b.evidence.join("; "))
            };
            EvidenceOutcome::Gap(format!(
                "declared build-clean but the build/test failed{detail}"
            ))
        }
        _ => EvidenceOutcome::Skip,
    }
}

/// `ContractMatches` contract → reuse the already-run contract floor: clean = a
/// positive pass, drift = a typed gap, nothing-to-compare = a neutral skip.
fn contract_outcome(contract: Option<&VerifyResult>) -> EvidenceOutcome {
    match contract {
        Some(c) if c.available && c.passed => EvidenceOutcome::Pass,
        Some(c) if c.available && !c.passed => {
            let detail = if c.evidence.is_empty() {
                String::new()
            } else {
                format!(": {}", c.evidence.join("; "))
            };
            EvidenceOutcome::Gap(format!(
                "declared contract-matches but the frontend↔backend contract drifted{detail}"
            ))
        }
        _ => EvidenceOutcome::Skip,
    }
}

/// `RouteResponds` contract → reuse the already-run runtime proof: if the app booted
/// (Verified), the named path must have answered with the expected status (`status ==
/// 0` ⇒ any non-error). A route that wasn't probed / answered wrong is a typed gap; a
/// runtime that could NOT be verified at all (no dev server / no curl) is a neutral
/// skip (fail-open — an unbootable app never blocks a step on this contract).
fn route_responds_outcome(
    runtime: Option<&crate::runtime_proof::RuntimeProof>,
    method: &str,
    path: &str,
    status: Option<u16>,
) -> EvidenceOutcome {
    let Some(proof) = runtime else {
        return EvidenceOutcome::Skip;
    };
    if !proof.status.is_verified() {
        // The app couldn't be booted/probed at all — neutral, not a false failure.
        return EvidenceOutcome::Skip;
    }
    let want = normalize_route(path);
    let Some(probe) = proof
        .routes
        .iter()
        .find(|r| normalize_route(&r.path) == want)
    else {
        return EvidenceOutcome::Gap(format!(
            "declared {method} {path} responds but that route was not among the probed routes"
        ));
    };
    // L2: `None` = any non-error response; `Some(code)` = require exactly `code`
    // (including a required error status like 401).
    let ok = match status {
        None => probe.ok,
        Some(want) => probe.status == want,
    };
    if ok {
        EvidenceOutcome::Pass
    } else {
        match status {
            None => EvidenceOutcome::Gap(format!(
                "declared {method} {path} responds OK but it returned status {}",
                probe.status
            )),
            Some(want) => EvidenceOutcome::Gap(format!(
                "declared {method} {path} responds {want} but it returned status {}",
                probe.status
            )),
        }
    }
}

/// Resolve whether a repo-relative `path` exists under `root`. A blank path is never
/// "present". Reads disk only; fail-open (a stat error ⇒ absent).
fn step_path_exists(root: &std::path::Path, path: &str) -> bool {
    let p = path.trim();
    !p.is_empty() && root.join(p).exists()
}

/// Normalise a route path for comparison: trim, ensure a single leading `/`, drop a
/// trailing `/` (except the root). So `api/users/` and `/api/users` compare equal.
fn normalize_route(path: &str) -> String {
    let t = path.trim();
    let t = t.strip_prefix('/').unwrap_or(t);
    let trimmed = t.trim_end_matches('/');
    format!("/{trimmed}")
}

/// Whether any of the project's source files mentions `needle` — the deterministic
/// "this named test actually exists" signal for [`test_passes_outcome`]. Reuses the
/// bounded source scan ([`crate::acceptance::source_files`]); fail-open (an unreadable
/// file is skipped). Bounded by the scan's own depth/file caps.
fn source_mentions(root: &std::path::Path, needle: &str) -> bool {
    crate::acceptance::source_files(root)
        .iter()
        .any(|f| std::fs::read_to_string(f).is_ok_and(|c| c.contains(needle)))
}

/// The final whole-build QC gate run once a step-driven plan has walked its DAG —
/// the SAME [`run_auto_qc`] pass the single-turn loop ends on, folded into ONE
/// bounded fix turn so a step-driven build is held to the identical objective floor.
/// Returns the fix turn's reply (empty when QC was already clean). Bounded by
/// [`MAX_QC_ROUNDS`]; fail-open throughout.
///
/// Wall-clock ceiling (graceful): the read-only QC READ ALWAYS runs (every iteration),
/// so the build is ALWAYS held to the objective floor even at the budget; only the
/// minute-level FIX TURN it would trigger is skipped once the deadline is spent (the
/// doc'd "hard ceiling" — the build could otherwise run several fix turns over budget
/// here). A residual finding is returned as a dirty outcome and becomes an honest
/// director failure; it is never delegated to a narrower source-only caller check.
/// The outcome of [`run_final_gate`]: the final fix-turn reply PLUS whether the gate
/// settled CLEAN. H1: the step-driven caller must AND `clean` into its finalize
/// decision — a build whose steps all ticked Done but whose final cross-cutting gate
/// (coverage / contract / runtime-proof / governance / fork review) stayed DIRTY must
/// NOT be finalized as a clean delivery (which would ship a full proof-pack/scorecard
/// disguising an incomplete build as success).
struct FinalGateOutcome {
    /// The last fix-turn's reply (empty when QC was already clean / no fix ran).
    reply: String,
    /// `true` only when the QC read came back clean within the bounded rounds;
    /// `false` when the gate settled with residual blocking findings (budget /
    /// deadline / dead session).
    clean: bool,
    /// The last objective blocking findings. Empty on a clean gate; retained on
    /// every dirty settle so the director's terminal failure carries evidence.
    blocking: Vec<String>,
}

#[allow(clippy::too_many_arguments)]
async fn run_final_gate(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    route: &RoutePlan,
    seed_reply: &str,
    deadline: std::time::Instant,
    // Optional structured CONTEXT front-loaded onto every fix directive. The
    // chat-build entry passes recalled knowledge text + exact memory identities;
    // `/run` passes an empty digest and creates no receipt.
    fix_context: &KnowledgeDigest,
    // OBSERVED-tool corroboration for the SEED reply: did a real build/test/lint runner
    // run producing `seed_reply`? Callers that can't observe it pass `false` (conservative
    // — round 0 then runs UmaDev's own read rather than trusting the seed's prose). Each
    // fix round below re-derives it from its OWN turn's observed tool calls.
    seed_ran_build_tool: bool,
) -> FinalGateOutcome {
    let mut last_reply = String::new();
    let mut last_blocking = Vec::new();
    // The incremental-verify signal seeds from the LAST step's reply (the steps just
    // ran the build/test); each fix round below then carries its own turn's reply.
    let mut verify_signal = seed_reply.to_string();
    // The observed-tool corroboration paired with `verify_signal`: seeds from the caller,
    // then tracks each fix turn's OWN observed run — so a fix turn's green claim can only
    // skip the read when THAT turn actually ran a runner.
    let mut verify_ran_build_tool = seed_ran_build_tool;
    // A fix turn's knowledge can be judged only by the NEXT QC read. Holding the
    // guard here makes cancellation, panic, or an absent next read settle Unknown.
    let mut pending_memory_receipt: Option<SentReceiptGuard> = None;
    let mut qc_blockers = crate::blocker::BlockerSetTracker::default();
    let mut last_qc_snapshot: Option<SourceTreeSnapshot> = None;
    for round in 0..MAX_QC_ROUNDS {
        // The QC read ALWAYS runs (it is read-only + cheap), so the build is held to
        // the objective floor every iteration — even at the budget. The final gate
        // sizes its review team from the ROUTE (deliverable 3). Pass the freshest reply
        // so the build/test read is skipped when the base already ran it green.
        let qc = run_auto_qc(
            session,
            options,
            events,
            Some(route),
            Some(verify_signal.as_str()),
            verify_ran_build_tool,
        )
        .await;
        if let Some(receipt) = pending_memory_receipt.take() {
            let outcome = if qc.is_clean() {
                TurnOutcome::Pass
            } else {
                TurnOutcome::Fail
            };
            let _ = receipt.settle(outcome);
        }
        if qc.is_clean() {
            return FinalGateOutcome {
                reply: last_reply,
                clean: true,
                blocking: Vec::new(),
            };
        }
        last_blocking = qc.blocking.clone();
        let current_qc_snapshot = source_tree_snapshot(&options.project_root);
        let workspace_progress = last_qc_snapshot
            .as_ref()
            .is_some_and(|previous| previous != &current_qc_snapshot);
        last_qc_snapshot = Some(current_qc_snapshot);
        let assessments = qc.assess_blockers(&mut qc_blockers, workspace_progress);
        emit_blocker_assessments(events, &assessments);
        if let Some(stuck) = assessments
            .iter()
            .find(|item| item.disposition == crate::blocker::BlockerDisposition::Escalate)
        {
            last_blocking.push(format!(
                "stuck detector: blocker `{}` repeated {} times without a source-tree change",
                stuck.diagnosis.fingerprint, stuck.repeat_count
            ));
            events.emit(EngineEvent::Note(
                "team · final QC stopped an unchanged repair loop and retained the evidence"
                    .to_string(),
            ));
            return FinalGateOutcome {
                reply: last_reply,
                clean: false,
                blocking: last_blocking,
            };
        }
        if round + 1 >= MAX_QC_ROUNDS {
            events.emit(EngineEvent::Note(
                "team · final QC reached its fix-round budget — stopping incomplete with residual evidence"
                    .to_string(),
            ));
            return FinalGateOutcome {
                reply: last_reply,
                clean: false,
                blocking: last_blocking,
            };
        }
        // Wall-clock ceiling (graceful): the QC READ above ran (the floor still bites),
        // but the minute-level FIX TURN it would trigger is skipped once the budget is
        // spent — the residual findings are retained in the dirty outcome rather than
        // driving more over-budget fix turns. This is
        // the doc'd "hard ceiling": the build can't keep grinding fix turns past it.
        if std::time::Instant::now() >= deadline {
            events.emit(EngineEvent::Note(
                "team · time budget reached — final QC findings retained as incomplete \
                 evidence (raise UMADEV_RUN_BUDGET_SECS for more fix rounds)"
                    .to_string(),
            ));
            return FinalGateOutcome {
                reply: last_reply,
                clean: false,
                blocking: last_blocking,
            };
        }
        // Fold the residual findings into ONE fix turn on the main session — with the
        // optional context prefix (knowledge + pitfalls) front-loaded for a chat-build.
        match drive_one_turn_with_memories(
            session,
            options,
            events,
            qc.fix_directive_with_assessments(&fix_context.text, &assessments),
            fix_context.memories.clone(),
            IdleBudget::from_env(),
            deadline,
        )
        .await
        {
            Ok(t) => {
                pending_memory_receipt = t.memory_receipt;
                verify_signal = t.text.clone();
                verify_ran_build_tool = t.ran_build_tool;
                last_reply = t.text;
            }
            // A dead/hung session → settle (fail-open). The gate did NOT clear, so the
            // residual findings stand and the caller must not finalize as clean.
            Err(_) => {
                return FinalGateOutcome {
                    reply: last_reply,
                    clean: false,
                    blocking: last_blocking,
                }
            }
        }
    }
    FinalGateOutcome {
        reply: last_reply,
        clean: false,
        blocking: last_blocking,
    }
}

/// **The full post-build QC pass for a CHAT-ORIGINATED build** — the architecture
/// unification (`became_build` chat surface earns the SAME flagship QC the explicit
/// `/run` path runs). A plain "做个落地页" typed in chat, whose base reacted by
/// writing files (`react_to_first_write` flipped it to a build), now gets:
///
/// 1. **governance / design-slop scan** (`run_auto_qc` runs `continuous::governance_scan`,
///    which is the same emoji-as-icon / hardcoded-color / AI-slop / purple-gradient
///    detection the `/run` path uses) + the build/test fact read + the deliberate
///    acceptance floor,
/// 2. **critic team review** (`run_auto_qc` → `review_with_seats` sized from
///    `route.team`, fork-based read-only critics),
/// 3. **bounded evidence-bearing rework** — blocking findings fold into ONE fix
///    directive per round, bounded by the internal QC-round limit + the wall-clock deadline,
///    fed back over the SAME continuous session (single-writer preserved), with the
///    recalled **knowledge digest + prior pitfalls** front-loaded (`post_build_rework_context`)
///    so the fix carries the team's commercial standards + memory,
/// 4. **usage + lessons capture** — every fix turn runs through the internal turn pump,
///    which records the token estimate (`/usage`) and distils failed-tool pitfalls
///    into the lessons KB (`/lessons`), so the chat build self-evolves like a `/run`.
///
/// Delegates the actual gate to the internal final-gate routine (the exact same bounded pass the
/// `/run` step-driver ends on) with the route's seats + the knowledge/lessons prefix,
/// so a chat build is held to the IDENTICAL floor as `/run` — not a re-implementation
/// that could drift. Returns the final fix-turn reply (empty when QC was already
/// clean). **Fail-open throughout**: a scan / fork / rework that can't run contributes
/// nothing and the build settles (a chat turn is never wedged by QC). The wall-clock
/// budget bounds the extra fix turns exactly like `/run`.
///
/// `seed_reply` is the build turn's own reply (so the incremental build/test read can
/// trust a green result the base already reported). Pure chat (no `became_build`) must
/// NOT call this — it stays on the light streaming path, fast.
pub async fn run_post_build_qc(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    route: &RoutePlan,
    seed_reply: &str,
) -> String {
    // The wall-clock budget bounds the EXTRA fix turns (graceful ceiling), exactly as
    // the `/run` loop reads it — a chat-build's post-QC rework can never run unbounded.
    let deadline = std::time::Instant::now() + run_budget();
    events.emit(EngineEvent::Note(
        "team · 构建完成 — 自动上设计/质量扫描 + 团队评审(和 /run 同一套验收)".to_string(),
    ));
    // Recall the commercial-engineering knowledge digest + the project's prior pitfalls
    // ONCE, to front-load onto every fix directive (deliverable 3). The chat session
    // opened firmware-light (no JIT knowledge), so this is where a chat-build's fix gets
    // the standards + memory. Fail-open: empty recall = the byte-for-byte plain directive.
    let context = post_build_rework_context(options);
    // The chat-build surface only needs the fix-turn reply; its caller does not gate a
    // finalize on the gate's clean-ness (the `/run` step path does — see H1). Seed
    // corroboration `false`: this entry has only the seed REPLY text, not an observed
    // run, so round 0 runs UmaDev's own build/test read rather than trusting the seed's
    // prose "it's green" — narration alone must not skip. Fix rounds re-derive per turn.
    run_final_gate(
        session, options, events, route, seed_reply, deadline, &context, false,
    )
    .await
    .reply
}

/// Build the CONTEXT prefix front-loaded onto a chat-build's post-QC fix directives —
/// the recalled commercial-engineering knowledge digest (`agentic_knowledge_digest`)
/// plus the project's prior pitfalls (`relevant_lessons_for_prompt`). The chat session
/// opens firmware-LIGHT (no JIT knowledge layer — that's the latency-saving default),
/// so a fix turn would otherwise repair blind; this restores the standards + memory at
/// the one point it matters (fixing real findings), without paying the full firmware
/// cost on every chat message. Pure + fully fail-open: each contributor swallows its
/// own errors into an empty string (the plain directive), never a panic or a block.
fn post_build_rework_context(options: &RunOptions) -> KnowledgeDigest {
    // Knowledge digest — small budget (3 chunks), matching the agentic light-turn size.
    // Keep retrieval pure while carrying exact identities through prompt
    // assembly. Receipt commit still happens only after a real host send.
    let mut digest = crate::phases::agentic_knowledge_digest_with_memories(
        &options.project_root,
        &options.requirement,
        3,
        false,
    );
    if !digest.text.trim().is_empty() {
        digest.text = digest.text.trim().to_string();
    }
    // Prior pitfalls on this project (recalled lessons) — what already bit us before.
    let lessons =
        crate::lessons::relevant_lessons_for_prompt(&options.project_root, &options.requirement);
    if !lessons.trim().is_empty() {
        if !digest.text.is_empty() {
            digest.text.push_str("\n\n");
        }
        digest.text.push_str(&render_project_learned_reference(
            umadev_knowledge::PromptReferenceKind::Lesson,
            ".umadev/learned/_raw",
            "post_build_rework",
            lessons.trim(),
        ));
    }
    digest
}

/// Distil a turn's failed-tool summaries into the lessons KB on the DEFAULT loop —
/// Wave 2 deliverable 4 (`/lessons` now learns from the director path, not just the
/// legacy runner). Emits a `[learned]` note so the user sees the agent remembering.
/// Fail-open: an empty feed is a no-op; capture never affects the schedule.
fn capture_turn_pitfalls(options: &RunOptions, events: &Arc<dyn EventSink>, pitfalls: &[String]) {
    if pitfalls.is_empty() {
        return;
    }
    // Each item is one failed ToolResult (or one deterministic acceptance
    // finding), hence one independent episode. Do not flatten the whole turn
    // into one capture batch: two separate failed executions of the same test
    // are two observations. `capture_dev_errors` still dedupes repeated lines
    // inside each individual event.
    let mut outcome = crate::lessons::PitfallCaptureOutcome::default();
    for pitfall in pitfalls {
        outcome.absorb(crate::lessons::capture_dev_errors_detailed(
            &options.project_root,
            std::slice::from_ref(pitfall),
            &options.effective_slug(),
            &options.requirement,
        ));
    }
    for note in outcome.progress_notes() {
        events.emit(EngineEvent::Note(note));
    }
}

/// How often (seconds) the in-turn step heartbeat emits a progress note while a long
/// doer turn is still running — frequent enough that a multi-minute step visibly
/// progresses on the TUI, infrequent enough not to spam the event stream.
const HEARTBEAT_SECS: u64 = 45;

/// Drive `fut` to completion while emitting a periodic [`EngineEvent::Note`] heartbeat
/// (FIX #7) so a long ACTIVE step shows live progress instead of a static spinner.
/// Delegates to [`with_step_heartbeat_every`] at the standard [`HEARTBEAT_SECS`]
/// cadence. Purely additive — it never changes the future's result, only surfaces
/// liveness.
async fn with_step_heartbeat<F>(events: &Arc<dyn EventSink>, title: &str, fut: F) -> F::Output
where
    F: std::future::Future,
{
    with_step_heartbeat_every(events, title, Duration::from_secs(HEARTBEAT_SECS), fut).await
}

/// [`with_step_heartbeat`] with an explicit interval (so a test can drive it with a
/// tiny window without a paused-clock harness). Each interval tick that fires before
/// the future resolves emits "step '{title}' still building ({elapsed}s)"; when the
/// future resolves, its value is returned. The first (immediate) interval tick is
/// consumed so a sub-interval step emits nothing.
async fn with_step_heartbeat_every<F>(
    events: &Arc<dyn EventSink>,
    title: &str,
    every: Duration,
    fut: F,
) -> F::Output
where
    F: std::future::Future,
{
    let started = std::time::Instant::now();
    let mut ticker = tokio::time::interval(every);
    // The first `tick()` resolves immediately; consume it so the first REAL heartbeat
    // fires one full interval in (a fast step never emits a heartbeat).
    ticker.tick().await;
    tokio::pin!(fut);
    loop {
        tokio::select! {
            out = &mut fut => return out,
            _ = ticker.tick() => {
                events.emit(EngineEvent::Note(format!(
                    "team · step '{title}' still building ({}s)…",
                    started.elapsed().as_secs()
                )));
            }
        }
    }
}

/// A short focus line appended to each step directive so the doer knows the overall
/// goal + the build's depth (proportional craft) without re-priming the whole
/// requirement every step (the base already holds it in the continuous session).
///
/// ROOT FIX (#6) — HARD-scope the per-turn ask to ONE step. Without this the base
/// (which holds the full goal in-session) builds the WHOLE project in step 1's turn,
/// and the plan sits at 0/N for an hour. The directive now explicitly constrains the
/// base to THIS step only, forbids implementing other steps (they are scheduled
/// separately and will fail their own acceptance if touched now), and tells it to
/// STOP as soon as this step's acceptance is met — which is what makes the DAG
/// actually walk step-by-step instead of one mega-turn. The base still has the full
/// goal in its session context; only the per-turn ASK is constrained.
fn route_focus_line(route: &RoutePlan) -> String {
    format!(
        "this is ONE step of a larger build (depth: {}) that is scheduled \
         step-by-step. Implement ONLY this step now, with real files on disk. Do NOT \
         implement any other part of the project in this turn — the other steps are \
         scheduled separately and will fail their own acceptance checks if you build \
         them here. STOP and end your turn as soon as THIS step's acceptance is met; \
         do not run ahead.",
        route.depth.as_str()
    )
}

/// Persist a `&Plan` (the step-driver holds `&mut Plan`, not `Option<Plan>`).
/// Best-effort + fail-open: a write error is ignored, never blocks the schedule.
fn persist_plan_ref(plan: &Plan, options: &RunOptions) {
    let _ = plan_state::save(plan, &options.project_root);
    record_artifact_versions(&options.project_root);
}

/// **BOUNDED RE-PLAN of a blocked subtree** (the coordinator's self-repair lever). A
/// plan step just ended [`StepStatus::Blocked`] — it could not pass its deterministic
/// acceptance after its bounded fix budget. Today that permanently strands its whole
/// dependent subtree (honestly reported, never repaired). This makes ONE bounded,
/// fail-open attempt to route around the blocker instead:
///
/// 1. **Trigger** — only when the blocked step actually STRANDS a Pending dependent
///    subtree ([`Plan::stranded_dependents`]). A leaf block strands nothing, so today's
///    honest strand is already correct → no consult, no budget spent.
/// 2. **Consult** — ONE read-only forked brain consult ([`crate::continuous::ForkConsult`]),
///    seeded with the TYPED gap evidence (WHY the step blocked — the acceptance /
///    evidence-contract gaps the floor already computed), the blocked step, and its
///    stranded subtree, asking for a REPLACEMENT sub-DAG (fresh steps that resolve /
///    route around the blocker).
/// 3. **Merge-through-normalized** — the returned sub-DAG is parsed
///    ([`plan_state::parse_brain_steps`]) and merged via [`Plan::merge_replan`], which
///    re-validates the WHOLE spliced plan through the same `normalized()` machinery
///    (dedup / dangling-dep strip / cycle-break / seat floors) and preserves the
///    survivors' statuses. The re-planned subtree faces the IDENTICAL acceptance floor.
/// 4. **Surface** — a merged re-plan re-emits [`EngineEvent::plan_posted`] (the plan
///    panel already renders it) + persists, so the user sees the revised plan.
///
/// **Strict bounds + fail-open.** `replanned` caps this at ONE attempt per run and is
/// consumed the moment the consult is committed (whether or not it helps), so a failed /
/// unhelpful consult can NEVER retry — re-planning never loops. EVERY failure mode
/// (already re-planned, no stranded dependents, budget spent, fork/consult failure,
/// unparseable reply, a sub-DAG that changes nothing) returns `false` and leaves the
/// plan EXACTLY as today's honest stranded-Blocked report expects. Returns `true` only
/// when a genuinely-new sub-DAG was merged.
#[allow(clippy::too_many_arguments)]
async fn attempt_replan_blocked_subtree(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    plan: &mut Plan,
    blocked_id: &str,
    blocked_title: &str,
    gap_evidence: &[String],
    replanned: &mut bool,
    deadline: std::time::Instant,
) -> bool {
    // BOUND: at most ONE re-plan per run (mirrors the `reflected` signature bound).
    if *replanned {
        return false;
    }
    // Only worth repairing when the block actually STRANDS a dependent subtree — a leaf
    // block has nothing to recover, so today's honest strand is already correct.
    let stranded = plan.stranded_dependents(blocked_id);
    if stranded.is_empty() {
        return false;
    }
    // Commit the single attempt NOW (whether or not it helps) so a failed / unhelpful
    // consult can never retry — re-planning must never loop; after one try honesty wins.
    *replanned = true;
    // Don't open a consult past the wall-clock budget (graceful ceiling).
    if std::time::Instant::now() >= deadline {
        return false;
    }

    // Build the read-only consult: the blocked step + WHY it blocked + the stranded
    // subtree, asking for a replacement sub-DAG that routes around / resolves it.
    let system = "You are a senior engineering director REPAIRING a build plan mid-flight. \
         ONE step has BLOCKED — it could not pass its deterministic acceptance after its \
         bounded fix budget — and its dependent steps are now STRANDED (they can never \
         become ready). Propose a small REPLACEMENT sub-DAG (1-5 steps) that ROUTES AROUND \
         or RESOLVES the blocker: use FRESH step ids (not already in the plan) that achieve \
         the stranded goals a DIFFERENT way, or split the blocked work into smaller, \
         separately-verifiable pieces. Do NOT re-emit the blocked step unchanged — give a \
         genuinely different route. A step MAY depend_on an EXISTING non-replaced step id \
         (e.g. an already-done scaffold or contract). Same vocab as the planner — \
         `seat`: product-manager|architect|uiux-designer|frontend-engineer|\
         backend-engineer|qa-engineer|security-engineer|devops-engineer; \
         `kind`: build|review; \
         `acceptance`: source-present|build-test|contract|design-tokens|review-clean; \
         `evidence` (preferred): an array of machine-checkable proofs, e.g. \
         {\"kind\":\"file-exists\",\"path\":\"src/foo.ts\"}, {\"kind\":\"build-clean\"}. \
         JSON shape: {\"steps\":[{\"id\":\"…\",\"title\":\"…\",\"seat\":\"…\",\"kind\":\"build\",\
         \"depends_on\":[],\"acceptance\":\"…\",\"evidence\":[…]}]}";
    let gap_line = if gap_evidence.is_empty() {
        "(the step produced no verifiable progress / no positive evidence)".to_string()
    } else {
        gap_evidence.join("; ")
    };
    let stranded_line = plan
        .steps
        .iter()
        .filter(|s| stranded.iter().any(|id| id == &s.id))
        .map(|s| format!("- {} ({}): {}", s.id, s.seat.role_id(), s.title))
        .collect::<Vec<_>>()
        .join("\n");
    let blocked_seat = plan
        .steps
        .iter()
        .find(|s| s.id == blocked_id)
        .map_or("?", |s| s.seat.role_id());
    let user = format!(
        "BLOCKED step: {blocked_id} — {blocked_title} (seat {blocked_seat}).\n\
         Why it blocked (typed gap evidence): {gap_line}.\n\
         STRANDED dependent steps needing a new route:\n{stranded_line}\n\n\
         Overall requirement:\n{}\n\n\
         Return ONE JSON object with the replacement sub-DAG.",
        options.requirement
    );

    // ONE read-only forked consult — same fail-open contract as every other consult: a
    // missing fork / offline brain / timeout / no JSON → `None` → fall back to honest strand.
    let fork = crate::continuous::fork_with_timeout(session).await;
    let consult = crate::continuous::ForkConsult::new(fork);
    let reply = consult.judge_json("replan", system, user).await;
    consult.end().await;
    let Some(reply) = reply else {
        return false; // consult failed / offline → today's honest strand
    };
    let new_steps = plan_state::parse_brain_steps(&reply);
    if new_steps.is_empty() {
        return false; // unparseable / empty sub-DAG → honest strand
    }

    // The subtree to replace = the blocked step + its stranded dependents.
    let mut replaced: std::collections::HashSet<String> = stranded.into_iter().collect();
    replaced.insert(blocked_id.to_string());
    // Merge-through-normalized; a no-op / invalid sub-DAG leaves the plan unchanged.
    if !plan.merge_replan(&replaced, new_steps) {
        return false; // nothing genuinely new → honest strand
    }

    // Merged: surface the REVISED plan (reuse the existing PlanPosted render), persist,
    // and re-sync the phase anchor. The honest stranded-report path below still runs at
    // the end of the schedule for anything the re-plan did NOT recover.
    events.emit(EngineEvent::Note(format!(
        "team · re-planned around a blocked step ({blocked_title}) — revised the plan to \
         route around it (bounded: one re-plan per run)"
    )));
    events.emit(EngineEvent::plan_posted(plan));
    persist_plan_ref(plan, options);
    sync_phase_from_plan(plan, options);
    true
}

/// MEDIUM #2 — after the schedule drains, honestly mark every Pending step that can
/// NEVER become ready because a dependency (directly or transitively) ended Blocked.
/// A Blocked step never flips to Done, so `ready_steps` (which requires every dep
/// Done) leaves its dependents Pending forever; left alone they'd silently vanish
/// from the conclusion while the run still reports Done. This flips each such step to
/// Blocked + emits a `PlanStepStatus`, so the checklist + the run's verdict are honest
/// about the skipped scope. Returns the count of newly-Blocked steps (0 = nothing
/// stranded). Pure + bounded (one fixpoint sweep per Pending step, ≤ steps²);
/// fail-open by construction (it only flips Pending→Blocked, never an error).
fn mark_unreachable_pending_blocked(plan: &mut Plan, events: &Arc<dyn EventSink>) -> usize {
    use std::collections::HashSet;
    // Seed the "blocked set" with the steps that are already Blocked.
    let mut blocked: HashSet<String> = plan
        .steps
        .iter()
        .filter(|s| s.status == StepStatus::Blocked)
        .map(|s| s.id.clone())
        .collect();
    if blocked.is_empty() {
        return 0; // nothing Blocked ⇒ nothing can be transitively stranded
    }
    // Fixpoint: a Pending step that depends on ANYTHING in the blocked set is itself
    // unreachable → add it and sweep again, until no new step joins (transitive
    // closure over `depends_on`). Bounded by the step count (each sweep adds ≥1 or
    // stops).
    loop {
        let mut grew = false;
        for s in &plan.steps {
            if s.status == StepStatus::Pending
                && !blocked.contains(&s.id)
                && s.depends_on.iter().any(|d| blocked.contains(d))
            {
                blocked.insert(s.id.clone());
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    // Flip the newly-unreachable Pending steps to Blocked + surface each transition.
    let to_block: Vec<(String, String)> = plan
        .steps
        .iter()
        .filter(|s| s.status == StepStatus::Pending && blocked.contains(&s.id))
        .map(|s| (s.id.clone(), s.title.clone()))
        .collect();
    for (id, title) in &to_block {
        if plan.mark(id, StepStatus::Blocked) {
            events.emit(EngineEvent::plan_step_status(
                id.clone(),
                title.clone(),
                StepStatus::Blocked,
            ));
        }
    }
    to_block.len()
}

// ───────────────────────────────────────────────────────────────────────────
// Plan progress surface (Wave 1) — emit PlanStepStatus + persist as the build
// moves. All helpers are fail-open no-ops when there is no plan.
// ───────────────────────────────────────────────────────────────────────────

/// Mark every ready BUILD step to `status` (typically `Active`) and emit a
/// [`EngineEvent::PlanStepStatus`] for each. No-op when there's no plan (fail-open).
/// Wave 1 surfaces the plan's motion around the existing single-turn build; Wave 2
/// drives each step independently via `summon`.
fn mark_ready_steps(plan: &mut Option<Plan>, events: &Arc<dyn EventSink>, status: StepStatus) {
    let Some(plan) = plan.as_mut() else {
        return;
    };
    // Snapshot the ready ids first (ready_steps borrows the plan immutably).
    let ready: Vec<(String, String)> = plan
        .ready_steps()
        .iter()
        .filter(|s| s.kind == plan_state::StepKind::Build)
        .map(|s| (s.id.clone(), s.title.clone()))
        .collect();
    for (id, title) in ready {
        if plan.mark(&id, status) {
            events.emit(EngineEvent::plan_step_status(id, title, status));
        }
    }
}

/// Emit a PERSISTENT per-step completion summary (a multi-line transcript Note) grouping
/// every plan step by its terminal status, so an incomplete / budget-reached / blocked
/// build still shows the user exactly which steps finished, which stalled, and which never
/// ran — the live plan panel is ephemeral and vanishes on settle. No-op on an empty plan.
/// Fail-open by construction (it only reads the plan + emits a note).
///
/// `step_reasons` maps a step id → the objective evidence recorded when that step
/// Blocked (`incomplete_evidence`). A Blocked step surfaces its OWN one-line WHY
/// directly under its marker line, so the cause reads per step instead of being
/// buried in the terminal residual-evidence string. Pass `&[]` when no per-step
/// evidence is available (an early floor-violation exit) — the summary then reads
/// exactly as before (fail-open).
fn emit_plan_completion_summary(
    plan: &Plan,
    events: &Arc<dyn EventSink>,
    step_reasons: &[(String, String)],
) {
    if plan.steps.is_empty() {
        return;
    }
    let count = |want: StepStatus| plan.steps.iter().filter(|s| s.status == want).count();
    let mut lines = vec![umadev_i18n::tlf(
        "plan.summary.header",
        &[
            &count(StepStatus::Done).to_string(),
            &count(StepStatus::Active).to_string(),
            &count(StepStatus::Blocked).to_string(),
            &count(StepStatus::Pending).to_string(),
        ],
    )];
    // One line per step, in plan order, marked by terminal status. Markers mirror the live
    // checklist ([√] done · [~] in progress · [ ] pending) and add [✗] for a blocked step.
    for s in &plan.steps {
        let marker = match s.status {
            StepStatus::Done => "[√]",
            StepStatus::Active => "[~]",
            StepStatus::Blocked => "[✗]",
            StepStatus::Pending => "[ ]",
        };
        lines.push(format!("  {marker} {} ({})", s.title, s.seat.role_id()));
        // A Blocked step carries its OWN concise WHY line so the reason is legible
        // per step. Fail-open: no recorded reason → the marker line alone.
        if s.status == StepStatus::Blocked {
            if let Some(reason) = blocked_step_reason(&s.id, step_reasons) {
                lines.push(format!("      ↳ {reason}"));
            }
        }
    }
    events.emit(EngineEvent::Note(lines.join("\n")));
}

/// The concise one-line WHY for a Blocked step: the FIRST evidence recorded for
/// this step id, stripped of the redundant ``step `<title>`[:]`` echo (the title
/// is already on the marker line) and bounded to one legible line. `None` when
/// nothing was recorded for the step, so the caller keeps the bare marker.
fn blocked_step_reason(step_id: &str, step_reasons: &[(String, String)]) -> Option<String> {
    const MAX_CHARS: usize = 200;
    let raw = step_reasons
        .iter()
        .find(|(id, _)| id == step_id)
        .map(|(_, reason)| reason.trim())
        .filter(|reason| !reason.is_empty())?;
    // Drop a leading ``step `<title>`[:]`` prefix; an unrecognised shape is kept
    // verbatim so a reason is never silently emptied.
    let concise = raw
        .strip_prefix("step `")
        .and_then(|rest| rest.split_once('`'))
        .map(|(_, after)| after.trim_start_matches(':').trim())
        .filter(|reason| !reason.is_empty())
        .unwrap_or(raw);
    Some(concise.chars().take(MAX_CHARS).collect())
}

/// A BOUNDED one-line description of a NON-success task-ledger readiness for the
/// terminal reason string.
///
/// The blocked task findings are ALREADY carried (bounded to 8 items by
/// [`qc_incomplete_reason`]) inside `plan_incomplete_reason`'s residual evidence,
/// so this names the KIND + COUNT only. The old `format!("… {readiness:?}")` dumped
/// the whole `Blocked([...])` vec via `Debug` — an UNBOUNDED second copy of the same
/// findings that dominated the transcript wall. Naming the count keeps the honest
/// "the ledger did not settle clean" signal without re-dumping the evidence.
fn summarize_ledger_readiness(readiness: &crate::task_lifecycle::RunReadiness) -> String {
    use crate::task_lifecycle::RunReadiness;
    match readiness {
        RunReadiness::Blocked(items) => {
            let n = items.iter().filter(|item| !item.trim().is_empty()).count();
            format!(
                "agent task ledger settled as Blocked ({n} task finding(s) — see residual evidence above)"
            )
        }
        // NotTracked / InProgress / (a non-clean) Succeeded carry no vec, so their
        // Debug is already a short bare word — safe to render directly.
        other => format!("agent task ledger did not settle clean ({other:?})"),
    }
}

/// Explain why a step-driven build cannot claim `Done`. Status counts come from
/// the persisted plan; verifier and final-QC details are bounded by
/// [`qc_incomplete_reason`]. Evidence from a step that a successful re-plan removed
/// is filtered out, so the terminal message only names residual blockers.
fn plan_incomplete_reason(
    plan: &Plan,
    budget_reached: bool,
    transition_limit_reached: bool,
    step_evidence: &[(String, String)],
    final_qc: &[String],
) -> String {
    let blocked = plan
        .steps
        .iter()
        .filter(|step| step.status == StepStatus::Blocked)
        .count();
    let unfinished = plan
        .steps
        .iter()
        .filter(|step| matches!(step.status, StepStatus::Active | StepStatus::Pending))
        .count();

    let mut reasons = Vec::new();
    if budget_reached {
        reasons.push("run time budget exhausted".to_string());
    }
    if transition_limit_reached {
        reasons.push(format!(
            "plan transition limit ({MAX_STEP_TRANSITIONS}) reached"
        ));
    }
    if blocked > 0 {
        reasons.push(format!("{blocked} plan step(s) blocked"));
    }
    if unfinished > 0 {
        reasons.push(format!("{unfinished} plan step(s) unfinished"));
    }
    if !final_qc.is_empty() {
        reasons.push("final QC retained blocking findings".to_string());
    }
    if reasons.is_empty() {
        reasons.push("the clean-delivery invariant was not satisfied".to_string());
    }

    let blocked_ids: std::collections::HashSet<&str> = plan
        .steps
        .iter()
        .filter(|step| step.status == StepStatus::Blocked)
        .map(|step| step.id.as_str())
        .collect();
    let mut evidence: Vec<String> = step_evidence
        .iter()
        .filter(|(step_id, _)| blocked_ids.contains(step_id.as_str()))
        .map(|(_, detail)| detail.clone())
        .collect();
    // A stranded dependent has no verifier gap of its own. Its persisted status +
    // title is still concrete evidence that the plan did not finish.
    for step in plan.steps.iter().filter(|step| {
        step.status == StepStatus::Blocked
            && !step_evidence.iter().any(|(step_id, _)| step_id == &step.id)
    }) {
        evidence.push(format!("step `{}` remains Blocked", step.title));
    }
    for step in plan
        .steps
        .iter()
        .filter(|step| matches!(step.status, StepStatus::Active | StepStatus::Pending))
    {
        evidence.push(format!("step `{}` remains {:?}", step.title, step.status));
    }
    evidence.extend(final_qc.iter().cloned());

    qc_incomplete_reason(&reasons.join("; "), &evidence)
}

/// On a clean settle: tick any non-Done step to `Done`, emit a status event for
/// each transition, and persist the completed plan. No-op without a plan.
fn complete_plan(plan: &mut Option<Plan>, options: &RunOptions, events: &Arc<dyn EventSink>) {
    if let Some(p) = plan.as_mut() {
        let transitions: Vec<(String, String)> = p
            .steps
            .iter()
            .filter(|s| s.status != StepStatus::Done)
            .map(|s| (s.id.clone(), s.title.clone()))
            .collect();
        for (id, title) in transitions {
            p.mark(&id, StepStatus::Done);
            events.emit(EngineEvent::plan_step_status(id, title, StepStatus::Done));
        }
    }
    persist_plan(plan, options);
}

/// Best-effort persist the plan's current state to `.umadev/plan.json` (fail-open:
/// a missing plan / a write error is ignored, never blocks the loop).
fn persist_plan(plan: &Option<Plan>, options: &RunOptions) {
    if let Some(p) = plan {
        let _ = plan_state::save(p, &options.project_root);
        record_artifact_versions(&options.project_root);
    }
}

/// H7 — reconcile a SINGLE-TURN build's plan against the workspace before a budget
/// pause. The single-turn loop drives the WHOLE goal in ONE base turn (it never
/// schedules steps individually), marking the ready steps [`StepStatus::Active`] but
/// only ticking [`StepStatus::Done`] in the clean branch. So a budget break would
/// report `done:0` and — because [`drive_director_loop_resume`] re-drives every
/// non-`Done` step via [`drive_plan_steps`] — REDO work the one mega-turn already
/// wrote to disk.
///
/// Reconcile honestly from reality: a mutating [`StepKind::Build`] step whose
/// acceptance is [`AcceptanceSpec::SourcePresent`] (its bar IS "the declared files
/// exist") and whose declared `files.create` paths ALL exist on disk was completed by
/// that mega-turn, so promote it to `Done`; a step whose outputs are still missing
/// stays as-is so a resume drives only it. This makes `done/total` honest AND stops
/// the resume redoing finished work. Only ever PROMOTES `Active`/`Pending` → `Done`
/// (never downgrades a settled step), and a step with a stricter acceptance, richer
/// evidence contract, or no declared create-file is left untouched (its completion
/// can't be proven from file existence alone). Fail-open: an unreadable path counts
/// as absent, so a step is never falsely marked Done.
fn reconcile_single_turn_plan_from_workspace(plan: &mut Plan, project_root: &std::path::Path) {
    for step in &mut plan.steps {
        if step.kind != plan_state::StepKind::Build
            || !matches!(step.status, StepStatus::Active | StepStatus::Pending)
            || step.acceptance != plan_state::AcceptanceSpec::SourcePresent
            || !step.evidence.is_empty()
            || step.files.create.is_empty()
        {
            continue;
        }
        if step
            .files
            .create
            .iter()
            .all(|rel| project_root.join(rel).exists())
        {
            step.status = StepStatus::Done;
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Workflow-phase sync — keep `.umadev/workflow-state.json` (the 9-phase state
// machine `/status` reads) in step with REAL plan progress on the director-loop
// path.
//
// THE BUG THIS FIXES: `workflow-state.json` was written ONLY by the legacy
// continuous phase-walk (`continuous::persist_state`). The director-loop / plan
// path that `/run` actually drives (`drive_director_loop_routed` → `drive_plan_steps`
// → `drive_build_step`) never wrote it, so after a multi-hour build that produced a
// real frontend + backend on disk, `/status` still showed `phase=research` with all
// 9 phases `[pending]` — stale, dishonest. These helpers map plan progress to a
// `umadev_spec::Phase` and persist it at the key transitions, NEVER moving backward
// and NEVER claiming a phase that didn't happen.
// ───────────────────────────────────────────────────────────────────────────

/// Map ONE plan step (by its responsible seat) to the pipeline [`Phase`] that step's
/// work belongs to — the HONEST anchor for "how far has the build actually reached".
///
/// The mapping reads the seat that OWNS each phase's deliverable:
/// - product-manager → `Docs` (owns scope / PRD — the three-core-docs phase)
/// - architect → `Spec` (owns the API surface + data model — docs→spec translation)
/// - uiux-designer / frontend-engineer → `Frontend` (the design lands as built UI)
/// - backend-engineer → `Backend`
/// - qa-engineer / security-engineer → `Quality` (test coverage + attack-surface review)
/// - devops-engineer → `Delivery` (build / deploy / CI — the hand-off phase)
///
/// The two GATE phases (`DocsConfirm` / `PreviewConfirm`) are deliberately never
/// returned here: they are human-confirmation pauses, not work a director-loop step
/// produces, so anchoring a step to a gate would falsely claim the user confirmed.
fn phase_for_seat(seat: crate::critics::Seat) -> Phase {
    use crate::critics::Seat;
    match seat {
        Seat::ProductManager => Phase::Docs,
        Seat::Architect => Phase::Spec,
        Seat::UiuxDesigner | Seat::FrontendEngineer => Phase::Frontend,
        Seat::BackendEngineer => Phase::Backend,
        Seat::QaEngineer | Seat::SecurityEngineer => Phase::Quality,
        Seat::DevopsEngineer => Phase::Delivery,
    }
}

/// The ordered position of a phase in [`umadev_spec::PHASE_CHAIN`] — the comparison
/// key for "further along". An unlisted phase (impossible for the spec enum, but
/// defensive) sorts first so it can never clamp a real phase backward.
fn phase_rank(phase: Phase) -> usize {
    umadev_spec::PHASE_CHAIN
        .iter()
        .position(|p| *p == phase)
        .unwrap_or(0)
}

/// The phase ANCHOR one plan step contributes when it completes — seat-based
/// ([`phase_for_seat`]) with ONE step-level override: a QA **BUILD** step is
/// TEST-AUTHORING (the codebase's test-first model — see
/// `plan_state::enforce_test_authoring_independence`; the doc-first skeleton
/// schedules it right after the docs, BEFORE any code), so it anchors to `Spec`
/// (executable acceptance criteria are spec-era prep), NOT `Quality`. Otherwise a
/// test-authoring step completing right after the docs would jump `/status` to
/// `quality` while no frontend/backend code exists yet — and a later non-clean
/// finalize would dishonestly claim the build reached `quality`. A QA REVIEW step
/// keeps the seat's `Quality` anchor (reviewing delivered code IS quality-era work).
fn phase_for_step(step: &plan_state::PlanStep) -> Phase {
    if step.seat == crate::critics::Seat::QaEngineer && step.kind == plan_state::StepKind::Build {
        return Phase::Spec;
    }
    phase_for_seat(step.seat)
}

/// The furthest-reached [`Phase`] implied by the plan's COMPLETED (Done) work.
///
/// Each `Done` step contributes its phase anchor ([`phase_for_step`]); the result is
/// the highest-ranked such phase (the deepest the build has honestly reached). A plan
/// with no Done steps yet — or no plan at all — has reached nothing concrete, so this
/// returns `None` (the caller then keeps the initial `research` phase / writes
/// nothing). A fully `Done` plan whose furthest step is e.g. a QA review seat reaches
/// `Quality`, NOT `Delivery` — `Delivery` is only asserted by [`finalize_phase`] when
/// the whole run genuinely finished clean.
fn furthest_done_phase(plan: &Plan) -> Option<Phase> {
    plan.steps
        .iter()
        .filter(|s| s.status == StepStatus::Done)
        .map(phase_for_step)
        .max_by_key(|p| phase_rank(*p))
}

/// The phase the build has reached based on its plan SO FAR — used when the plan is
/// synthesised (move off `research` to honestly show "I'm planning / building this")
/// and as each step completes. Anchors to the furthest Done step; before any step is
/// Done it falls back to `Docs` (the build has at least produced a plan = the docs-era
/// of the work), which is still HONEST: a synthesised plan is real planning output and
/// strictly past bare `research`. Returns `None` only when there are literally no
/// steps to anchor to.
fn in_progress_phase(plan: &Plan) -> Option<Phase> {
    if plan.steps.is_empty() {
        return None;
    }
    Some(furthest_done_phase(plan).unwrap_or(Phase::Docs))
}

/// Read the current persisted phase (defaults to the chain head, `research`, when no
/// state file exists yet) so a phase write can CLAMP to max-so-far and never regress.
fn current_persisted_phase(options: &RunOptions) -> Phase {
    crate::state::read_workflow_state(&options.project_root)
        .and_then(|s| {
            umadev_spec::PHASE_CHAIN
                .iter()
                .copied()
                .find(|p| p.id() == s.phase)
        })
        .unwrap_or(Phase::Research)
}

/// Persist `phase` to `.umadev/workflow-state.json`, CLAMPED so it never moves
/// backward (the phase machine is monotonic — a later step completing can advance it,
/// but nothing regresses it). Mirrors [`crate::continuous::persist_state`]'s
/// `WorkflowState` construction EXACTLY (same shape, same `write_workflow_state`), so
/// `/status` / `continue` read the director-loop's progress the same way they read
/// the legacy walk's. **Fail-open by contract:** a failed write is swallowed
/// (`let _ =`) — a disk / permission error can never wedge an otherwise-healthy run.
/// Canonical CLEAN-completion note - the ONLY note `is_pipeline_complete` (the CLI
/// `continue` guard) treats as "done". Distinct from the per-phase "Advanced to ..." note
/// that `persist_phase` writes on EVERY sync, so a mid-run or non-clean state that merely
/// reached the delivery phase is never mistaken for a finished build (H1).
pub(crate) const DIRECTOR_COMPLETE_NOTE: &str = "Pipeline complete.";

/// [`persist_phase`] but writing an explicit completion note instead of the per-phase
/// "Advanced to ..." note. Used only by a CLEAN finalize.
fn persist_phase_complete(options: &RunOptions, phase: Phase) {
    persist_phase_impl(options, phase, Some(DIRECTOR_COMPLETE_NOTE));
}

fn persist_phase(options: &RunOptions, phase: Phase) {
    persist_phase_impl(options, phase, None);
}

fn persist_phase_impl(options: &RunOptions, phase: Phase, note_override: Option<&str>) {
    // Read the existing state ONCE so we can both clamp the phase AND carry the
    // base session id forward (a phase-transition write must NEVER erase the
    // cross-session resume pointer the run-open path captured — otherwise a
    // `/continue` mid-build would read None and cold-prime a fresh brain).
    let current_state = crate::state::read_workflow_state(&options.project_root);
    let current = current_state
        .as_ref()
        .and_then(|s| {
            umadev_spec::PHASE_CHAIN
                .iter()
                .copied()
                .find(|p| p.id() == s.phase)
        })
        .unwrap_or(Phase::Research);
    // Clamp: never regress below what's already on disk.
    let phase = if phase_rank(phase) >= phase_rank(current) {
        phase
    } else {
        current
    };
    let state = crate::state::WorkflowState {
        phase: phase.id().to_string(),
        active_gate: String::new(),
        slug: options.effective_slug(),
        requirement: options.requirement.clone(),
        last_transition_at: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        note: note_override.map_or_else(
            || format!("Advanced to {} (director loop)", phase.id()),
            str::to_string,
        ),
        backend: options.backend.clone(),
        // Preserve the resume pointer across every phase transition of THIS run.
        base_session_id: current_state
            .as_ref()
            .and_then(|s| s.base_session_id.clone()),
        base_resume_identity: current_state
            .as_ref()
            .and_then(|s| s.base_resume_identity.clone()),
        permission_profile: Some(options.mode.base_permissions()),
        spec_version: umadev_spec::SPEC_VERSION.to_string(),
    };
    let _ = crate::state::write_workflow_state(&options.project_root, &state);
}

/// Sync `workflow-state.json` to the build's in-progress phase derived from `plan`.
/// No-op (writes nothing) when there's no plan or no steps to anchor to. Fail-open.
fn sync_phase_from_plan(plan: &Plan, options: &RunOptions) {
    if let Some(phase) = in_progress_phase(plan) {
        persist_phase(options, phase);
    }
}

/// Sync `workflow-state.json` at FINALIZE. `clean` is whether the run genuinely
/// finished its QC/acceptance floor: a clean finish over a plan whose deepest seat is
/// DevOps reaches `Delivery`; a clean finish whose deepest seat is earlier reaches
/// THAT phase (not an optimistic `Delivery`). A NON-clean finish never claims
/// `Delivery` — it persists only the furthest phase actually completed, so the state
/// stays HONEST about where the build really stopped. No plan / no Done steps → keep
/// the in-progress anchor (still never regresses). Fail-open.
fn finalize_phase_from_plan(plan: &Plan, options: &RunOptions, clean: bool) {
    let reached = furthest_done_phase(plan);
    match (clean, reached) {
        // A clean finish: advance to Delivery (the terminal hand-off) AND stamp the
        // distinct completion note so `continue` recognizes a genuinely-done build (and
        // ONLY a done build - a mid-run sync to the delivery phase keeps "Advanced to ...").
        (true, _) => persist_phase_complete(options, Phase::Delivery),
        // Not clean: persist only what genuinely completed, with the ordinary per-phase
        // note (never the completion note), so `continue` can still resume the build.
        (false, Some(p)) => persist_phase(options, p),
        // Not clean and nothing completed: leave the on-disk phase as-is (the clamp in
        // `persist_phase` keeps whatever the in-progress sync already wrote).
        (false, None) => persist_phase(options, current_persisted_phase(options)),
    }
}

/// [`finalize_phase_from_plan`] over the single-turn loop's `&Option<Plan>`. A
/// CLEAN finalize with no plan (the single-turn fallback) is a genuine clean
/// finish → `delivery`. A NON-clean finalize with no plan leaves the on-disk phase
/// as-is (whatever the in-progress sync wrote, clamped — never regressed, never
/// optimistically jumped to delivery). Fail-open.
fn finalize_phase_from_plan_opt(plan: &Option<Plan>, options: &RunOptions, clean: bool) {
    match plan {
        Some(p) => finalize_phase_from_plan(p, options, clean),
        None if clean => persist_phase_complete(options, Phase::Delivery),
        None => {} // non-clean + no plan: keep the current on-disk phase (no regress)
    }
}

/// One base turn's observable result.
struct TurnResult {
    /// The accumulated assistant text. The caller reads it for the "claimed a build"
    /// hard-gate; this loop reads it to decide whether QC is even warranted.
    text: String,
    /// `true` when the base ACTUALLY invoked a build/test/lint runner on the tool-call
    /// stream THIS turn (an observed `SessionEvent::ToolCall` whose command matched
    /// [`crate::gates::command_is_build_test_runner`]). This is the OBSERVED-tool
    /// corroboration the auto-QC requires before trusting the reply's prose "it's green"
    /// enough to SKIP UmaDev's own build/test read — narration alone (a green claim with
    /// NO runner ever invoked) leaves this `false`, so UmaDev runs its own read instead.
    ran_build_tool: bool,
    /// Receipt armed after the exact initial directive was accepted. Callers
    /// settle it only from the next deterministic verification pass; errors or
    /// cancellation before that point consume it as Unknown on drop.
    memory_receipt: Option<SentReceiptGuard>,
}

/// The outcome of [`resolve_approval`]: the decision plus whether it was made
/// HEADLESSLY (the deterministic floor auto-decided with no live user) — the
/// caller keeps its own existing note behaviour for the headless deny, so the
/// legacy paths stay byte-for-byte silent/loud exactly as they were.
pub(crate) struct ResolvedApproval {
    /// The decision to answer the base with.
    pub decision: ApprovalDecision,
    /// `true` when no live user was consulted (unscoped / no callback) — today's
    /// deterministic floor decided.
    pub headless: bool,
}

/// Resolve a base [`SessionEvent::NeedApproval`] with the trust floor FIRST and —
/// new (A2#3) — a live user SECOND: when the floor says the action needs
/// confirmation and the run is TUI-hosted ([`crate::interaction::request_approval`]),
/// PAUSE and ask the user (the same y/n `await_user_approval` flow the chat drain
/// uses; bounded, fail-open deny), remembering an approved reversible class in the
/// project trust ledger so it is not re-asked (exactly like the chat drain).
/// Headless (CLI / CI / no callback) keeps today's behaviour byte-for-byte: the
/// floor's escalation degrades to DENY and the CALLER emits (or doesn't emit) its
/// existing note — this helper only emits the interactive allow/deny notes.
pub(crate) async fn resolve_approval(
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    action: &str,
    target: &str,
) -> ResolvedApproval {
    let ledger = crate::trust::TrustLedger::load(&options.project_root);
    if !requires_confirmation_with_ledger(
        options.mode,
        action,
        target,
        &options.project_root,
        &ledger,
    ) {
        return ResolvedApproval {
            decision: ApprovalDecision::Allow,
            headless: true,
        };
    }
    match crate::interaction::request_approval(action, target).await {
        Some(true) => {
            // Remember this reversible class so it is not re-asked (an
            // irreversible-floor action records nothing → always re-asks) —
            // mirrors the chat drain's approval bookkeeping exactly.
            crate::trust::remember_project_approval(&options.project_root, action, target);
            events.emit(EngineEvent::Note(umadev_i18n::tlf(
                "trust.pause.allowed",
                &[action, target],
            )));
            ResolvedApproval {
                decision: ApprovalDecision::Allow,
                headless: false,
            }
        }
        Some(false) => {
            events.emit(EngineEvent::Note(umadev_i18n::tlf(
                "trust.pause.denied",
                &[action, target],
            )));
            ResolvedApproval {
                decision: ApprovalDecision::Deny,
                headless: false,
            }
        }
        // Headless — the floor's escalation degrades to DENY, exactly as today;
        // the caller keeps its own existing note behaviour.
        None => ResolvedApproval {
            decision: ApprovalDecision::Deny,
            headless: true,
        },
    }
}

fn approval_option_id(
    options: &[HostApprovalOption],
    decision: ApprovalDecision,
) -> Option<String> {
    let preferred = match decision {
        ApprovalDecision::Allow => [
            HostApprovalOptionKind::AllowOnce,
            HostApprovalOptionKind::AllowAlways,
        ],
        ApprovalDecision::Deny => [
            HostApprovalOptionKind::RejectOnce,
            HostApprovalOptionKind::RejectAlways,
        ],
    };
    preferred
        .iter()
        .find_map(|kind| options.iter().find(|option| &option.kind == kind))
        .map(|option| option.id.clone())
}

/// Resolve a typed base-to-host request without coercing questions or MCP
/// elicitation into binary approval. A hosted TUI gets the exact request;
/// headless and unknown paths return the request's protocol-shaped safe denial.
pub(crate) async fn resolve_host_request(
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    req_id: &str,
    request: &HostRequest,
) -> HostResponse {
    match request {
        HostRequest::Approval {
            action,
            target,
            options: approval_options,
            ..
        } => {
            let resolved = resolve_approval(options, events, action, target).await;
            HostResponse::Approval {
                decision: resolved.decision,
                selected_option_id: approval_option_id(approval_options, resolved.decision),
                message: matches!(resolved.decision, ApprovalDecision::Deny)
                    .then(|| "denied by UmaDev trust policy or user".to_string()),
            }
        }
        HostRequest::PermissionExpansion {
            permissions,
            reason,
            ..
        } => {
            let target = permissions
                .iter()
                .map(|permission| {
                    permission.target.as_ref().map_or_else(
                        || permission.kind.clone(),
                        |target| format!("{}:{target}", permission.kind),
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            events.emit(EngineEvent::Note(format!(
                "[permission] {}{}",
                reason
                    .as_deref()
                    .unwrap_or("the base requested additional access"),
                if target.is_empty() {
                    String::new()
                } else {
                    format!("\n{target}")
                }
            )));
            let resolved = resolve_approval(options, events, "permission-expansion", &target).await;
            HostResponse::PermissionExpansion {
                decision: resolved.decision,
                granted: matches!(resolved.decision, ApprovalDecision::Allow)
                    .then(|| permissions.clone())
                    .unwrap_or_default(),
                message: reason.clone(),
            }
        }
        HostRequest::PlanConfirmation { plan, message, .. } => {
            events.emit(EngineEvent::Note(format!(
                "[plan] {}\n{}",
                message
                    .as_deref()
                    .unwrap_or("the base requests confirmation"),
                plan.chars().take(12_000).collect::<String>()
            )));
            let target = plan
                .lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or("plan");
            let resolved = resolve_approval(options, events, "plan-confirmation", target).await;
            HostResponse::PlanConfirmation {
                decision: resolved.decision,
                feedback: matches!(resolved.decision, ApprovalDecision::Deny)
                    .then(|| "plan execution was not approved".to_string()),
            }
        }
        HostRequest::FolderTrust {
            cwd,
            workspace,
            config_kinds,
        } => {
            events.emit(EngineEvent::Note(format!(
                "[folder-trust] kept gated: cwd={} workspace={} config={}",
                cwd.display(),
                workspace.display(),
                config_kinds.join(", ")
            )));
            request
                .safe_rejection("Folder Trust requires the dedicated live human decision surface")
        }
        HostRequest::Unknown { method, .. } => {
            events.emit(EngineEvent::Note(format!(
                "base requested unsupported host method `{method}`; safely rejected"
            )));
            request.safe_rejection("unsupported host request")
        }
        HostRequest::UserInput { .. } | HostRequest::McpElicitation { .. } => {
            if let Some(response) = crate::interaction::request_host_response(req_id, request).await
            {
                response
            } else {
                events.emit(EngineEvent::Note(
                    "base requested interactive input, but no live response surface was available"
                        .to_string(),
                ));
                request.safe_rejection("interactive response unavailable")
            }
        }
    }
}

/// Send one directive and pump the base's event stream to its `TurnDone`, forwarding
/// tool calls + text to the live sink (the SAME `WorkerStream` render path the
/// pipeline uses), answering approvals via the always-on irreversible floor, and
/// accumulating the assistant text. Returns the turn's text, or `Err` with a
/// machine-true reason on a failed / dead turn (fail-open: the caller maps it to a
/// hard stop, never a panic).
///
/// **Two bounded, VISIBLE self-heals layer on the bare pump, both fail-open:**
/// - A base **turn-failure** the [`crate::base_error`] classifier reads as TRANSIENT
///   (a 429 / overloaded / a network blip) is BACKED OFF and re-driven — emitting a
///   COUNTDOWN Note before each wait (never a silent backoff) — bounded by
///   [`MAX_TRANSIENT_RETRIES`] and the run `deadline`. A HARD failure (auth / context /
///   exit) is returned verbatim with no retry.
/// - A **non-tool silent hang on a still-alive base** (the watchdog's
///   [`IdleEvent::IdleTimedOut`] with no tool in flight and no captured exit — the base
///   may have silently dropped its stream) is re-driven ONCE before failing. It never
///   fires for the legitimate long-tool wait (that keeps `in_tool_call` true) or for a
///   dead base.
///
/// The thin wrapper supplies the real backoff schedule; the `_with_backoff` core takes
/// it as parameters so the test drives a tiny, fast window.
async fn drive_one_turn(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    directive: String,
    idle: IdleBudget,
    deadline: std::time::Instant,
) -> Result<TurnResult, String> {
    drive_one_turn_with_backoff(
        session,
        options,
        events,
        directive,
        idle,
        deadline,
        TRANSIENT_BACKOFF_BASE,
        TRANSIENT_BACKOFF_CAP,
    )
    .await
}

/// [`drive_one_turn`] with exact retrieved-memory identities carried through the
/// final send boundary. This is intentionally used only by chat-originated
/// post-build QC fixes; planning, reports, critics, and background wait turns do
/// not create knowledge receipts.
async fn drive_one_turn_with_memories(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    directive: String,
    memories: Vec<umadev_knowledge::MemoryRef>,
    idle: IdleBudget,
    deadline: std::time::Instant,
) -> Result<TurnResult, String> {
    drive_one_turn_with_backoff_and_memories(
        session,
        options,
        events,
        directive,
        memories,
        idle,
        deadline,
        TRANSIENT_BACKOFF_BASE,
        TRANSIENT_BACKOFF_CAP,
    )
    .await
}

/// [`drive_one_turn`] with the transient-backoff schedule injected (so the test drives
/// a tiny, deterministic window). See [`drive_one_turn`] for the two self-heals.
#[allow(clippy::too_many_arguments)]
async fn drive_one_turn_with_backoff(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    directive: String,
    idle: IdleBudget,
    deadline: std::time::Instant,
    backoff_base: Duration,
    backoff_cap: Duration,
) -> Result<TurnResult, String> {
    drive_one_turn_with_backoff_and_memories(
        session,
        options,
        events,
        directive,
        Vec::new(),
        idle,
        deadline,
        backoff_base,
        backoff_cap,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn drive_one_turn_with_backoff_and_memories(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    directive: String,
    memories: Vec<umadev_knowledge::MemoryRef>,
    idle: IdleBudget,
    deadline: std::time::Instant,
    backoff_base: Duration,
    backoff_cap: Duration,
) -> Result<TurnResult, String> {
    // Estimate the directive's token cost up front (the session stream carries no
    // usage on `TurnDone`, unlike the single-shot path), so `/usage` is real on the
    // default loop for all five bases (three native plus Grok Build/Kimi Code), not just
    // Claude in the legacy runner.
    let mut est_tokens: u64 = approx_tokens(&directive);
    // Keep the directive OWNED (clone for the send) so a transient backoff-retry or a
    // silent-hang watchdog re-drive can re-send the SAME directive on this session.
    //
    // Base-call gate: hold ONE permit for this whole director turn (the send + the
    // event drain + any transient re-send), so a build's doer turn is the only base
    // connection in flight — a background pre-warm or a stray fork can't open a 2nd
    // one that a low-concurrency gateway rejects with 529. Released when this fn
    // returns, BEFORE the runner fans out the (separately-gated) critics, so it is
    // never held while acquiring another permit and can never deadlock.
    let _base_permit = crate::base_gate::base_permit().await;
    if let Err(e) = session.send_turn(directive.clone()).await {
        return Err(format!("session send: {e}"));
    }
    let memory_receipt = commit_sent_memories(&options.project_root, &directive, &memories)
        .map(|receipt| SentReceiptGuard::new(&options.project_root, receipt));
    let mut text = String::new();
    let mut pitfalls: Vec<String> = Vec::new();
    // Bounded counters for the two self-heals (see the fn docs): how many transient
    // backoff-retries this turn has already spent, and whether the silent-hang watchdog
    // re-drive has already fired (a SINGLE re-drive).
    let mut transient_retries: u32 = 0;
    let mut watchdog_retried = false;
    // Capability degrade (bounded SINGLE re-drive): a research/planning turn whose
    // hosted web tool the gateway refused is re-driven ONCE, told to proceed on local
    // knowledge WITHOUT web research, instead of hard-failing the whole build. Gated
    // strictly on the tight capability class AND this directive's research/planning
    // framing (see the `TurnStatus::Failed` arm below); a genuine build failure is
    // never this class, and a second failure falls through to the honest hard-fail.
    let mut capability_redriven = false;
    // Tool-aware idle grace: while the base is plausibly mid-tool (a tool-use event
    // seen, no result yet) it is legitimately SILENT for minutes (a docker build / a
    // compile / npm install / a long test), so the next wait uses the extended tool
    // window; otherwise the base default — so a truly hung base still settles.
    let mut in_tool_call = false;
    let mut tool_activity = ToolActivity::default();
    // SLIDING run-budget clock (Stage 3): the instant of the last genuine base
    // productivity WITHIN this turn — reset by every content-bearing event (see
    // [`is_productive_event`]). The turn/tool budget checks below key off it via
    // [`sliding_deadline`] / [`budget_exhausted`], so a turn that keeps producing
    // (streaming text, a live tool) is NOT cut at a fixed wall-clock instant — it
    // runs on up to the ABSOLUTE cap `deadline`, while a genuinely silent one still
    // winds down after a full idle window. `run_budget()` is that idle window.
    let idle_window = run_budget();
    let mut last_progress = std::time::Instant::now();
    // Did the base ACTUALLY run a build/test/lint runner this turn? Set from the OBSERVED
    // tool-call stream below (not the reply prose), it is the corroboration the auto-QC
    // requires before a green CLAIM is trusted to skip UmaDev's own build/test read. Reset
    // alongside `text` on a transient re-drive so it reflects only the FINAL attempt.
    let mut ran_build_tool = false;
    // Outstanding-background-agents guard (the premature-final-report fix): counts
    // the base's OWN background sub-agents still running (observed via the driver's
    // BackgroundTask frames + the "Async agent launched" tool_result fallback). A
    // `Completed` settle with agents outstanding is converted into a bounded "wait
    // for your agents, collect their results, THEN report" re-drive — at most
    // `bg_agents::MAX_BG_REDRIVES` per turn, then an incomplete failure. Fail-open:
    // a base that surfaces no background signal keeps a zero count → today's behavior.
    let mut bg = crate::bg_agents::BgAgentTracker::new();
    loop {
        // Wall-clock budget reached DURING a turn (not just between steps/rounds). A
        // base that stays ACTIVE — keeps emitting tool-calls / text deltas (e.g.
        // writing code) — never trips the idle watchdog below, so without this check
        // a single turn runs UNBOUNDED past the run budget (the between-step deadline
        // checks can never be reached while one pump turn is still draining). Settle
        // GRACEFULLY on the work produced so far: best-effort interrupt (the SAME
        // bounded interrupt `next_event_idle` issues on an idle hang), record the
        // turn's usage estimate (no `TurnDone` arrived → no real usage, F3), distil
        // the pitfalls seen so far, and return the accumulated text as a completed-ish
        // turn — so the caller treats it as "this turn produced what it produced" and
        // the between-step deadline checks wind the run down to the final gate.
        //
        // SLIDING (Stage 3): `eff` is the sliding budget deadline — the idle window
        // from the last productive event, clamped to the absolute cap `deadline`. A
        // streaming turn keeps resetting `last_progress`, so `eff` stays ahead and the
        // turn runs to the absolute cap; a silent one settles after one idle window.
        let eff = sliding_deadline(deadline, last_progress, idle_window);
        if std::time::Instant::now() >= eff {
            let _ = tokio::time::timeout(
                Duration::from_secs(INTERRUPT_TIMEOUT_SECS),
                session.interrupt(),
            )
            .await;
            record_turn_usage(options, events, None, est_tokens);
            capture_turn_pitfalls(options, events, &pitfalls);
            events.emit(EngineEvent::Note(
                "team · run budget reached mid-turn — interrupted the base and finalizing \
                 on what's built (raise UMADEV_RUN_BUDGET_SECS for a longer run)"
                    .to_string(),
            ));
            return Ok(TurnResult {
                text,
                ran_build_tool,
                memory_receipt,
            });
        }
        // Idle watchdog (P0-3 / P1-11): a base that HANGS (stops emitting stdout
        // but never exits) would leave `next_event()` blocked forever — no
        // `TurnDone`, no settle, `thinking` stuck. The shared [`next_event_idle`]
        // converts pure silence into a settle (ANY event resets it, so a long
        // streaming compile/test turn survives as long as it emits SOMETHING). It is
        // LIVENESS-based while a tool runs: a tool of any duration with a live base
        // keeps waiting (only a dead base or the run `deadline` settles it), while a
        // non-tool hang still settles at the base window. The SAME primitive guards
        // every main-session pump (here + `continuous::drive_phase` /
        // `drive_rework_turn`), so the protection can't be "fixed in one, forgotten in
        // another".
        // H6: while a tool is in flight, bound the wait by the ABSOLUTE cap `deadline`
        // (a live-but-silent tool must survive to the hard ceiling, not be cut at the
        // sliding `eff` window one idle window in); with no tool in flight the sliding
        // `eff` still catches a genuinely hung base after one idle window.
        let ev = match next_event_idle(
            session,
            idle,
            in_tool_call,
            Some(idle_wait_deadline(in_tool_call, deadline, eff)),
        )
        .await
        {
            IdleEvent::Event(ev) => ev,
            IdleEvent::SessionEnded { exit, stderr_tail } => {
                // `None` = the session ended (process dead / EOF). Per the
                // BaseSession contract, treat as a failed turn — fail-open, no panic.
                // LOW #2: an interrupted/dead turn still consumed tokens (the directive
                // + whatever streamed before the cut) — record the estimate so `/usage`
                // is honest about cost on a failed turn, not just a clean one. No
                // `TurnDone` arrived → no real usage available, so estimate (F3).
                record_turn_usage(options, events, None, est_tokens);
                // Surface the base's OWN stderr/exit (captured at the settle) so the
                // user sees WHY it ended, not a bare literal — mirrors the chat path.
                return Err(enrich_idle_reason(
                    "base session ended mid-turn",
                    exit,
                    stderr_tail,
                    &options.backend,
                ));
            }
            IdleEvent::IdleTimedOut { exit, stderr_tail } => {
                // MEDIUM M5 — run budget reached DURING a silent tool turn. While a tool
                // runs, `next_event_idle` is liveness-based and only settles on a dead
                // base or the run `deadline`, so a deadline crossing mid-tool surfaces
                // HERE as `IdleTimedOut` (the top-of-loop budget settle only fires
                // between waits). This is the SAME budget-reached condition as that
                // block — NOT a hang — so settle GRACEFULLY on the work produced so far
                // (Ok with the accumulated text) instead of returning Err, which would
                // mark the run Failed and SKIP run_auto_qc + finalize (losing the QC +
                // delivery purely because the deadline happened to land mid-tool rather
                // than mid-stream).
                if in_tool_call && std::time::Instant::now() >= eff {
                    let _ = tokio::time::timeout(
                        Duration::from_secs(INTERRUPT_TIMEOUT_SECS),
                        session.interrupt(),
                    )
                    .await;
                    record_turn_usage(options, events, None, est_tokens);
                    capture_turn_pitfalls(options, events, &pitfalls);
                    events.emit(EngineEvent::Note(
                        "team · run budget reached mid-tool — interrupted the base and \
                         finalizing on what's built (raise UMADEV_RUN_BUDGET_SECS for a \
                         longer run)"
                            .to_string(),
                    ));
                    return Ok(TurnResult {
                        text,
                        ran_build_tool,
                        memory_receipt,
                    });
                }
                // Watchdog re-drive (bounded SINGLE retry): a NON-tool silent hang on a
                // base that is STILL ALIVE (no exit captured even after the watchdog's
                // bounded interrupt) may be a SILENTLY DROPPED stream, not a dead base —
                // so re-drive the SAME directive ONCE before failing. Strictly gated so
                // it never fights the legitimate long-tool wait: it fires ONLY when no
                // tool was in flight (`!in_tool_call`, so the in-tool budget-reached
                // settle is excluded), the base is alive (`exit.is_none()`), and we have
                // not already re-driven. Fail-open: a re-send error, a second hang, or a
                // dead base all fall through to the honest failure below.
                if !in_tool_call && exit.is_none() && !watchdog_retried {
                    watchdog_retried = true;
                    // The abandoned (hung) attempt still spent its tokens — record the
                    // estimate (F3) before the fresh drive.
                    record_turn_usage(options, events, None, est_tokens);
                    events.emit(EngineEvent::Note(
                        umadev_i18n::tl("tui.retry.silent_redrive").to_string(),
                    ));
                    // Re-send the SAME directive on the still-live session. A send error
                    // means the session really is broken → fail honestly.
                    if let Err(e) = session.send_turn(directive.clone()).await {
                        return Err(format!("session send: {e}"));
                    }
                    // Fresh turn: reset the accumulators (the hang produced no output).
                    est_tokens = approx_tokens(&directive);
                    text.clear();
                    pitfalls.clear();
                    in_tool_call = false;
                    tool_activity.clear();
                    // The re-drive is a fresh attempt — the corroboration must reflect
                    // ONLY tools the FINAL attempt actually runs, never the abandoned
                    // (hung) attempt's runner: a stale `true` here would let the
                    // re-driven turn's bare green CLAIM skip UmaDev's own build/test
                    // read (mirrors the transient-retry reset below).
                    ran_build_tool = false;
                    continue;
                }
                // No event within the idle window → the base is hung. Settle as a
                // Failed outcome so the loop ends and `thinking` clears, rather than
                // blocking forever (the interrupt was already issued, bounded).
                // LOW #2: record the tokens spent up to the hang (fail-open). The
                // turn hung with no `TurnDone` → estimate (no real usage). F3.
                record_turn_usage(options, events, None, est_tokens);
                // Fold in the base's stderr tail / exit so a hung build no longer
                // settles with a cause-less "base went idle — …". Report the BASE idle
                // window (the `UMADEV_IDLE_TIMEOUT_SECS` knob), since IdleTimedOut now
                // means a non-tool hang at that window, or the run budget reached mid-tool.
                return Err(enrich_idle_reason(
                    &idle_reason(idle.window(false)),
                    exit,
                    stderr_tail,
                    &options.backend,
                ));
            }
        };
        // Update the mid-tool state from this event BEFORE handling it: a tool-use
        // arms the extended grace for the next wait, a tool-result disarms it.
        in_tool_call = tool_activity.observe(&ev);
        // SLIDING run-budget reset (Stage 3): a content-bearing event is genuine base
        // productivity — slide the idle window so a turn that keeps producing runs on
        // up to the absolute cap. A pure control frame does not reset it.
        if is_productive_event(&ev) {
            last_progress = std::time::Instant::now();
        }
        // Feed the outstanding-background-agents guard (cheap, fail-open).
        bg.observe(&ev);
        let event_tool_call_id = ev.tool_call_id().map(str::to_owned);
        match ev {
            SessionEvent::TextDelta(delta) => {
                est_tokens = est_tokens.saturating_add(approx_tokens(&delta));
                text.push_str(&delta);
                events.emit(EngineEvent::WorkerStream {
                    event: StreamEvent::Text { delta },
                });
            }
            SessionEvent::ThinkingDelta(delta) => {
                // The base's extended-thinking reasoning — surfaced as a collapsed
                // `[thinking]` block (transparency) and NOT folded into the answer
                // `text` (which the deterministic floor / acceptance keys off).
                events.emit(EngineEvent::WorkerStream {
                    event: StreamEvent::ThinkingDelta(delta),
                });
            }
            SessionEvent::SessionModel(id) => {
                // The base reported its resolved model at session init — surface it
                // so the TUI's context gauge uses the REAL window, not a per-backend
                // guess. Purely informational; drives no loop control or acceptance.
                events.emit(EngineEvent::BaseModel { id });
            }
            SessionEvent::StateUpdate(update) => {
                events.emit(EngineEvent::BaseSessionState {
                    backend_id: options.backend.clone(),
                    update,
                });
            }
            SessionEvent::ToolCall { name, input }
            | SessionEvent::ToolCallCorrelated { name, input, .. } => {
                // OBSERVED-tool corroboration (honesty floor): if THIS tool call is a real
                // build/test/lint runner, record it — the auto-QC will require this signal
                // before trusting the reply's prose "it's green" enough to skip UmaDev's
                // own build/test read. We read the actual shell `command` the base RAN (not
                // its narration), so a green claim with no runner ever invoked can't skip.
                // Fail-open: a non-shell tool / no command → no signal (UmaDev verifies).
                if let Some(cmd) = input.get("command").and_then(serde_json::Value::as_str) {
                    if crate::gates::command_is_build_test_runner(cmd) {
                        ran_build_tool = true;
                    }
                }
                // Surface what the base actually DID (the source of truth). The
                // governance hook governs the write itself in real time (claude); the
                // content-governance QC scan is the craft floor for ALL bases. Here we
                // (a) render the tool row, and (b) record the call to the audit trail
                // (UD-EVID-002) so the audit is honest on the DEFAULT loop for every
                // base — not just claude in the legacy runner. Fail-open: a recording
                // error is swallowed and never blocks the turn.
                let mut detail = tool_call_target(&input);
                // The base asked the user a structured multiple-choice question via
                // its OWN `AskUserQuestion` tool. Driven non-interactively, that call
                // can't render its picker and auto-cancels — was a bare optionless
                // stub read as cancelled. Surface the question + numbered options as a
                // Note + give the tool row a real detail. A2#6: on THIS mid-run
                // director path the honest hint is the MID-RUN variant — the build
                // continues with the base's default; a typed answer folds in as
                // follow-up steering at the next step boundary (never "the base is
                // waiting on you", which is only true on the chat surface's relay).
                // Fail-open: a non-question call → None.
                if let Some(surface) = crate::ask_question::surface_mid_run(&name, &input) {
                    detail = surface.detail;
                    events.emit(EngineEvent::Note(surface.note));
                } else if let Some(surface) = crate::ask_question::exit_plan_surface(&name, &input)
                {
                    // The base called its OWN `ExitPlanMode` — surface the full plan
                    // markdown as a Note labeled as the BASE's plan mode (distinct
                    // from UmaDev guarded). Fail-open: no readable plan → plain row.
                    detail = surface.detail;
                    events.emit(EngineEvent::Note(surface.note));
                }
                record_tool_call_audit(options, &name, &detail, &input);
                // P1: forward the structured before/after for a Write/Edit so the
                // TUI can draw a live diff card on the DEFAULT loop (the user hit
                // "no real-time feedback when writing code"). Fail-open: a
                // non-edit tool / unreadable input → None → the plain tool row.
                let edit = umadev_runtime::ToolEdit::from_claude_tool_input(&name, &input);
                let stream_event = match event_tool_call_id {
                    None => StreamEvent::ToolUse { name, detail, edit },
                    Some(call_id) => StreamEvent::ToolUseCorrelated {
                        call_id,
                        name,
                        detail,
                        edit,
                    },
                };
                events.emit(EngineEvent::WorkerStream {
                    event: stream_event,
                });
            }
            SessionEvent::ToolProgressCorrelated { call_id, title } => {
                events.emit(EngineEvent::WorkerStream {
                    event: StreamEvent::ToolProgressCorrelated { call_id, title },
                });
            }
            SessionEvent::ToolOutputDelta(delta) => {
                // Process output is a non-terminal progress frame. Forward it
                // for visibility without disarming the tool grace or treating
                // verification as complete.
                events.emit(EngineEvent::WorkerStream {
                    event: StreamEvent::ToolOutputDelta { delta },
                });
            }
            SessionEvent::ToolOutputDeltaCorrelated { call_id, delta } => {
                events.emit(EngineEvent::WorkerStream {
                    event: StreamEvent::ToolOutputDeltaCorrelated { call_id, delta },
                });
            }
            SessionEvent::ToolOutputSnapshot(output) => {
                events.emit(EngineEvent::WorkerStream {
                    event: StreamEvent::ToolOutputSnapshot { output },
                });
            }
            SessionEvent::ToolOutputSnapshotCorrelated { call_id, output } => {
                events.emit(EngineEvent::WorkerStream {
                    event: StreamEvent::ToolOutputSnapshotCorrelated { call_id, output },
                });
            }
            SessionEvent::ToolResult { ok, summary } => {
                if !ok {
                    // A failed tool call is a development pitfall — feed it to the
                    // lessons KB at turn end (Wave 2 deliverable 4 on the default loop).
                    pitfalls.push(summary.clone());
                }
                events.emit(EngineEvent::WorkerStream {
                    event: StreamEvent::ToolResult { ok, summary },
                });
            }
            SessionEvent::ToolResultCorrelated {
                call_id,
                ok,
                summary,
            } => {
                if !ok {
                    pitfalls.push(summary.clone());
                }
                events.emit(EngineEvent::WorkerStream {
                    event: StreamEvent::ToolResultCorrelated {
                        call_id,
                        ok,
                        summary,
                    },
                });
            }
            SessionEvent::NeedApproval {
                req_id,
                action,
                target,
            } => {
                // Mode-aware floor + self-learning ledger: deny an irreversible
                // action even headless (the floor the `auto` tier can't skip) plus
                // the per-mode reversible policy, but honour a class the user has
                // already approved for this project (`.umadev/trust.json`) so it
                // isn't re-denied. Fail-open: a missing/corrupt ledger behaves as
                // the bare mode policy. Reversible in-tree edits stay allowed so a
                // headless build isn't wedged waiting on a human.
                //
                // A2#3: when the run is TUI-HOSTED, an escalation PAUSES and asks
                // the live user (the same y/n flow as the chat drain — bounded,
                // fail-open deny) instead of headlessly auto-denying while the UI
                // copy claims "needs your confirmation". Headless keeps the deny.
                let resolved = resolve_approval(options, events, &action, &target).await;
                if resolved.headless && matches!(resolved.decision, ApprovalDecision::Deny) {
                    events.emit(EngineEvent::Note(umadev_i18n::tlf(
                        "continuous.dangerous_action_denied",
                        &[&action, &target],
                    )));
                }
                if let Err(e) = session.respond(&req_id, resolved.decision).await {
                    // LOW #2: a turn that dies on the approval round-trip still spent
                    // its tokens — record the estimate (fail-open) before bailing. No
                    // `TurnDone` yet → estimate (F3).
                    record_turn_usage(options, events, None, est_tokens);
                    return Err(format!("session respond: {e}"));
                }
            }
            SessionEvent::HostRequest { req_id, request } => {
                let response = resolve_host_request(options, events, &req_id, &request).await;
                if let Err(error) = session.respond_host(&req_id, response).await {
                    record_turn_usage(options, events, None, est_tokens);
                    return Err(format!("session host response: {error}"));
                }
            }
            SessionEvent::BackgroundTask(_) => {
                // Already folded into the tracker above; carries no render row.
            }
            SessionEvent::BackgroundProcess(_) => {
                // A base-owned shell/monitor process has an independent
                // lifecycle. It is observable in the resident UI, but it must
                // not enter the sub-agent completion guard or block TurnDone.
            }
            SessionEvent::PromptQueueChanged(_) => {
                // The native queue belongs to the interactive resident session;
                // a headless director turn never materializes it as output.
            }
            SessionEvent::TurnDone { status, usage } => match status {
                // Completed / Truncated → accept the turn (the deterministic floor
                // downstream is the real stop signal; forcing a fail here would
                // hard-stop a build that may have produced usable output).
                TurnStatus::Completed | TurnStatus::Truncated => {
                    // Outstanding-background-agents guard: a CLEAN finish while the
                    // base's own background sub-agents are still running is a
                    // premature settle — the "final report" (if any) predates their
                    // results, and settling would eventually tear the session down
                    // and kill them mid-write. Convert it into a bounded "wait for
                    // your agents" re-drive (at most `MAX_BG_REDRIVES`, and never
                    // past the run deadline). Truncated is NOT re-driven — the base
                    // hit a turn/budget ceiling, spending more would fight the cap.
                    // Exhausting the bound is an incomplete turn, never success.
                    if matches!(status, TurnStatus::Completed)
                        && std::time::Instant::now() < deadline
                        && bg.begin_redrive()
                    {
                        events.emit(EngineEvent::Note(umadev_i18n::tlf(
                            "bg.redrive",
                            &[
                                &bg.outstanding().to_string(),
                                &bg.redrives().to_string(),
                                &crate::bg_agents::MAX_BG_REDRIVES.to_string(),
                            ],
                        )));
                        let wait = bg.wait_directive();
                        est_tokens = est_tokens.saturating_add(approx_tokens(&wait));
                        if session.send_turn(wait).await.is_ok() {
                            // Keep `text` (nothing is lost; the re-driven turn
                            // appends the REAL final report) and keep
                            // `ran_build_tool` (tools genuinely ran this turn).
                            in_tool_call = false;
                            tool_activity.clear();
                            continue;
                        }
                        // A send failure means the session is going away — fall
                        // through and settle honestly on what landed (fail-open).
                    }
                    // Wave 2 deliverable 4: record usage + distil pitfalls on the
                    // DEFAULT loop, for every base. Both fail-open. F3: prefer the
                    // base's REAL reported usage, fall back to the chars/4 estimate.
                    record_turn_usage(options, events, usage, est_tokens);
                    capture_turn_pitfalls(options, events, &pitfalls);
                    if bg.outstanding() > 0 {
                        // The base gave us positive lifecycle evidence that work is
                        // still live. A warning followed by `Ok` still lets the outer
                        // plan mark the step complete, so fail the logical turn after
                        // the bounded recovery attempts instead.
                        let incomplete = umadev_i18n::tlf(
                            "bg.outstanding_note",
                            &[&bg.outstanding().to_string()],
                        );
                        events.emit(EngineEvent::Note(incomplete.clone()));
                        return Err(incomplete);
                    }
                    return Ok(TurnResult {
                        text,
                        ran_build_tool,
                        memory_receipt,
                    });
                }
                // LOW #2: an Interrupted/Failed turn still consumed tokens — record the
                // usage on these paths too (not just Completed/Truncated), so `/usage`
                // reflects the real cost of a turn that didn't finish clean. F3: a
                // Failed/Interrupted `TurnDone` may still carry real usage — use it.
                TurnStatus::Interrupted => {
                    record_turn_usage(options, events, usage, est_tokens);
                    return Err("director turn interrupted".to_string());
                }
                TurnStatus::Failed(reason) => {
                    record_turn_usage(options, events, usage, est_tokens);
                    let failure = crate::base_error::classify(None, None, Some(&reason));
                    // CAPABILITY DEGRADE (research/planning turns only): the base is
                    // ALIVE and answered, but the gateway refused ONE optional hosted
                    // tool it reached for (a hosted `web_search`). On the advisory
                    // research/planning seam this must NOT hard-fail the whole build —
                    // re-drive ONCE, telling the base to proceed on LOCAL KNOWLEDGE
                    // without web research. Gated STRICTLY on the tight capability class
                    // AND this directive's research/planning framing, and bounded to a
                    // single re-drive within the run `deadline`; a genuine build failure
                    // is never this class, a non-research turn never matches the framing,
                    // and a SECOND failure falls through to the honest hard-fail below.
                    if crate::base_error::is_capability_degradable(&failure)
                        && is_research_or_planning_directive(&directive)
                        && !capability_redriven
                        && std::time::Instant::now() < deadline
                    {
                        capability_redriven = true;
                        events.emit(EngineEvent::Note(capability_degrade_note().to_string()));
                        let redirective =
                            format!("{directive}{}", local_knowledge_without_web_directive());
                        if let Err(e) = session.send_turn(redirective.clone()).await {
                            return Err(format!("session send: {e}"));
                        }
                        // Fresh attempt: the refused turn produced no usable output.
                        est_tokens = approx_tokens(&redirective);
                        text.clear();
                        pitfalls.clear();
                        in_tool_call = false;
                        tool_activity.clear();
                        ran_build_tool = false;
                        continue;
                    }
                    // Visible bounded backoff-retry on a TRANSIENT base failure (a 429
                    // rate limit, an overloaded base, a network blip): the base hit a
                    // RECOVERABLE hiccup, so emit a COUNTDOWN Note (never a silent wait),
                    // back off, and re-drive the SAME directive — bounded by
                    // `MAX_TRANSIENT_RETRIES` AND the run `deadline`. A HARD failure
                    // (auth / context / a non-zero exit / unclassifiable) is returned
                    // verbatim with NO retry (retrying is futile → fail at once). The
                    // classifier reads the base's OWN error text only (this `reason`),
                    // never an idle/ended settle, so an idle hang is never mistaken for a
                    // transient API error. Fail-open: the caller still NAMES the fix.
                    if crate::base_error::is_transient(&failure)
                        && transient_retries < MAX_TRANSIENT_RETRIES
                        && std::time::Instant::now() < deadline
                    {
                        transient_retries += 1;
                        let wait =
                            transient_backoff_wait(backoff_base, backoff_cap, transient_retries);
                        events.emit(EngineEvent::Note(umadev_i18n::tlf(
                            "tui.retry.countdown",
                            &[
                                &wait.as_secs().to_string(),
                                &transient_retries.to_string(),
                                &MAX_TRANSIENT_RETRIES.to_string(),
                            ],
                        )));
                        tokio::time::sleep(wait).await;
                        // Re-drive the SAME directive on the still-live session.
                        if let Err(e) = session.send_turn(directive.clone()).await {
                            return Err(format!("session send: {e}"));
                        }
                        est_tokens = approx_tokens(&directive);
                        text.clear();
                        pitfalls.clear();
                        in_tool_call = false;
                        tool_activity.clear();
                        // The re-drive is a fresh attempt — the corroboration must reflect
                        // ONLY tools the FINAL attempt actually runs, never a prior try's.
                        ran_build_tool = false;
                        continue;
                    }
                    return Err(reason);
                }
            },
        }
    }
}

/// A cheap, deterministic token estimate for a piece of text — `~chars/4`, the
/// standard rough heuristic (the continuous-session stream surfaces no real usage on
/// `TurnDone`, so this is the honest fallback that keeps `/usage` non-empty on the
/// default loop). Never panics; an empty string is 0. `pub` so the shared rework pump
/// (`continuous::drive_rework_turn_with_idle`) AND the TUI's chat hot path estimate
/// identically.
pub fn approx_tokens(s: &str) -> u64 {
    (s.chars().count() as u64).div_ceil(4)
}

/// Record an estimated-token usage row for the default loop, attributed to the
/// canonical "build" phase. `pub` so the shared rework pump — and the TUI's chat hot
/// path — record usage the same way the single-turn loop does. Fail-open: a zero
/// estimate is a no-op.
pub fn record_estimated_usage(backend: &str, est_tokens: u64) {
    if est_tokens == 0 {
        return;
    }
    crate::runner::record_usage(backend, umadev_spec::Phase::Frontend, est_tokens);
}

/// The token count to record for one turn: the base's REAL usage when its live
/// stream reported it on `TurnDone` (F3 — claude's `result` line, codex's
/// `thread/tokenUsage/updated` / inline `turn/completed`), else the `chars/4`
/// estimate. opencode's SSE carries no usage → it always estimates (honest).
///
/// "Tokens" here is `input + output` (the same single-number convention
/// `record_usage` already uses). Fail-open: a `None` usage simply yields the
/// estimate; the real path can never make `/usage` read lower than honest. `pub` so
/// the TUI's chat hot path records the ledger identically.
pub fn real_or_estimated_tokens(usage: Option<Usage>, est_tokens: u64) -> u64 {
    match usage {
        // An incomplete empty report is explicitly "unknown/lower bound 0",
        // not proof that the turn was free. Preserve any non-zero lower bound;
        // otherwise use the deterministic estimate for the legacy flat ledger.
        Some(u) if u.usage_incomplete && u.has_empty_lower_bound() => est_tokens,
        Some(u) => u.total_tokens,
        None => est_tokens,
    }
}

/// Record one director turn's token usage to `~/.umadev/usage.jsonl` so `/usage`
/// is real on the default loop for all five bases (three native plus Grok Build/Kimi Code) —
/// preferring the base's REAL
/// reported usage and falling back to the `chars/4` estimate (F3). Fail-open: a
/// zero count / a write error is a no-op. Mirrors [`crate::runner::record_usage`].
pub(crate) fn record_turn_usage(
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    usage: Option<Usage>,
    est_tokens: u64,
) {
    // Surface the base's REAL reported usage to the live UI session total. The
    // estimate rides ALONGSIDE (never summed into the real count): an estimate is
    // not the base's own number, so it can never inflate the live real total — the
    // meter keeps the two strictly separate and shows the estimate ONLY as a
    // clearly-LABELED lower-bound fallback when the base has reported nothing all
    // session (a conscious, documented reversal of the former "live gauge is
    // real-only" policy — a labeled floor beats a bare "usage unknown", and the real
    // number still wins the instant the base reports one). The ledger row below still
    // records the estimate fallback so `/usage` stays honest.
    events.emit(EngineEvent::TurnUsage { usage, est_tokens });
    record_estimated_usage(
        &options.backend,
        real_or_estimated_tokens(usage, est_tokens),
    );
}

/// Record one base tool call to the audit trail (UD-EVID-002) on the default loop.
///
/// Records the call + target with the REAL governance verdict — the same policy +
/// project-context + rule evaluation the continuous path applies (see
/// [`tool_call_governance_verdict`]), not a hardcoded `allow`. Previously this
/// loop always wrote `allow`, so the SAME base got inconsistent audit semantics
/// across the two loops (a secret/dangerous write recorded as `block` in the
/// continuous path but `allow` here). Now both agree. Fail-open: verdict
/// computation degrades to a passing decision and any record error is swallowed —
/// the turn is never blocked.
fn record_tool_call_audit(
    options: &RunOptions,
    name: &str,
    target: &str,
    input: &serde_json::Value,
) {
    let decision = tool_call_governance_verdict(options, name, input);
    let decision_word = if decision.block { "block" } else { "allow" };
    let _ = umadev_governance::record_tool_call(
        &options.project_root,
        name,
        target,
        decision_word,
        &decision.clause,
        &decision.reason,
        &options.effective_slug(),
        None,
    );
}

/// Compute the governance verdict for one base tool call — the SAME policy +
/// project-context + rule evaluation `continuous::govern_tool_call` runs (bash →
/// `check_dangerous_bash`; a file-mutating tool → content scan; everything else →
/// pass), so the DEFAULT loop's audit records the true allow/deny instead of a
/// hardcoded `allow`. Kept in lockstep with `continuous::evaluate_tool_call`.
///
/// Fully fail-open + deterministic given the on-disk policy/context: it emits no
/// event and never blocks a turn — it is pure evaluation feeding only the audit
/// record.
fn tool_call_governance_verdict(
    options: &RunOptions,
    name: &str,
    input: &serde_json::Value,
) -> umadev_governance::Decision {
    let policy = umadev_governance::Policy::load(&options.project_root);
    let ctx = crate::planner::derive_project_context(
        &options.requirement,
        &options.project_root,
        &options.effective_slug(),
    );
    let lname = name.to_ascii_lowercase();
    if lname == "bash" || lname == "shell" || lname == "run" {
        let cmd = input
            .get("command")
            .and_then(serde_json::Value::as_str)
            .or_else(|| input.get("cmd").and_then(serde_json::Value::as_str))
            .unwrap_or_default();
        return umadev_governance::check_dangerous_bash(cmd);
    }
    // File-mutating tools (aligned with the continuous path + the hook's `is_write`
    // matcher): scan the proposed content. Any other tool is observe-only → pass.
    if lname == "write"
        || lname == "edit"
        || lname == "multiedit"
        || lname == "notebookedit"
        || lname == "update"
        || lname == "create"
    {
        let path = umadev_runtime::write_scan_path(input);
        let content = umadev_runtime::write_scan_content(input);
        // Bypass-immune irreversible floor FIRST (ignores disabled clauses), in
        // lockstep with `continuous::evaluate_tool_call`, so a non-Claude base's
        // write to a leaked secret / sensitive `.env`/`.ssh`/no-extension path is
        // recorded as blocked regardless of a rules.toml disable. Clean → the
        // policy-aware content scan.
        let floor = umadev_governance::pre_write_floor_decision(&path, &content);
        if floor.block {
            return floor;
        }
        return umadev_governance::scan_content_with_context(&path, &content, &policy, ctx);
    }
    umadev_governance::Decision::pass()
}

/// Best-effort human-readable target of a base tool call (a file path / command)
/// for the live tool row — fail-open to an empty string on any unexpected shape.
/// Includes `plan` so an `ExitPlanMode` call's proposed plan text reaches the row
/// instead of an empty target (the dedicated surface then supersedes it).
fn tool_call_target(input: &serde_json::Value) -> String {
    for key in ["file_path", "path", "command", "url", "pattern", "plan"] {
        if let Some(s) = input.get(key).and_then(serde_json::Value::as_str) {
            return s.to_string();
        }
    }
    String::new()
}

// ───────────────────────────────────────────────────────────────────────────
// Auto-QC — UmaDev's objective quality pass (NOT the base summoning a team)
// ───────────────────────────────────────────────────────────────────────────

/// What one auto-QC pass found. Empty `blocking` = clean (the build is genuinely
/// done). Non-empty = the factual problems UmaDev folds into ONE fix directive for
/// the base. Built fail-open: any QC step that can't run contributes nothing (a
/// neutral skip), never a false blocking finding.
#[derive(Debug, Clone, Default)]
struct QcReport {
    /// The deduped, source-tagged union of blocking problems (e.g. `verify build:
    /// FAILED …`, `[security] no input validation`). Empty = clean.
    blocking: Vec<String>,
    /// B1#2 — a BOUNDED verbatim tail of the failing build/test output (last ~60
    /// lines, char-capped; see [`director::verify_build_test_raw`]), captured when
    /// the build/test fact read FAILED. Folded into the fix directive as raw
    /// evidence the brain adapts from — never a substitute for the one-line
    /// distillation in `blocking`. `None` when no raw log exists (clean build,
    /// skipped read, or a non-build finding) — the directive skips it cleanly.
    raw_failure_log: Option<String>,
}

/// Build a bounded, machine-true terminal diagnosis from residual QC findings.
/// The outcome text is rendered directly by CLI/TUI failure surfaces, so it must
/// carry useful evidence without dumping an unbounded governance/build log.
fn qc_incomplete_reason(reason: &str, blocking: &[String]) -> String {
    const MAX_ITEMS: usize = 8;
    const MAX_ITEM_CHARS: usize = 320;

    let mut evidence: Vec<String> = blocking
        .iter()
        .filter_map(|item| {
            let item = item.trim();
            (!item.is_empty()).then(|| item.chars().take(MAX_ITEM_CHARS).collect())
        })
        .take(MAX_ITEMS)
        .collect();
    if blocking
        .iter()
        .filter(|item| !item.trim().is_empty())
        .count()
        > evidence.len()
    {
        evidence.push(format!(
            "... {} additional blocking finding(s)",
            blocking
                .iter()
                .filter(|item| !item.trim().is_empty())
                .count()
                .saturating_sub(evidence.len())
        ));
    }

    if evidence.is_empty() {
        format!("director build incomplete: {reason}")
    } else {
        format!(
            "director build incomplete: {reason}. Residual evidence: {}",
            evidence.join(" | ")
        )
    }
}

impl QcReport {
    /// Whether the build passed QC clean (no blocking problem found).
    fn is_clean(&self) -> bool {
        self.blocking.is_empty()
    }

    /// Fold the QC findings into ONE fix directive fed back to the base over the
    /// same session. The BASE'S BODY does the fixing (and the build/test) with its
    /// own tools — UmaDev only hands it the facts and asks it to act. Lean +
    /// command-style so the base fixes rather than narrates; it already holds the
    /// full build context in this one continuous session, so no role re-priming.
    #[cfg(test)]
    fn fix_directive(&self) -> String {
        self.fix_directive_with_context("")
    }

    /// [`Self::fix_directive`] with an optional CONTEXT prefix front-loaded before
    /// the findings — used by the chat-build post-QC entry to inject the recalled
    /// commercial-engineering knowledge digest plus the project's prior pitfalls
    /// (`post_build_rework_context`) so the fix turn fixes WITH the team's standards
    /// and memory, not blind. An empty prefix yields the byte-for-byte original
    /// directive, so the `/run` callers are unchanged. Fail-open by construction.
    #[cfg(test)]
    fn fix_directive_with_context(&self, prefix: &str) -> String {
        let mut tracker = crate::blocker::BlockerSetTracker::default();
        let assessments = self.assess_blockers(&mut tracker, false);
        self.fix_directive_with_assessments(prefix, &assessments)
    }

    /// Diagnose the current set with classifier-owned playbooks while preserving
    /// recurrence state in `tracker` across QC rounds.
    fn assess_blockers(
        &self,
        tracker: &mut crate::blocker::BlockerSetTracker,
        workspace_progress: bool,
    ) -> Vec<crate::blocker::BlockerAssessment> {
        let mut evidence = self.blocking.clone();
        if let Some(raw) = self
            .raw_failure_log
            .as_ref()
            .filter(|raw| !raw.trim().is_empty())
        {
            evidence.push(raw.clone());
        }
        tracker.assess_all(&evidence, "objective QC", true, workspace_progress)
    }

    /// Render a directive from assessments already computed by the run-local
    /// tracker, so the prompt and the visible loop decision cannot disagree.
    fn fix_directive_with_assessments(
        &self,
        prefix: &str,
        assessments: &[crate::blocker::BlockerAssessment],
    ) -> String {
        let mut body = String::new();
        for b in &self.blocking {
            body.push_str("- ");
            body.push_str(b);
            body.push('\n');
        }
        let lead = if prefix.trim().is_empty() {
            String::new()
        } else {
            format!("{}\n\n", prefix.trim_end())
        };
        // B1#2: when the build/test fact read captured the failing output, append its
        // bounded verbatim tail so the fix turn works from the RAW compiler/test
        // evidence too (adaptable), not only the one-line distillation above. Absent
        // raw log (clean build / non-build findings) → byte-for-byte the plain form.
        let raw = match self.raw_failure_log.as_deref().map(str::trim) {
            Some(t) if !t.is_empty() => {
                format!("\n\n## Raw failing build/test output (verbatim tail)\n```text\n{t}\n```")
            }
            _ => String::new(),
        };
        let diagnosis = assessments
            .iter()
            .take(8)
            .map(crate::blocker::BlockerAssessment::prompt_block)
            .collect::<Vec<_>>()
            .join("\n\n");
        format!(
            "{lead}An objective check of what you just built surfaced problems that must be \
             fixed (these are real facts read from disk / review, not your memory):\n\
             {body}\n{diagnosis}\n\nFix the cause of each one yourself with your tools — edit/create \
             the real files — then RUN the project's own build and tests to confirm \
             they pass. When it is genuinely clean, end your turn and report honestly \
             what you fixed.{raw}"
        )
    }
}

fn diagnosed_blockers_for_prompt(findings: &[String], criterion: &str) -> String {
    let mut tracker = crate::blocker::BlockerSetTracker::default();
    tracker
        .assess_all(findings, criterion, true, false)
        .iter()
        .take(8)
        .map(crate::blocker::BlockerAssessment::prompt_block)
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn emit_blocker_assessments(
    events: &Arc<dyn EventSink>,
    assessments: &[crate::blocker::BlockerAssessment],
) {
    for item in assessments.iter().take(8) {
        events.emit(EngineEvent::Note(format!(
            "team · blocker {}/{} · {} · unchanged repeat {}",
            item.diagnosis.class.as_str(),
            item.diagnosis.fingerprint,
            item.disposition.as_str(),
            item.repeat_count,
        )));
    }
}

/// Run UmaDev's READ-ONLY QC pass over what the base just built — UmaDev judges, it
/// does not operate. The base's body did the building; here UmaDev only reads
/// reality. Checks, cheapest-first, each fail-open:
///
/// 1. **Honesty hard floor (UmaDev's own deterministic, read-only check)** — real
///    source files actually landed ([`crate::director::verify`] with
///    [`VerifyKind::SourcePresent`], which just reads disk). Zero source after a
///    claimed build is the decisive blocking finding (nothing was built) and short-
///    circuits the rest.
/// 2. **Build/test FACT read (optional, reuse of an existing reader)** — when a
///    project manifest is present, [`VerifyKind::BuildTest`] surfaces an objective
///    failure fact. This is positioned as reading a fact, NOT UmaDev's gate: the fix
///    directive asks the BASE to run its own build/test. A skipped check is neutral.
/// 3. **Optional fork review (read-only, borrows the brain)** — only when real code
///    exists, fork the review team on read-only sessions and collect blocking
///    findings ([`crate::director::review`]). Advisory: findings only seed the fix
///    directive the base acts on.
///
/// Single-writer preserved: every step is read-only (disk read / build-test read /
/// isolated read-only forks) — NOTHING here writes the workspace.
async fn run_auto_qc(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    route: Option<&RoutePlan>,
    last_turn_text: Option<&str>,
    // OBSERVED-tool corroboration: did the base ACTUALLY run a build/test/lint runner this
    // turn (a real `SessionEvent::ToolCall`, not reply prose)? Required — alongside a green
    // CLAIM in `last_turn_text` — before the duplicate build/test read is skipped. `false`
    // (the conservative default when no observation is available) → UmaDev runs its own read.
    ran_build_tool: bool,
) -> QcReport {
    events.emit(EngineEvent::Note("team · honesty + QC read".to_string()));
    let route_team = route.map(|r| r.team.as_slice());
    let mut blocking: Vec<String> = Vec::new();
    // B1#2: the failing build/test output's bounded raw tail (when the fact read
    // below fails) — threaded into the fix directive as adaptable raw evidence.
    let mut raw_failure_log: Option<String> = None;

    // 1. Honesty hard floor (UmaDev's own read-only check): did real source actually
    //    land? A claimed build with zero source is the decisive blocking finding —
    //    feed it back so the base's body actually writes the code. This floor is
    //    ALWAYS run, on EVERY tier — it is the non-negotiable "did anything get
    //    built" reality check, the one invariant the lean tier must never drop.
    let src = director::verify(options, events, VerifyKind::SourcePresent).await;
    if src.available && !src.passed {
        blocking.push(format!(
            "source-present: FAILED — {} (the build claimed done but no real source \
             files exist on disk; actually create the code with your tools)",
            src.evidence.first().cloned().unwrap_or_default()
        ));
        // No source means nothing to build/test or review — return now with the
        // decisive finding rather than reading over an empty tree.
        return QcReport {
            blocking,
            raw_failure_log: None,
        };
    }

    // CONTENT GOVERNANCE: scan what the base actually wrote for craft/quality
    // violations (emoji-as-icon, hardcoded colors, missing a11y, AI-slop,
    // swallowed errors, …). This is now the PRIMARY craft-enforcement pass for
    // EVERY backend. The real-time PreToolUse hook (claude only) deliberately no
    // longer blocks these — it screens just the irreversible-if-written floor
    // (a leaked secret / sensitive path), so the base's body is never pinned
    // mid-write for a fixable nit (which once left it producing ZERO output). The
    // craft is instead caught HERE and repaired by the feedback loop: the base
    // wrote the file, UmaDev reads it, folds violations into the fix directive,
    // and the base edits. Runs for all backends (claude included now). It is
    // CONTEXT-AWARE (`governance_scan` derives a `ProjectContext`), so a clean
    // static page is governed leniently (no false "missing CSP" on a page that
    // serves none). It runs BEFORE the lean short-circuit so even the lean fast
    // path keeps this moat — only the duplicate build + fork review are skipped
    // for a lean goal, never the craft floor. Fail-open: a clean / empty scan
    // contributes nothing, an unreadable file is skipped.
    let violations = crate::continuous::governance_scan(options);
    if !violations.is_empty() {
        events.emit(EngineEvent::Note(format!(
            "team · content governance flagged {} issue(s) in what the base wrote",
            violations.len()
        )));
        for v in violations {
            blocking.push(format!("[governance] {v}"));
        }
    }

    // LEAN TIER: for a small, clearly-lean goal (a todo/记账 single page, a bug fix,
    // a refactor — `planner::is_lean_build`) the heavy half of QC is pure overhead
    // over a base that already ran its own build inside its turn:
    //   - the `BuildTest` read re-runs the project's FULL build/test (a SECOND
    //     `npm install` + build — minutes), even though the base just ran it and
    //     the fix directive asks it to run it again, and
    //   - the fork review convenes a per-seat `fork()` team (independent base
    //     handshakes + full judge round-trips) — which for these kinds is ALREADY
    //     an empty team (`quality_team_for_kind` → `Vec::new()`), so it can only
    //     ever return "no blocking" anyway.
    // So for the lean tier we stop after the honesty hard floor + the content
    // governance scan: source present + no governance violation ⇒ clean. The
    // objective source-present floor AND the context-aware content governance both
    // ran above (the latter is the moat — kept even on the lean path); only the
    // duplicate build + fork review are skipped here. The heavyweight tiers below
    // are untouched. This is the single change that brings a simple page close to
    // "the base just did it" without dropping the content floor.
    // Fail-open + safe: `is_lean_build` only fires on a clearly-lean classification
    // (an unrecognised / real-product goal stays heavyweight), so a real product is
    // never under-checked by accident.
    //
    // DOCUMENT-AWARE (the token-burn fix): a document task (a PRD / spec / design doc
    // / report — `is_document_task`) and, equivalently, any turn that produced ZERO
    // source on disk (a documentation delivery — the source-present floor above
    // already neutrally passed it) is in the SAME position as a lean goal: there is
    // no code to re-build or fork-review, so the duplicate build + the fork team are
    // pure overhead over a .md. The governance scan above still ran (the moat is
    // kept); only the code-shaped half of QC is skipped. A real product (non-empty
    // source, non-document) is untouched.
    // M2: gate the lean short-circuit on the ROUTE's brain-decided `depth`, NOT a
    // re-derived keyword `classify(requirement)`. A deliberate (Standard/Deep) build
    // whose requirement happens to read "lean" must take the FULL gate (build/test +
    // the acceptance floor + fork review) — keying off the keyword classifier could
    // DISAGREE with the brain's depth and let a real build settle after only source-
    // present + governance (which compounds H1). A deliberate empty build was already
    // caught by the source-present floor above, so this only fast-paths a genuinely
    // light/non-deliberate goal. Fail-open: no route in hand → keyword classify (the
    // single-turn legacy behaviour) is retained.
    let route_is_deliberate = route.map(|r| r.depth.is_deliberate()).unwrap_or(false);
    if !route_is_deliberate
        && (crate::planner::is_lean_build(&options.requirement)
            || crate::planner::is_document_task(&options.requirement)
            || crate::acceptance::source_files(&options.project_root).is_empty())
    {
        events.emit(EngineEvent::Note(
            "team · lean / document goal — source check done, skipping the duplicate build + fork review"
                .to_string(),
        ));
        return QcReport {
            blocking,
            raw_failure_log: None,
        };
    }

    // 2. Build/test FACT read (optional, when a manifest is present). UmaDev reads
    //    the objective result; the FIX is the base's job (the fix directive tells it
    //    to run its own build/test). A skipped check is neutral (fail-open).
    //
    // INCREMENTAL VERIFY (Wave 3): the base's body holds the build/test tools and,
    // inside its turn, usually already ran them. When its reply explicitly reports a
    // PASSED build/test (and shows NO failure signal — `base_ran_build_test_clean`,
    // conservative by contract), re-running the project's FULL build/test here is a
    // pure-overhead duplicate (an `npm install` + build can be minutes). So we read
    // the base's own already-run result instead of re-running the whole suite. This
    // skips ONLY the duplicate build/test read; the source-present hard floor + the
    // content-governance scan above and the fork review below are UNCHANGED — the
    // objective floor still governs. Fail-open + safe: any ambiguity or any failure
    // whiff in the reply (or no reply at all) falls back to running our own read, so
    // a real failure is never skipped over.
    //
    // HONESTY TIGHTENING: a green CLAIM in the reply is now honored to skip our read
    // ONLY IF it is CORROBORATED by an OBSERVED build/test/lint runner this turn
    // (`ran_build_tool`, set from the actual tool-call stream). The prose claim alone —
    // even a creatively-worded "cargo test passed, exit 0" the base merely NARRATED with
    // no runner ever invoked — no longer skips the floor: absent corroboration, UmaDev
    // runs its OWN verify. That is a re-verify, never a false FAIL — a genuinely clean
    // build simply re-passes; only an unproven claim is downgraded from "trusted" to
    // "checked". Fail-open: no reply / no observation → run our own read.
    let base_already_verified = ran_build_tool
        && last_turn_text
            .map(crate::gates::base_ran_build_test_clean)
            .unwrap_or(false);
    if base_already_verified {
        events.emit(EngineEvent::Note(
            "team · base already ran build/test green this turn — trusting its result, skipping the duplicate full build"
                .to_string(),
        ));
    } else {
        // B1#2: the log-capturing variant — a FAILED read keeps the raw output's
        // bounded tail alongside the one-line blocking distillation. Same check,
        // same note as `director::verify(BuildTest)` emitted before.
        events.emit(EngineEvent::Note("team · verify build-test".to_string()));
        let (bt, raw) = director::verify_build_test_raw(options).await;
        if let Some(line) = build_test_blocking(&bt) {
            blocking.push(line);
            raw_failure_log = raw;
        }
    }

    // 2b. REQUIRED ACCEPTANCE FLOOR (Wave 4, §L4 / task 2). For a DELIBERATE build
    //     (Standard/Deep) the spec→tasks + spec→code verification becomes a REQUIRED
    //     blocking signal on the default path — not legacy-only. We fold in:
    //       - coverage gaps   (FR-NNN declared in the PRD but no task cites it),
    //       - acceptance gaps (planned API endpoints with no implementation),
    //       - contract drift  (frontend fetch URLs with no matching backend route),
    //       - runtime-proof   (a written runtime-proof.json that did NOT verify).
    //     For a BUGFIX, additionally require a reproduction test (red→green): a fix
    //     with no test asserting the bug is a fix that can silently regress.
    //     Lean/Fast already returned above, so this only runs on the heavyweight
    //     path — speed is preserved. Each contributor is fail-open (a missing
    //     artifact / unreadable doc yields no gap, never a false alarm), so a check
    //     that genuinely can't run is a NEUTRAL skip, not a fabricated failure.
    if route.map(|r| r.depth.is_deliberate()).unwrap_or(false) {
        // Compute scope drift once. New surfaces and edits to existing files are both
        // execution-contract violations; a missing diff remains a silent fail-open.
        let scope: Vec<crate::scope_creep::ScopeFinding> =
            crate::plan_state::load(&options.project_root)
                .map(|plan| crate::scope_creep::unclaimed_changes(&options.project_root, &plan))
                .unwrap_or_default();
        for line in acceptance_floor_blocking_with(options, route, Some(&scope)) {
            blocking.push(line);
        }
    }

    // Run fork critics only over a deterministically viable candidate; otherwise
    // repair the objective blockers first and review the next bounded round.
    if quality_evidence::should_run_critic_review(&blocking) {
        let review = match route_team {
            Some(seats) => director::review_with_seats(session, options, events, seats).await,
            None => {
                director::review(
                    session,
                    options,
                    events,
                    crate::continuous::ReviewKind::Quality,
                )
                .await
            }
        };
        blocking.extend(review_blocking(&review));
    } else {
        events.emit(EngineEvent::Note(
            quality_evidence::DEFERRED_CRITIC_REVIEW_NOTE.to_string(),
        ));
    }

    QcReport {
        blocking,
        raw_failure_log,
    }
}

/// The REQUIRED acceptance floor for a deliberate build (Wave 4, §L4 / task 2) —
/// the spec→tasks + spec→code verification, promoted to a blocking signal on the
/// default deliberate path. Folds in coverage gaps, interface-acceptance gaps,
/// frontend↔contract drift, an unverified runtime-proof, and (for a Bugfix) a
/// missing reproduction test. Each contributor is fail-open: a missing artifact /
/// unparseable doc yields no gap (a neutral skip), so a check that genuinely
/// cannot run never fabricates a failure. Returns the blocking lines (empty =
/// the floor is clean OR nothing could be checked).
#[cfg(test)]
fn acceptance_floor_blocking(options: &RunOptions, route: Option<&RoutePlan>) -> Vec<String> {
    acceptance_floor_blocking_with(options, route, None)
}

/// The acceptance-floor core, optionally reusing scope drift the caller already
/// computed.
///
/// `unclaimed_changes` is not cheap: it stages the whole work-tree into the shadow index
/// (`git add -A --force`), reads a full run diff, and runs a repo-wide backend-route
/// extraction. A QC pass reuses that one snapshot so every scope finding is judged
/// against the same workspace state.
///
/// `scope: None` keeps the standalone behaviour (compute it here) for callers that have
/// no precomputed set.
fn acceptance_floor_blocking_with(
    options: &RunOptions,
    route: Option<&RoutePlan>,
    scope: Option<&[crate::scope_creep::ScopeFinding]>,
) -> Vec<String> {
    let slug = options.effective_slug();
    let root = &options.project_root;
    let mut out: Vec<String> = Vec::new();

    // spec→tasks: a declared FR-NNN no task covers (a requirement at risk of being
    // silently dropped). Fail-open: no PRD / no FR ids → empty.
    for r in crate::coverage::uncovered_requirements(root, &slug) {
        out.push(format!(
            "coverage gap: requirement {r} is declared in the PRD but no task implements it — \
             build it, or remove it from scope honestly"
        ));
    }
    // spec→code: a planned API endpoint with no implementation evidence on disk.
    // Fail-open: no architecture doc / no endpoints → empty.
    for g in crate::acceptance::task_acceptance_gaps(root, &slug) {
        out.push(format!(
            "acceptance gap: planned endpoint not implemented — {g}"
        ));
    }
    // frontend↔backend contract drift: a fetch URL with no matching backend route.
    // Reuses the same `quality_floor` machinery the legacy gate used; here we pull
    // ONLY the qa half (coverage/acceptance already counted above are re-derived,
    // so we filter to the genuinely-new "contract drift:" lines to avoid dup text).
    let (qa_floor, _sec) = crate::continuous::quality_floor(options);
    for line in qa_floor.split('\n').map(str::trim) {
        let line = line.trim_start_matches("- ").trim();
        if line.starts_with("contract drift:") {
            out.push(line.to_string());
        }
    }

    // runtime-proof: when a `runtime-proof.json` was written (by `verify --runtime`)
    // and it did NOT verify, that is a real, recorded failure (the app didn't boot /
    // a route didn't answer). Absent file → neutral skip (the runtime check simply
    // wasn't run this loop; we never fabricate a "didn't boot" from a missing file).
    if let Some(line) = runtime_proof_blocking(root) {
        out.push(line);
    }

    // ARCHITECTURE FITNESS (UD-CODE-006, spec §3.6): the REPO-GLOBAL
    // half of the anti-spaghetti floor — the architecture doc's declared
    // layer-dependency rules (`## Layering` order / `LAYER-RULE: a !-> b`),
    // verified against the repo-map's resolved import edges. The touched-file
    // rules (god-file / added-code clones / comment hygiene) run at the STEP level in
    // `drive_build_step`, where the changed-file set is known from the pre-step
    // baseline; here the empty touched set makes them a silent no-op by
    // construction. Fail-open: no doc / no declaration / no resolved edges →
    // empty, never a fabricated failure.
    for f in crate::arch_fitness::arch_fitness_findings(root, &slug, &[]) {
        if f.blocking {
            out.push(f.message);
        }
    }

    // SCOPE CREEP — the DUAL of the coverage check above. Coverage asks "which declared
    // requirement has no step?" (UNDER-building). This asks the opposite: "which CHANGE
    // belongs to no step?" (OVER-building) — an unplanned dependency, an unplanned
    // source file, an unplanned public route: work nobody sized, nobody asked for, and
    // nobody reviewed. New surfaces and edits to existing files both violate the
    // execution contract. A missing run baseline or unreadable diff remains fail-open;
    // malformed/missing step file declarations are rejected by plan preflight.
    // See [`crate::scope_creep`]. Reuse the caller's set when it already paid for one.
    match scope {
        Some(findings) => out.extend(
            findings
                .iter()
                .filter(|f| f.blocking)
                .map(|f| f.message.clone()),
        ),
        None => {
            if let Some(plan) = crate::plan_state::load(root) {
                for f in crate::scope_creep::unclaimed_changes(root, &plan) {
                    if f.blocking {
                        out.push(f.message);
                    }
                }
            }
        }
    }

    // BUGFIX: require a reproduction test (red→green). A fix that lands no test
    // asserting the bug can silently regress. Fail-open: only fires when the route
    // is classified Bugfix AND we can read the source tree.
    if route
        .map(|r| r.kind == crate::planner::TaskKind::Bugfix)
        .unwrap_or(false)
        && !has_reproduction_test(root)
    {
        out.push(
            "bugfix without a reproduction test: add a test that FAILS on the bug before the fix \
             and PASSES after (red→green), and keep the rest of the suite green — a fix with no \
             test asserting the bug can silently regress"
                .to_string(),
        );
    }

    // DESIGN-SYSTEM CONFORMANCE (UD-CODE-007, spec §3.7): the deterministic half
    // of the design moat. The firmware PREACHES token discipline, paired
    // foregrounds, measured contrast, and one committed hue — but a prompt is not
    // a floor. This is the floor:
    //
    //   - `007a` schema     — a real system (>= 6 color roles each with a paired
    //                         `on-` foreground, a >= 4-step type scale at ratio
    //                         >= 1.125, a 4pt spacing scale, a radius scale,
    //                         >= 2 durations + >= 1 easing), not `:root{--bg:#000}`.
    //   - `007b` contrast   — every DECLARED (surface, on-surface) pair MEASURED
    //                         with the WCAG formula in pure Rust (no browser, no
    //                         deps): 4.5:1 body, 3:1 large/UI.
    //   - `007c` drift      — the UI actually DRAWS from the token set (a literal
    //                         color / font / radius / size off the scale is drift).
    //   - `007d` hue        — no AI indigo/violet primary/accent unless the
    //                         requirement asked for purple.
    //   - `007e` lints      — the register-scoped design-lint registry; only its
    //                         small P0 tier blocks, the advisory tier is a Note.
    //   - `007f` direction  — the designer decided a DIRECTION before any token.
    //
    // Fail-open at EVERY edge: no `design-tokens.{json,css}` → the report is
    // `unavailable` and contributes nothing (a project that never asked for a
    // design system is completely unaffected); no UIUX doc → no direction finding.
    // Only a project that SHIPPED a design system is held to the contract it
    // implicitly claimed.
    let register = crate::design_system::register_for_project(root, &slug);
    let report = crate::design_system::verify_design_system(root, &options.requirement, register);
    for f in report.blocking() {
        out.push(f.message.clone());
    }
    // `007f` is gated on the ROUTE, not on a file: an `output/*-uiux.md` left behind by
    // an earlier UI run (or already present in a brownfield repo) is not a reason to
    // hold a backend-only task to a design contract it never entered. No route → no UI
    // claim → nothing (fail-open).
    let needs_ui = route.is_some_and(RoutePlan::needs_ui);
    for f in crate::design_system::visual_direction_findings(root, &slug, needs_ui) {
        if f.blocking {
            out.push(f.message);
        }
    }

    out
}

/// Map a [`VerifyResult`] from a build/test check to a blocking line, or `None` when
/// it passed / was skipped (an unavailable check is neutral — fail-open).
fn build_test_blocking(r: &VerifyResult) -> Option<String> {
    if !r.available || r.passed {
        return None;
    }
    let detail = if r.evidence.is_empty() {
        String::new()
    } else {
        format!(" — {}", r.evidence.join("; "))
    };
    Some(format!("verify build-test: FAILED{detail}"))
}

/// Pull every reason a required review is not clean. Operational unavailability is
/// distinct in [`ReviewResult`] but blocks commercial completion just like a
/// residual must-fix finding; it must never disappear as an empty pass.
fn review_blocking(r: &ReviewResult) -> Vec<String> {
    let mut findings = r.blocking.clone();
    findings.extend(
        r.unavailable
            .iter()
            .map(|item| format!("review unavailable: {item}")),
    );
    findings
}

#[cfg(test)]
#[path = "director_loop_tests.rs"]
mod director_loop_tests;
