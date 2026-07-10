//! The director build loop — the USB / smart-hardware model of
//! `docs/AGENT_WIELDS_BASE_ARCHITECTURE.md` (simplified: NO marker protocol).
//!
//! ## The model: UmaDev is firmware; the base is the brain + hands
//!
//! UmaDev is a smart device with its own firmware — a senior team-director
//! identity, engineering taste, accumulated knowledge, governance, and memory —
//! but **no compute of its own**. Plugged into a base (claude / codex / opencode)
//! over the continuous session, it **borrows the base's intelligence and hands**
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
//!    own tools**. Then re-read. **Bounded** by [`MAX_QC_ROUNDS`].
//! 4. Clean (or no code claimed — a chat/plan answer) → done; report honestly.
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
//! 2. **Objective floor untouched.** The source-present floor is a deterministic,
//!    read-only reality check; review verdicts stay advisory (they only seed a fix
//!    directive the base acts on, they never themselves decide "done"). The caller's
//!    source-present hard-gate still runs at the boundary, unchanged.
//! 3. **Governance + audit.** Every base turn (the first build and every fix pass)
//!    drives the SAME governed/audited session; the PreToolUse hook still fires
//!    under every write.
//! 4. **No new endpoint.** Every QC review reads over the SAME borrowed brain + its
//!    `fork()`; no extra model endpoint, no API key.
//! 5. **Fail-open.** A QC read that can't run degrades to "no problem found" (a
//!    review that can't fork accepts), never a false failure that wedges the loop. A
//!    dead session is an honest `Failed`, never a panic.
//! 6. **Reversible.** This loop is the DEFAULT `/run` path; the legacy fixed
//!    pipeline (`UMADEV_LEGACY_PIPELINE=1`) is untouched.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use umadev_runtime::{ApprovalDecision, BaseSession, SessionEvent, StreamEvent, TurnStatus, Usage};

use crate::director::{self, ReviewResult, VerifyKind, VerifyResult};
use crate::events::{EngineEvent, EventSink};
use crate::plan_state::{self, Plan, StepStatus};
use crate::router::RoutePlan;
use crate::runner::RunOptions;
use crate::trust::requires_confirmation_with_ledger;
use umadev_spec::Phase;

/// The hard ceiling on auto-QC feedback-fix rounds in one `/run`. One round is: the
/// base builds (or fixes) end to end, then UmaDev runs its objective QC pass. A
/// clean pass ends the loop immediately, so a simple goal that builds correctly the
/// first time spends ZERO fix rounds. The cap only bounds a goal that keeps failing
/// QC — after it, the loop ends and the caller's source-present hard-gate has the
/// final say. Mirrors the proven bounded-rework shape (`continuous::MAX_REWORK_ROUNDS`)
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
/// `umadev_host`), falling back to [`DEFAULT_IDLE_TIMEOUT_SECS`]. A non-positive /
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
/// `UMADEV_TOOL_IDLE_TIMEOUT_SECS`, falling back to [`DEFAULT_TOOL_IDLE_TIMEOUT_SECS`]
/// (5 min). It is how OFTEN the watchdog re-checks the base is alive while a tool runs,
/// NOT a cap on how long the tool may take — a tool of any duration with a live base
/// keeps waiting (see [`next_event_idle`]). A non-positive / unparseable value falls
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
/// [`tool`](Self::tool) interval and, each time the interval elapses with no event,
/// re-checks the base is alive — a live base keeps waiting, only a DEAD base (or the
/// overall run-budget deadline) settles (see [`next_event_idle`]). When NO tool is in
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
        SessionEvent::ToolCall { .. } => Some(true),
        SessionEvent::ToolResult { .. } => Some(false),
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
pub(crate) fn run_budget() -> Duration {
    let secs = std::env::var("UMADEV_RUN_BUDGET_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_RUN_BUDGET_SECS);
    Duration::from_secs(secs)
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
const MAX_TRANSIENT_RETRIES: u32 = 3;

/// The base unit of the exponential transient backoff (attempt 1 → 1×, 2 → 2×, 3 →
/// 4× this, capped at [`TRANSIENT_BACKOFF_CAP`]). 2s keeps a single retry quick yet
/// gives a rate limit room to clear before the next attempt.
const TRANSIENT_BACKOFF_BASE: Duration = Duration::from_secs(2);

/// The cap on any single transient backoff wait, so the schedule stays bounded even
/// if [`MAX_TRANSIENT_RETRIES`] grows. 30s is the longest a transient retry ever waits.
const TRANSIENT_BACKOFF_CAP: Duration = Duration::from_secs(30);

/// The backoff wait before transient-retry `attempt` (1-based): exponential off `base`,
/// capped at `cap` — attempt 1 → `base`, 2 → 2×`base`, 3 → 4×`base`, … never exceeding
/// `cap`. Pure + total + bounded: the shift is clamped (so it can never overflow) and a
/// multiply overflow saturates at `cap`, so the schedule is deterministic and can never
/// balloon. `base`/`cap` are parameters so the test drives a tiny, fast window.
fn transient_backoff_wait(base: Duration, cap: Duration, attempt: u32) -> Duration {
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
    /// The build finished — the base built end to end and UmaDev's auto-QC either
    /// passed or exhausted its bounded fix budget. The caller then runs the
    /// objective source-present hard-gate to verify reality.
    Done {
        /// The base's final assistant text — the caller reads it for a "claimed a
        /// build" check against the real source files.
        reply: String,
    },
    /// The session died / a turn failed — an honest hard stop, never disguised as
    /// success. Carries a machine-true reason.
    Failed(String),
}

/// Drive an explicit `/run` (full product build) through the **director build loop**
/// — the USB-model engine. ONE live [`BaseSession`] is the director's brain; the
/// firmware (team identity + craft + knowledge) is already injected by the caller
/// into `first_directive` (and, on the TUI path, the system prompt). The base builds
/// the goal end to end with its OWN internal agentic loop; then UmaDev runs an
/// objective QC pass ITSELF ([`run_auto_qc`]) and, if QC found blocking problems,
/// feeds ONE fix directive back over the same session for another pass — bounded by
/// [`MAX_QC_ROUNDS`].
///
/// `first_directive` is the goal framing the caller already built (e.g.
/// [`crate::experts::director_build_directive`]). The caller owns the session
/// lifetime (and the run-lock) and `end()`s it after this returns.
///
/// Floor preserved (see the module docs): single-writer, governance + audit,
/// advisory review, objective verify, fail-open, no endpoint. The loop never blocks
/// the host — every failure mode degrades to a graceful settle.
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

    // Plan (read-only) mode must NOT drive a code-writing BUILD. The continuous engine guards
    // its executing phases with `plan_mode_skip` (continuous.rs), but director_loop had NO such
    // guard - so a build SPAWNED in Plan drove every step while the approval floor DENIED each
    // write (Plan escalates every non-read action), producing ZERO source files and a confusing
    // deny-storm before aborting (the opencode "edit -> pyproject.toml 按拒绝处理" report). Stop
    // cleanly with the actionable read-only notice: no code is written, the source-present
    // hard-gate does NOT fire (the reply claims no build), and the user is told to switch to
    // /mode guarded or auto to execute.
    if !options.mode.executes() {
        events.emit(EngineEvent::Note(
            umadev_i18n::tl("continuous.plan_mode_skip").to_string(),
        ));
        events.emit(EngineEvent::Note(
            umadev_i18n::tl("mode.plan.gate").to_string(),
        ));
        return DirectorLoopOutcome::Done {
            reply: umadev_i18n::tl("continuous.plan_mode_skip").to_string(),
        };
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
    let deadline = std::time::Instant::now() + run_budget();

    // 2. Synthesise + persist + post the owned plan. Fail-open at every step: a
    //    `None` plan (offline / no fork / unparseable) simply means no checklist —
    //    the build loop below runs exactly as it does today. Plan synthesis runs on
    //    a READ-ONLY fork (single-writer preserved); it never writes the workspace.
    let plan = match route {
        Some(r) => synthesize_and_post_plan(session, options, events, r, deadline).await,
        None => None,
    };

    drive_director_loop_with_idle(
        session,
        options,
        events,
        first_directive,
        plan,
        route,
        IdleBudget::from_env(),
        deadline,
    )
    .await
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

/// Whether `plan` still has work left to drive — at least one step that is NOT
/// terminal (`Done` / `Blocked`). A fully `Done`/`Blocked` plan has nothing to
/// resume.
fn plan_has_incomplete_step(plan: &Plan) -> bool {
    plan.steps
        .iter()
        .any(|s| matches!(s.status, StepStatus::Pending | StepStatus::Active))
}

/// Reset any `Active` step back to `Pending` so a resume re-drives the step that was
/// interrupted mid-flight. The scheduler only surfaces `Pending` steps via
/// [`Plan::ready_steps`], so a step persisted as `Active` (the TUI closed while it
/// ran) would otherwise be stranded — never re-driven, never finished. The old
/// subprocess is gone; the fresh session must re-run it from a clean state. `Done`
/// steps are left exactly as persisted (they are never re-driven), and `Blocked`
/// steps stay an honest gap. Returns the count reset (0 = nothing was Active).
fn reset_active_to_pending(plan: &mut Plan) -> usize {
    let mut reset = 0;
    for s in &mut plan.steps {
        if s.status == StepStatus::Active {
            s.status = StepStatus::Pending;
            reset += 1;
        }
    }
    reset
}

/// Load the persisted plan for a RESUME, returning it ONLY when it is genuinely
/// resumable: it parses AND has at least one incomplete (`Pending`/`Active`) step.
/// Any `Active` step is reset to `Pending` (the interrupted step must re-drive on the
/// fresh session — see [`reset_active_to_pending`]). Fail-open: an absent / corrupt /
/// fully-terminal plan → `None`.
fn load_resumable_plan(root: &Path) -> Option<Plan> {
    let mut plan = plan_state::load(root)?;
    // Item C staleness: re-open any step whose upstream doc artifact changed since
    // the last recorded save, BEFORE the resumability check - a changed upstream can
    // revive an otherwise-complete plan so the director re-derives the poisoned
    // subtree instead of trusting it.
    invalidate_stale_steps(root, &mut plan);
    if !plan_has_incomplete_step(&plan) {
        return None; // every step Done/Blocked → nothing left to resume
    }
    reset_active_to_pending(&mut plan);
    Some(plan)
}

// ── Artifact staleness (item C) ── record output/ doc versions on each plan save,
//    and on resume re-open steps whose upstream doc changed. All read-only /
//    best-effort / fail-open: an unreadable dir, an empty store, or an unmapped
//    file is simply skipped and never blocks the run.

/// Canonical artifact name for an `output/<slug>-<kind>.md` file, or `None`.
fn artifact_name_from_filename(fname: &str) -> Option<&'static str> {
    let stem = fname.strip_suffix(".md")?;
    if stem.ends_with("-architecture") {
        Some("architecture")
    } else if stem.ends_with("-prd") {
        Some("prd")
    } else if stem.ends_with("-uiux") {
        Some("uiux")
    } else {
        None
    }
}

/// [`crate::critics::ArtifactKind`] for a canonical doc-artifact name.
fn artifact_kind_from_name(name: &str) -> Option<crate::critics::ArtifactKind> {
    use crate::critics::ArtifactKind as A;
    match name {
        "architecture" => Some(A::Architecture),
        "prd" => Some(A::Prd),
        "uiux" => Some(A::Uiux),
        _ => None,
    }
}

/// The current content-version of each `output/` doc artifact, keyed by canonical
/// name. Read-only + fail-open (an unreadable dir/file is skipped).
fn current_artifact_versions(root: &Path) -> Vec<(String, String)> {
    let mut v = Vec::new();
    let Ok(entries) = std::fs::read_dir(root.join("output")) else {
        return v;
    };
    for e in entries.flatten() {
        let fname = e.file_name().to_string_lossy().into_owned();
        let Some(name) = artifact_name_from_filename(&fname) else {
            continue;
        };
        if let Ok(content) = std::fs::read_to_string(e.path()) {
            v.push((name.to_string(), crate::critics::artifact_version(&content)));
        }
    }
    v
}

/// Record the current `output/` doc versions into the staleness store - the "we
/// built against these versions" checkpoint, called after each plan save.
fn record_artifact_versions(root: &Path) {
    let current = current_artifact_versions(root);
    if current.is_empty() {
        return;
    }
    let map = current.into_iter().collect();
    crate::critics::write_artifact_versions(root, &map);
}

/// Re-open any plan step whose upstream doc artifact CHANGED since the last recorded
/// save (via [`Plan::invalidate_stale`]). Fail-open: empty store / no change = no-op.
fn invalidate_stale_steps(root: &Path, plan: &mut Plan) {
    let current = current_artifact_versions(root);
    if current.is_empty() {
        return;
    }
    let stale = crate::critics::stale_artifacts(root, &current);
    if stale.is_empty() {
        return;
    }
    use crate::critics::ArtifactKind as A;
    let mut kinds: Vec<A> = Vec::new();
    for n in &stale {
        let Some(k) = artifact_kind_from_name(n) else {
            continue;
        };
        kinds.push(k);
        // A stale SOURCE doc must also invalidate the steps that read its DERIVED typed
        // contracts (mirrors CriticArtifacts::present): FrontendEngineer reads ApiContract
        // (derived from architecture.md), NOT Architecture directly, so without expanding
        // to the derived kinds a revised architecture.md never reopened the frontend step
        // and it kept building against a STALE contract.
        match k {
            A::Architecture => {
                kinds.push(A::ApiContract);
                kinds.push(A::DataModel);
            }
            A::Uiux => kinds.push(A::DesignTokens),
            A::Prd => kinds.push(A::Acceptance),
            _ => {}
        }
    }
    plan.invalidate_stale(&kinds);
}

/// Whether `root` holds a director-loop run that can be RESUMED on a fresh session:
/// either a persisted `.umadev/plan.json` with an incomplete step, OR a
/// `.umadev/workflow-state.json` parked at a gate / in a non-terminal phase. Pure,
/// read-only, fail-open — a missing/corrupt plan or state is simply "not resumable"
/// (never a panic). The TUI uses this so `/continue` on a fresh session re-attaches
/// to the previous run instead of telling the user to restart the whole pipeline.
#[must_use]
pub fn has_resumable_run(root: &Path) -> bool {
    // A persisted plan with remaining work is the strongest resume signal.
    if load_resumable_plan(root).is_some() {
        return true;
    }
    // Else a workflow state parked at a gate, or short of the terminal `delivery`
    // phase, is also resumable (a run that produced state but no plan — e.g. the
    // legacy walk, or a build interrupted before the plan was synthesised).
    if let Some(state) = crate::state::read_workflow_state(root) {
        if !state.active_gate.trim().is_empty() {
            return true;
        }
        if state.phase != Phase::Delivery.id() {
            return true;
        }
    }
    false
}

/// **Cross-session resume** — re-attach to a persisted director-loop run on a FRESH
/// base session instead of synthesising a new plan.
///
/// Loads `.umadev/plan.json`; when a RESUMABLE plan exists (≥1 incomplete step) it
/// re-emits [`EngineEvent::IntentDecided`] + [`EngineEvent::PlanPosted`] so the TUI
/// re-renders the checklist with the already-`Done` steps checked, then drives ONLY
/// the remaining steps via [`drive_plan_steps`] — which walks [`Plan::ready_steps`],
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
    // MED-3: a RESUME must honor the same Plan (read-only) gate as a fresh routed run.
    // Without this, `/continue` in Plan mode re-drove every remaining step while the
    // approval floor denied each write (Plan escalates all non-read actions) — the same
    // deny-storm the fresh path already guards at `drive_director_loop_routed`. Stop
    // cleanly with the read-only notice instead of resuming into a write storm.
    if !options.mode.executes() {
        events.emit(EngineEvent::Note(
            umadev_i18n::tl("continuous.plan_mode_skip").to_string(),
        ));
        events.emit(EngineEvent::Note(
            umadev_i18n::tl("mode.plan.gate").to_string(),
        ));
        return Some(DirectorLoopOutcome::Done {
            reply: umadev_i18n::tl("continuous.plan_mode_skip").to_string(),
        });
    }

    let mut plan = load_resumable_plan(&options.project_root)?;

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

    // One shared clock for the resumed build (same as a fresh routed run).
    let deadline = std::time::Instant::now() + run_budget();
    let idle = IdleBudget::from_env();
    // Drive ONLY the remaining steps. `drive_plan_steps` schedules by readiness, so
    // the already-Done steps are skipped and only the Pending ones drive; it persists
    // the plan + finalizes exactly as a fresh deliberate build does. A first-step
    // drive failure returns `None` (the caller fails open to a fresh run).
    drive_plan_steps(session, options, events, route, &mut plan, idle, deadline).await
}

/// Synthesise the owned plan, persist it best-effort, and emit [`EngineEvent::PlanPosted`].
/// Returns the plan when one was produced, else `None` (the caller then runs the
/// existing single-turn build). Fully fail-open: synthesis / persistence failures
/// degrade to `None` / a skipped write, never an error.
async fn synthesize_and_post_plan(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    route: &RoutePlan,
    deadline: std::time::Instant,
) -> Option<Plan> {
    // A plan is warranted whenever there's a BUILD to make visible — every Build
    // route, even a lean single-page one, gets a (proportionally short) plan so the
    // user SEES the director think, not just a deliberate/deep one. A fast chat /
    // explain / quick-edit needs no DAG (and would just pay a fork round-trip for
    // nothing).
    if !(matches!(route.class, crate::router::RouteClass::Build) || route.depth.is_deliberate()) {
        return None;
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
    let plan = plan_state::synthesize_plan(session, options, &options.requirement, route, deadline)
        .await?;
    // Persist best-effort; a failed write is ignored (fail-open — never blocks).
    let _ = plan_state::save(&plan, &options.project_root);
    // Sync the 9-phase workflow state off its initial `research` value the moment a
    // plan exists — so `/status` stops reporting "research / all pending" while the
    // build is actually planning + working. Fail-open (swallows write errors).
    sync_phase_from_plan(&plan, options);
    events.emit(EngineEvent::plan_posted(&plan));
    Some(plan)
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
    let mut last_reply = String::new();

    for round in 0..MAX_QC_ROUNDS {
        // Wall-clock ceiling (graceful): a fix round past the budget is abandoned —
        // the build so far stands on its own deterministic floor (the source-present
        // hard-gate the caller runs). Round 0 (the build itself) always runs.
        if round > 0 && std::time::Instant::now() >= deadline {
            events.emit(EngineEvent::Note(
                "team · time budget reached — settling with the current build (raise \
                 UMADEV_RUN_BUDGET_SECS for more fix rounds)"
                    .to_string(),
            ));
            break;
        }
        // Plan visibility (Wave 1): mark the ready BUILD steps Active before this
        // turn drives the base over them, so the checklist shows live progress. The
        // base still executes the whole goal in one turn this wave (step-by-step
        // `summon` driving is Wave 2); here we surface the plan's motion. Fail-open:
        // no plan → nothing emitted, current behaviour.
        if round == 0 {
            mark_ready_steps(&mut plan, events, StepStatus::Active);
        }

        // 1. Drive ONE end-to-end base turn (build, or fix-the-QC-findings). The
        //    base runs its own agentic tool loop (PM→…→QA internally) and writes
        //    real files under the run-lock the caller holds (single-writer).
        let turn =
            match drive_one_turn(session, options, events, next_directive, idle, deadline).await {
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
        last_reply = turn.text.clone();

        // 2. On the FIRST turn only: if the base didn't claim it built/changed code
        //    (a chat / plan / "I read it" answer), there is nothing to QC — settle.
        //    This keeps a simple goal the base just answered directly from being
        //    forced through QC. A FIX turn (round >= 1) is NEVER short-circuited
        //    here: the previous QC already proved there were blocking problems, so
        //    the fix MUST be re-verified — a fix reply that only says "confirmed it
        //    passes" (no change-verb) must not be mistaken for "nothing to check"
        //    and settle with the problems still unfixed. QC is read-only + cheap.
        if round == 0 && !crate::gates::claims_code_changes(&turn.text) {
            // No code claimed → nothing the plan describes actually ran; leave the
            // steps as-is (the caller decides) and settle. SIZING calibration: a route
            // that mutated nothing is a TRIVIAL actual outcome — a route sized as a real
            // build here OVER-sized the turn (advisory, fail-open; see `record_run_sizing`).
            record_run_sizing(options, route, crate::sizing_calibration::SizeRank::Trivial);
            return DirectorLoopOutcome::Done { reply: last_reply };
        }

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
            // ACTIVE FACT-RECORDING BACKSTOP (single-turn path): this clean build
            // turn changed code (it passed `claims_code_changes` on round 0), so it
            // is a real work turn — extract its durable facts ourselves and persist
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
            return DirectorLoopOutcome::Done { reply: last_reply };
        }

        // 5. QC found blocking problems. Out of fix budget → settle (the caller's
        //    source-present hard-gate is the objective backstop).
        if round + 1 >= MAX_QC_ROUNDS {
            events.emit(EngineEvent::Note(
                "team · auto-QC reached its fix-round budget — settling (objective hard-gate decides reality)"
                    .to_string(),
            ));
            // The objective hard-gate decides reality; the plan steps stay where
            // they are (Active), honestly reflecting that QC didn't fully clear.
            // Persist the final state for resume.
            persist_plan(&plan, options);
            // Sync the 9-phase state at a NON-clean settle: never claim `delivery` —
            // advance only to the furthest phase that actually completed (no plan / no
            // Done step → keep the in-progress anchor). Fail-open.
            finalize_phase_from_plan_opt(&plan, options, false);
            // SIZING calibration: the cheap path burned its whole QC fix budget without
            // clearing → the work was HEAVIER than the single-turn sizing assumed.
            record_run_sizing(options, route, crate::sizing_calibration::SizeRank::Heavy);
            return DirectorLoopOutcome::Done { reply: last_reply };
        }

        // 6. Fold the QC findings into ONE fix directive and feed it back over the
        //    USB channel for another build pass → re-QC.
        next_directive = qc.fix_directive();
    }

    // Loop fell through (exhausted the bounded rounds) — persist the plan's final
    // state for resume; reality is the caller's hard-gate.
    persist_plan(&plan, options);
    // Non-clean settle (the bounded rounds didn't fully clear): honest phase only.
    finalize_phase_from_plan_opt(&plan, options, false);
    // SIZING calibration: exhausting the bounded rounds means the work outran the
    // single-turn sizing → HEAVY actual outcome. Advisory, fail-open.
    record_run_sizing(options, route, crate::sizing_calibration::SizeRank::Heavy);
    DirectorLoopOutcome::Done { reply: last_reply }
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
    let mut transitions = 0usize;
    // The running count of completed BUILD steps — the "work turn" tally that the
    // active fact-extraction backstop throttles on (see `crate::fact_extract`).
    let mut work_turns = 0usize;
    // CIRCUIT BREAKER (UD-FLOW-008): trip after CONSECUTIVE_FAILURE_THRESHOLD
    // consecutive same-class step-verification failures with NO intervening progress,
    // so a build where the base keeps failing the same way STOPS with a diagnosis
    // instead of grinding to MAX_STEP_TRANSITIONS burning effort. A Done step resets it.
    let mut failure_breaker = crate::trust::ConsecutiveFailureBreaker::new();
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
        // starves the very first doer turn).
        if transitions > 0 && std::time::Instant::now() >= deadline {
            events.emit(EngineEvent::Note(
                "team · time budget reached — finalizing what's complete (raise \
                 UMADEV_RUN_BUDGET_SECS for a longer run)"
                    .to_string(),
            ));
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
        let StepOutcome {
            accepted,
            reply,
            drove,
            made_progress,
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
            // caller's hard-gate still decide overall reality.
            StepStatus::Blocked
        };
        plan.mark(&step_id, status);
        events.emit(EngineEvent::plan_step_status(step_id, step.title, status));
        persist_plan_ref(plan, options);
        // Advance the 9-phase workflow state to the furthest phase the plan's Done
        // steps now imply (monotonic — `persist_phase` clamps, never regresses). Only
        // a step that actually ticked Done moves the phase; a Blocked step leaves it.
        // Fail-open. This is what keeps `/status` honest as the build progresses.
        sync_phase_from_plan(plan, options);

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
    let final_gate = run_final_gate(
        session,
        options,
        events,
        route,
        &last_reply,
        deadline,
        "",
        false,
    )
    .await;
    if !final_gate.reply.is_empty() {
        last_reply = final_gate.reply;
    }

    // Persist the plan's terminal state for resume.
    persist_plan_ref(plan, options);
    // Sync the 9-phase workflow state at finalize. HONEST clean signal: every step
    // reached Done (none Blocked / stranded) AND the final whole-build gate settled
    // clean — only then may the build claim `delivery`. H1: the final gate runs the
    // cross-cutting checks (coverage / contract / runtime-proof / governance / fork
    // review); its clean-ness was previously DISCARDED, so a build with every step
    // Done but a DIRTY final gate (a dropped FR / contract drift / unverified
    // runtime-proof) finalized as success. AND the gate's clean signal in, so an
    // incomplete build can never be disguised as a clean delivery. This makes the
    // step path's gate never weaker than the single-turn loop (which already gates
    // finalize INSIDE `qc.is_clean()`). Fail-open: a dirty gate just means "not clean".
    let clean = plan.steps.iter().all(|s| s.status == StepStatus::Done) && final_gate.clean;
    finalize_phase_from_plan(plan, options, clean);
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
        // detected stack + requirement shape — into a reusable cross-project recipe,
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
    Some(DirectorLoopOutcome::Done { reply: last_reply })
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
    if !pitfalls.trim().is_empty() {
        instruction.push_str("\n\n## Known pitfalls to avoid (from past runs)\n");
        instruction.push_str(pitfalls.trim());
    }

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

    let mut drove = false;
    let mut last_reply = String::new();
    // SELF-EVOLUTION accounting across this step's fix rounds (a SIDE EFFECT of the
    // deterministic acceptance verdict, never a driver of it). Carries the previous
    // round's FAILING evidence so a pass that RECOVERS from it can reward + mark
    // resolved the pitfall whose recorded fix just held. Empty on a clean first pass.
    let mut prior_fail_errors: Vec<String> = Vec::new();
    for round in 0..=max_fix_rounds {
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
        if summoned.done {
            drove = true;
        }
        if !summoned.text.trim().is_empty() {
            last_reply = summoned.text.clone();
        }
        // Wave 2 deliverable 4: distil this turn's failed-tool pitfalls into the
        // lessons KB on the DEFAULT loop (audit recording already happened inside
        // summon's governed pump). Fail-open: capture never affects the schedule.
        capture_turn_pitfalls(options, events, &summoned.pitfalls);
        // Verify against THIS step's acceptance on the deterministic floor.
        let mut verdict = verify_step_acceptance(session, options, events, route, step).await;
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
        // MEDIUM #3 — a dead/hung summon turn that never actually ran (`!drove`) must
        // not "complete" a Build step on a NEUTRAL-SKIP acceptance (an unavailable
        // check / a TurnSettled free pass). Require REAL evidence: either the doer
        // turn actually ran, OR the floor produced positive evidence (real source on
        // disk / a green build). Without either, a build step over a dead session is
        // honestly left unaccepted (→ the caller marks it Blocked), so a dead session
        // can't silently tick steps 2..N Done over an empty build.
        if verdict.accepted && (drove || verdict.has_positive_evidence) {
            // SELF-EVOLUTION (a SIDE EFFECT of the PASS verdict; best-effort +
            // fail-open, never changes the outcome below): the recalled lessons were
            // in front of the doer and the step PASSED — reward their trust. If this
            // pass RECOVERED from a recorded failing round, that IS proof the pitfall's
            // recorded fix held: reward its dev-error trust and mark it resolved.
            crate::self_evolve::reward_on_pass(&options.project_root, &prior_fail_errors);
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
        let fail_errors = verdict.evidence.clone();
        crate::self_evolve::penalise_on_fail(&options.project_root, &fail_errors);
        crate::self_evolve::reflect_on_recurring_failure(
            session,
            &options.project_root,
            events,
            &evidence_line,
            reflected,
        )
        .await;
        // Remember this round's failing evidence so a recovery on a LATER round can
        // reward + mark-resolved the pitfall whose recorded fix then holds.
        prior_fail_errors = fail_errors;
        // Out of fix budget → leave the step unaccepted (the caller marks it Blocked
        // and the final gate still has the last word). Bounded — never an open grind.
        if round >= max_fix_rounds {
            break;
        }
        // Highest-precision FAILURE-TIME recall: prior lessons with the SAME error
        // signature ("you hit this N times before; here's what worked; it keeps
        // recurring") + any base-reflected strategy. Fingerprint-gated + abstaining, so
        // an unclassifiable failure injects nothing. Fail-open (empty string on a miss).
        let prior = crate::lessons::lessons_for_error(&options.project_root, &evidence_line);
        // Fold this step's failing acceptance into the NEXT re-drive's directive so
        // the same seat fixes the cause with raw evidence, in the same session. The
        // overall-goal frame is re-prepended so a fix turn keeps the product context.
        instruction = format!(
            "{}{} — {}\n\n## This step did not pass its acceptance check yet — fix the cause\n{}{prior}\n\
             Edit the real files, run any build/test you need, and make this step's \
             acceptance ({}) actually pass.",
            step_goal_frame(options),
            step.title,
            route_focus_line(route),
            evidence_line,
            step_criterion_label(step),
        );
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
        // The last failing round's typed evidence — WHY this step could not pass its
        // acceptance — so a bounded re-plan can route around the diagnosed blocker.
        gap_evidence: prior_fail_errors,
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
/// blackboard. A review step is "accepted" when no seat raises a blocking finding;
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
    if !review.has_blocking() {
        // A team actually convened (seats > 0) ⇒ real review progress; an empty team
        // (seats == 0) is a neutral skip that must NOT advance the done count.
        let reviewed = review.seats > 0;
        return StepOutcome {
            accepted: true,
            reply: String::new(),
            drove: reviewed,
            made_progress: reviewed,
            gap_evidence: Vec::new(), // review accepted → no gap
        };
    }
    // Wall-clock ceiling: the team found blocking issues, but the budget is already
    // spent — skip the (minute-level) fix turn and surface the findings honestly. A
    // review step is advisory, so we still "accept" it (the final gate / hard-gate
    // own reality); we just don't grind another doer turn past the deadline. A team
    // DID convene + raised findings (seats > 0), so this is real review progress.
    if std::time::Instant::now() >= deadline {
        events.emit(EngineEvent::Note(
            "team · time budget reached — review findings left for the final gate \
             (raise UMADEV_RUN_BUDGET_SECS to repair them in this run)"
                .to_string(),
        ));
        return StepOutcome {
            accepted: true,
            reply: String::new(),
            drove: false,
            made_progress: true,
            gap_evidence: Vec::new(), // advisory review deferred to the final gate
        };
    }
    // The team found blocking issues — fold them into ONE bounded fix turn on the
    // main session (the doer repairs), then accept (advisory: the deterministic
    // floor in the final gate is the real stop, never a critic verdict — invariant).
    let mut body = String::new();
    for b in &review.blocking {
        body.push_str("- ");
        body.push_str(b);
        body.push('\n');
    }
    let directive = format!(
        "The review team flagged MUST-FIX issues in what was built so far. Fix EVERY one \
         now by editing the files directly — do not narrate, just apply the fixes and \
         re-run any build/test you already ran. Issues:\n{body}\nWhen all are fixed, end \
         your turn."
    );
    let drove =
        crate::continuous::drive_rework_turn(session, options, events, directive, deadline).await;
    // LOW #1 — re-VERIFY the repair instead of blindly accepting it: re-run the
    // (read-only, cheap) cross-review once after the fix turn. A review step stays
    // advisory (the final QC gate + hard-gate own termination, never an LLM verdict),
    // so we always "accept" to keep the schedule moving — but we now report HONESTLY
    // whether the fix actually cleared the findings rather than silently assuming it
    // did. Fail-open: if the re-review can't fork it returns no-blocking (accept).
    let recheck = director::review_with_seats(session, options, events, &route.team).await;
    if recheck.has_blocking() {
        // FLOOR-INTEGRITY (a corroborated residual must not silently pass). A residual
        // blocking finding after the fix turn is, on its own, a critic OPINION —
        // ADVISORY by invariant 2, never a hard-fail. But before letting the step tick
        // Done as FULLY CLEAN, corroborate that residual against UmaDev's OWN
        // deterministic floor: a content-governance hit, a failed
        // contract/coverage/acceptance/runtime check, or a failed source verify (the
        // SAME objective signals the final QC folds in). Deterministic + read-only +
        // fail-open: an uncomputable / neutral floor yields NO corroboration and today's
        // advisory behaviour stands (a Note, the step proceeds).
        let corroboration = review_residual_floor_corroboration(options, events, route).await;
        if !corroboration.is_empty() {
            events.emit(EngineEvent::Note(format!(
                "team · review step's residual finding(s) are CORROBORATED by the \
                 deterministic floor ({}) — marking the step not-clean so the final gate \
                 can't drop it (the FLOOR fails it; critics stay advisory)",
                corroboration.join("; ")
            )));
            // Honest status via the EXISTING machinery: `made_progress = false` marks
            // this step Blocked (NOT Done-clean) at the scheduler, which folds into the
            // final `clean` computation (`all steps Done && final_gate.clean`) so the
            // corroborated residual can't vanish. `accepted` STAYS true so the circuit
            // breaker (`drove && !accepted`) is UNTOUCHED — a floor-corroborated residual
            // is not a driven-verify failure, and a critic never trips loop control
            // (invariant 2 / the deterministic floor governs). The team convened + a
            // repair turn ran, so this is real (if not-clean) review work.
            return StepOutcome {
                accepted: true,
                reply: String::new(),
                drove,
                made_progress: false,
                // The corroborating floor lines — WHY this review step is not clean — so
                // a bounded re-plan (only if it strands dependents) can route around it.
                gap_evidence: corroboration,
            };
        }
        // NOT corroborated — a bare critic opinion only. Invariant 2 holds: advisory,
        // surface the honest Note and let the step proceed exactly as before (a critic's
        // opinion never hard-fails a step; the objective hard-gate still owns reality).
        events.emit(EngineEvent::Note(format!(
            "team · review step repaired but {} finding(s) remain after the fix turn — \
             NOT corroborated by the deterministic floor, so advisory-only (left for the \
             final gate; the objective hard-gate owns reality)",
            recheck.blocking.len()
        )));
    }
    // A team convened, raised findings, and a repair turn ran — real review progress
    // regardless of whether the repair turn fully settled (`drove`).
    StepOutcome {
        accepted: true,
        reply: String::new(),
        drove,
        made_progress: true,
        gap_evidence: Vec::new(), // review accepted (advisory) → no gap to re-plan
    }
}

/// DETERMINISTIC floor corroboration for a review step's RESIDUAL blocking finding
/// (the floor-integrity tightening). After a review step's bounded fix turn the
/// cross-review team may STILL raise a blocking finding; on its own that is a critic
/// OPINION — ADVISORY by invariant 2, and a bare opinion must NEVER hard-fail a step.
/// So before a residual can hold the step back it must be CORROBORATED by UmaDev's OWN
/// deterministic floor. This runs the SAME read-only, deterministic checks the final QC
/// gate folds in and returns the corroborating lines:
///
/// 1. **content-governance scan** (`continuous::governance_scan`) — emoji-as-icon /
///    hardcoded color / AI-slop / swallowed-error hits in what the base wrote,
/// 2. **required acceptance floor** (`acceptance_floor_blocking`) — a coverage gap /
///    an unimplemented planned endpoint / frontend↔backend contract drift / an
///    unverified `runtime-proof.json`,
/// 3. **source-present verify** (`director::verify` / [`VerifyKind::SourcePresent`]) —
///    a claimed-done build with ZERO real source on disk is an objective floor failure.
///
/// An EMPTY result means NOT corroborated → the caller keeps today's advisory behaviour
/// (a Note, the step proceeds). Read-only + fail-open + CHEAP by construction (disk
/// reads only — no second full build, no fork): a build failure is already re-found
/// INDEPENDENTLY by the final gate, so this closes the gap only for findings that could
/// otherwise VANISH (governance / contract / coverage / runtime / source) without paying
/// for a duplicate build. Every contributor degrades to a neutral skip when it can't run,
/// so an uncomputable corroboration yields an empty result — never a spurious fail, never
/// a crash. This does NOT change the advisory nature of critics: it only lets an
/// independently-observed FLOOR signal — not the critic's opinion — mark the step honest.
async fn review_residual_floor_corroboration(
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    route: &RoutePlan,
) -> Vec<String> {
    let mut corroborating: Vec<String> = Vec::new();
    // 1. Content-governance scan (deterministic, read-only) — the same craft/quality
    //    floor `run_auto_qc` folds in. A hit here is an objective, base-agnostic signal.
    for v in crate::continuous::governance_scan(options) {
        corroborating.push(format!("[governance] {v}"));
    }
    // 2. Required acceptance floor (coverage / acceptance / contract / runtime-proof) —
    //    the SAME deterministic contract/acceptance checks the final QC folds in. Called
    //    unconditionally: it is fail-open (no PRD / architecture / runtime-proof → empty),
    //    so it can only ever add a REAL, machine-checked gap, never a fabricated one.
    for line in acceptance_floor_blocking(options, Some(route)) {
        corroborating.push(line);
    }
    // 3. Source-present verify (a failed verify) — a claimed-done build with no real
    //    source on disk. Cheap: a disk read, never a build. A failing BUILD is left to
    //    the final gate, which re-runs it independently (so it can't vanish anyway); we
    //    corroborate here only with the signals that COULD otherwise be dropped.
    let src = director::verify(options, events, VerifyKind::SourcePresent).await;
    if src.available && !src.passed {
        corroborating.push(format!(
            "source-present: FAILED — {}",
            src.evidence.first().cloned().unwrap_or_default()
        ));
    }
    corroborating
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
    /// Concrete evidence lines from the check (failed-step names / drift / count).
    evidence: Vec<String>,
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
        return verify_step_evidence(options, events, step, is_build).await;
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
            evidence: Vec::new(),
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
            with_source_evidence(
                acceptance_from_verify(
                    director::verify(options, events, VerifyKind::BuildTest).await,
                ),
                src_positive,
            )
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
                evidence: review.blocking.clone(),
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
                evidence: Vec::new(),
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
        evidence: if r.available && !r.passed {
            r.evidence
        } else {
            Vec::new()
        },
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
    let build = if needs_build {
        Some(director::verify(options, events, VerifyKind::BuildTest).await)
    } else {
        None
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
        Some(crate::runtime_proof::run_runtime_proof(root).await)
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

    StepVerdict {
        // Accept iff NO declared contract is unsatisfied. (A neutral-skip-only step on
        // a Build path still has the honesty floor's positive source evidence; a
        // non-Build step with only skips is a neutral accept, matching the acceptance
        // path's fail-open posture.)
        accepted: gaps.is_empty(),
        has_positive_evidence: any_positive,
        evidence: gaps,
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
/// here). The objective hard-gate the caller runs still owns reality.
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
}

#[allow(clippy::too_many_arguments)]
async fn run_final_gate(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    route: &RoutePlan,
    seed_reply: &str,
    deadline: std::time::Instant,
    // Optional CONTEXT prefix front-loaded onto every fix directive (the chat-build
    // post-QC entry passes the recalled knowledge digest + prior pitfalls so a fix
    // turn carries the team's standards + memory). `""` = the byte-for-byte original
    // directive (the `/run` step-driver passes this), so existing callers are unchanged.
    fix_prefix: &str,
    // OBSERVED-tool corroboration for the SEED reply: did a real build/test/lint runner
    // run producing `seed_reply`? Callers that can't observe it pass `false` (conservative
    // — round 0 then runs UmaDev's own read rather than trusting the seed's prose). Each
    // fix round below re-derives it from its OWN turn's observed tool calls.
    seed_ran_build_tool: bool,
) -> FinalGateOutcome {
    let mut last_reply = String::new();
    // The incremental-verify signal seeds from the LAST step's reply (the steps just
    // ran the build/test); each fix round below then carries its own turn's reply.
    let mut verify_signal = seed_reply.to_string();
    // The observed-tool corroboration paired with `verify_signal`: seeds from the caller,
    // then tracks each fix turn's OWN observed run — so a fix turn's green claim can only
    // skip the read when THAT turn actually ran a runner.
    let mut verify_ran_build_tool = seed_ran_build_tool;
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
        if qc.is_clean() {
            return FinalGateOutcome {
                reply: last_reply,
                clean: true,
            };
        }
        if round + 1 >= MAX_QC_ROUNDS {
            events.emit(EngineEvent::Note(
                "team · final QC reached its fix-round budget — settling (objective hard-gate decides reality)"
                    .to_string(),
            ));
            return FinalGateOutcome {
                reply: last_reply,
                clean: false,
            };
        }
        // Wall-clock ceiling (graceful): the QC READ above ran (the floor still bites),
        // but the minute-level FIX TURN it would trigger is skipped once the budget is
        // spent — the residual findings are surfaced honestly and left for the
        // objective hard-gate rather than driving more over-budget fix turns. This is
        // the doc'd "hard ceiling": the build can't keep grinding fix turns past it.
        if std::time::Instant::now() >= deadline {
            events.emit(EngineEvent::Note(
                "team · time budget reached — final QC findings left for the objective \
                 hard-gate (raise UMADEV_RUN_BUDGET_SECS for more fix rounds)"
                    .to_string(),
            ));
            return FinalGateOutcome {
                reply: last_reply,
                clean: false,
            };
        }
        // Fold the residual findings into ONE fix turn on the main session — with the
        // optional context prefix (knowledge + pitfalls) front-loaded for a chat-build.
        match drive_one_turn(
            session,
            options,
            events,
            qc.fix_directive_with_context(fix_prefix),
            IdleBudget::from_env(),
            deadline,
        )
        .await
        {
            Ok(t) => {
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
                }
            }
        }
    }
    FinalGateOutcome {
        reply: last_reply,
        clean: false,
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
///    directive per round, bounded by [`MAX_QC_ROUNDS`] + the wall-clock deadline,
///    fed back over the SAME continuous session (single-writer preserved), with the
///    recalled **knowledge digest + prior pitfalls** front-loaded (`post_build_rework_context`)
///    so the fix carries the team's commercial standards + memory,
/// 4. **usage + lessons capture** — every fix turn runs through [`drive_one_turn`],
///    which records the token estimate (`/usage`) and distils failed-tool pitfalls
///    into the lessons KB (`/lessons`), so the chat build self-evolves like a `/run`.
///
/// Delegates the actual gate to [`run_final_gate`] (the exact same bounded pass the
/// `/run` step-driver ends on) with the route's seats + the knowledge/lessons prefix,
/// so a chat build is held to the IDENTICAL floor as `/run` — not a re-implementation
/// that could drift. Returns the final fix-turn reply (empty when QC was already
/// clean). **Fail-open throughout**: a scan / fork / rework that can't run contributes
/// nothing and the build settles (a chat turn is never wedged by QC). The wall-clock
/// budget ([`run_budget`]) bounds the extra fix turns exactly like `/run`.
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
    let prefix = post_build_rework_context(options);
    // The chat-build surface only needs the fix-turn reply; its caller does not gate a
    // finalize on the gate's clean-ness (the `/run` step path does — see H1). Seed
    // corroboration `false`: this entry has only the seed REPLY text, not an observed
    // run, so round 0 runs UmaDev's own build/test read rather than trusting the seed's
    // prose "it's green" — narration alone must not skip. Fix rounds re-derive per turn.
    run_final_gate(
        session, options, events, route, seed_reply, deadline, &prefix, false,
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
fn post_build_rework_context(options: &RunOptions) -> String {
    let mut out = String::new();
    // Knowledge digest — small budget (3 chunks), matching the agentic light-turn size.
    // `record_feedback = true`: this prefix front-loads a chat-BUILD's post-QC fix
    // directives (only reached once a turn is a real build), and that fix loop runs
    // the same `self_evolve` reward/penalise seam as `/run` — so the surfaced chunks
    // are snapshotted here for that outcome to attribute.
    let digest = crate::phases::agentic_knowledge_digest(
        &options.project_root,
        &options.requirement,
        3,
        true,
    );
    if !digest.trim().is_empty() {
        out.push_str(digest.trim());
    }
    // Prior pitfalls on this project (recalled lessons) — what already bit us before.
    let lessons =
        crate::lessons::relevant_lessons_for_prompt(&options.project_root, &options.requirement);
    if !lessons.trim().is_empty() {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(lessons.trim());
    }
    out
}

/// Distil a turn's failed-tool summaries into the lessons KB on the DEFAULT loop —
/// Wave 2 deliverable 4 (`/lessons` now learns from the director path, not just the
/// legacy runner). Emits a `[learned]` note so the user sees the agent remembering.
/// Fail-open: an empty feed is a no-op; capture never affects the schedule.
fn capture_turn_pitfalls(options: &RunOptions, events: &Arc<dyn EventSink>, pitfalls: &[String]) {
    if pitfalls.is_empty() {
        return;
    }
    let n = crate::lessons::capture_dev_errors(
        &options.project_root,
        pitfalls,
        &options.effective_slug(),
        &options.requirement,
    );
    if n > 0 {
        events.emit(EngineEvent::Note(format!(
            "[learned] 识别并记录了 {n} 条开发踩坑,已写入知识库 — 下次遇到同类问题会提前规避。"
        )));
    }
}

/// A human label for a step's acceptance criterion, used in the fix directive so
/// the doer knows exactly what mechanical bar this step must clear.
fn acceptance_label(spec: &plan_state::AcceptanceSpec) -> &'static str {
    use plan_state::AcceptanceSpec as A;
    match spec {
        A::SourcePresent => "real source files exist on disk",
        A::BuildTest => "the project's build/test passes",
        A::Contract => "the frontend↔backend API contract holds",
        A::DesignTokensPresent => "the design-tokens.{json,css} design system exists on disk",
        A::ReviewClean => "the review team raises no blocking issue",
        A::TurnSettled => "the work turn completes",
    }
}

/// The mechanical bar a step must clear, for the fix directive — the TYPED EVIDENCE
/// CONTRACT summary when the step declares one (so the doer is told exactly which
/// specific files/tests/routes must check out), else the generic
/// [`acceptance_label`]. Keeps the directive truthful about what UmaDev will actually
/// verify before it ticks the step done.
fn step_criterion_label(step: &plan_state::PlanStep) -> String {
    if step.evidence.is_empty() {
        acceptance_label(&step.acceptance).to_string()
    } else {
        step.evidence
            .iter()
            .map(plan_state::EvidenceContract::label)
            .collect::<Vec<_>>()
            .join(", ")
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

/// The furthest-reached [`Phase`] implied by the plan's COMPLETED (Done) work.
///
/// Each `Done` step contributes its seat's phase; the result is the highest-ranked
/// such phase (the deepest the build has honestly reached). A plan with no Done steps
/// yet — or no plan at all — has reached nothing concrete, so this returns `None`
/// (the caller then keeps the initial `research` phase / writes nothing). A fully
/// `Done` plan whose furthest step is e.g. a QA seat reaches `Quality`, NOT
/// `Delivery` — `Delivery` is only asserted by [`finalize_phase`] when the whole run
/// genuinely finished clean.
fn furthest_done_phase(plan: &Plan) -> Option<Phase> {
    plan.steps
        .iter()
        .filter(|s| s.status == StepStatus::Done)
        .map(|s| phase_for_seat(s.seat))
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
        base_session_id: current_state.and_then(|s| s.base_session_id),
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
    // Estimate the directive's token cost up front (the session stream carries no
    // usage on `TurnDone`, unlike the single-shot path), so `/usage` is real on the
    // default loop for ALL three bases — not just claude in the legacy runner.
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
    let mut text = String::new();
    let mut pitfalls: Vec<String> = Vec::new();
    // Bounded counters for the two self-heals (see the fn docs): how many transient
    // backoff-retries this turn has already spent, and whether the silent-hang watchdog
    // re-drive has already fired (a SINGLE re-drive).
    let mut transient_retries: u32 = 0;
    let mut watchdog_retried = false;
    // Tool-aware idle grace: while the base is plausibly mid-tool (a tool-use event
    // seen, no result yet) it is legitimately SILENT for minutes (a docker build / a
    // compile / npm install / a long test), so the next wait uses the extended tool
    // window; otherwise the base default — so a truly hung base still settles.
    let mut in_tool_call = false;
    // Did the base ACTUALLY run a build/test/lint runner this turn? Set from the OBSERVED
    // tool-call stream below (not the reply prose), it is the corroboration the auto-QC
    // requires before a green CLAIM is trusted to skip UmaDev's own build/test read. Reset
    // alongside `text` on a transient re-drive so it reflects only the FINAL attempt.
    let mut ran_build_tool = false;
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
        if std::time::Instant::now() >= deadline {
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
        let ev = match next_event_idle(session, idle, in_tool_call, Some(deadline)).await {
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
                if in_tool_call && std::time::Instant::now() >= deadline {
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
        if let Some(t) = tool_phase_transition(&ev) {
            in_tool_call = t;
        }
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
            SessionEvent::ToolCall { name, input } => {
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
                // Note + give the tool row a real detail, so the user sees what's asked
                // and that the base awaits their choice (relayed next turn — see
                // `crate::ask_question`). Fail-open: a non-question call → None.
                if let Some(surface) = crate::ask_question::surface(&name, &input) {
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
                events.emit(EngineEvent::WorkerStream {
                    event: StreamEvent::ToolUse { name, detail, edit },
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
                let ledger = crate::trust::TrustLedger::load(&options.project_root);
                let decision = if requires_confirmation_with_ledger(
                    options.mode,
                    &action,
                    &target,
                    &options.project_root,
                    &ledger,
                ) {
                    events.emit(EngineEvent::Note(umadev_i18n::tlf(
                        "continuous.dangerous_action_denied",
                        &[&action, &target],
                    )));
                    ApprovalDecision::Deny
                } else {
                    ApprovalDecision::Allow
                };
                if let Err(e) = session.respond(&req_id, decision).await {
                    // LOW #2: a turn that dies on the approval round-trip still spent
                    // its tokens — record the estimate (fail-open) before bailing. No
                    // `TurnDone` yet → estimate (F3).
                    record_turn_usage(options, events, None, est_tokens);
                    return Err(format!("session respond: {e}"));
                }
            }
            SessionEvent::TurnDone { status, usage } => match status {
                // Completed / Truncated → accept the turn (the deterministic floor
                // downstream is the real stop signal; forcing a fail here would
                // hard-stop a build that may have produced usable output).
                TurnStatus::Completed | TurnStatus::Truncated => {
                    // Wave 2 deliverable 4: record usage + distil pitfalls on the
                    // DEFAULT loop, for every base. Both fail-open. F3: prefer the
                    // base's REAL reported usage, fall back to the chars/4 estimate.
                    record_turn_usage(options, events, usage, est_tokens);
                    capture_turn_pitfalls(options, events, &pitfalls);
                    return Ok(TurnResult {
                        text,
                        ran_build_tool,
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
                    let failure = crate::base_error::classify(None, None, Some(&reason));
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
/// default loop). Never panics; an empty string is 0. `pub(crate)` so the shared
/// rework pump (`continuous::drive_rework_turn_with_idle`) estimates identically.
pub(crate) fn approx_tokens(s: &str) -> u64 {
    (s.chars().count() as u64).div_ceil(4)
}

/// Record an estimated-token usage row for the default loop, attributed to the
/// canonical "build" phase. `pub(crate)` so the shared rework pump records usage the
/// same way the single-turn loop does. Fail-open: a zero estimate is a no-op.
pub(crate) fn record_estimated_usage(backend: &str, est_tokens: u64) {
    if est_tokens == 0 {
        return;
    }
    crate::runner::record_usage(
        backend,
        umadev_spec::Phase::Frontend,
        u32::try_from(est_tokens).unwrap_or(u32::MAX),
    );
}

/// The token count to record for one turn: the base's REAL usage when its live
/// stream reported it on `TurnDone` (F3 — claude's `result` line, codex's
/// `thread/tokenUsage/updated` / inline `turn/completed`), else the `chars/4`
/// estimate. opencode's SSE carries no usage → it always estimates (honest).
///
/// "Tokens" here is `input + output` (the same single-number convention
/// `record_usage` already uses). Fail-open: a `None` usage simply yields the
/// estimate; the real path can never make `/usage` read lower than honest.
pub(crate) fn real_or_estimated_tokens(usage: Option<Usage>, est_tokens: u64) -> u64 {
    match usage {
        Some(u) => u64::from(u.input_tokens) + u64::from(u.output_tokens),
        None => est_tokens,
    }
}

/// Record one director turn's token usage to `~/.umadev/usage.jsonl` so `/usage`
/// is real on the default loop for all three bases — preferring the base's REAL
/// reported usage and falling back to the `chars/4` estimate (F3). Fail-open: a
/// zero count / a write error is a no-op. Mirrors [`crate::runner::record_usage`].
fn record_turn_usage(
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    usage: Option<Usage>,
    est_tokens: u64,
) {
    // Surface the base's REAL reported usage to the live UI session total — only
    // the real path (an estimate is not the base's own number, so we don't inflate
    // the live count with it). The ledger row below still records the estimate
    // fallback so `/usage` stays honest.
    if let Some(u) = &usage {
        events.emit(EngineEvent::TurnUsage {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
        });
    }
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
    fn fix_directive(&self) -> String {
        self.fix_directive_with_context("")
    }

    /// [`Self::fix_directive`] with an optional CONTEXT prefix front-loaded before
    /// the findings — used by the chat-build post-QC entry to inject the recalled
    /// commercial-engineering knowledge digest plus the project's prior pitfalls
    /// (`post_build_rework_context`) so the fix turn fixes WITH the team's standards
    /// and memory, not blind. An empty prefix yields the byte-for-byte original
    /// directive, so the `/run` callers are unchanged. Fail-open by construction.
    fn fix_directive_with_context(&self, prefix: &str) -> String {
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
        format!(
            "{lead}An objective check of what you just built surfaced problems that must be \
             fixed (these are real facts read from disk / review, not your memory):\n\
             {body}\nFix the cause of each one yourself with your tools — edit/create \
             the real files — then RUN the project's own build and tests to confirm \
             they pass. When it is genuinely clean, end your turn and report honestly \
             what you fixed."
        )
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
        return QcReport { blocking };
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
        return QcReport { blocking };
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
        let bt = director::verify(options, events, VerifyKind::BuildTest).await;
        if let Some(line) = build_test_blocking(&bt) {
            blocking.push(line);
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
        for line in acceptance_floor_blocking(options, route) {
            blocking.push(line);
        }
    }

    // 3. Optional fork review (UmaDev's read-only QC over read-only forks). The team
    //    scales to the task, so a lean goal convenes no team and this contributes
    //    nothing. Advisory — the base's body acts on whatever it surfaces. When a
    //    route is in hand (the deliberate step path's final gate), size the team
    //    from the ROUTE's seats (deliverable 3); otherwise (the single-turn loop)
    //    fall back to the kind-derived team — same roster, sized from the same kind.
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
    for finding in review_blocking(&review) {
        blocking.push(finding);
    }

    QcReport { blocking }
}

/// The REQUIRED acceptance floor for a deliberate build (Wave 4, §L4 / task 2) —
/// the spec→tasks + spec→code verification, promoted to a blocking signal on the
/// default deliberate path. Folds in coverage gaps, interface-acceptance gaps,
/// frontend↔contract drift, an unverified runtime-proof, and (for a Bugfix) a
/// missing reproduction test. Each contributor is fail-open: a missing artifact /
/// unparseable doc yields no gap (a neutral skip), so a check that genuinely
/// cannot run never fabricates a failure. Returns the blocking lines (empty =
/// the floor is clean OR nothing could be checked).
fn acceptance_floor_blocking(options: &RunOptions, route: Option<&RoutePlan>) -> Vec<String> {
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

    out
}

/// Read a written `runtime-proof.json` and, if it recorded a real (non-skipped)
/// FAILURE to boot/answer, return a blocking line. A missing file → `None` (the
/// runtime check simply wasn't run this loop — neutral, never a fabricated fail).
/// A written-but-not-verified proof whose reason is a SKIP (no dev server / no
/// curl) is also neutral; only a proof that ran and failed blocks. Fail-open: an
/// unreadable / unparseable file → `None`.
fn runtime_proof_blocking(root: &std::path::Path) -> Option<String> {
    let path = root.join(crate::runtime_proof::runtime_proof_rel_path());
    let body = std::fs::read_to_string(path).ok()?;
    let proof: crate::runtime_proof::RuntimeProof = serde_json::from_str(&body).ok()?;
    if proof.status.is_verified() {
        return None; // booted + answered → no problem
    }
    // Not verified. Distinguish a real failure from a neutral skip: a skip reason
    // names an absent precondition (no dev server / curl / not detected). Only a
    // genuine boot/route failure is blocking.
    let reason = proof.summary_line().to_ascii_lowercase();
    let is_skip = reason.contains("not found")
        || reason.contains("no dev server")
        || reason.contains("not detected")
        || reason.contains("skipped");
    if is_skip {
        return None;
    }
    Some(format!(
        "runtime-proof: the app did not boot + answer its routes — {} (fix the cause so it \
         actually runs, then re-verify)",
        proof.summary_line()
    ))
}

/// Heuristic: does the project carry at least one real test file? Used only for the
/// Bugfix reproduction-test floor. Looks for the universal test-file conventions
/// (`*.test.*` / `*.spec.*` / a `tests/` or `__tests__` dir / a `test_*.py` /
/// `*_test.go` / a Rust `#[test]`). Pure + fail-open (bounded by `source_files`):
/// an empty tree → `false`. Conservative — a false "has a test" only DROPS a
/// blocking floor (never fabricates one), so we require a reasonably strong signal.
fn has_reproduction_test(root: &std::path::Path) -> bool {
    for f in crate::acceptance::source_files(root) {
        let name = f
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let path_str = f.to_string_lossy().to_ascii_lowercase();
        let by_name = name.contains(".test.")
            || name.contains(".spec.")
            || name.starts_with("test_")
            || name.ends_with("_test.go")
            || name.ends_with("_test.py")
            || name.ends_with(".test.rs");
        let by_dir = path_str.contains("/tests/")
            || path_str.contains("/__tests__/")
            || path_str.contains("/test/")
            || path_str.contains("/spec/");
        if by_name || by_dir {
            return true;
        }
        // A Rust file carrying `#[test]` / `#[tokio::test]` is a real test too.
        if name.to_ascii_lowercase().ends_with(".rs") {
            if let Ok(content) = std::fs::read_to_string(&f) {
                if content.contains("#[test]") || content.contains("#[tokio::test]") {
                    return true;
                }
            }
        }
    }
    false
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

/// Pull the blocking findings out of a [`ReviewResult`] (already seat-tagged +
/// deduped by the review team). Empty when the team accepted or no team convened.
fn review_blocking(r: &ReviewResult) -> Vec<String> {
    r.blocking.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes the tests that mutate the process-global `UMADEV_IDLE_TIMEOUT_SECS`
    /// / `UMADEV_TOOL_IDLE_TIMEOUT_SECS` env (read by `IdleBudget::from_env`):
    /// `set_var` / `remove_var` are process-wide, so without this lock concurrent env
    /// tests race and flake. Poison-tolerant so one failing test can't cascade.
    static IDLE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    use crate::events::RecordingSink;
    use crate::trust::TrustMode;
    use umadev_runtime::{SessionError, SessionEvent, TurnStatus};

    struct EnvRestore {
        key: &'static str,
        prior: Option<std::ffi::OsString>,
    }

    impl EnvRestore {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let prior = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, prior }
        }

        fn remove(key: &'static str) -> Self {
            let prior = std::env::var_os(key);
            std::env::remove_var(key);
            Self { key, prior }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            match self.prior.take() {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    fn opts(root: &std::path::Path) -> RunOptions {
        RunOptions {
            project_root: root.to_path_buf(),
            requirement: "做一个登录系统".to_string(),
            slug: "demo".to_string(),
            model: String::new(),
            backend: String::new(),
            design_system: String::new(),
            seed_template: String::new(),
            mode: TrustMode::Auto,
            strict_coverage: false,
        }
    }

    fn sink() -> (Arc<dyn EventSink>, RecordingSink) {
        let rec = RecordingSink::default();
        (Arc::new(rec.clone()), rec)
    }

    #[test]
    fn default_loop_audit_records_the_real_verdict_not_hardcoded_allow() {
        // P2: the default director loop used to record every tool call as `allow`,
        // inconsistent with the continuous path's real governance verdict. Now the
        // audit computes the SAME verdict: a dangerous bash → `block`, a benign
        // read → `allow`.
        let tmp = tempfile::TempDir::new().expect("tmp");
        let options = opts(tmp.path());

        // Dangerous command → the verdict blocks (not a hardcoded allow).
        let danger = serde_json::json!({ "command": "rm -rf /" });
        let d = tool_call_governance_verdict(&options, "Bash", &danger);
        assert!(d.block, "a root-wipe must produce a blocking verdict");

        // A benign read → pass.
        let benign = serde_json::json!({ "file_path": "src/main.rs" });
        let r = tool_call_governance_verdict(&options, "Read", &benign);
        assert!(!r.block, "an observe-only read passes");

        // The audit record carries the REAL decision word, not `allow`.
        record_tool_call_audit(&options, "Bash", "rm -rf /", &danger);
        let log = tmp
            .path()
            .join(".umadev")
            .join("audit")
            .join("tool-calls.jsonl");
        let recorded = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(
            recorded.contains("\"decision\":\"block\""),
            "the audit trail records the real `block` verdict, not a hardcoded allow: {recorded}"
        );
    }

    #[test]
    fn default_loop_floor_blocks_env_write_even_when_clauses_disabled() {
        // P2: the default loop's verdict must apply the bypass-immune floor. Even
        // with the secret/path clauses disabled in `.umadev/rules.toml`, a write to
        // `.env` (no source extension → a content scan alone would miss it) is
        // blocked by the floor's path guard (UD-SEC-001) — the same un-closable
        // floor `continuous::evaluate_tool_call` and the Claude hook apply.
        let tmp = tempfile::TempDir::new().expect("tmp");
        let udir = tmp.path().join(".umadev");
        std::fs::create_dir_all(&udir).unwrap();
        std::fs::write(
            udir.join("rules.toml"),
            "[disabled]\nclauses = [\"UD-SEC-001\", \"UD-SEC-003\", \"UD-SEC-018\", \"UD-SEC-026\"]\n",
        )
        .unwrap();
        let options = opts(tmp.path());

        let env_write = serde_json::json!({ "file_path": ".env", "content": "PORT=3000" });
        let d = tool_call_governance_verdict(&options, "Write", &env_write);
        assert!(
            d.block,
            "the floor must block a .env write despite the disabled clauses"
        );
        assert_eq!(d.clause, "UD-SEC-001");
    }

    // ── A scripted fake BaseSession: each `send_turn` loads the next scripted
    // batch of events (a turn). Forks emit a fixed JSON verdict so a QC review gets
    // a verdict. `next_event` drains the current batch. ──
    #[derive(Clone)]
    struct FakeSession {
        /// One event-batch per upcoming MAIN turn, consumed front-to-back.
        turns: std::collections::VecDeque<Vec<SessionEvent>>,
        /// The currently-draining batch.
        current: std::collections::VecDeque<SessionEvent>,
        /// Directives the MAIN session received, in order (asserted by tests).
        sent: Arc<std::sync::Mutex<Vec<String>>>,
        /// Whether `fork()` succeeds.
        can_fork: bool,
        /// JSON a forked judge turn emits.
        fork_reply: String,
        /// `true` once this is a forked (read-only) session.
        is_fork: bool,
    }

    impl FakeSession {
        fn new(turns: Vec<Vec<SessionEvent>>, can_fork: bool, fork_reply: &str) -> Self {
            Self {
                turns: turns.into_iter().collect(),
                current: std::collections::VecDeque::new(),
                sent: Arc::new(std::sync::Mutex::new(Vec::new())),
                can_fork,
                fork_reply: fork_reply.to_string(),
                is_fork: false,
            }
        }
        fn sent_handle(&self) -> Arc<std::sync::Mutex<Vec<String>>> {
            Arc::clone(&self.sent)
        }
    }

    #[async_trait::async_trait]
    impl BaseSession for FakeSession {
        async fn fork(&mut self) -> Result<Box<dyn BaseSession>, SessionError> {
            if !self.can_fork {
                return Err(SessionError::ForkUnsupported("test".into()));
            }
            let mut f = self.clone();
            f.is_fork = true;
            f.current.clear();
            f.turns.clear();
            Ok(Box::new(f))
        }
        async fn send_turn(&mut self, directive: String) -> Result<(), SessionError> {
            if self.is_fork {
                // A forked judge turn emits its JSON verdict then ends.
                self.current = [
                    SessionEvent::TextDelta(self.fork_reply.clone()),
                    SessionEvent::TurnDone {
                        status: TurnStatus::Completed,
                        usage: None,
                    },
                ]
                .into_iter()
                .collect();
                return Ok(());
            }
            self.sent.lock().unwrap().push(directive);
            self.current = self
                .turns
                .pop_front()
                .unwrap_or_else(|| {
                    vec![SessionEvent::TurnDone {
                        status: TurnStatus::Completed,
                        usage: None,
                    }]
                })
                .into_iter()
                .collect();
            Ok(())
        }
        async fn next_event(&mut self) -> Option<SessionEvent> {
            self.current.pop_front()
        }
        async fn respond(
            &mut self,
            _req_id: &str,
            _decision: ApprovalDecision,
        ) -> Result<(), SessionError> {
            Ok(())
        }
        async fn interrupt(&mut self) -> Result<(), SessionError> {
            Ok(())
        }
        async fn end(&mut self) -> Result<(), SessionError> {
            Ok(())
        }
    }

    fn text_turn(s: &str) -> Vec<SessionEvent> {
        vec![
            SessionEvent::TextDelta(s.to_string()),
            SessionEvent::TurnDone {
                status: TurnStatus::Completed,
                usage: None,
            },
        ]
    }

    /// A turn that ends with REAL reported usage (F3) — for asserting the
    /// consumer prefers the base's reported usage over the chars/4 estimate.
    fn text_turn_with_usage(s: &str, input: u32, output: u32) -> Vec<SessionEvent> {
        vec![
            SessionEvent::TextDelta(s.to_string()),
            SessionEvent::TurnDone {
                status: TurnStatus::Completed,
                usage: Some(Usage {
                    input_tokens: input,
                    output_tokens: output,
                }),
            },
        ]
    }

    /// Write a minimal real source file so the source-present floor passes and QC
    /// moves on to build/test + review (instead of stopping at the hard floor).
    fn seed_source(root: &std::path::Path) {
        std::fs::write(root.join("app.ts"), "export const x = 1;").unwrap();
    }

    #[test]
    fn real_usage_is_preferred_over_the_estimate() {
        // F3: when the base reports REAL per-turn usage on `TurnDone`, the consumer
        // records input+output, NOT the chars/4 estimate. When it doesn't (None,
        // e.g. opencode), it falls back to the estimate — so `/usage` stays honest.
        let real = Some(Usage {
            input_tokens: 1500,
            output_tokens: 450,
        });
        // Estimate (99) is ignored when real usage is present.
        assert_eq!(real_or_estimated_tokens(real, 99), 1950);
        // No reported usage → the estimate stands (opencode path / failed parse).
        assert_eq!(real_or_estimated_tokens(None, 99), 99);
        // A reported zero-usage turn records zero (honest), not the estimate.
        assert_eq!(real_or_estimated_tokens(Some(Usage::default()), 99), 0);
    }

    #[tokio::test]
    async fn turn_done_real_usage_flows_through_drive_one_turn() {
        // F3 end-to-end on the DEFAULT loop: a turn whose `TurnDone` carries real
        // usage drives cleanly to completion (the real-usage path must not change
        // loop control, only what `/usage` records). The recorded number lands in
        // ~/.umadev (HOME) so we assert the turn SUCCEEDS rather than the file.
        let tmp = tempfile::TempDir::new().unwrap();
        let (events, _rec) = sink();
        let turns = vec![text_turn_with_usage("done, real usage attached", 1200, 300)];
        let mut sess = FakeSession::new(turns, false, "");
        let out = drive_one_turn(
            &mut sess,
            &opts(tmp.path()),
            &events,
            "build it".to_string(),
            IdleBudget::new(
                std::time::Duration::from_secs(5),
                std::time::Duration::from_secs(5),
            ),
            std::time::Instant::now() + std::time::Duration::from_secs(3600),
        )
        .await;
        match out {
            Ok(r) => assert_eq!(r.text, "done, real usage attached"),
            Err(e) => panic!("a turn with real usage must complete cleanly: {e}"),
        }
    }

    /// A turn that RUNS a shell command (a `ToolCall`) before finishing, for asserting
    /// the observed-tool corroboration (`ran_build_tool`) the auto-QC honesty floor reads.
    fn tool_then_text_turn(command: &str, s: &str) -> Vec<SessionEvent> {
        vec![
            SessionEvent::ToolCall {
                name: "Bash".to_string(),
                input: serde_json::json!({ "command": command }),
            },
            SessionEvent::TextDelta(s.to_string()),
            SessionEvent::TurnDone {
                status: TurnStatus::Completed,
                usage: None,
            },
        ]
    }

    #[tokio::test]
    async fn drive_one_turn_records_an_observed_build_runner() {
        // The observed-tool corroboration is set from the ACTUAL tool-call stream: a turn
        // that runs `cargo test` marks `ran_build_tool = true`; a turn that only runs a
        // non-runner (`cat package.json`) leaves it `false` even when the reply CLAIMS a
        // green build — so narration alone can never corroborate.
        let tmp = tempfile::TempDir::new().unwrap();
        let (events, _rec) = sink();
        let idle = IdleBudget::new(
            std::time::Duration::from_secs(5),
            std::time::Duration::from_secs(5),
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3600);

        let mut ran = FakeSession::new(
            vec![tool_then_text_turn(
                "cargo test --workspace",
                "All tests pass.",
            )],
            false,
            "",
        );
        let out = drive_one_turn(
            &mut ran,
            &opts(tmp.path()),
            &events,
            "build it".to_string(),
            idle,
            deadline,
        )
        .await
        .expect("turn completes");
        assert!(
            out.ran_build_tool,
            "an observed `cargo test` runner must set ran_build_tool"
        );

        let mut narrated = FakeSession::new(
            vec![tool_then_text_turn(
                "cat package.json",
                "Ran cargo test — all tests pass (exit code 0).",
            )],
            false,
            "",
        );
        let out2 = drive_one_turn(
            &mut narrated,
            &opts(tmp.path()),
            &events,
            "build it".to_string(),
            idle,
            deadline,
        )
        .await
        .expect("turn completes");
        assert!(
            !out2.ran_build_tool,
            "a non-runner tool + a NARRATED green claim must NOT set ran_build_tool"
        );
    }

    // ── The USB-model loop: base builds end to end → UmaDev auto-QC → bounded fix ──

    #[tokio::test]
    async fn clean_build_passes_qc_with_no_markers_and_finishes() {
        // The base builds end to end and ends WITHOUT any scheduling marker (the
        // whole point: the team lives in the base's head). With real source on disk
        // and no fork (no review team), auto-QC is clean → done in one base turn.
        let tmp = tempfile::TempDir::new().unwrap();
        seed_source(tmp.path());
        let (events, _rec) = sink();
        let turns = vec![text_turn(
            "I created the login form, the route, and the tests — implemented it end \
             to end. All done.",
        )];
        let mut sess = FakeSession::new(turns, false, "");
        let sent = sess.sent_handle();
        let o = opts(tmp.path());

        let outcome = drive_director_loop(&mut sess, &o, &events, "GO".to_string()).await;
        match outcome {
            DirectorLoopOutcome::Done { reply } => assert!(reply.contains("created the login")),
            other @ DirectorLoopOutcome::Failed(_) => panic!("expected Done, got {other:?}"),
        }
        let sent = sent.lock().unwrap();
        // Exactly ONE main directive: the opening build. Clean QC → no fix pass.
        assert_eq!(sent.len(), 1, "clean QC → no feedback-fix turn: {sent:?}");
        assert!(sent[0].contains("GO"), "opening directive sent");
    }

    #[tokio::test]
    async fn lean_clean_build_finishes_in_one_turn_without_review() {
        // The headline speed case: a simple page that the base builds correctly the
        // first time spends ZERO fix rounds AND skips the fork review entirely.
        // Even though the session CAN fork and would raise a blocking verdict, the
        // lean tier never convenes the review, so the loop settles in ONE base turn.
        let tmp = tempfile::TempDir::new().unwrap();
        seed_source(tmp.path());
        let (events, _rec) = sink();
        let reply = r#"{"accepts": false, "blocking": ["MUST NOT trigger a fix round"]}"#;
        let turns = vec![text_turn(
            "Created the single-page todo app — index.html, styles, the add/delete \
             logic. Implemented it end to end. Done.",
        )];
        let mut sess = FakeSession::new(turns, true, reply);
        let sent = sess.sent_handle();
        let mut o = opts(tmp.path());
        o.requirement = "做一个简单的待办清单单页应用,纯前端".to_string();

        let outcome = drive_director_loop(&mut sess, &o, &events, "GO".to_string()).await;
        assert!(matches!(outcome, DirectorLoopOutcome::Done { .. }));
        // EXACTLY one main directive — the opening build. The lean QC is clean (no
        // review), so no fix directive is ever fed back.
        assert_eq!(
            sent.lock().unwrap().len(),
            1,
            "a lean clean build finishes in one turn — no review-driven fix pass"
        );
    }

    #[tokio::test]
    async fn qc_finds_no_source_and_feeds_a_fix_directive_back() {
        // The base CLAIMS a build but writes no source. UmaDev's hard-floor QC
        // catches it and feeds a fix directive back over the USB channel; the next
        // base turn writes real source → re-QC clean → done.
        let tmp = tempfile::TempDir::new().unwrap();
        let (events, _rec) = sink();
        // Turn 1 CLAIMS a build (a change verb) but writes no source → the hard-floor
        // QC FAILS and a fix directive is fed back. Turn 2 claims done again (the
        // tree stays empty in this scripted fake, but we only assert the fix
        // directive was injected, which proves the feedback path fired).
        let turns = vec![
            text_turn("Implemented it. (but the fake wrote nothing to disk)"),
            text_turn("Now created app.ts and the tests. Done."),
        ];
        let mut sess = FakeSession::new(turns, false, "");
        let sent = sess.sent_handle();
        let o = opts(tmp.path());

        let outcome = drive_director_loop(&mut sess, &o, &events, "GO".to_string()).await;
        assert!(matches!(outcome, DirectorLoopOutcome::Done { .. }));
        let sent = sent.lock().unwrap();
        // The opening build, then a fix directive carrying the source-present finding.
        assert!(
            sent.iter()
                .any(|d| d.contains("source-present") && d.contains("must be fixed")),
            "the QC finding was fed back as a fix directive: {sent:?}"
        );
    }

    #[tokio::test]
    async fn qc_review_blocking_is_fed_back_as_a_fix_directive() {
        // Real source exists, build/test is skipped (no manifest), but a forked
        // review seat raises a blocking finding → UmaDev folds it into a fix
        // directive over the same session.
        let tmp = tempfile::TempDir::new().unwrap();
        seed_source(tmp.path());
        let (events, _rec) = sink();
        let reply = r#"{"accepts": false, "blocking": ["登录失败路径无测试"]}"#;
        let turns = vec![
            text_turn("Created the login form and route. Done."),
            text_turn("Added the failure-path tests. Done."),
        ];
        let mut sess = FakeSession::new(turns, true, reply);
        let sent = sess.sent_handle();
        let o = opts(tmp.path());

        let outcome = drive_director_loop(&mut sess, &o, &events, "GO".to_string()).await;
        assert!(matches!(outcome, DirectorLoopOutcome::Done { .. }));
        let sent = sent.lock().unwrap();
        assert!(
            sent.iter()
                .any(|d| d.contains("登录失败路径无测试") && d.contains("must be fixed")),
            "the review blocking finding was fed back as a fix directive: {sent:?}"
        );
    }

    #[tokio::test]
    async fn a_chat_answer_with_no_code_claim_skips_qc_entirely() {
        // A goal the base just answers in prose (no claim of code changes) → no QC,
        // settle after one turn. Keeps a simple ask from being forced through QC.
        let tmp = tempfile::TempDir::new().unwrap();
        let (events, _rec) = sink();
        let turns = vec![text_turn(
            "Here is how I would approach this conceptually, before any code — let me \
             walk through the trade-offs first.",
        )];
        let mut sess = FakeSession::new(turns, false, "");
        let sent = sess.sent_handle();
        let o = opts(tmp.path());

        let outcome = drive_director_loop(&mut sess, &o, &events, "GO".to_string()).await;
        assert!(matches!(outcome, DirectorLoopOutcome::Done { .. }));
        assert_eq!(
            sent.lock().unwrap().len(),
            1,
            "a non-code answer skips QC → one turn, no re-injection"
        );
    }

    #[tokio::test]
    async fn fix_loop_is_bounded_by_max_qc_rounds() {
        // The base keeps claiming a build but never writes source — QC keeps failing.
        // The loop must STOP at MAX_QC_ROUNDS, never spin forever (bounded), and end
        // gracefully (the caller's hard-gate decides reality).
        let tmp = tempfile::TempDir::new().unwrap();
        let (events, _rec) = sink();
        // Every turn claims a build (a change verb) but the tree stays empty → the
        // hard-floor QC fails every round, so the loop keeps feeding fix directives
        // until it hits MAX_QC_ROUNDS.
        let turns: Vec<Vec<SessionEvent>> = (0..MAX_QC_ROUNDS + 3)
            .map(|_| text_turn("Implemented it (but still wrote nothing)."))
            .collect();
        let mut sess = FakeSession::new(turns, false, "");
        let sent = sess.sent_handle();
        let o = opts(tmp.path());

        let outcome = drive_director_loop(&mut sess, &o, &events, "GO".to_string()).await;
        assert!(matches!(outcome, DirectorLoopOutcome::Done { .. }));
        // Exactly MAX_QC_ROUNDS base build turns were driven, then the loop settled
        // gracefully — the fix loop is BOUNDED, never an open-ended grind.
        assert_eq!(
            sent.lock().unwrap().len(),
            MAX_QC_ROUNDS,
            "the fix loop is bounded by MAX_QC_ROUNDS"
        );
    }

    #[tokio::test]
    async fn dead_session_is_a_failed_outcome_not_a_panic() {
        // A session that ends mid-turn (next_event → None with no TurnDone) is an
        // honest Failed outcome — fail-open, never a panic.
        let tmp = tempfile::TempDir::new().unwrap();
        let (events, _rec) = sink();
        // A turn whose batch has a text delta but NO TurnDone → next_event drains
        // to None mid-turn.
        let turns = vec![vec![SessionEvent::TextDelta("partial".to_string())]];
        let mut sess = FakeSession::new(turns, false, "");
        let o = opts(tmp.path());

        let outcome = drive_director_loop(&mut sess, &o, &events, "GO".to_string()).await;
        assert!(
            matches!(outcome, DirectorLoopOutcome::Failed(_)),
            "a dead session is a Failed outcome: {outcome:?}"
        );
    }

    /// A session that HANGS: `send_turn` succeeds, but `next_event` never resolves
    /// (it returns a future that stays `Pending` forever) — the real "base wrote
    /// nothing and never exits" hang the idle watchdog must catch.
    struct HangingSession;

    #[async_trait::async_trait]
    impl BaseSession for HangingSession {
        async fn fork(&mut self) -> Result<Box<dyn BaseSession>, SessionError> {
            Err(SessionError::ForkUnsupported("hang".into()))
        }
        async fn send_turn(&mut self, _directive: String) -> Result<(), SessionError> {
            Ok(())
        }
        async fn next_event(&mut self) -> Option<SessionEvent> {
            // Never resolves — simulate a base that hangs holding the pipe open.
            std::future::pending::<()>().await;
            None
        }
        async fn respond(
            &mut self,
            _req_id: &str,
            _decision: ApprovalDecision,
        ) -> Result<(), SessionError> {
            Ok(())
        }
        async fn interrupt(&mut self) -> Result<(), SessionError> {
            Ok(())
        }
        async fn end(&mut self) -> Result<(), SessionError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn idle_watchdog_settles_a_hung_base_as_failed() {
        // P1-2: a base that hangs (no output, never exits) must NOT block the
        // director loop forever — the idle watchdog settles it as a Failed outcome.
        // Drive the deterministic core directly with a tiny window (no process-env
        // mutation, so nothing to race), keeping the real wait at ~100ms.
        let tmp = tempfile::TempDir::new().unwrap();
        let (events, _rec) = sink();
        let mut sess = HangingSession;
        let o = opts(tmp.path());
        let outcome = drive_director_loop_with_idle(
            &mut sess,
            &o,
            &events,
            "GO".to_string(),
            None,
            None,
            IdleBudget::new(Duration::from_millis(100), Duration::from_millis(100)),
            std::time::Instant::now() + Duration::from_secs(3_600),
        )
        .await;
        if let DirectorLoopOutcome::Failed(reason) = outcome {
            assert!(
                reason.contains("UMADEV_IDLE_TIMEOUT_SECS"),
                "a hung base settles as an idle Failed: {reason}"
            );
        } else {
            panic!("expected a Failed (idle) outcome, got {outcome:?}");
        }
    }

    /// A hung session that ALSO exposes a stderr tail — the broken-base case where
    /// the real cause (a bad model id / "not logged in") was written to STDERR and
    /// never stdout, so the bare idle reason gave no diagnosis. Used to prove the
    /// run path now folds that stderr into the user-visible Failed reason (parity
    /// with the chat path's `enrich_base_failure`).
    struct HangingSessionWithStderr;

    #[async_trait::async_trait]
    impl BaseSession for HangingSessionWithStderr {
        async fn fork(&mut self) -> Result<Box<dyn BaseSession>, SessionError> {
            Err(SessionError::ForkUnsupported("hang".into()))
        }
        async fn send_turn(&mut self, _directive: String) -> Result<(), SessionError> {
            Ok(())
        }
        async fn next_event(&mut self) -> Option<SessionEvent> {
            std::future::pending::<()>().await;
            None
        }
        async fn respond(
            &mut self,
            _req_id: &str,
            _decision: ApprovalDecision,
        ) -> Result<(), SessionError> {
            Ok(())
        }
        async fn interrupt(&mut self) -> Result<(), SessionError> {
            Ok(())
        }
        async fn end(&mut self) -> Result<(), SessionError> {
            Ok(())
        }
        fn stderr_tail(&self) -> Option<String> {
            Some("error: model X not available".to_string())
        }
    }

    #[tokio::test]
    async fn idle_settle_folds_in_the_base_stderr_tail() {
        // The gap this fix closes: on the run / director-loop path a hung build used
        // to settle with a bare "base went idle — …" and NO cause. Now the watchdog
        // captures the base's own `stderr_tail()` at the settle and folds it into the
        // Failed reason, so the user sees WHY — exactly as the chat path does.
        let tmp = tempfile::TempDir::new().unwrap();
        let (events, _rec) = sink();
        let mut sess = HangingSessionWithStderr;
        let o = opts(tmp.path());
        let outcome = drive_director_loop_with_idle(
            &mut sess,
            &o,
            &events,
            "GO".to_string(),
            None,
            None,
            IdleBudget::new(Duration::from_millis(100), Duration::from_millis(100)),
            std::time::Instant::now() + Duration::from_secs(3_600),
        )
        .await;
        if let DirectorLoopOutcome::Failed(reason) = outcome {
            assert!(
                reason.contains("UMADEV_IDLE_TIMEOUT_SECS"),
                "still settles as an idle Failed: {reason}"
            );
            assert!(
                reason.contains("error: model X not available"),
                "the run-path idle reason must now CONTAIN the base's stderr tail: {reason}"
            );
            assert!(
                reason.contains("base stderr"),
                "the stderr tail is labelled like the chat path: {reason}"
            );
        } else {
            panic!("expected a Failed (idle) outcome, got {outcome:?}");
        }
    }

    #[test]
    fn enrich_idle_reason_is_fail_open_and_bounded() {
        // No exit, no tail, an opaque idle reason → no family matches → today's
        // bare reason, unchanged (fail-open: Unknown prepends nothing).
        let base = idle_reason(Duration::from_secs(7));
        assert_eq!(enrich_idle_reason(&base, None, None, "claude-code"), base);
        // A present tail is folded in, last 3 non-empty lines, joined (a 4th-from-end
        // line and blank lines are dropped). The tail is still appended verbatim
        // even when the classifier also fires.
        let enriched = enrich_idle_reason(
            "base session ended mid-turn",
            None,
            Some("DROPPED\n\nmodel not found\nlogin required\nfinal line\n".to_string()),
            "claude-code",
        );
        assert!(enriched.contains("base stderr: model not found | login required | final line"));
        assert!(
            !enriched.contains("DROPPED"),
            "only the last 3 lines: {enriched}"
        );
        // A long tail is bounded to ≤280 chars of snippet (never unbounded).
        let long = "x".repeat(1_000);
        let enriched = enrich_idle_reason("r", None, Some(long), "claude-code");
        let tail = enriched.split("base stderr: ").nth(1).unwrap();
        assert!(tail.chars().count() <= 280, "stderr tail is bounded");
    }

    #[test]
    fn enrich_idle_reason_prepends_actionable_line_for_a_known_stderr() {
        // D1: a known stderr (here an auth error) now classifies and PREPENDS the
        // per-base actionable diagnosis, while still appending the raw stderr tail
        // as the technical detail — so a hung claude with a bad key reads e.g.
        // "底座未登录 — 运行 claude auth login … — base stderr: error: invalid x-api-key"
        // instead of a blind "base session idle".
        let enriched = enrich_idle_reason(
            "base session idle",
            None,
            Some("error: invalid x-api-key".to_string()),
            "claude-code",
        );
        // The actionable line is prepended (auth → claude-code key)…
        assert!(
            enriched.starts_with(&crate::base_error::actionable_message(
                &crate::base_error::BaseFailure::Auth,
                "claude-code"
            )),
            "actionable line is prepended: {enriched}"
        );
        // …and the raw stderr tail is still appended for power users.
        assert!(enriched.contains("base stderr: error: invalid x-api-key"));
    }

    #[test]
    fn idle_timeout_reads_env_and_falls_back_safely() {
        let _env = IDLE_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _restore = EnvRestore::set("UMADEV_IDLE_TIMEOUT_SECS", "42");
        // A valid positive value is honoured.
        assert_eq!(idle_timeout(), Duration::from_secs(42));
        // A non-positive / garbage value falls back to the default (fail-open: a
        // bad env never DISABLES the watchdog, which would re-open the hang).
        std::env::set_var("UMADEV_IDLE_TIMEOUT_SECS", "0");
        assert_eq!(
            idle_timeout(),
            Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS)
        );
        std::env::set_var("UMADEV_IDLE_TIMEOUT_SECS", "nonsense");
        assert_eq!(
            idle_timeout(),
            Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS)
        );
        std::env::remove_var("UMADEV_IDLE_TIMEOUT_SECS");
        assert_eq!(
            idle_timeout(),
            Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS)
        );
    }

    #[test]
    fn tool_idle_timeout_reads_env_and_falls_back_safely() {
        let _env = IDLE_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // The EXTENDED tool-grace window honours its own env knob and is fail-open:
        // a non-positive / unparseable value falls back to the default (a bad env
        // never DISABLES the grace, and because the default is finite it can never
        // make the watchdog unbounded).
        let _restore = EnvRestore::set("UMADEV_TOOL_IDLE_TIMEOUT_SECS", "2400");
        assert_eq!(tool_idle_timeout(), Duration::from_secs(2400));
        std::env::set_var("UMADEV_TOOL_IDLE_TIMEOUT_SECS", "0");
        assert_eq!(
            tool_idle_timeout(),
            Duration::from_secs(DEFAULT_TOOL_IDLE_TIMEOUT_SECS)
        );
        std::env::set_var("UMADEV_TOOL_IDLE_TIMEOUT_SECS", "garbage");
        assert_eq!(
            tool_idle_timeout(),
            Duration::from_secs(DEFAULT_TOOL_IDLE_TIMEOUT_SECS)
        );
        std::env::remove_var("UMADEV_TOOL_IDLE_TIMEOUT_SECS");
        assert_eq!(
            tool_idle_timeout(),
            Duration::from_secs(DEFAULT_TOOL_IDLE_TIMEOUT_SECS)
        );
    }

    #[test]
    fn idle_defaults_dont_kill_ordinary_builds() {
        // The base default is 1200s so an ordinary slow non-tool turn - or a base pointed at
        // a rate-limited third-party model doing its OWN internal retry backoff - is not
        // mis-killed before its retry can land. The tool default is a 300s LIVENESS-POLL
        // interval (a re-check cadence, NOT a grace cap), so a tool of any duration with a
        // live base is never killed on silence — only the run budget bounds it.
        assert_eq!(DEFAULT_IDLE_TIMEOUT_SECS, 1200);
        assert_eq!(DEFAULT_TOOL_IDLE_TIMEOUT_SECS, 300);
        // Compile-time invariant: the poll interval is a positive, finite cadence (a
        // poll of 0 would busy-spin). A `const` block keeps the check at build time (and
        // satisfies clippy's `assertions_on_constants`, which forbids a runtime assert
        // over constants).
        const {
            assert!(
                DEFAULT_TOOL_IDLE_TIMEOUT_SECS > 0,
                "the liveness-poll interval must be a positive cadence"
            );
        }
    }

    #[test]
    fn tool_phase_transition_maps_tool_start_and_finish() {
        // A tool-use arms the liveness poll; a tool-result disarms it; everything
        // else leaves the flag unchanged (so text streaming never resets it).
        assert_eq!(
            tool_phase_transition(&SessionEvent::ToolCall {
                name: "Bash".into(),
                input: serde_json::json!({"command": "docker build ."}),
            }),
            Some(true)
        );
        assert_eq!(
            tool_phase_transition(&SessionEvent::ToolResult {
                ok: true,
                summary: "built".into(),
            }),
            Some(false)
        );
        assert_eq!(
            tool_phase_transition(&SessionEvent::TextDelta("…".into())),
            None
        );
        assert_eq!(
            tool_phase_transition(&SessionEvent::TurnDone {
                status: TurnStatus::Completed,
                usage: None,
            }),
            None
        );
    }

    #[test]
    fn idle_budget_picks_the_poll_window_only_while_in_a_tool_call() {
        let _env = IDLE_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // `window` picks the `tool` liveness-POLL interval while a tool is mid-flight,
        // and the `base` window otherwise (so a truly hung base — no tool running —
        // settles at the base window). Note the poll interval is no longer a "longer"
        // grace cap: with the defaults it is SHORTER than the base window (300s vs
        // 600s), because it is a re-check cadence, not a deadline.
        let budget = IdleBudget::new(Duration::from_secs(600), Duration::from_secs(300));
        assert_eq!(budget.window(false), Duration::from_secs(600));
        assert_eq!(budget.window(true), Duration::from_secs(300));
        // `from_env` wires the two env knobs (defaults here, no override set).
        let _base_env = EnvRestore::remove("UMADEV_IDLE_TIMEOUT_SECS");
        let _tool_env = EnvRestore::remove("UMADEV_TOOL_IDLE_TIMEOUT_SECS");
        let env_budget = IdleBudget::from_env();
        assert_eq!(
            env_budget.window(false),
            Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS)
        );
        assert_eq!(
            env_budget.window(true),
            Duration::from_secs(DEFAULT_TOOL_IDLE_TIMEOUT_SECS)
        );
    }

    #[test]
    fn idle_reason_names_the_long_task_case_not_a_login_problem() {
        // The misleading "check your login/model config" framing is gone: an idle
        // settle now leads with the long-task case (build/compile/install/test) and
        // points at the env knob — and carries the stable, locale-independent marker
        // the pumps/tests key off.
        let reason = idle_reason(Duration::from_secs(600));
        assert!(
            reason.contains("UMADEV_IDLE_TIMEOUT_SECS"),
            "names the env knob to raise: {reason}"
        );
        assert!(
            reason.contains("600"),
            "reports the elapsed window: {reason}"
        );
        // Not a login/auth scare line (the old chat-path framing).
        assert!(
            !reason.contains("登录") && !reason.to_lowercase().contains("log in"),
            "must not frame a silent build as a login problem: {reason}"
        );
    }

    /// A session that emits ONE tool-use event then HANGS forever while staying ALIVE
    /// (`try_exit_status` is the default `None`) — the legitimate-long-tool case (a
    /// `docker build` kicks off, then runs silently for minutes or hours). Used to prove
    /// the liveness watchdog keeps such a base alive INDEFINITELY past the base idle
    /// window: each poll re-checks the (live) base and keeps waiting, never settling on
    /// silence alone.
    struct ToolThenHangSession {
        emitted: bool,
    }

    #[async_trait::async_trait]
    impl BaseSession for ToolThenHangSession {
        async fn fork(&mut self) -> Result<Box<dyn BaseSession>, SessionError> {
            Err(SessionError::ForkUnsupported("hang".into()))
        }
        async fn send_turn(&mut self, _directive: String) -> Result<(), SessionError> {
            Ok(())
        }
        async fn next_event(&mut self) -> Option<SessionEvent> {
            if self.emitted {
                // The tool is running silently — never resolves.
                std::future::pending::<()>().await;
                None
            } else {
                self.emitted = true;
                Some(SessionEvent::ToolCall {
                    name: "Bash".into(),
                    input: serde_json::json!({"command": "docker build ."}),
                })
            }
        }
        async fn respond(
            &mut self,
            _req_id: &str,
            _decision: ApprovalDecision,
        ) -> Result<(), SessionError> {
            Ok(())
        }
        async fn interrupt(&mut self) -> Result<(), SessionError> {
            Ok(())
        }
        async fn end(&mut self) -> Result<(), SessionError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn mid_tool_silence_survives_the_base_window_but_a_bare_hang_settles() {
        // The regression this fixes: a base that fires a tool then goes silent for the
        // tool's whole duration must NOT be killed. With a TINY base window (50ms) and a
        // tiny tool POLL interval (20ms), the liveness watchdog re-checks the (live)
        // ToolCall-then-hang base every 20ms and keeps waiting — so it is still draining
        // well past the base window (we cancel at 300ms to keep the test fast), proof
        // the silence was never capped while the base stayed alive.
        let tmp = tempfile::TempDir::new().unwrap();
        let (events, _rec) = sink();
        let mut sess = ToolThenHangSession { emitted: false };
        let o = opts(tmp.path());
        let budget = IdleBudget::new(Duration::from_millis(50), Duration::from_millis(20));
        let pumped = tokio::time::timeout(
            Duration::from_millis(300),
            drive_one_turn(
                &mut sess,
                &o,
                &events,
                "build it".to_string(),
                budget,
                std::time::Instant::now() + Duration::from_secs(3_600),
            ),
        )
        .await;
        assert!(
            pumped.is_err(),
            "a base mid-tool must NOT settle on silence — the liveness poll keeps the \
             live base alive (so the outer 300ms cancel fires instead)"
        );

        // Control: the SAME tiny windows, but a base that hangs with NO tool in flight
        // settles promptly at the base window (the watchdog still catches a true hang —
        // the liveness model did not make the non-tool case unbounded).
        let mut hung = HangingSession;
        let bare = tokio::time::timeout(
            Duration::from_secs(2),
            drive_one_turn(
                &mut hung,
                &o,
                &events,
                "build it".to_string(),
                budget,
                std::time::Instant::now() + Duration::from_secs(3_600),
            ),
        )
        .await
        .expect("a bare hang (no tool running) must settle at the base window");
        match bare {
            Err(reason) => assert!(
                reason.contains("UMADEV_IDLE_TIMEOUT_SECS"),
                "a true hang still settles as an idle reason: {reason}"
            ),
            Ok(_) => panic!("a hung base must settle as an Err, not Ok"),
        }
    }

    #[tokio::test]
    async fn run_budget_reached_mid_tool_settles_gracefully_not_failed() {
        // MEDIUM M5: when the wall-clock run budget expires DURING a silent tool turn,
        // the turn must settle GRACEFULLY so the run still reaches run_auto_qc +
        // finalize — exactly like a budget reached mid-STREAM. Before the fix the
        // in-tool `IdleTimedOut` returned Err → the run was Failed and SKIPPED QC +
        // delivery, purely because the deadline happened to land mid-tool rather than
        // mid-stream. A near-future deadline + a base that fires a tool then hangs
        // exercises that exact path; the loop must end as Done, not Failed.
        let tmp = tempfile::TempDir::new().unwrap();
        let (events, _rec) = sink();
        let mut sess = ToolThenHangSession { emitted: false };
        let o = opts(tmp.path());
        let outcome = drive_director_loop_with_idle(
            &mut sess,
            &o,
            &events,
            "GO".to_string(),
            None,
            None,
            IdleBudget::new(Duration::from_millis(40), Duration::from_millis(40)),
            std::time::Instant::now() + Duration::from_millis(140),
        )
        .await;
        assert!(
            matches!(outcome, DirectorLoopOutcome::Done { .. }),
            "a budget reached mid-tool settles gracefully (Done), not Failed: {outcome:?}"
        );
    }

    /// A real, already-exited `ExitStatus` for the "base died mid-tool" fixtures —
    /// constructed by running a trivial process, so no platform-specific / unsafe
    /// `from_raw`. Deterministic on every Unix-like CI / dev box.
    fn a_real_exit_status() -> std::process::ExitStatus {
        std::process::Command::new("true")
            .status()
            .expect("spawn `true` to obtain a real ExitStatus")
    }

    /// A base whose `next_event` never resolves (a tool runs silently) with a
    /// configurable `try_exit_status` (alive = `None`, dead = `Some`) and an interrupt
    /// counter — the fixture for the liveness watchdog's three in-tool / non-tool settle
    /// paths. `next_event_idle` is driven directly so the four behaviours are asserted
    /// without going through a whole turn.
    struct ProbeSession {
        exit: Option<std::process::ExitStatus>,
        interrupts: Arc<std::sync::Mutex<u32>>,
    }

    impl ProbeSession {
        fn new(exit: Option<std::process::ExitStatus>) -> Self {
            Self {
                exit,
                interrupts: Arc::new(std::sync::Mutex::new(0)),
            }
        }
        fn interrupts(&self) -> Arc<std::sync::Mutex<u32>> {
            Arc::clone(&self.interrupts)
        }
    }

    #[async_trait::async_trait]
    impl BaseSession for ProbeSession {
        async fn fork(&mut self) -> Result<Box<dyn BaseSession>, SessionError> {
            Err(SessionError::ForkUnsupported("probe".into()))
        }
        async fn send_turn(&mut self, _directive: String) -> Result<(), SessionError> {
            Ok(())
        }
        async fn next_event(&mut self) -> Option<SessionEvent> {
            // A silently-running tool: never resolves.
            std::future::pending::<()>().await;
            None
        }
        async fn respond(
            &mut self,
            _req_id: &str,
            _decision: ApprovalDecision,
        ) -> Result<(), SessionError> {
            Ok(())
        }
        async fn interrupt(&mut self) -> Result<(), SessionError> {
            *self.interrupts.lock().unwrap() += 1;
            Ok(())
        }
        async fn end(&mut self) -> Result<(), SessionError> {
            Ok(())
        }
        fn try_exit_status(&self) -> Option<std::process::ExitStatus> {
            self.exit
        }
    }

    #[tokio::test]
    async fn next_event_idle_in_tool_with_a_live_base_keeps_waiting_past_the_poll_window() {
        // (a) The crux of the liveness refinement: a tool in flight + a LIVE base
        // (try_exit_status None) must NOT settle just because the poll window elapsed —
        // it keeps re-checking and waiting. With a 20ms poll and a far-future deadline,
        // `next_event_idle` should still be running well past several poll windows (we
        // cancel at 250ms), i.e. it did NOT return an IdleTimedOut on silence alone.
        let mut sess = ProbeSession::new(None);
        let budget = IdleBudget::new(Duration::from_millis(50), Duration::from_millis(20));
        let out = tokio::time::timeout(
            Duration::from_millis(250),
            next_event_idle(
                &mut sess,
                budget,
                true,
                Some(std::time::Instant::now() + Duration::from_secs(3_600)),
            ),
        )
        .await;
        assert!(
            out.is_err(),
            "an in-tool LIVE base must keep waiting past the poll window, never settle on \
             silence (the outer 250ms cancel must fire instead)"
        );
        assert_eq!(
            *sess.interrupts().lock().unwrap(),
            0,
            "a live in-tool base is never interrupted by the watchdog"
        );
    }

    #[tokio::test]
    async fn next_event_idle_in_tool_with_a_dead_base_settles_as_session_ended() {
        // (b) A base that died mid-tool (try_exit_status Some, no event) is caught by
        // the liveness poll within ONE poll window and settles as SessionEnded — NOT an
        // unbounded wait, and NOT a misleading idle-hang.
        let mut sess = ProbeSession::new(Some(a_real_exit_status()));
        let budget = IdleBudget::new(Duration::from_millis(50), Duration::from_millis(20));
        let ev = tokio::time::timeout(
            Duration::from_secs(2),
            next_event_idle(
                &mut sess,
                budget,
                true,
                Some(std::time::Instant::now() + Duration::from_secs(3_600)),
            ),
        )
        .await
        .expect("a dead in-tool base must settle within one poll window, not hang");
        match ev {
            IdleEvent::SessionEnded { exit, .. } => {
                assert!(
                    exit.is_some(),
                    "the base's exit status is surfaced: {exit:?}"
                );
            }
            other => panic!("expected SessionEnded for a dead in-tool base, got {other:?}"),
        }
        assert_eq!(
            *sess.interrupts().lock().unwrap(),
            0,
            "an already-dead base is not interrupted (it has already exited)"
        );
    }

    #[tokio::test]
    async fn next_event_idle_non_tool_hang_settles_at_the_base_window_with_a_bounded_interrupt() {
        // (c) A genuinely hung base that is NOT in a tool still settles at the base
        // window (the non-tool case is never made unbounded), and the watchdog issues
        // its ONE best-effort bounded interrupt before settling.
        let mut sess = ProbeSession::new(None);
        let budget = IdleBudget::new(Duration::from_millis(20), Duration::from_millis(20));
        let ev = tokio::time::timeout(
            Duration::from_secs(2),
            next_event_idle(&mut sess, budget, false, None),
        )
        .await
        .expect("a non-tool hang must settle at the base window, not run forever");
        assert!(
            matches!(ev, IdleEvent::IdleTimedOut { .. }),
            "a non-tool hang settles as IdleTimedOut: {ev:?}"
        );
        assert_eq!(
            *sess.interrupts().lock().unwrap(),
            1,
            "the non-tool hang path issues exactly one best-effort interrupt"
        );
    }

    #[tokio::test]
    async fn next_event_idle_in_tool_live_base_settles_when_the_run_budget_is_exhausted() {
        // (d) The outer backstop: a LIVE base mid-tool keeps waiting, but only until the
        // overall run-budget deadline. A deadline already in the PAST settles the very
        // first poll as IdleTimedOut — the run budget is the single bound on the
        // otherwise-indefinite in-tool wait. No interrupt here (the run finalization /
        // session.end() owns releasing the still-live base).
        let mut sess = ProbeSession::new(None);
        let budget = IdleBudget::new(Duration::from_millis(50), Duration::from_millis(20));
        let past = std::time::Instant::now()
            .checked_sub(Duration::from_secs(1))
            .unwrap();
        let ev = tokio::time::timeout(
            Duration::from_secs(2),
            next_event_idle(&mut sess, budget, true, Some(past)),
        )
        .await
        .expect("an in-tool live base past its run budget must settle promptly");
        assert!(
            matches!(ev, IdleEvent::IdleTimedOut { .. }),
            "the run-budget deadline settles an in-tool live base as IdleTimedOut: {ev:?}"
        );
        assert_eq!(
            *sess.interrupts().lock().unwrap(),
            0,
            "the in-tool budget backstop does not interrupt (the run finalization does)"
        );
    }

    // ── Visible retry + silent-hang watchdog re-drive ──────────────────────

    /// A failed-turn batch carrying the base's OWN error text (the transient/hard
    /// failure the `TurnStatus::Failed` retry path classifies).
    fn fail_turn(reason: &str) -> Vec<SessionEvent> {
        vec![SessionEvent::TurnDone {
            status: TurnStatus::Failed(reason.to_string()),
            usage: None,
        }]
    }

    /// A base whose every turn is a NON-tool silent hang (`next_event` never resolves)
    /// while it stays ALIVE (`try_exit_status` defaults to `None`), counting each
    /// `send_turn` — the fixture for the silent-hang WATCHDOG RE-DRIVE (a live base that
    /// may have dropped its stream is re-driven once before failing).
    struct CountingHangSession {
        sends: Arc<std::sync::Mutex<u32>>,
    }

    #[async_trait::async_trait]
    impl BaseSession for CountingHangSession {
        async fn fork(&mut self) -> Result<Box<dyn BaseSession>, SessionError> {
            Err(SessionError::ForkUnsupported("hang".into()))
        }
        async fn send_turn(&mut self, _directive: String) -> Result<(), SessionError> {
            *self.sends.lock().unwrap() += 1;
            Ok(())
        }
        async fn next_event(&mut self) -> Option<SessionEvent> {
            std::future::pending::<()>().await;
            None
        }
        async fn respond(
            &mut self,
            _req_id: &str,
            _decision: ApprovalDecision,
        ) -> Result<(), SessionError> {
            Ok(())
        }
        async fn interrupt(&mut self) -> Result<(), SessionError> {
            Ok(())
        }
        async fn end(&mut self) -> Result<(), SessionError> {
            Ok(())
        }
    }

    /// A base that emits ONE `ToolCall` per turn then hangs silently while staying ALIVE
    /// — the IN-TOOL case the watchdog must NOT re-drive (a long-running tool is
    /// legitimately silent). Counts each `send_turn` so a spurious re-drive is caught.
    struct CountingToolHangSession {
        sends: Arc<std::sync::Mutex<u32>>,
        emitted_tool: bool,
    }

    #[async_trait::async_trait]
    impl BaseSession for CountingToolHangSession {
        async fn fork(&mut self) -> Result<Box<dyn BaseSession>, SessionError> {
            Err(SessionError::ForkUnsupported("hang".into()))
        }
        async fn send_turn(&mut self, _directive: String) -> Result<(), SessionError> {
            *self.sends.lock().unwrap() += 1;
            self.emitted_tool = false;
            Ok(())
        }
        async fn next_event(&mut self) -> Option<SessionEvent> {
            if self.emitted_tool {
                std::future::pending::<()>().await;
                None
            } else {
                self.emitted_tool = true;
                Some(SessionEvent::ToolCall {
                    name: "Bash".into(),
                    input: serde_json::json!({"command": "docker build ."}),
                })
            }
        }
        async fn respond(
            &mut self,
            _req_id: &str,
            _decision: ApprovalDecision,
        ) -> Result<(), SessionError> {
            Ok(())
        }
        async fn interrupt(&mut self) -> Result<(), SessionError> {
            Ok(())
        }
        async fn end(&mut self) -> Result<(), SessionError> {
            Ok(())
        }
    }

    #[test]
    fn transient_backoff_is_exponential_capped_and_bounded() {
        // Exponential off the base, capped — deterministic + never unbounded.
        let base = Duration::from_secs(2);
        let cap = Duration::from_secs(30);
        assert_eq!(transient_backoff_wait(base, cap, 1), Duration::from_secs(2));
        assert_eq!(transient_backoff_wait(base, cap, 2), Duration::from_secs(4));
        assert_eq!(transient_backoff_wait(base, cap, 3), Duration::from_secs(8));
        // A large attempt saturates at the cap, never overflows.
        assert_eq!(transient_backoff_wait(base, cap, 50), cap);
        // attempt 0 is total (yields the base), never a panic.
        assert_eq!(transient_backoff_wait(base, cap, 0), base);
    }

    #[tokio::test]
    async fn a_transient_failure_emits_a_countdown_note_then_recovers() {
        // Part 1: a base turn-failure the classifier reads as TRANSIENT (a 429) is backed
        // off and re-driven, and the wait is VISIBLE — a countdown Note is emitted BEFORE
        // the backoff. The second turn completes, so the turn recovers.
        let tmp = tempfile::TempDir::new().unwrap();
        let (events, rec) = sink();
        let turns = vec![
            fail_turn("API Error: Request rejected (429) — rate limit"),
            text_turn("recovered"),
        ];
        let mut sess = FakeSession::new(turns, false, "");
        let sent = sess.sent_handle();
        let out = drive_one_turn_with_backoff(
            &mut sess,
            &opts(tmp.path()),
            &events,
            "build it".to_string(),
            IdleBudget::new(Duration::from_secs(5), Duration::from_secs(5)),
            std::time::Instant::now() + Duration::from_secs(3_600),
            // Tiny, fast backoff window so the test never really waits seconds.
            Duration::from_millis(2),
            Duration::from_millis(20),
        )
        .await;
        match out {
            Ok(r) => assert_eq!(r.text, "recovered", "the turn recovered after one backoff"),
            Err(e) => panic!("a transient 429 must be retried to recovery: {e}"),
        }
        // Re-driven once: the initial directive + one retry = 2 sends.
        assert_eq!(
            sent.lock().unwrap().len(),
            2,
            "the transient failure is re-driven once"
        );
        // The backoff is VISIBLE — a countdown Note with the stable, locale-independent
        // "(attempt 1/3)" marker was surfaced before recovery.
        assert!(
            rec.events()
                .iter()
                .any(|e| matches!(e, EngineEvent::Note(n) if n.contains("1/3"))),
            "a countdown Note is surfaced before the backoff wait"
        );
    }

    #[tokio::test]
    async fn transient_retries_are_bounded_and_fail_open() {
        // Part 1 boundedness: a base that ALWAYS fails transiently is retried only a
        // bounded number of times, then fails honestly with the raw reason intact
        // (fail-open) — never an infinite retry loop.
        let tmp = tempfile::TempDir::new().unwrap();
        let (events, rec) = sink();
        let turns = vec![fail_turn("429 too many requests"); 6];
        let mut sess = FakeSession::new(turns, false, "");
        let sent = sess.sent_handle();
        let out = drive_one_turn_with_backoff(
            &mut sess,
            &opts(tmp.path()),
            &events,
            "build it".to_string(),
            IdleBudget::new(Duration::from_secs(5), Duration::from_secs(5)),
            std::time::Instant::now() + Duration::from_secs(3_600),
            Duration::from_millis(1),
            Duration::from_millis(10),
        )
        .await;
        match out {
            Err(reason) => assert!(
                reason.contains("429"),
                "the base's raw error survives the bounded retry: {reason}"
            ),
            Ok(_) => {
                panic!("a base that always fails transiently must still fail, not loop forever")
            }
        }
        // Bounded: the initial send + EXACTLY `MAX_TRANSIENT_RETRIES` retries.
        assert_eq!(
            sent.lock().unwrap().len(),
            (MAX_TRANSIENT_RETRIES + 1) as usize,
            "transient retries are bounded by MAX_TRANSIENT_RETRIES"
        );
        // One visible countdown per retry (the "/3" max is locale-independent).
        let countdowns = rec
            .events()
            .iter()
            .filter(|e| matches!(e, EngineEvent::Note(n) if n.contains("/3")))
            .count();
        assert_eq!(
            countdowns, MAX_TRANSIENT_RETRIES as usize,
            "one visible countdown Note per bounded retry"
        );
    }

    #[tokio::test]
    async fn a_hard_failure_is_not_retried() {
        // A HARD failure (auth) is returned at once — retrying it is futile, so NO
        // backoff, NO countdown, exactly ONE send.
        let tmp = tempfile::TempDir::new().unwrap();
        let (events, rec) = sink();
        let turns = vec![fail_turn("401 Unauthorized — not logged in")];
        let mut sess = FakeSession::new(turns, false, "");
        let sent = sess.sent_handle();
        let out = drive_one_turn_with_backoff(
            &mut sess,
            &opts(tmp.path()),
            &events,
            "build it".to_string(),
            IdleBudget::new(Duration::from_secs(5), Duration::from_secs(5)),
            std::time::Instant::now() + Duration::from_secs(3_600),
            Duration::from_millis(1),
            Duration::from_millis(10),
        )
        .await;
        assert!(out.is_err(), "an auth failure fails honestly");
        assert_eq!(
            sent.lock().unwrap().len(),
            1,
            "a hard (auth) failure is never retried"
        );
        assert_eq!(
            rec.count(|e| matches!(e, EngineEvent::Note(n) if n.contains("/3"))),
            0,
            "a hard failure emits no countdown Note"
        );
    }

    #[tokio::test]
    async fn non_tool_silent_hang_on_a_live_base_redrives_once_then_fails() {
        // Part 2: a NON-tool silent hang on a STILL-ALIVE base (it may have dropped its
        // stream) is re-driven EXACTLY once before failing — a bounded single retry, not
        // an infinite re-drive.
        let tmp = tempfile::TempDir::new().unwrap();
        let (events, rec) = sink();
        let sends = Arc::new(std::sync::Mutex::new(0u32));
        let mut sess = CountingHangSession {
            sends: Arc::clone(&sends),
        };
        // Tiny base window so the non-tool hang settles fast.
        let budget = IdleBudget::new(Duration::from_millis(20), Duration::from_millis(20));
        let out = tokio::time::timeout(
            Duration::from_secs(2),
            drive_one_turn(
                &mut sess,
                &opts(tmp.path()),
                &events,
                "build it".to_string(),
                budget,
                std::time::Instant::now() + Duration::from_secs(3_600),
            ),
        )
        .await
        .expect("the bounded re-drive must settle, never hang forever");
        match out {
            Err(reason) => assert!(
                reason.contains("UMADEV_IDLE_TIMEOUT_SECS"),
                "a second hang fails honestly as an idle settle: {reason}"
            ),
            Ok(_) => panic!("a base that only ever hangs must fail, not succeed"),
        }
        // Re-driven EXACTLY once: the initial send + one watchdog re-drive = 2 sends.
        assert_eq!(
            *sends.lock().unwrap(),
            2,
            "the silent-hang watchdog re-drives exactly once (bounded)"
        );
        // The re-drive is VISIBLE.
        let redrive = umadev_i18n::tl("tui.retry.silent_redrive");
        assert!(
            rec.events()
                .iter()
                .any(|e| matches!(e, EngineEvent::Note(n) if n == redrive)),
            "the silent-hang re-drive emits a visible Note"
        );
    }

    #[tokio::test]
    async fn in_tool_silent_hang_on_a_live_base_never_redrives() {
        // Part 2 guard: an IN-TOOL live base (a long `docker build`) goes silent but must
        // NOT be re-driven — the liveness watchdog keeps waiting, so the only base call is
        // the original send. Proves the re-drive never fights the legitimate long-tool
        // wait (no spurious retry).
        let tmp = tempfile::TempDir::new().unwrap();
        let (events, _rec) = sink();
        let sends = Arc::new(std::sync::Mutex::new(0u32));
        let mut sess = CountingToolHangSession {
            sends: Arc::clone(&sends),
            emitted_tool: false,
        };
        let budget = IdleBudget::new(Duration::from_millis(20), Duration::from_millis(20));
        let pumped = tokio::time::timeout(
            Duration::from_millis(300),
            drive_one_turn(
                &mut sess,
                &opts(tmp.path()),
                &events,
                "build it".to_string(),
                budget,
                std::time::Instant::now() + Duration::from_secs(3_600),
            ),
        )
        .await;
        assert!(
            pumped.is_err(),
            "an in-tool live base keeps waiting (no settle, no re-drive — the outer cancel fires)"
        );
        // Driven exactly ONCE — the in-tool wait never re-drives (no spurious retry).
        assert_eq!(
            *sends.lock().unwrap(),
            1,
            "an in-tool live hang is never re-driven"
        );
    }

    // ── Auto-QC units ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn auto_qc_clean_when_source_present_and_no_review_team() {
        // Real source on disk, no manifest (build/test skipped → neutral), no fork
        // (no review) → QC clean.
        let tmp = tempfile::TempDir::new().unwrap();
        seed_source(tmp.path());
        let (events, _rec) = sink();
        let mut sess = FakeSession::new(vec![], false, "");
        let o = opts(tmp.path());
        let qc = run_auto_qc(&mut sess, &o, &events, None, None, false).await;
        assert!(qc.is_clean(), "source present + nothing to fail → clean QC");
    }

    /// A codex-tier `RunOptions` — a non-claude backend (no real-time governance
    /// hook), so the director auto-QC must run the content-governance catch-up.
    fn codex_opts(root: &std::path::Path) -> RunOptions {
        let mut o = opts(root);
        o.backend = "codex".to_string();
        o
    }

    #[tokio::test]
    async fn auto_qc_governs_codex_writes_and_blocks_on_emoji_icon() {
        // P1-1: codex / opencode have NO real-time hook, so the director QC pass is
        // their ONLY content-governance gate. A file the base wrote using an emoji
        // as a functional icon must surface as a `[governance]` blocking finding,
        // which the loop folds into a fix directive.
        let tmp = tempfile::TempDir::new().unwrap();
        // A clean source so the source-present floor passes, plus a button that uses
        // an emoji as its icon (a universal-floor violation, context-independent).
        std::fs::write(
            tmp.path().join("button.tsx"),
            "export const Btn = () => <button>\u{1F680} Launch</button>;",
        )
        .unwrap();
        let (events, _rec) = sink();
        let mut sess = FakeSession::new(vec![], false, "");
        let o = codex_opts(tmp.path());
        let qc = run_auto_qc(&mut sess, &o, &events, None, None, false).await;
        assert!(
            !qc.is_clean(),
            "an emoji-as-icon write by codex must be governed: {:?}",
            qc.blocking
        );
        assert!(
            qc.blocking.iter().any(|b| b.starts_with("[governance]")),
            "the finding is tagged [governance]: {:?}",
            qc.blocking
        );
    }

    #[tokio::test]
    async fn auto_qc_governs_craft_for_claude_too() {
        // The claude real-time hook no longer screens CRAFT (it now refuses only the
        // irreversible-if-written floor — secrets/paths — so it never pins the
        // base's hands for a fixable nit). So the QC content-governance scan is the
        // craft moat for EVERY backend, claude included: the same emoji-as-icon file
        // that codex's QC flags must be flagged here too, then repaired by the
        // feedback loop.
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("button.tsx"),
            "export const Btn = () => <button>\u{1F680} Launch</button>;",
        )
        .unwrap();
        let (events, _rec) = sink();
        let mut sess = FakeSession::new(vec![], false, "");
        let mut o = opts(tmp.path());
        o.backend = "claude-code".to_string();
        let qc = run_auto_qc(&mut sess, &o, &events, None, None, false).await;
        assert!(
            !qc.is_clean(),
            "an emoji-as-icon write must be governed by QC even on claude: {:?}",
            qc.blocking
        );
        assert!(
            qc.blocking.iter().any(|b| b.starts_with("[governance]")),
            "the finding is tagged [governance]: {:?}",
            qc.blocking
        );
    }

    #[tokio::test]
    async fn auto_qc_governance_does_not_falsely_flag_a_clean_static_page() {
        // Context-aware: a clean static frontend page (codex backend) must NOT be
        // flagged for a missing server-surface rule (CSP / HSTS / structured log) —
        // it serves none. A benign HTML page → clean QC even on the governed path.
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("index.html"),
            "<!doctype html><html><body><h1>Hello</h1><p>A static page.</p></body></html>",
        )
        .unwrap();
        let (events, _rec) = sink();
        let mut sess = FakeSession::new(vec![], false, "");
        let mut o = codex_opts(tmp.path());
        o.requirement = "做一个简单的静态介绍页,纯前端".to_string();
        let qc = run_auto_qc(&mut sess, &o, &events, None, None, false).await;
        assert!(
            qc.is_clean(),
            "a clean static page must not be falsely flagged: {:?}",
            qc.blocking
        );
    }

    #[tokio::test]
    async fn auto_qc_blocks_when_no_source_present() {
        // No source on disk after a claimed build → the hard floor is the decisive
        // blocking finding (and QC returns early without running build/review).
        let tmp = tempfile::TempDir::new().unwrap();
        let (events, _rec) = sink();
        let mut sess = FakeSession::new(vec![], false, "");
        let o = opts(tmp.path());
        let qc = run_auto_qc(&mut sess, &o, &events, None, None, false).await;
        assert!(!qc.is_clean(), "no source → blocking");
        assert!(
            qc.blocking.iter().any(|b| b.contains("source-present")),
            "the hard-floor finding is present: {:?}",
            qc.blocking
        );
    }

    /// A lean-tier `RunOptions` — a clearly-small requirement that
    /// `planner::is_lean_build` classifies as lean (Light), so QC takes the
    /// stripped-down path (source floor only, no duplicate build / fork review).
    fn lean_opts(root: &std::path::Path) -> RunOptions {
        let mut o = opts(root);
        o.requirement = "做一个简单的待办清单单页应用,纯前端".to_string();
        o
    }

    #[tokio::test]
    async fn lean_goal_qc_stops_at_source_floor_and_skips_review() {
        // A lean goal with real source on disk → QC is clean WITHOUT convening the
        // fork review. The session here CAN fork and would return a BLOCKING verdict
        // if the review ran; the lean tier must short-circuit BEFORE that, so the
        // blocking finding never appears → clean.
        let tmp = tempfile::TempDir::new().unwrap();
        seed_source(tmp.path());
        let (events, _rec) = sink();
        let reply = r#"{"accepts": false, "blocking": ["a review nit that must NOT surface"]}"#;
        let mut sess = FakeSession::new(vec![], true, reply);
        let o = lean_opts(tmp.path());
        let qc = run_auto_qc(&mut sess, &o, &events, None, None, false).await;
        assert!(
            qc.is_clean(),
            "a lean goal with source present is clean — the fork review is skipped: {:?}",
            qc.blocking
        );
    }

    #[tokio::test]
    async fn lean_goal_qc_still_enforces_the_source_present_hard_floor() {
        // The lean tier must NEVER drop the honesty hard floor: a lean goal that
        // CLAIMED a build but wrote zero source is STILL caught (the one invariant
        // the fast path keeps). Empty tree → the source-present blocking finding.
        let tmp = tempfile::TempDir::new().unwrap();
        let (events, _rec) = sink();
        let mut sess = FakeSession::new(vec![], false, "");
        let o = lean_opts(tmp.path());
        let qc = run_auto_qc(&mut sess, &o, &events, None, None, false).await;
        assert!(!qc.is_clean(), "a lean goal with no source still blocks");
        assert!(
            qc.blocking.iter().any(|b| b.contains("source-present")),
            "the hard-floor finding fires on the lean tier too: {:?}",
            qc.blocking
        );
    }

    /// Did the sink record a Note whose text contains `needle`?
    fn note_seen(rec: &RecordingSink, needle: &str) -> bool {
        rec.events().iter().any(|e| match e {
            EngineEvent::Note(n) => n.contains(needle),
            _ => false,
        })
    }

    #[tokio::test]
    async fn incremental_verify_skips_the_duplicate_build_when_base_ran_it_green() {
        // Wave 3 incremental verify (honesty-tightened fast path): a base reply that
        // reports a PASSED build/test AND is CORROBORATED by an OBSERVED build/test runner
        // this turn (`ran_build_tool = true`) skips UmaDev's OWN duplicate read — it emits
        // the "trusting its result" note and NOT the "verify build-test" note. This is the
        // honest run's fast path, preserved. The source-present floor + governance still
        // ran (clean here), so QC is clean.
        let tmp = tempfile::TempDir::new().unwrap();
        seed_source(tmp.path());
        let (events, rec) = sink();
        let mut sess = FakeSession::new(vec![], false, "");
        let o = opts(tmp.path()); // "做一个登录系统" — non-lean, so it reaches the build read
        let reply = "Implemented the login system end to end. Ran `npm test` and `npm run build` — all tests pass and the build succeeded (exit code 0).";
        // ran_build_tool = true: a real runner WAS observed on the tool-call stream.
        let qc = run_auto_qc(&mut sess, &o, &events, None, Some(reply), true).await;
        assert!(
            qc.is_clean(),
            "clean source + corroborated build → clean: {:?}",
            qc.blocking
        );
        assert!(
            note_seen(&rec, "base already ran build/test green"),
            "the incremental-verify skip note must be emitted"
        );
        assert!(
            !note_seen(&rec, "verify build-test"),
            "the duplicate build/test read must be skipped (no verify note)"
        );
    }

    #[tokio::test]
    async fn incremental_verify_does_not_skip_a_green_claim_with_no_observed_run() {
        // HONESTY TIGHTENING (the hole closed): the base CLAIMS a green build with all the
        // machine-evidence words a text scan would trust ("ran npm test", "exit code 0"),
        // but NO build/test runner was observed on the tool-call stream this turn
        // (`ran_build_tool = false`) — it narrated the run without running it. UmaDev must
        // NOT skip its own read: it does NOT emit the "trusting its result" note and DOES
        // run its own build/test read. A re-verify, never a false FAIL — with no manifest
        // the read is neutral, so QC is still clean (a genuinely clean build re-passes).
        let tmp = tempfile::TempDir::new().unwrap();
        seed_source(tmp.path());
        let (events, rec) = sink();
        let mut sess = FakeSession::new(vec![], false, "");
        let o = opts(tmp.path());
        let reply = "Implemented the login system end to end. Ran `npm test` and `npm run build` — all tests pass and the build succeeded (exit code 0).";
        // ran_build_tool = false: the base narrated a run it never actually performed.
        let qc = run_auto_qc(&mut sess, &o, &events, None, Some(reply), false).await;
        assert!(
            qc.is_clean(),
            "no manifest → neutral re-verify, still clean (no false FAIL): {:?}",
            qc.blocking
        );
        assert!(
            !note_seen(&rec, "base already ran build/test green"),
            "an un-corroborated green claim must NOT trigger the skip"
        );
        assert!(
            note_seen(&rec, "verify build-test"),
            "UmaDev runs its OWN read when a green claim is not corroborated by a real run"
        );
    }

    #[tokio::test]
    async fn incremental_verify_runs_our_own_read_when_reply_is_ambiguous() {
        // No reply / an ambiguous reply (no explicit passed-run) → UmaDev falls back to
        // running its OWN build/test read (prior behaviour, no regression). With no
        // manifest the read returns unavailable (neutral) fast, but the verify note
        // proves UmaDev did NOT trust an unproven build.
        let tmp = tempfile::TempDir::new().unwrap();
        seed_source(tmp.path());
        let (events, rec) = sink();
        let mut sess = FakeSession::new(vec![], false, "");
        let o = opts(tmp.path());
        // Ambiguous "done" — no "tests pass"/"build succeeded" → must NOT skip.
        let qc = run_auto_qc(
            &mut sess,
            &o,
            &events,
            None,
            Some("Done — implemented it."),
            false,
        )
        .await;
        assert!(
            qc.is_clean(),
            "no manifest → neutral build read, still clean"
        );
        assert!(
            !note_seen(&rec, "base already ran build/test green"),
            "an ambiguous reply must NOT trigger the skip"
        );
        assert!(
            note_seen(&rec, "verify build-test"),
            "UmaDev runs its own build/test read when the base's result is unproven"
        );
    }

    #[test]
    fn build_test_blocking_is_none_when_skipped_or_passed() {
        // An unavailable (skipped) check is neutral, not a false failure (fail-open).
        let skipped = VerifyResult {
            available: false,
            passed: true,
            evidence: vec![],
        };
        assert!(build_test_blocking(&skipped).is_none());
        // A passing check is not blocking.
        let ok = VerifyResult {
            available: true,
            passed: true,
            evidence: vec!["build: ok".into()],
        };
        assert!(build_test_blocking(&ok).is_none());
        // A real failure is blocking, carrying the evidence.
        let bad = VerifyResult {
            available: true,
            passed: false,
            evidence: vec!["build: FAILED (exit 1)".into()],
        };
        let line = build_test_blocking(&bad).expect("a failed step blocks");
        assert!(line.contains("FAILED") && line.contains("exit 1"));
    }

    #[test]
    fn fix_directive_lists_every_blocking_finding() {
        let qc = QcReport {
            blocking: vec![
                "verify build-test: FAILED — build: FAILED (exit 1)".into(),
                "[security] no input validation".into(),
            ],
        };
        let d = qc.fix_directive();
        assert!(d.contains("must be fixed"));
        assert!(d.contains("build: FAILED"));
        assert!(d.contains("no input validation"));
    }

    // ── Wave 4: required acceptance floor (deliberate only; bugfix repro test) ──

    /// Write a PRD declaring FR-001 + FR-002 and a tasks list covering only FR-001,
    /// so `uncovered_requirements` reports FR-002 as a coverage gap.
    fn seed_coverage_gap(root: &std::path::Path) {
        std::fs::create_dir_all(root.join("output")).unwrap();
        std::fs::write(
            root.join("output").join("demo-prd.md"),
            "| FR-001 | login |\n| FR-002 | logout |",
        )
        .unwrap();
        let cdir = root.join(".umadev").join("changes").join("demo-1");
        std::fs::create_dir_all(&cdir).unwrap();
        std::fs::write(cdir.join("tasks.md"), "- [ ] login _(FR-001)_").unwrap();
    }

    /// A Bugfix route (Standard depth) for the reproduction-test floor test.
    fn bugfix_route() -> crate::router::RoutePlan {
        let mut r = build_route();
        r.kind = crate::planner::TaskKind::Bugfix;
        r
    }

    #[test]
    fn acceptance_floor_blocks_a_deliberate_build_with_a_coverage_gap() {
        // A deliberate build with a declared-but-unimplemented requirement must
        // surface a coverage gap as a blocking finding (the required floor).
        let tmp = tempfile::TempDir::new().unwrap();
        seed_coverage_gap(tmp.path());
        let o = opts(tmp.path());
        let route = build_route();
        let blocking = acceptance_floor_blocking(&o, Some(&route));
        assert!(
            blocking
                .iter()
                .any(|b| b.contains("coverage gap") && b.contains("FR-002")),
            "the uncovered requirement is a blocking finding: {blocking:?}"
        );
    }

    #[tokio::test]
    async fn deliberate_qc_enforces_the_acceptance_floor_lean_skips_it() {
        // The acceptance floor is REQUIRED on the deliberate path but NOT on lean.
        // Same project (a coverage gap) → blocks on a deliberate route, clean on a
        // lean requirement (which returns before the floor — speed preserved).
        let tmp = tempfile::TempDir::new().unwrap();
        seed_source(tmp.path());
        seed_coverage_gap(tmp.path());
        let (events, _rec) = sink();
        let mut sess = FakeSession::new(vec![], false, "");

        // Deliberate route → the floor runs → the coverage gap blocks.
        let mut deliberate = opts(tmp.path());
        deliberate.requirement = "做一个完整的任务管理产品".to_string();
        let route = build_route();
        let qc = run_auto_qc(&mut sess, &deliberate, &events, Some(&route), None, false).await;
        assert!(
            qc.blocking.iter().any(|b| b.contains("coverage gap")),
            "deliberate QC enforces the acceptance floor: {:?}",
            qc.blocking
        );

        // Lean requirement → QC returns at the lean short-circuit, BEFORE the floor.
        let mut lean = opts(tmp.path());
        lean.requirement = "做一个简单的待办清单单页应用,纯前端".to_string();
        let qc2 = run_auto_qc(&mut sess, &lean, &events, None, None, false).await;
        assert!(
            !qc2.blocking.iter().any(|b| b.contains("coverage gap")),
            "a lean goal does NOT pay the acceptance floor (speed): {:?}",
            qc2.blocking
        );
    }

    #[tokio::test]
    async fn deliberate_route_with_lean_reading_requirement_still_runs_full_gate() {
        // M2 regression: the lean short-circuit must key off the ROUTE's brain-decided
        // depth, NOT a re-derived keyword classify(requirement). A DELIBERATE route whose
        // requirement happens to READ lean ("做一个简单的待办单页") must still run the
        // FULL gate (the acceptance floor), not settle after source-present + governance.
        let tmp = tempfile::TempDir::new().unwrap();
        seed_source(tmp.path());
        seed_coverage_gap(tmp.path());
        let (events, _rec) = sink();
        let mut sess = FakeSession::new(vec![], false, "");
        let mut o = opts(tmp.path());
        // A requirement the keyword classifier would call LEAN…
        o.requirement = "做一个简单的待办清单单页应用,纯前端".to_string();
        assert!(
            crate::planner::is_lean_build(&o.requirement),
            "precondition: the requirement reads lean by the keyword classifier"
        );
        // …but the ROUTE is deliberate (Standard depth) → the full gate must run.
        let route = build_route();
        let qc = run_auto_qc(&mut sess, &o, &events, Some(&route), None, false).await;
        assert!(
            qc.blocking.iter().any(|b| b.contains("coverage gap")),
            "a deliberate route runs the full gate even when the requirement reads lean: {:?}",
            qc.blocking
        );
    }

    #[test]
    fn bugfix_without_a_reproduction_test_blocks_and_a_test_clears_it() {
        // A Bugfix with source but NO test → the reproduction-test floor blocks.
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("fix.ts"), "export const x = 1;").unwrap();
        let o = opts(tmp.path());
        let route = bugfix_route();
        let blocking = acceptance_floor_blocking(&o, Some(&route));
        assert!(
            blocking.iter().any(|b| b.contains("reproduction test")),
            "a bugfix with no test must demand a reproduction test: {blocking:?}"
        );

        // Add a real reproduction test → the floor clears (red→green is now possible).
        std::fs::write(
            tmp.path().join("fix.test.ts"),
            "test('reproduces the bug', () => { expect(fixed()).toBe(true); });",
        )
        .unwrap();
        let blocking2 = acceptance_floor_blocking(&o, Some(&route));
        assert!(
            !blocking2.iter().any(|b| b.contains("reproduction test")),
            "a reproduction test clears the bugfix floor: {blocking2:?}"
        );
    }

    #[test]
    fn acceptance_floor_is_fail_open_when_artifacts_are_missing() {
        // No PRD / no architecture / no source → every contributor reads empty →
        // the floor is clean (a neutral skip, never a fabricated failure).
        let tmp = tempfile::TempDir::new().unwrap();
        let o = opts(tmp.path());
        let route = build_route();
        assert!(
            acceptance_floor_blocking(&o, Some(&route)).is_empty(),
            "an empty project yields no fabricated acceptance failures"
        );
    }

    #[test]
    fn runtime_proof_blocking_distinguishes_failure_from_skip() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp
            .path()
            .join(crate::runtime_proof::runtime_proof_rel_path());
        std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
        // A SKIP (no dev server) → neutral, no block.
        std::fs::write(
            &dir,
            r#"{"timestamp":"t","status":{"kind":"not_verified","reason":"no dev server detected"},"dev_server":null,"command":null,"base_url":null,"ready_ms":null,"routes":[],"e2e":null}"#,
        )
        .unwrap();
        assert!(
            runtime_proof_blocking(tmp.path()).is_none(),
            "a runtime SKIP is neutral, not a block"
        );
        // A real boot FAILURE → blocking.
        std::fs::write(
            &dir,
            r#"{"timestamp":"t","status":{"kind":"not_verified","reason":"server did not become ready within 60s"},"dev_server":"vite","command":"npm run dev","base_url":"http://localhost:5173","ready_ms":null,"routes":[],"e2e":null}"#,
        )
        .unwrap();
        let line = runtime_proof_blocking(tmp.path()).expect("a real boot failure blocks");
        assert!(line.contains("runtime-proof"));
    }

    // ── Wave 1: routed entry — visible intent + owned plan, fully fail-open ──

    /// A deliberate Build route for the wiring tests.
    fn build_route() -> crate::router::RoutePlan {
        crate::router::RoutePlan {
            class: crate::router::RouteClass::Build,
            kind: crate::planner::TaskKind::Greenfield,
            depth: crate::router::Depth::Standard,
            team: vec![crate::critics::Seat::FrontendEngineer],
            scope: vec![],
            needs_clarify: None,
            est_budget: crate::router::Budget::for_route(
                crate::router::RouteClass::Build,
                crate::router::Depth::Standard,
            ),
            confidence: 0.7,
        }
    }

    #[test]
    fn run_budget_reads_env_and_falls_back_safely() {
        let _env = EnvRestore::set("UMADEV_RUN_BUDGET_SECS", "120");
        assert_eq!(run_budget(), Duration::from_secs(120));
        std::env::set_var("UMADEV_RUN_BUDGET_SECS", "0"); // non-positive → default
        assert_eq!(run_budget(), Duration::from_secs(DEFAULT_RUN_BUDGET_SECS));
        std::env::set_var("UMADEV_RUN_BUDGET_SECS", "nonsense");
        assert_eq!(run_budget(), Duration::from_secs(DEFAULT_RUN_BUDGET_SECS));
        std::env::remove_var("UMADEV_RUN_BUDGET_SECS");
        assert_eq!(run_budget(), Duration::from_secs(DEFAULT_RUN_BUDGET_SECS));
    }

    #[test]
    fn seat_driven_decision_is_router_driven_with_an_escape_hatch() {
        // Wave A: the build-path decision is AUTOMATIC from the route (no user flag,
        // no new classifier — it reuses the router's own `depth` signal). A DELIBERATE
        // full build (Greenfield → Standard) builds SEAT-BY-SEAT; a lean/Fast build
        // stays the single end-to-end turn so token cost stays proportional.
        let deliberate = build_route(); // Greenfield / Standard (deliberate)
        let lean = fast_build_route(); // Light / Fast (not deliberate)
        assert!(
            seat_driven_build_warranted(&deliberate, false),
            "a deliberate full build warrants seat-by-seat building"
        );
        assert!(
            !seat_driven_build_warranted(&lean, false),
            "a lean/Fast build stays single-turn (no per-step scheduling)"
        );
        // The escape hatch can only DISABLE seat-driving (force the cheaper single
        // turn); it can NEVER force seat-driving on, and it leaves the lean default
        // exactly where it was — the default remains router-driven.
        assert!(
            !seat_driven_build_warranted(&deliberate, true),
            "the escape hatch forces even a deliberate build back to a single turn"
        );
        assert!(
            !seat_driven_build_warranted(&lean, true),
            "the escape hatch never turns a lean build into a seat-driven one"
        );
    }

    #[tokio::test]
    async fn deliberate_build_winds_down_gracefully_at_the_time_budget() {
        // A deliberate build whose wall-clock budget is ALREADY spent drives its
        // first step, then stops scheduling new steps and settles via the final gate
        // (graceful — never a mid-write abort, never unbounded). The honest budget
        // note fires.
        use crate::plan_state::{AcceptanceSpec, Plan, PlanStep, StepKind, StepStatus};
        let tmp = tempfile::TempDir::new().unwrap();
        seed_source(tmp.path());
        let (events, rec) = sink();
        let mk = |id: &str| PlanStep {
            id: id.to_string(),
            title: format!("step {id}"),
            seat: crate::critics::Seat::FrontendEngineer,
            kind: StepKind::Build,
            depends_on: vec![],
            acceptance: AcceptanceSpec::SourcePresent,
            evidence: Vec::new(),
            status: StepStatus::Pending,
        };
        let plan = Plan {
            steps: vec![mk("a"), mk("b"), mk("c")],
            risks: vec![],
            open_questions: vec![],
        };
        let turns = vec![
            text_turn("step a done"),
            text_turn("step b done"),
            text_turn("step c done"),
            text_turn("final gate ok"),
        ];
        let mut sess = FakeSession::new(turns, true, "");
        let o = opts(tmp.path());
        let route = build_route(); // deliberate Standard
                                   // An already-spent budget (deadline in the past). `checked_sub` avoids the
                                   // unchecked-Instant-subtraction lint; fall back to "now" (still ≤ now).
        let already_past = std::time::Instant::now()
            .checked_sub(Duration::from_secs(1))
            .unwrap_or_else(std::time::Instant::now);
        let outcome = drive_director_loop_with_idle(
            &mut sess,
            &o,
            &events,
            "GO".into(),
            Some(plan),
            Some(&route),
            IdleBudget::new(Duration::from_millis(200), Duration::from_millis(200)),
            already_past,
        )
        .await;
        assert!(
            matches!(outcome, DirectorLoopOutcome::Done { .. }),
            "the build settles cleanly (never hangs) at the budget: {outcome:?}"
        );
        assert!(
            rec.events().iter().any(|e| matches!(
                e,
                EngineEvent::Note(n) if n.contains("time budget reached")
            )),
            "the graceful budget wind-down note fires: {:?}",
            rec.events()
        );
    }

    #[tokio::test]
    async fn routed_loop_in_plan_mode_stops_read_only_without_driving_writes() {
        // A director BUILD spawned in Plan (read-only) mode must STOP cleanly with the
        // read-only notice, NOT drive the plan and deny-storm every write (the reported
        // opencode "edit -> pyproject.toml 按拒绝处理 -> 0 source files" storm). The guard
        // returns BEFORE any turn is driven, so an empty FakeSession is never touched.
        let tmp = tempfile::TempDir::new().unwrap();
        seed_source(tmp.path());
        let (events, rec) = sink();
        let mut sess = FakeSession::new(vec![], false, "");
        let mut o = opts(tmp.path());
        o.mode = TrustMode::Plan;
        let route = build_route();
        let outcome =
            drive_director_loop_routed(&mut sess, &o, &events, "GO".into(), Some(&route)).await;
        assert!(
            matches!(outcome, DirectorLoopOutcome::Done { .. }),
            "plan mode settles cleanly (Done, not a deny-storm): {outcome:?}"
        );
        assert!(
            rec.events().iter().any(|e| matches!(
                e,
                EngineEvent::Note(n) if n.contains("计划模式") || n.contains("plan")
            )),
            "the read-only plan-mode notice fires: {:?}",
            rec.events()
        );
    }

    #[tokio::test]
    async fn routed_loop_emits_intent_decided() {
        // The routed entry surfaces the routing decision BEFORE any work, so the
        // user sees "I'll BUILD this …". A non-forking session means no plan, which
        // is fine — IntentDecided must still fire.
        let tmp = tempfile::TempDir::new().unwrap();
        seed_source(tmp.path());
        let (events, rec) = sink();
        let turns = vec![text_turn("Built it end to end. Done.")];
        let mut sess = FakeSession::new(turns, false, "");
        let o = opts(tmp.path());
        let route = build_route();

        let outcome =
            drive_director_loop_routed(&mut sess, &o, &events, "GO".into(), Some(&route)).await;
        assert!(matches!(outcome, DirectorLoopOutcome::Done { .. }));
        assert!(
            rec.count(
                |e| matches!(e, EngineEvent::IntentDecided { class, .. } if class == "build")
            ) == 1,
            "exactly one IntentDecided(build) is emitted"
        );
    }

    #[tokio::test]
    async fn routed_loop_synthesizes_and_posts_a_plan_when_the_brain_replies() {
        // The planning turn runs on the MAIN session (its first turn) and replies
        // with a valid plan JSON → the loop synthesises the plan, persists
        // `.umadev/plan.json`, posts it, and ticks a step active. Because the route
        // is DELIBERATE (Standard), Wave 2 then DRIVES the plan step-by-step via
        // `summon` (the second scripted turn is the first step's doer turn), so the
        // doer's reply text threads back through `SummonResult.text`.
        let tmp = tempfile::TempDir::new().unwrap();
        seed_source(tmp.path());
        let (events, rec) = sink();
        let plan_json = r#"{"steps":[
            {"id":"scaffold","title":"Scaffold","seat":"frontend-engineer","kind":"build","depends_on":[],"acceptance":"source-present"},
            {"id":"ui","title":"Build the UI","seat":"frontend-engineer","kind":"build","depends_on":["scaffold"],"acceptance":"source-present"}
        ],"risks":["state mgmt"],"open_questions":[]}"#;
        // Turn 1 = the JSON plan (main-session planning turn); turn 2 = the build.
        let turns = vec![
            text_turn(plan_json),
            text_turn("Built the whole app end to end. Done."),
        ];
        let mut sess = FakeSession::new(turns, true, plan_json);
        let mut o = opts(tmp.path());
        // A lean requirement would skip the heavy review but the plan path keys off
        // the ROUTE's deliberate depth, not the requirement — keep it a real build.
        o.requirement = "做一个完整的任务管理产品".to_string();
        let route = build_route();

        let outcome =
            drive_director_loop_routed(&mut sess, &o, &events, "GO".into(), Some(&route)).await;
        assert!(matches!(outcome, DirectorLoopOutcome::Done { .. }));

        // The plan was posted with both steps.
        assert!(
            rec.count(|e| matches!(e, EngineEvent::PlanPosted { total, .. } if *total == 2)) == 1,
            "a 2-step plan was posted: {:?}",
            rec.events()
        );
        // At least one step was surfaced as active (the ready scaffold step).
        assert!(
            rec.count(
                |e| matches!(e, EngineEvent::PlanStepStatus { status, .. } if status == "active")
            ) >= 1,
            "a ready step ticked active"
        );
        // It was persisted to disk and is loadable.
        let loaded = crate::plan_state::load(tmp.path()).expect("plan persisted");
        assert_eq!(loaded.steps.len(), 2);
        // The step-driven loop drove the doer turn and threaded its reply back.
        match outcome {
            DirectorLoopOutcome::Done { reply } => assert!(reply.contains("Built the whole app")),
            other @ DirectorLoopOutcome::Failed(_) => panic!("expected Done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn routed_loop_fails_open_to_single_turn_when_plan_unparseable() {
        // The fork replies with garbage (no JSON object) → synthesize_plan returns
        // None → the loop behaves EXACTLY like today's single-turn build. No
        // PlanPosted, but the build still completes.
        let tmp = tempfile::TempDir::new().unwrap();
        seed_source(tmp.path());
        let (events, rec) = sink();
        let turns = vec![text_turn("Built it. Done.")];
        let mut sess = FakeSession::new(turns, true, "not json at all, sorry");
        let mut o = opts(tmp.path());
        o.requirement = "做一个完整的产品".to_string();
        let route = build_route();

        let outcome =
            drive_director_loop_routed(&mut sess, &o, &events, "GO".into(), Some(&route)).await;
        assert!(matches!(outcome, DirectorLoopOutcome::Done { .. }));
        // No plan could be parsed → none posted (fail-open to single-turn behaviour).
        assert_eq!(
            rec.count(|e| matches!(e, EngineEvent::PlanPosted { .. })),
            0,
            "an unparseable plan posts nothing — single-turn fallback"
        );
        // IntentDecided still fired (it never depends on the plan).
        assert!(rec.count(|e| matches!(e, EngineEvent::IntentDecided { .. })) == 1);
    }

    #[tokio::test]
    async fn non_routed_entry_is_unchanged_no_intent_or_plan() {
        // The legacy entry (drive_director_loop) passes route = None → no
        // IntentDecided, no plan, exactly today's behaviour. This guards the
        // backward-compatible contract the TUI/CLI callers rely on.
        let tmp = tempfile::TempDir::new().unwrap();
        seed_source(tmp.path());
        let (events, rec) = sink();
        let turns = vec![text_turn("Built it. Done.")];
        let mut sess = FakeSession::new(turns, true, r#"{"steps":[]}"#);
        let o = opts(tmp.path());

        let outcome = drive_director_loop(&mut sess, &o, &events, "GO".into()).await;
        assert!(matches!(outcome, DirectorLoopOutcome::Done { .. }));
        assert_eq!(
            rec.count(|e| matches!(e, EngineEvent::IntentDecided { .. })),
            0
        );
        assert_eq!(
            rec.count(|e| matches!(e, EngineEvent::PlanPosted { .. })),
            0
        );
    }

    // ── Wave 2: drive the plan step-by-step (deliberate) vs single-turn (lean) ──

    /// A FAST (lean) Build route — proportional, convenes no team, NOT deliberate.
    fn fast_build_route() -> crate::router::RoutePlan {
        crate::router::RoutePlan {
            class: crate::router::RouteClass::Build,
            kind: crate::planner::TaskKind::Light,
            depth: crate::router::Depth::Fast,
            team: vec![],
            scope: vec![],
            needs_clarify: None,
            est_budget: crate::router::Budget::for_route(
                crate::router::RouteClass::Build,
                crate::router::Depth::Fast,
            ),
            confidence: 0.6,
        }
    }

    #[tokio::test]
    async fn deliberate_build_drives_each_step_via_summon_and_ticks_done() {
        // The headline Wave 2 behaviour: a DELIBERATE build with a 2-step plan drives
        // EACH step on its own summon turn (so the main session receives the plan
        // turn + one doer directive PER step), verifies each against source-present,
        // and ticks each step Done on the checklist.
        let tmp = tempfile::TempDir::new().unwrap();
        seed_source(tmp.path()); // source present → each step's acceptance passes
        let (events, rec) = sink();
        let plan_json = r#"{"steps":[
            {"id":"scaffold","title":"Scaffold","seat":"frontend-engineer","kind":"build","depends_on":[],"acceptance":"source-present"},
            {"id":"ui","title":"Build the UI","seat":"frontend-engineer","kind":"build","depends_on":["scaffold"],"acceptance":"source-present"}
        ],"risks":[],"open_questions":[]}"#;
        // Turn 1 = plan JSON; turn 2 = scaffold doer; turn 3 = ui doer. The
        // FakeSession default-completes any further turns (the final QC gate).
        let turns = vec![
            text_turn(plan_json),
            text_turn("Scaffolded the app skeleton. Done."),
            text_turn("Built the UI. Done."),
        ];
        let mut sess = FakeSession::new(turns, false, "");
        let sent = sess.sent_handle();
        let mut o = opts(tmp.path());
        o.requirement = "做一个完整的任务管理产品".to_string();
        let route = build_route();

        let outcome =
            drive_director_loop_routed(&mut sess, &o, &events, "GO".into(), Some(&route)).await;
        assert!(matches!(outcome, DirectorLoopOutcome::Done { .. }));

        // BOTH steps ticked Done (the real "checklist ticks off" outcome).
        let done = rec
            .count(|e| matches!(e, EngineEvent::PlanStepStatus { status, .. } if status == "done"));
        assert!(done >= 2, "both build steps ticked done: {done}");

        // The main session received the plan turn AND a separate focused directive
        // per step — proof the plan was DRIVEN step-by-step, not in one mega-turn.
        let sent = sent.lock().unwrap();
        assert!(
            sent.iter().any(|d| d.contains("Scaffold")),
            "the scaffold step got its own focused directive: {sent:?}"
        );
        assert!(
            sent.iter().any(|d| d.contains("Build the UI")),
            "the ui step got its own focused directive: {sent:?}"
        );
        // FIX #6: each per-step directive HARD-scopes the base to ONE step (the root
        // fix for "the base builds the whole project in step 1's turn"). The focused
        // directive must carry the single-step constraint phrasing.
        assert!(
            sent.iter().any(|d| d.contains("ONE step of a larger build")
                && d.contains("Do NOT implement any other part of the project")),
            "the per-step directive hard-scopes the base to ONE step: {sent:?}"
        );
        // Persisted terminal plan is all-Done.
        let loaded = crate::plan_state::load(tmp.path()).expect("plan persisted");
        assert!(loaded
            .steps
            .iter()
            .all(|s| s.status == crate::plan_state::StepStatus::Done));
    }

    // ── workflow-state.json phase sync — the state-sync bug fix. The director-loop
    //    path must keep `.umadev/workflow-state.json` (the 9-phase machine `/status`
    //    reads) in step with REAL progress; before the fix it stayed frozen at
    //    `research` / all-pending while the build moved on. ──

    /// Read the persisted workflow phase id from `.umadev/workflow-state.json`, or
    /// `None` when no state was written.
    fn persisted_phase_id(root: &std::path::Path) -> Option<String> {
        crate::state::read_workflow_state(root).map(|s| s.phase)
    }

    #[tokio::test]
    async fn director_loop_advances_workflow_state_off_research() {
        // THE BUG: a `/run` over the director-loop / plan path never wrote
        // workflow-state.json, so `/status` showed `phase=research` / all-pending even
        // after real code landed. Now a deliberate step-driven build (a frontend +
        // backend plan) must leave a workflow-state.json whose phase is PAST research
        // and reflects the completed steps (backend completed → `backend`/`delivery`).
        let tmp = tempfile::TempDir::new().unwrap();
        seed_source(tmp.path()); // source present → each step's acceptance passes
        let (events, _rec) = sink();
        let plan_json = r#"{"steps":[
            {"id":"fe","title":"Build the frontend","seat":"frontend-engineer","kind":"build","depends_on":[],"acceptance":"source-present"},
            {"id":"be","title":"Build the backend","seat":"backend-engineer","kind":"build","depends_on":["fe"],"acceptance":"source-present"}
        ],"risks":[],"open_questions":[]}"#;
        let turns = vec![
            text_turn(plan_json),
            text_turn("Built the frontend. Done."),
            text_turn("Built the backend. Done."),
        ];
        let mut sess = FakeSession::new(turns, false, "");
        let mut o = opts(tmp.path());
        o.requirement = "做一个完整的任务管理产品".to_string();
        let route = build_route();

        // Before the run there is NO state file (this is the frozen-at-research case).
        assert!(persisted_phase_id(tmp.path()).is_none());

        let outcome =
            drive_director_loop_routed(&mut sess, &o, &events, "GO".into(), Some(&route)).await;
        assert!(matches!(outcome, DirectorLoopOutcome::Done { .. }));

        // A state file now exists and its phase is NOT the initial `research`.
        let phase = persisted_phase_id(tmp.path()).expect("workflow-state.json was written");
        assert_ne!(
            phase, "research",
            "the director loop advanced the phase off the initial research value"
        );
        // Both steps reached Done (a clean finish over a backend seat) → the terminal
        // phase is the deepest the build honestly reached. A clean finalize claims
        // `delivery`; it must at MINIMUM be past the frontend phase the backend follows.
        let rank = |id: &str| {
            umadev_spec::PHASE_CHAIN
                .iter()
                .position(|p| p.id() == id)
                .unwrap_or(0)
        };
        assert!(
            rank(&phase) >= rank("backend"),
            "a build whose backend step completed reaches at least `backend` (got {phase})"
        );
    }

    #[test]
    fn phase_for_seat_maps_each_seat_honestly() {
        use crate::critics::Seat;
        assert_eq!(phase_for_seat(Seat::ProductManager), Phase::Docs);
        assert_eq!(phase_for_seat(Seat::Architect), Phase::Spec);
        assert_eq!(phase_for_seat(Seat::UiuxDesigner), Phase::Frontend);
        assert_eq!(phase_for_seat(Seat::FrontendEngineer), Phase::Frontend);
        assert_eq!(phase_for_seat(Seat::BackendEngineer), Phase::Backend);
        assert_eq!(phase_for_seat(Seat::QaEngineer), Phase::Quality);
        assert_eq!(phase_for_seat(Seat::SecurityEngineer), Phase::Quality);
        assert_eq!(phase_for_seat(Seat::DevopsEngineer), Phase::Delivery);
        // The gate phases are never the anchor for a step (they are human pauses).
        for seat in [
            Seat::ProductManager,
            Seat::Architect,
            Seat::UiuxDesigner,
            Seat::FrontendEngineer,
            Seat::BackendEngineer,
            Seat::QaEngineer,
            Seat::SecurityEngineer,
            Seat::DevopsEngineer,
        ] {
            assert!(
                !phase_for_seat(seat).is_gate(),
                "a step never anchors to a gate phase"
            );
        }
    }

    #[test]
    fn persisted_phase_never_regresses_across_writes() {
        // The monotonic clamp: once the state reached a deeper phase, a later write of
        // an EARLIER phase is ignored (a backend step finishing after a frontend step
        // must not pull the phase back to `frontend`). This is the "never move
        // BACKWARD" invariant the fix promises.
        let tmp = tempfile::TempDir::new().unwrap();
        let o = opts(tmp.path());
        // Advance frontend → backend → (try to regress) frontend.
        persist_phase(&o, Phase::Frontend);
        assert_eq!(persisted_phase_id(tmp.path()).as_deref(), Some("frontend"));
        persist_phase(&o, Phase::Backend);
        assert_eq!(persisted_phase_id(tmp.path()).as_deref(), Some("backend"));
        // A regressing write is clamped — the phase stays at the deeper `backend`.
        persist_phase(&o, Phase::Frontend);
        assert_eq!(
            persisted_phase_id(tmp.path()).as_deref(),
            Some("backend"),
            "a write of an earlier phase is clamped to the deepest reached (no regress)"
        );
    }

    #[tokio::test]
    async fn step_completions_advance_phase_monotonically_never_backward() {
        // End-to-end monotonicity across the step driver: a plan whose steps complete
        // in seat order frontend → backend ticks the phase forward and NEVER backward,
        // even though the backend step's seat maps to a LATER phase than the frontend's.
        // (A regression would surface if a later-finishing earlier-phase step pulled it
        // back; here the clamp guarantees a non-decreasing phase rank at every Done.)
        let tmp = tempfile::TempDir::new().unwrap();
        seed_source(tmp.path());
        let (events, _rec) = sink();
        // backend (later phase) is the FIRST step; frontend (earlier phase) depends on
        // it — so the EARLIER-phase step finishes LAST. The clamp must keep the phase
        // at `backend` after the trailing frontend step, never regress to `frontend`.
        let plan_json = r#"{"steps":[
            {"id":"be","title":"Build the backend","seat":"backend-engineer","kind":"build","depends_on":[],"acceptance":"source-present"},
            {"id":"fe","title":"Polish the frontend","seat":"frontend-engineer","kind":"build","depends_on":["be"],"acceptance":"source-present"}
        ],"risks":[],"open_questions":[]}"#;
        let turns = vec![
            text_turn(plan_json),
            text_turn("Built the backend. Done."),
            text_turn("Polished the frontend. Done."),
        ];
        let mut sess = FakeSession::new(turns, false, "");
        let mut o = opts(tmp.path());
        o.requirement = "做一个完整的产品".to_string();
        let route = build_route();

        let outcome =
            drive_director_loop_routed(&mut sess, &o, &events, "GO".into(), Some(&route)).await;
        assert!(matches!(outcome, DirectorLoopOutcome::Done { .. }));

        // After the EARLIER-phase frontend step finished LAST, the phase must still be
        // at least `backend` — it never regressed to `frontend`.
        let phase = persisted_phase_id(tmp.path()).expect("state written");
        let rank = |id: &str| {
            umadev_spec::PHASE_CHAIN
                .iter()
                .position(|p| p.id() == id)
                .unwrap_or(0)
        };
        assert!(
            rank(&phase) >= rank("backend"),
            "the phase never regressed below the deepest step reached (got {phase})"
        );
    }

    #[test]
    fn finalize_phase_is_honest_clean_vs_unclean() {
        use crate::plan_state::{AcceptanceSpec, Plan, PlanStep, StepKind, StepStatus};
        let mk = |id: &str, seat: crate::critics::Seat, status: StepStatus| PlanStep {
            id: id.into(),
            title: format!("step {id}"),
            seat,
            kind: StepKind::Build,
            depends_on: vec![],
            acceptance: AcceptanceSpec::SourcePresent,
            evidence: Vec::new(),
            status,
        };

        // CLEAN finish (every step Done) over a QA-deepest plan → the build claims the
        // terminal `delivery` phase.
        let tmp = tempfile::TempDir::new().unwrap();
        let o = opts(tmp.path());
        let clean_plan = Plan {
            steps: vec![
                mk(
                    "fe",
                    crate::critics::Seat::FrontendEngineer,
                    StepStatus::Done,
                ),
                mk("qa", crate::critics::Seat::QaEngineer, StepStatus::Done),
            ],
            risks: vec![],
            open_questions: vec![],
        };
        finalize_phase_from_plan(&clean_plan, &o, true);
        assert_eq!(
            persisted_phase_id(tmp.path()).as_deref(),
            Some("delivery"),
            "a genuinely clean finish reaches delivery"
        );

        // NON-clean finish (backend step Blocked, frontend Done) → the state must NOT
        // claim delivery; it reflects only the furthest phase that actually completed
        // (frontend), so `/status` stays honest about where the build stopped.
        let tmp2 = tempfile::TempDir::new().unwrap();
        let o2 = opts(tmp2.path());
        let unclean_plan = Plan {
            steps: vec![
                mk(
                    "fe",
                    crate::critics::Seat::FrontendEngineer,
                    StepStatus::Done,
                ),
                mk(
                    "be",
                    crate::critics::Seat::BackendEngineer,
                    StepStatus::Blocked,
                ),
            ],
            risks: vec![],
            open_questions: vec![],
        };
        finalize_phase_from_plan(&unclean_plan, &o2, false);
        assert_eq!(
            persisted_phase_id(tmp2.path()).as_deref(),
            Some("frontend"),
            "a non-clean finish never optimistically claims delivery"
        );
    }

    #[tokio::test]
    async fn lean_fast_build_stays_single_turn_no_step_scheduling() {
        // A LEAN/Fast Build route must NOT take the step-driven path — it stays ONE
        // end-to-end build turn (the Wave 1 speed invariant). A Fast Build still gets
        // a short VISIBLE plan (the planning turn), but the step-driver only fires on
        // DELIBERATE depth, so the build itself is a single fast turn: the planning
        // turn + exactly ONE build directive, never decomposed into per-step summons.
        let tmp = tempfile::TempDir::new().unwrap();
        seed_source(tmp.path());
        let (events, _rec) = sink();
        let plan_json = r#"{"steps":[
            {"id":"a","title":"Page","seat":"frontend-engineer","kind":"build","depends_on":[],"acceptance":"source-present"}
        ],"risks":[],"open_questions":[]}"#;
        // Turn 1 = the (short) plan; turn 2 = the single end-to-end build.
        let turns = vec![
            text_turn(plan_json),
            text_turn("Built the single page end to end. Done."),
        ];
        let mut sess = FakeSession::new(turns, true, plan_json);
        let sent = sess.sent_handle();
        let mut o = opts(tmp.path());
        o.requirement = "做一个简单的待办清单单页应用,纯前端".to_string();
        let route = fast_build_route();

        let outcome =
            drive_director_loop_routed(&mut sess, &o, &events, "GO".into(), Some(&route)).await;
        assert!(matches!(outcome, DirectorLoopOutcome::Done { .. }));
        // The planning turn + EXACTLY ONE build directive — the lean build is a single
        // fast turn, never decomposed into per-step summon turns (the speed invariant).
        let sent = sent.lock().unwrap();
        assert_eq!(
            sent.len(),
            2,
            "a lean/Fast build is the plan turn + ONE build turn (no step scheduling): {sent:?}"
        );
        // The single build directive is the caller's "GO" framing, NOT a per-step
        // focused directive (which would carry the HARD-scoped "ONE step of a larger
        // build" phrasing from `route_focus_line`).
        assert!(
            sent.iter().any(|d| d.contains("GO")),
            "the build ran the caller's single directive: {sent:?}"
        );
        assert!(
            !sent
                .iter()
                .any(|d| d.contains("ONE step of a larger build")),
            "no per-step summon directive on a lean/Fast build: {sent:?}"
        );
    }

    #[tokio::test]
    async fn step_scheduling_fails_open_to_single_turn_when_first_step_cannot_drive() {
        // Fail-open: if the FIRST step can't drive at all (a dead session on the very
        // first doer turn), the step path returns None and the loop falls back to the
        // single end-to-end turn — the build is never lost to a scheduling failure.
        let tmp = tempfile::TempDir::new().unwrap();
        seed_source(tmp.path());
        let (events, rec) = sink();
        let plan_json = r#"{"steps":[
            {"id":"a","title":"Step A","seat":"frontend-engineer","kind":"build","depends_on":[],"acceptance":"source-present"}
        ],"risks":[],"open_questions":[]}"#;
        // Turn 1 = plan JSON. Turn 2 (the first step's doer) has NO TurnDone → the
        // session drains to None mid-turn → summon's pump returns done=false with no
        // text, so the first step "didn't drive" → fall back to the single turn.
        let turns = vec![
            text_turn(plan_json),
            vec![SessionEvent::TextDelta("partial, no TurnDone".into())],
            text_turn("Fallback single-turn build. Done."),
        ];
        let mut sess = FakeSession::new(turns, false, "");
        let mut o = opts(tmp.path());
        o.requirement = "做一个完整的产品".to_string();
        let route = build_route();

        let outcome =
            drive_director_loop_routed(&mut sess, &o, &events, "GO".into(), Some(&route)).await;
        // The build still completes (via the single-turn fallback), never a panic.
        assert!(matches!(outcome, DirectorLoopOutcome::Done { .. }));
        // The fallback note was emitted.
        assert!(
            rec.events().iter().any(|e| matches!(
                e,
                EngineEvent::Note(n) if n.contains("step scheduling unavailable")
            )),
            "a first-step drive failure falls back to the single turn"
        );
    }

    #[tokio::test]
    async fn a_failing_step_acceptance_is_bounded_and_marks_blocked() {
        // A step whose acceptance NEVER passes (claims a build but the tree stays
        // empty so source-present fails every round) must be BOUNDED by the per-step
        // fix budget, then marked Blocked (honest) — never an infinite re-drive.
        let tmp = tempfile::TempDir::new().unwrap();
        // NO source seeded → the source-present acceptance fails every round.
        let (events, rec) = sink();
        let plan_json = r#"{"steps":[
            {"id":"a","title":"Step A","seat":"frontend-engineer","kind":"build","depends_on":[],"acceptance":"source-present"}
        ],"risks":[],"open_questions":[]}"#;
        // Every doer turn claims done but writes nothing → acceptance fails; the
        // FakeSession default-completes once the scripted turns run out.
        let turns = vec![
            text_turn(plan_json),
            text_turn("Worked on it. Done."),
            text_turn("Tried again. Done."),
            text_turn("Once more. Done."),
        ];
        let mut sess = FakeSession::new(turns, false, "");
        let sent = sess.sent_handle();
        let mut o = opts(tmp.path());
        o.requirement = "做一个完整的产品".to_string();
        let route = build_route();

        let outcome =
            drive_director_loop_routed(&mut sess, &o, &events, "GO".into(), Some(&route)).await;
        assert!(matches!(outcome, DirectorLoopOutcome::Done { .. }));
        // The step was driven a BOUNDED number of times (1 plan turn + at most
        // MAX_STEP_FIX_ROUNDS+1 doer turns + the final-gate fix turns) — never a spin.
        let n = sent.lock().unwrap().len();
        assert!(
            n <= 1 + (MAX_STEP_FIX_ROUNDS + 1) + MAX_QC_ROUNDS,
            "the failing step is bounded, not an infinite re-drive: {n} turns"
        );
        // The step ended Blocked (its acceptance never passed) — honest, not Done.
        assert!(
            rec.count(
                |e| matches!(e, EngineEvent::PlanStepStatus { status, .. } if status == "blocked")
            ) >= 1,
            "an unacceptable step is marked Blocked"
        );
    }

    // ── Blast-radius-weighted verification ordering: among ready peers the highest-
    //    blast-radius (most-depended-on, expensive-to-unwind) step is scheduled +
    //    reworked FIRST; a dependency still never runs before its prerequisite; a
    //    high-blast-radius step earns one extra rigor fix round. ──

    /// The ordered ids of the `active` PlanStepStatus events the run emitted — the
    /// drive order the scheduler actually chose.
    fn active_order(rec: &RecordingSink) -> Vec<String> {
        rec.events()
            .iter()
            .filter_map(|e| match e {
                EngineEvent::PlanStepStatus { id, status, .. } if status == "active" => {
                    Some(id.clone())
                }
                _ => None,
            })
            .collect()
    }

    /// A 4-step plan: an independent low-impact peer (`config`, blast radius 0) listed
    /// FIRST in plan order, an upstream `schema` (blast radius 2: `api` + `ui` depend on
    /// it), and its two dependents. `config` and `schema` are both ready initially; the
    /// blast-radius scheduler must drive `schema` first despite `config`'s earlier plan
    /// position. `api`/`ui` can only run AFTER `schema` is Done (DAG order).
    fn upstream_peer_plan() -> crate::plan_state::Plan {
        use crate::plan_state::{AcceptanceSpec, Plan, PlanStep, StepKind, StepStatus};
        let mk = |id: &str, deps: &[&str]| PlanStep {
            id: id.into(),
            title: format!("Build the {id}"),
            seat: crate::critics::Seat::FrontendEngineer,
            kind: StepKind::Build,
            depends_on: deps.iter().map(|d| (*d).to_string()).collect(),
            acceptance: AcceptanceSpec::SourcePresent,
            evidence: Vec::new(),
            status: StepStatus::Pending,
        };
        Plan {
            steps: vec![
                mk("config", &[]),      // radius 0, first in plan order
                mk("schema", &[]),      // radius 2 (api + ui)
                mk("api", &["schema"]), // gated by schema
                mk("ui", &["schema"]),  // gated by schema
            ],
            risks: vec![],
            open_questions: vec![],
        }
    }

    #[tokio::test]
    async fn scheduler_drives_high_blast_radius_ready_peer_first_keeping_dag_order() {
        // Source seeded → every source-present step PASSES in one turn, so the schedule
        // walks cleanly and we can read the pure DRIVE order. `schema` (radius 2) must be
        // driven BEFORE `config` (radius 0) even though `config` is earlier in plan order
        // — the expensive-to-unwind upstream work goes first. And `api`/`ui` (which
        // depend on `schema`) must run AFTER `schema`, never before (DAG order intact).
        let tmp = tempfile::TempDir::new().unwrap();
        seed_source(tmp.path());
        let (events, rec) = sink();
        let mut sess = FakeSession::new(vec![], false, "");
        let mut o = opts(tmp.path());
        o.requirement = "做一个完整的产品".to_string();
        let route = build_route();
        let mut plan = upstream_peer_plan();

        let outcome = drive_plan_steps(
            &mut sess,
            &o,
            &events,
            &route,
            &mut plan,
            IdleBudget::new(Duration::from_millis(200), Duration::from_millis(200)),
            std::time::Instant::now() + Duration::from_secs(3_600),
        )
        .await;
        assert!(matches!(outcome, Some(DirectorLoopOutcome::Done { .. })));

        let order = active_order(&rec);
        let pos = |id: &str| order.iter().position(|x| x == id).expect("step ran");
        // Priority among the initial ready PEERS: schema (radius 2) before config (0).
        assert!(
            pos("schema") < pos("config"),
            "the higher-blast-radius peer is scheduled first: {order:?}"
        );
        // DAG order preserved: a dependent never runs before its prerequisite.
        assert!(
            pos("schema") < pos("api") && pos("schema") < pos("ui"),
            "a dependency (schema) runs before its dependents (api, ui): {order:?}"
        );
        // Every step completed cleanly (source present → all accepted).
        assert!(
            plan.steps
                .iter()
                .all(|s| s.status == crate::plan_state::StepStatus::Done),
            "the whole DAG drained Done: {:?}",
            plan.steps
                .iter()
                .map(|s| (s.id.clone(), s.status))
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn rework_prioritizes_the_higher_blast_radius_blocking_peer() {
        // NO source → both ready peers (schema, config) FAIL their source-present
        // acceptance and are reworked, then marked Blocked. The blast-radius scheduler
        // must rework the higher-impact `schema` (radius 2) FIRST — all of schema's
        // directives land before any of config's. (schema's block then strands api/ui,
        // which are pruned — its handling obviates their rework.)
        let tmp = tempfile::TempDir::new().unwrap();
        // NO source seeded.
        let (events, rec) = sink();
        // Plenty of default-completing turns; a FUTURE deadline so the full per-step fix
        // budget runs (isolates the rework ORDER from the wall-clock ceiling).
        let mut sess = FakeSession::new(vec![], false, "");
        let sent = sess.sent_handle();
        let mut o = opts(tmp.path());
        o.requirement = "做一个完整的产品".to_string();
        let route = build_route();
        let mut plan = upstream_peer_plan();

        let outcome = drive_plan_steps(
            &mut sess,
            &o,
            &events,
            &route,
            &mut plan,
            IdleBudget::new(Duration::from_millis(200), Duration::from_millis(200)),
            std::time::Instant::now() + Duration::from_secs(3_600),
        )
        .await;
        assert!(matches!(outcome, Some(DirectorLoopOutcome::Done { .. })));

        // schema is reworked before config: schema becomes Active first.
        let order = active_order(&rec);
        let pos = |id: &str| order.iter().position(|x| x == id);
        assert!(
            pos("schema").is_some() && pos("config").is_some(),
            "both failing peers were driven: {order:?}"
        );
        assert!(
            pos("schema") < pos("config"),
            "the higher-blast-radius blocking peer is reworked first: {order:?}"
        );
        // Directive order confirms it at the turn level: every 'schema' directive lands
        // before the first 'config' directive (schema's whole rework finishes first).
        let sent = sent.lock().unwrap();
        let last_schema = sent
            .iter()
            .rposition(|d| d.contains("Build the schema"))
            .expect("schema was driven");
        let first_config = sent
            .iter()
            .position(|d| d.contains("Build the config"))
            .expect("config was driven");
        assert!(
            last_schema < first_config,
            "schema's rework completes before config's begins: {sent:?}"
        );
        // schema ended Blocked; its dependents api/ui were stranded (pruned), not
        // reworked — the upstream block obviated the downstream rework.
        use crate::plan_state::StepStatus;
        let by = |id: &str| plan.steps.iter().find(|s| s.id == id).unwrap().status;
        assert_eq!(by("schema"), StepStatus::Blocked);
        assert_eq!(by("config"), StepStatus::Blocked);
        assert_eq!(by("api"), StepStatus::Blocked, "stranded behind schema");
        assert_eq!(by("ui"), StepStatus::Blocked, "stranded behind schema");
        assert!(
            !order.contains(&"api".to_string()) && !order.contains(&"ui".to_string()),
            "stranded dependents were never driven (rework obviated): {order:?}"
        );
    }

    // ── First-pass acceptance signal: the measured engineering-doctrine telemetry
    //    (advisory, fail-open). A step that PASSES on the first acceptance check
    //    (no rework) is recorded first_pass+attempts; a step that needed rework /
    //    never passed is recorded attempts-only — keyed by BOTH the doer-seat kind
    //    and the route-class kind. It never changes a step's outcome. ──

    #[tokio::test]
    async fn first_pass_signal_records_clean_steps_as_first_pass() {
        // Source seeded → every source-present step PASSES on round 0 (zero rework).
        // Each of the 4 FrontendEngineer Build steps on a Build route is therefore a
        // FIRST-PASS, recorded under both the seat kind and the class kind. The run
        // still completes Done — the signal is pure telemetry.
        let tmp = tempfile::TempDir::new().unwrap();
        seed_source(tmp.path());
        let (events, _rec) = sink();
        let mut sess = FakeSession::new(vec![], false, "");
        let mut o = opts(tmp.path());
        o.requirement = "做一个完整的产品".to_string();
        let route = build_route(); // class = Build
        let mut plan = upstream_peer_plan(); // 4 Build steps, all FrontendEngineer

        let outcome = drive_plan_steps(
            &mut sess,
            &o,
            &events,
            &route,
            &mut plan,
            IdleBudget::new(Duration::from_millis(200), Duration::from_millis(200)),
            std::time::Instant::now() + Duration::from_secs(3_600),
        )
        .await;
        assert!(matches!(outcome, Some(DirectorLoopOutcome::Done { .. })));
        // Advisory invariant: the signal did NOT change the build outcome.
        assert!(
            plan.steps
                .iter()
                .all(|s| s.status == crate::plan_state::StepStatus::Done),
            "all steps still drained Done (advisory only): {:?}",
            plan.steps
                .iter()
                .map(|s| (s.id.clone(), s.status))
                .collect::<Vec<_>>()
        );
        // The recorded aggregate: 4 first-pass attempts under each dimension.
        let stats = crate::first_pass::load(tmp.path());
        let class = crate::first_pass::class_kind("build");
        let seat = crate::first_pass::seat_kind("frontend-engineer");
        let cs = stats.kinds.get(&class).copied().expect("class recorded");
        let ss = stats.kinds.get(&seat).copied().expect("seat recorded");
        assert_eq!(
            (cs.attempts, cs.first_pass),
            (4, 4),
            "class:build all first-pass"
        );
        assert_eq!((ss.attempts, ss.first_pass), (4, 4), "seat all first-pass");
    }

    #[tokio::test]
    async fn first_pass_signal_records_reworked_steps_as_attempts_only() {
        // NO source → the two ready peers (schema, config) FAIL their source-present
        // acceptance through every fix round and are marked Blocked (api/ui are
        // stranded, never driven). Each driven step is recorded attempts+1 /
        // first_pass+0. The Blocked outcome is unchanged — the signal is advisory.
        let tmp = tempfile::TempDir::new().unwrap();
        // NO source seeded.
        let (events, _rec) = sink();
        let mut sess = FakeSession::new(vec![], false, "");
        let mut o = opts(tmp.path());
        o.requirement = "做一个完整的产品".to_string();
        let route = build_route();
        let mut plan = upstream_peer_plan();

        let outcome = drive_plan_steps(
            &mut sess,
            &o,
            &events,
            &route,
            &mut plan,
            IdleBudget::new(Duration::from_millis(200), Duration::from_millis(200)),
            std::time::Instant::now() + Duration::from_secs(3_600),
        )
        .await;
        assert!(matches!(outcome, Some(DirectorLoopOutcome::Done { .. })));
        // Advisory invariant: schema + config still ended Blocked (signal changed
        // nothing about loop termination / the deterministic floor).
        use crate::plan_state::StepStatus;
        let by = |id: &str| plan.steps.iter().find(|s| s.id == id).unwrap().status;
        assert_eq!(by("schema"), StepStatus::Blocked);
        assert_eq!(by("config"), StepStatus::Blocked);
        // Only schema + config were driven (api/ui stranded) → 2 attempts, 0 first-pass.
        let stats = crate::first_pass::load(tmp.path());
        let class = crate::first_pass::class_kind("build");
        let cs = stats.kinds.get(&class).copied().expect("class recorded");
        assert_eq!(
            (cs.attempts, cs.first_pass),
            (2, 0),
            "reworked/failed steps bump attempts only"
        );
        // The signal is correctly NOT first-pass; the rate is 0% but below the min
        // sample so it stays untrusted (None) — no false confidence on 2 samples.
        assert_eq!(crate::first_pass::first_pass_rate(tmp.path(), &class), None);
    }

    #[tokio::test]
    async fn routed_loop_surfaces_a_low_confidence_nudge_advisory() {
        // Pre-seed a trustworthy-LOW first-pass history for the build class, then run
        // the routed entry: it surfaces the IntentDecided card AND an advisory nudge
        // toward more consult / lower autonomy — without changing the build outcome.
        let tmp = tempfile::TempDir::new().unwrap();
        seed_source(tmp.path());
        let class = crate::first_pass::class_kind("build");
        for _ in 0..6 {
            crate::first_pass::record(tmp.path(), &class, false); // 0/6 → low
        }
        let (events, rec) = sink();
        let turns = vec![text_turn("Built it end to end. Done.")];
        let mut sess = FakeSession::new(turns, false, "");
        let o = opts(tmp.path());
        let route = build_route();

        let outcome =
            drive_director_loop_routed(&mut sess, &o, &events, "GO".into(), Some(&route)).await;
        assert!(matches!(outcome, DirectorLoopOutcome::Done { .. }));
        // The advisory nudge fired (it never blocks the run).
        assert!(
            rec.events().iter().any(|e| matches!(
                e,
                EngineEvent::Note(n) if n.contains("一次过验收率偏低")
            )),
            "a low-confidence advisory nudge is surfaced: {:?}",
            rec.events()
        );
        // IntentDecided still fired exactly once (the nudge is additive, not a swap).
        assert_eq!(
            rec.count(|e| matches!(e, EngineEvent::IntentDecided { .. })),
            1
        );
    }

    #[tokio::test]
    async fn routed_loop_emits_no_nudge_without_a_signal() {
        // A FRESH project (no stats file) → the consult finds no signal → NO nudge is
        // emitted and behaviour is byte-for-byte the pre-signal path. Guards fail-open.
        let tmp = tempfile::TempDir::new().unwrap();
        seed_source(tmp.path());
        let (events, rec) = sink();
        let turns = vec![text_turn("Built it. Done.")];
        let mut sess = FakeSession::new(turns, false, "");
        let o = opts(tmp.path());
        let route = build_route();

        let outcome =
            drive_director_loop_routed(&mut sess, &o, &events, "GO".into(), Some(&route)).await;
        assert!(matches!(outcome, DirectorLoopOutcome::Done { .. }));
        assert!(
            !rec.events().iter().any(|e| matches!(
                e,
                EngineEvent::Note(n) if n.contains("一次过验收率偏低")
            )),
            "no signal → no nudge (fail-open, unchanged behaviour)"
        );
    }

    #[tokio::test]
    async fn high_blast_radius_step_earns_an_extra_fix_round() {
        // Rigor weighted by blast radius: a HIGH-blast-radius failing step (schema,
        // radius 2 ≥ HIGH_BLAST_RADIUS) is re-driven one MORE bounded round than a
        // radius-0 leaf. With no source, schema fails every round; count the directives
        // that carry ITS title (the final-gate fix turns don't) → MAX_STEP_FIX_ROUNDS+1
        // base rounds + 1 rigor bonus.
        use crate::plan_state::{AcceptanceSpec, Plan, PlanStep, StepKind, StepStatus};
        let tmp = tempfile::TempDir::new().unwrap();
        let (events, _rec) = sink();
        let mut sess = FakeSession::new(vec![], false, "");
        let sent = sess.sent_handle();
        let mut o = opts(tmp.path());
        o.requirement = "做一个完整的产品".to_string();
        let route = build_route();
        // schema (radius 2: api + ui depend on it) is the only initially-ready step.
        let mk = |id: &str, deps: &[&str]| PlanStep {
            id: id.into(),
            title: format!("Build the {id}"),
            seat: crate::critics::Seat::FrontendEngineer,
            kind: StepKind::Build,
            depends_on: deps.iter().map(|d| (*d).to_string()).collect(),
            acceptance: AcceptanceSpec::SourcePresent,
            evidence: Vec::new(),
            status: StepStatus::Pending,
        };
        let mut plan = Plan {
            steps: vec![
                mk("schema", &[]),
                mk("api", &["schema"]),
                mk("ui", &["schema"]),
            ],
            risks: vec![],
            open_questions: vec![],
        };
        assert_eq!(
            plan.blast_radius("schema"),
            2,
            "schema is high-blast-radius"
        );

        let _ = drive_plan_steps(
            &mut sess,
            &o,
            &events,
            &route,
            &mut plan,
            IdleBudget::new(Duration::from_millis(200), Duration::from_millis(200)),
            std::time::Instant::now() + Duration::from_secs(3_600),
        )
        .await;
        let schema_directives = sent
            .lock()
            .unwrap()
            .iter()
            .filter(|d| d.contains("Build the schema"))
            .count();
        // Base budget (MAX_STEP_FIX_ROUNDS + 1 = 3) + the rigor bonus (1) = 4 doer turns.
        assert_eq!(
            schema_directives,
            MAX_STEP_FIX_ROUNDS + 1 + 1,
            "a high-blast-radius step earns one extra fix round"
        );
    }

    // ── HIGH #1: the wall-clock deadline binds the step-internal + final-gate fix
    //    rounds (round 0 always runs; extra fix rounds past budget are skipped). ──

    /// A 1-step Build plan whose acceptance NEVER passes (no source on disk). The
    /// `id` lets the caller assert the step.
    fn one_failing_build_plan() -> crate::plan_state::Plan {
        use crate::plan_state::{AcceptanceSpec, Plan, PlanStep, StepKind, StepStatus};
        Plan {
            steps: vec![PlanStep {
                id: "a".into(),
                title: "Step A".into(),
                seat: crate::critics::Seat::FrontendEngineer,
                kind: StepKind::Build,
                depends_on: vec![],
                acceptance: AcceptanceSpec::SourcePresent,
                evidence: Vec::new(),
                status: StepStatus::Pending,
            }],
            risks: vec![],
            open_questions: vec![],
        }
    }

    /// A deadline already in the past (the budget is fully spent before the call).
    fn spent_deadline() -> std::time::Instant {
        std::time::Instant::now()
            .checked_sub(Duration::from_secs(1))
            .unwrap_or_else(std::time::Instant::now)
    }

    #[tokio::test]
    async fn budget_skips_step_internal_fix_rounds_round0_still_runs() {
        // HIGH #1: a Build step whose acceptance fails would normally re-drive
        // MAX_STEP_FIX_ROUNDS extra summon turns. With the wall-clock budget ALREADY
        // spent, round 0 (the real work) STILL runs once, but every EXTRA fix round is
        // skipped — so the step drives exactly ONE doer turn, not three. The honest
        // "skipping further fix rounds" note fires. (Compare
        // a_failing_step_acceptance_is_bounded_and_marks_blocked, which lets the full
        // fix budget run under a future deadline.)
        let tmp = tempfile::TempDir::new().unwrap();
        // NO source → the source-present acceptance fails every round.
        let (events, rec) = sink();
        let mut sess = FakeSession::new(
            vec![
                text_turn("Worked on it. Done."),
                text_turn("Tried again. Done."),
                text_turn("Once more. Done."),
            ],
            false,
            "",
        );
        let sent = sess.sent_handle();
        let mut o = opts(tmp.path());
        o.requirement = "做一个完整的产品".to_string();
        let route = build_route();
        let mut plan = one_failing_build_plan();

        let outcome = drive_plan_steps(
            &mut sess,
            &o,
            &events,
            &route,
            &mut plan,
            IdleBudget::new(Duration::from_millis(200), Duration::from_millis(200)),
            spent_deadline(),
        )
        .await;
        assert!(matches!(outcome, Some(DirectorLoopOutcome::Done { .. })));
        // EXACTLY ONE doer turn drove the step (round 0) — the extra fix rounds were
        // skipped by the budget. The final gate also adds NO fix turn (its own round-0
        // QC read found the gap but the budget skipped the fix turn), so the main
        // session received exactly one directive total.
        let n = sent.lock().unwrap().len();
        assert_eq!(
            n, 1,
            "round 0 runs but extra fix rounds + final-gate fix turns are skipped: {n}"
        );
        assert!(
            note_seen(&rec, "skipping further fix rounds on this step"),
            "the step-internal budget note fires"
        );
        // The step still ended Blocked (round 0's acceptance failed) — honest.
        assert_eq!(plan.steps[0].status, crate::plan_state::StepStatus::Blocked);
    }

    #[tokio::test]
    async fn budget_skips_final_gate_fix_turns_round0_qc_still_runs() {
        // HIGH #1: the final whole-build QC gate's round 0 (the read-only QC read)
        // always runs so the build is held to the floor; but its minute-level FIX
        // turns past the budget are skipped. With source present but a governance
        // violation (an emoji-as-icon write on codex), round-0 QC flags a finding —
        // and with the budget spent NO fix turn is driven for it.
        let tmp = tempfile::TempDir::new().unwrap();
        // Source present (so the step's acceptance passes + the step ticks Done), plus
        // a governance violation the FINAL gate's QC will flag.
        std::fs::write(
            tmp.path().join("button.tsx"),
            "export const Btn = () => <button>\u{1F680} Launch</button>;",
        )
        .unwrap();
        let (events, rec) = sink();
        let mut sess = FakeSession::new(vec![text_turn("Built step a. Done.")], false, "");
        let sent = sess.sent_handle();
        let mut o = codex_opts(tmp.path()); // codex → the QC governance scan is its gate
        o.requirement = "做一个完整的产品".to_string();
        let route = build_route();
        let mut plan = one_failing_build_plan(); // acceptance is source-present → passes here

        let outcome = drive_plan_steps(
            &mut sess,
            &o,
            &events,
            &route,
            &mut plan,
            IdleBudget::new(Duration::from_millis(200), Duration::from_millis(200)),
            spent_deadline(),
        )
        .await;
        assert!(matches!(outcome, Some(DirectorLoopOutcome::Done { .. })));
        // The step drove ONCE (its acceptance passed → Done). The final gate's round-0
        // QC flagged the governance violation, but the budget skipped its fix turn —
        // so the main session saw exactly ONE directive (the step), no final-gate fix.
        let n = sent.lock().unwrap().len();
        assert_eq!(
            n, 1,
            "the step ran; the final-gate fix turn was skipped past budget: {n}"
        );
        assert!(
            note_seen(&rec, "final QC findings left for the objective"),
            "the final-gate budget note fires"
        );
        assert_eq!(plan.steps[0].status, crate::plan_state::StepStatus::Done);
    }

    // ── MEDIUM #2: a Pending step stranded behind a Blocked dependency is honestly
    //    re-marked Blocked + a Note fires (no silent scope loss). ──

    #[test]
    fn unreachable_pending_behind_a_blocked_dep_is_marked_blocked() {
        // The pure helper: a → (Blocked); b depends on a (Pending); c depends on b
        // (Pending); d is independent (Pending). a's block transitively strands b AND
        // c, but NOT the independent d. Marks b + c Blocked, leaves d Pending.
        use crate::plan_state::{AcceptanceSpec, Plan, PlanStep, StepKind, StepStatus};
        let (events, rec) = sink();
        let mk = |id: &str, deps: &[&str], status: StepStatus| PlanStep {
            id: id.into(),
            title: format!("step {id}"),
            seat: crate::critics::Seat::FrontendEngineer,
            kind: StepKind::Build,
            depends_on: deps.iter().map(|d| (*d).to_string()).collect(),
            acceptance: AcceptanceSpec::SourcePresent,
            evidence: Vec::new(),
            status,
        };
        let mut plan = Plan {
            steps: vec![
                mk("a", &[], StepStatus::Blocked),
                mk("b", &["a"], StepStatus::Pending),
                mk("c", &["b"], StepStatus::Pending),
                mk("d", &[], StepStatus::Pending),
            ],
            risks: vec![],
            open_questions: vec![],
        };
        let n = mark_unreachable_pending_blocked(&mut plan, &events);
        assert_eq!(n, 2, "b and c are transitively stranded → 2 newly Blocked");
        let by = |id: &str| plan.steps.iter().find(|s| s.id == id).unwrap().status;
        assert_eq!(by("b"), StepStatus::Blocked);
        assert_eq!(by("c"), StepStatus::Blocked);
        assert_eq!(
            by("d"),
            StepStatus::Pending,
            "the independent step is untouched"
        );
        // A Blocked status event was emitted for each stranded step.
        assert_eq!(
            rec.count(
                |e| matches!(e, EngineEvent::PlanStepStatus { status, .. } if status == "blocked")
            ),
            2
        );
        // A clean plan (nothing Blocked) strands nothing.
        let mut clean = Plan {
            steps: vec![
                mk("x", &[], StepStatus::Done),
                mk("y", &["x"], StepStatus::Pending),
            ],
            risks: vec![],
            open_questions: vec![],
        };
        let (e2, _r2) = sink();
        assert_eq!(mark_unreachable_pending_blocked(&mut clean, &e2), 0);
    }

    #[tokio::test]
    async fn blocked_step_strands_its_dependent_which_is_honestly_marked_and_noted() {
        // End-to-end MEDIUM #2: a 2-step plan where step a (no source → acceptance
        // fails, bounded) ends Blocked, and step b depends on a. b never becomes ready
        // (its dep a is not Done), so the scheduler leaves it Pending — the silent
        // scope loss. The post-schedule honesty pass marks b Blocked + emits the
        // "因前置被阻塞而跳过" Note, so the checklist and the conclusion are honest.
        use crate::plan_state::{AcceptanceSpec, Plan, PlanStep, StepKind, StepStatus};
        let tmp = tempfile::TempDir::new().unwrap();
        // NO source → step a's source-present acceptance fails every round → Blocked.
        let (events, rec) = sink();
        let mk = |id: &str, deps: &[&str]| PlanStep {
            id: id.into(),
            title: format!("step {id}"),
            seat: crate::critics::Seat::FrontendEngineer,
            kind: StepKind::Build,
            depends_on: deps.iter().map(|d| (*d).to_string()).collect(),
            acceptance: AcceptanceSpec::SourcePresent,
            evidence: Vec::new(),
            status: StepStatus::Pending,
        };
        let mut plan = Plan {
            steps: vec![mk("a", &[]), mk("b", &["a"])],
            risks: vec![],
            open_questions: vec![],
        };
        // Plenty of default-completing turns; a future deadline so the FULL fix budget
        // runs (this isolates MEDIUM #2 from HIGH #1 — the strand, not the budget).
        let turns: Vec<Vec<SessionEvent>> =
            (0..6).map(|_| text_turn("Worked on it. Done.")).collect();
        let mut sess = FakeSession::new(turns, false, "");
        let mut o = opts(tmp.path());
        o.requirement = "做一个完整的产品".to_string();
        let route = build_route();

        let outcome = drive_plan_steps(
            &mut sess,
            &o,
            &events,
            &route,
            &mut plan,
            IdleBudget::new(Duration::from_millis(200), Duration::from_millis(200)),
            std::time::Instant::now() + Duration::from_secs(3_600),
        )
        .await;
        assert!(matches!(outcome, Some(DirectorLoopOutcome::Done { .. })));
        // BOTH a (drove + failed) and b (stranded) ended Blocked — no Pending leftover.
        let by = |id: &str| plan.steps.iter().find(|s| s.id == id).unwrap().status;
        assert_eq!(by("a"), StepStatus::Blocked, "step a failed its acceptance");
        assert_eq!(
            by("b"),
            StepStatus::Blocked,
            "step b is honestly marked Blocked (stranded), not silently left Pending"
        );
        // The honest skip Note fired so the conclusion isn't silently incomplete.
        assert!(
            note_seen(&rec, "因前置被阻塞而跳过"),
            "the stranded-scope Note is surfaced"
        );
    }

    #[tokio::test]
    async fn circuit_breaker_stops_a_flailing_plan_early_with_a_diagnosis() {
        // UD-FLOW-008 circuit breaker: a plan of INDEPENDENT build steps that each
        // fail their acceptance (no source on disk → source-present rejects every step)
        // is a same-class flail. After CONSECUTIVE_FAILURE_THRESHOLD consecutive
        // build-verify failures the breaker trips: the schedule STOPS early (later steps
        // are never driven) and a typed diagnosis Note is surfaced — instead of looping
        // through all MAX_STEP_TRANSITIONS burning the base's effort. The run still
        // settles cleanly (Done, honestly NOT clean), never a wedge.
        use crate::plan_state::{AcceptanceSpec, Plan, PlanStep, StepKind, StepStatus};
        let tmp = tempfile::TempDir::new().unwrap();
        // NO source seeded → every source-present Build step fails its acceptance.
        let (events, rec) = sink();
        // More INDEPENDENT steps than the threshold, so the breaker (not exhaustion)
        // is what stops the loop — proving an EARLY, diagnosed stop.
        let n_steps = (crate::trust::CONSECUTIVE_FAILURE_THRESHOLD as usize) + 2;
        let mk = |id: &str| PlanStep {
            id: id.into(),
            title: format!("step {id}"),
            seat: crate::critics::Seat::FrontendEngineer,
            kind: StepKind::Build,
            depends_on: vec![], // all independent → all ready, all driven in turn
            acceptance: AcceptanceSpec::SourcePresent,
            evidence: Vec::new(),
            status: StepStatus::Pending,
        };
        let mut plan = Plan {
            steps: (0..n_steps).map(|i| mk(&format!("s{i}"))).collect(),
            risks: vec![],
            open_questions: vec![],
        };
        // Plenty of default-completing turns; a future deadline so the breaker (not the
        // wall-clock budget) is what stops the schedule.
        let turns: Vec<Vec<SessionEvent>> =
            (0..40).map(|_| text_turn("Worked on it. Done.")).collect();
        let mut sess = FakeSession::new(turns, false, "");
        let mut o = opts(tmp.path());
        o.requirement = "做一个完整的产品".to_string(); // a build (not a document task)
        let route = build_route();

        let outcome = drive_plan_steps(
            &mut sess,
            &o,
            &events,
            &route,
            &mut plan,
            IdleBudget::new(Duration::from_millis(200), Duration::from_millis(200)),
            std::time::Instant::now() + Duration::from_secs(3_600),
        )
        .await;
        // Settles cleanly (never hangs / never disguises the failures as success).
        assert!(matches!(outcome, Some(DirectorLoopOutcome::Done { .. })));
        // EARLY stop: exactly threshold steps were ever driven (went Active); the rest
        // were never scheduled because the breaker tripped.
        let driven = active_order(&rec).len();
        assert_eq!(
            driven,
            crate::trust::CONSECUTIVE_FAILURE_THRESHOLD as usize,
            "the breaker stops the schedule after N consecutive failures, before exhausting \
             all {n_steps} steps (drove {driven})"
        );
        // A typed diagnosis was surfaced naming WHAT kept failing.
        assert!(
            note_seen(&rec, "circuit breaker tripped")
                && note_seen(&rec, "build-verify")
                && note_seen(&rec, "stopping the schedule early"),
            "a typed circuit-breaker diagnosis is surfaced: {:?}",
            rec.events()
        );
    }

    // ── HIGH #1 / MEDIUM #3: a step can no longer be marked Done over ZERO real work
    //    (an empty-team ReviewClean, or a Build step over a dead summon turn). ──

    /// A 1-step plan whose single Build step declares `ReviewClean` acceptance — the
    /// weak criterion that, pre-fix, accepted over an empty team (no source check).
    fn one_review_clean_build_plan() -> crate::plan_state::Plan {
        use crate::plan_state::{AcceptanceSpec, Plan, PlanStep, StepKind, StepStatus};
        Plan {
            steps: vec![PlanStep {
                id: "a".into(),
                title: "Build with a weak review-clean acceptance".into(),
                seat: crate::critics::Seat::FrontendEngineer,
                kind: StepKind::Build,
                depends_on: vec![],
                acceptance: AcceptanceSpec::ReviewClean,
                evidence: Vec::new(),
                status: StepStatus::Pending,
            }],
            risks: vec![],
            open_questions: vec![],
        }
    }

    #[tokio::test]
    async fn turn_settled_build_step_with_no_source_is_not_done() {
        // HIGH #1: a Build step that declares the WEAKEST acceptance (TurnSettled)
        // must STILL honour the source-present honesty floor — a turn that settled but
        // wrote ZERO source must NOT mark the step Done. Verify the floor directly.
        use crate::plan_state::{AcceptanceSpec, PlanStep, StepKind, StepStatus};
        let tmp = tempfile::TempDir::new().unwrap();
        // NO source seeded → the honesty floor must reject.
        let (events, _rec) = sink();
        let mut sess = FakeSession::new(vec![], false, "");
        let o = opts(tmp.path());
        let route = build_route();
        let step = PlanStep {
            id: "a".into(),
            title: "claimed done, wrote nothing".into(),
            seat: crate::critics::Seat::FrontendEngineer,
            kind: StepKind::Build,
            depends_on: vec![],
            acceptance: AcceptanceSpec::TurnSettled,
            evidence: Vec::new(),
            status: StepStatus::Active,
        };
        let verdict = verify_step_acceptance(&mut sess, &o, &events, &route, &step).await;
        assert!(
            !verdict.accepted,
            "a TurnSettled Build over an empty tree must NOT pass the honesty floor"
        );
    }

    // ── Evidence-contract-per-step: deterministic verify wiring ──────────────

    /// A Build step carrying a typed evidence contract (for the verify tests).
    fn evidence_step(evidence: Vec<crate::plan_state::EvidenceContract>) -> plan_state::PlanStep {
        use crate::plan_state::{AcceptanceSpec, PlanStep, StepKind, StepStatus};
        PlanStep {
            id: "a".into(),
            title: "deliver the thing".into(),
            seat: crate::critics::Seat::FrontendEngineer,
            kind: StepKind::Build,
            depends_on: vec![],
            // Acceptance is the FALLBACK; the typed evidence is what's actually checked.
            acceptance: AcceptanceSpec::SourcePresent,
            evidence,
            status: StepStatus::Active,
        }
    }

    #[tokio::test]
    async fn evidence_file_exists_satisfied_marks_the_step_accepted() {
        use crate::plan_state::EvidenceContract as E;
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(
            tmp.path().join("src/App.tsx"),
            "export const App = () => null;",
        )
        .unwrap();
        let (events, _rec) = sink();
        let mut sess = FakeSession::new(vec![], false, "");
        let o = opts(tmp.path());
        let route = build_route();
        let step = evidence_step(vec![E::FileExists {
            path: "src/App.tsx".into(),
        }]);
        let v = verify_step_acceptance(&mut sess, &o, &events, &route, &step).await;
        assert!(v.accepted, "the declared file exists → the step is done");
        assert!(v.has_positive_evidence);
        assert!(v.evidence.is_empty(), "no gap: {:?}", v.evidence);
    }

    #[tokio::test]
    async fn evidence_malformed_is_an_unmet_gap_even_with_real_source() {
        use crate::plan_state::EvidenceContract as E;
        // M6 regression: a step whose ONLY declared evidence is an under-specified
        // (Malformed) contract must NOT be accepted just because some source exists —
        // it is held to a falsifiable bar (an explicit gap), never silently degraded to
        // the coarse "any source exists" default.
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/other.tsx"), "export const X = 1;").unwrap();
        let (events, _rec) = sink();
        let mut sess = FakeSession::new(vec![], false, "");
        let o = opts(tmp.path());
        let route = build_route();
        let step = evidence_step(vec![E::Malformed {
            detail: "file-exists: missing a non-empty `path`".into(),
        }]);
        let v = verify_step_acceptance(&mut sess, &o, &events, &route, &step).await;
        assert!(
            !v.accepted,
            "an under-specified evidence contract must leave the step NOT done despite source"
        );
        assert!(
            v.evidence_line().contains("under-specified"),
            "the gap names the under-specification: {}",
            v.evidence_line()
        );
    }

    #[tokio::test]
    async fn evidence_file_exists_absent_stays_not_done_with_a_typed_gap() {
        use crate::plan_state::EvidenceContract as E;
        let tmp = tempfile::TempDir::new().unwrap();
        // Real source present (so the honesty floor passes) but NOT the declared file.
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/other.tsx"), "export const X = 1;").unwrap();
        let (events, _rec) = sink();
        let mut sess = FakeSession::new(vec![], false, "");
        let o = opts(tmp.path());
        let route = build_route();
        let step = evidence_step(vec![E::FileExists {
            path: "src/App.tsx".into(),
        }]);
        let v = verify_step_acceptance(&mut sess, &o, &events, &route, &step).await;
        assert!(
            !v.accepted,
            "the declared file is absent → the step is NOT done"
        );
        let line = v.evidence_line();
        assert!(
            line.contains("file-exists `src/App.tsx`") && line.contains("absent"),
            "a typed evidence-gap directive is surfaced: {line}"
        );
    }

    #[tokio::test]
    async fn evidence_file_contains_checks_the_specific_substring() {
        use crate::plan_state::EvidenceContract as E;
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/api.ts"), "export const base = '/api';").unwrap();
        let (events, _rec) = sink();
        let mut sess = FakeSession::new(vec![], false, "");
        let o = opts(tmp.path());
        let route = build_route();
        // The file exists but does NOT contain the declared needle → a typed gap.
        let miss = evidence_step(vec![E::FileContains {
            path: "src/api.ts".into(),
            needle: "/api/login".into(),
        }]);
        let v = verify_step_acceptance(&mut sess, &o, &events, &route, &miss).await;
        assert!(!v.accepted);
        assert!(
            v.evidence_line().contains("does not contain"),
            "{}",
            v.evidence_line()
        );
        // Now it contains the needle → satisfied.
        std::fs::write(
            tmp.path().join("src/api.ts"),
            "fetch('/api/login', { method: 'POST' });",
        )
        .unwrap();
        let hit = evidence_step(vec![E::FileContains {
            path: "src/api.ts".into(),
            needle: "/api/login".into(),
        }]);
        let v2 = verify_step_acceptance(&mut sess, &o, &events, &route, &hit).await;
        assert!(v2.accepted && v2.has_positive_evidence);
    }

    #[tokio::test]
    async fn step_with_no_evidence_falls_back_to_the_current_acceptance() {
        // Fail-open: an empty evidence contract uses the step's AcceptanceSpec EXACTLY as
        // before — source present + SourcePresent acceptance accepts; an empty tree is
        // rejected by the SAME honesty floor (the acceptance path still governs).
        use crate::plan_state::{AcceptanceSpec, PlanStep, StepKind, StepStatus};
        let (events, _rec) = sink();
        let mut sess = FakeSession::new(vec![], false, "");
        let route = build_route();
        let mk = |status| PlanStep {
            id: "a".into(),
            title: "t".into(),
            seat: crate::critics::Seat::FrontendEngineer,
            kind: StepKind::Build,
            depends_on: vec![],
            acceptance: AcceptanceSpec::SourcePresent,
            evidence: Vec::new(), // ← no typed contract → fall back to acceptance
            status,
        };
        // With real source → accepted.
        let with_src = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(with_src.path().join("src")).unwrap();
        std::fs::write(with_src.path().join("src/a.ts"), "export const x = 1;").unwrap();
        let v = verify_step_acceptance(
            &mut sess,
            &opts(with_src.path()),
            &events,
            &route,
            &mk(StepStatus::Active),
        )
        .await;
        assert!(
            v.accepted,
            "no-evidence step accepts via SourcePresent acceptance"
        );
        // Empty tree → the acceptance honesty floor rejects (unchanged behaviour).
        let empty = tempfile::TempDir::new().unwrap();
        let v2 = verify_step_acceptance(
            &mut sess,
            &opts(empty.path()),
            &events,
            &route,
            &mk(StepStatus::Active),
        )
        .await;
        assert!(
            !v2.accepted,
            "no-evidence step over an empty tree still rejects"
        );
    }

    #[tokio::test]
    async fn evidence_route_responds_is_fail_open_skip_when_app_cannot_boot() {
        use crate::plan_state::EvidenceContract as E;
        // A Build step with real source + a RouteResponds contract, but the tmp tree has
        // no dev server → the runtime proof degrades to NotVerified → the route check is
        // a NEUTRAL skip (fail-open), so the step accepts on the source floor's evidence
        // rather than being falsely blocked.
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/a.ts"), "export const x = 1;").unwrap();
        let (events, _rec) = sink();
        let mut sess = FakeSession::new(vec![], false, "");
        let o = opts(tmp.path());
        let route = build_route();
        let step = evidence_step(vec![E::RouteResponds {
            method: "GET".into(),
            path: "/api/x".into(),
            status: Some(200),
        }]);
        let v = verify_step_acceptance(&mut sess, &o, &events, &route, &step).await;
        assert!(
            v.accepted,
            "an unbootable route check is a neutral skip, never a false block"
        );
        assert!(v.evidence.is_empty());
    }

    #[tokio::test]
    async fn evidence_test_passes_flags_a_declared_but_absent_named_test() {
        use crate::plan_state::EvidenceContract as E;
        // Source present (so the honesty floor passes) but NO test mentions "checkout" →
        // the named test is absent from the codebase → a typed gap, not done.
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/app.ts"), "export const x = 1;").unwrap();
        let (events, _rec) = sink();
        let mut sess = FakeSession::new(vec![], false, "");
        let o = opts(tmp.path());
        let route = build_route();
        let step = evidence_step(vec![E::TestPasses {
            name: Some("checkout".into()),
        }]);
        let v = verify_step_acceptance(&mut sess, &o, &events, &route, &step).await;
        assert!(!v.accepted);
        assert!(
            v.evidence_line().contains("checkout")
                && v.evidence_line().contains("no test by that name"),
            "{}",
            v.evidence_line()
        );
    }

    #[tokio::test]
    async fn evidence_test_passes_accepts_a_present_named_test_when_suite_unavailable() {
        use crate::plan_state::EvidenceContract as E;
        // A test file mentions "login"; no manifest → the suite half is a neutral skip,
        // but the named test IS present → positive evidence, accepted.
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(
            tmp.path().join("src/login.test.ts"),
            "test('login flow works', () => { expect(1).toBe(1); });",
        )
        .unwrap();
        let (events, _rec) = sink();
        let mut sess = FakeSession::new(vec![], false, "");
        let o = opts(tmp.path());
        let route = build_route();
        let step = evidence_step(vec![E::TestPasses {
            name: Some("login".into()),
        }]);
        let v = verify_step_acceptance(&mut sess, &o, &events, &route, &step).await;
        assert!(v.accepted && v.has_positive_evidence);
    }

    #[tokio::test]
    async fn evidence_all_contracts_must_hold_one_gap_blocks_the_step() {
        use crate::plan_state::EvidenceContract as E;
        // Two contracts: one satisfied (file exists), one not (a missing file). ALL must
        // hold → the step stays not-done and the single typed gap is surfaced.
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/a.tsx"), "export const A = 1;").unwrap();
        let (events, _rec) = sink();
        let mut sess = FakeSession::new(vec![], false, "");
        let o = opts(tmp.path());
        let route = build_route();
        let step = evidence_step(vec![
            E::FileExists {
                path: "src/a.tsx".into(),
            },
            E::FileExists {
                path: "src/b.tsx".into(),
            },
        ]);
        let v = verify_step_acceptance(&mut sess, &o, &events, &route, &step).await;
        assert!(!v.accepted, "one unmet contract blocks the whole step");
        assert!(v.evidence_line().contains("src/b.tsx"));
        assert!(
            !v.evidence_line().contains("src/a.tsx"),
            "the met one is not a gap"
        );
    }

    #[tokio::test]
    async fn empty_team_review_clean_build_step_over_no_source_is_blocked_not_done() {
        // HIGH #1 + MEDIUM #3 (combined): a Build step that declares ReviewClean but
        // has an EMPTY route team (so 0 seats actually review) used to accept over zero
        // work — the empty-team review found "no blocking", and there was no source
        // floor on the ReviewClean path. Now: the source floor binds the Build step
        // (no source → reject), AND an empty-team review is a NEUTRAL skip that is not
        // positive progress. The step ends Blocked (honest), never Done.
        let tmp = tempfile::TempDir::new().unwrap();
        // NO source seeded.
        let (events, rec) = sink();
        // A single dead-ish doer turn (claims done, writes nothing). `fast_build_route`
        // has an EMPTY team → the ReviewClean check convenes 0 seats (neutral skip).
        let turns = vec![
            text_turn("Worked on it. Done."),
            text_turn("Tried again. Done."),
            text_turn("Once more. Done."),
        ];
        let mut sess = FakeSession::new(turns, false, "");
        let mut o = opts(tmp.path());
        o.requirement = "做一个完整的产品".to_string();
        // A deliberate route but with NO standing team → the review is an empty skip.
        let mut route = build_route();
        route.team = vec![];
        let mut plan = one_review_clean_build_plan();

        let outcome = drive_plan_steps(
            &mut sess,
            &o,
            &events,
            &route,
            &mut plan,
            IdleBudget::new(Duration::from_millis(200), Duration::from_millis(200)),
            std::time::Instant::now() + Duration::from_secs(3_600),
        )
        .await;
        assert!(matches!(outcome, Some(DirectorLoopOutcome::Done { .. })));
        // The step must NOT be Done over zero real work — it is honestly Blocked.
        assert_eq!(
            plan.steps[0].status,
            crate::plan_state::StepStatus::Blocked,
            "an empty-team ReviewClean Build over no source is Blocked, not Done"
        );
        assert_eq!(
            rec.count(
                |e| matches!(e, EngineEvent::PlanStepStatus { status, .. } if status == "done")
            ),
            0,
            "no step ticked Done over zero work"
        );
    }

    #[tokio::test]
    async fn dead_summon_does_not_complete_a_later_step_via_a_neutral_skip() {
        // MEDIUM #3: a dead/hung summon turn that never actually ran (`!drove`) must
        // not "complete" a Build step on a NEUTRAL-SKIP acceptance. Here a Build step
        // with ReviewClean acceptance + an empty team would (pre-fix) accept over a
        // dead turn; now the (drove || positive-evidence) guard refuses it. Driven via
        // `drive_build_step` directly so the dead turn + neutral acceptance are exact.
        let tmp = tempfile::TempDir::new().unwrap();
        // NO source → no positive evidence; the doer turn is dead (no TurnDone).
        let (events, _rec) = sink();
        let turns = vec![
            // A turn with text but NO TurnDone → summon's pump returns done=false.
            vec![SessionEvent::TextDelta("partial, never settled".into())],
            vec![SessionEvent::TextDelta("partial again".into())],
            vec![SessionEvent::TextDelta("still partial".into())],
        ];
        let mut sess = FakeSession::new(turns, false, "");
        let mut o = opts(tmp.path());
        o.requirement = "做一个完整的产品".to_string();
        let mut route = build_route();
        route.team = vec![]; // empty team → the ReviewClean check is a neutral skip
        let step = one_review_clean_build_plan()
            .steps
            .into_iter()
            .next()
            .unwrap();

        let outcome = drive_build_step(
            &mut sess,
            &o,
            &events,
            &route,
            &step,
            "", // no plan-progress recitation in this single-step unit test
            0,  // a leaf step (no dependents) → base fix budget, no rigor bonus
            std::time::Instant::now() + Duration::from_secs(3_600),
            &mut std::collections::HashSet::new(),
        )
        .await;
        assert!(
            !outcome.accepted,
            "a dead summon + neutral-skip acceptance must NOT accept the step"
        );
        assert!(
            !outcome.drove,
            "the doer turn never actually settled (dead session)"
        );
        assert!(
            !outcome.made_progress,
            "a dead turn over a neutral skip is not real progress"
        );
    }

    /// A doer session that GAMES the tests on every turn: it deletes a pre-existing
    /// test file while leaving the real impl source in place (so the source-present
    /// floor still passes). The ONLY thing that can fail the step is the
    /// test-integrity guard — exactly what we want to prove. Records the directives
    /// it received so the test can assert the guard's evidence was folded back in.
    struct GamingSession {
        root: std::path::PathBuf,
        test_rel: String,
        sent: Arc<std::sync::Mutex<Vec<String>>>,
        current: std::collections::VecDeque<SessionEvent>,
    }
    impl GamingSession {
        fn new(root: &std::path::Path, test_rel: &str) -> Self {
            Self {
                root: root.to_path_buf(),
                test_rel: test_rel.to_string(),
                sent: Arc::new(std::sync::Mutex::new(Vec::new())),
                current: std::collections::VecDeque::new(),
            }
        }
        fn sent_handle(&self) -> Arc<std::sync::Mutex<Vec<String>>> {
            Arc::clone(&self.sent)
        }
    }
    #[async_trait::async_trait]
    impl BaseSession for GamingSession {
        async fn fork(&mut self) -> Result<Box<dyn BaseSession>, SessionError> {
            Err(SessionError::ForkUnsupported("test".into()))
        }
        async fn send_turn(&mut self, directive: String) -> Result<(), SessionError> {
            self.sent.lock().unwrap().push(directive);
            // Game the tests: delete the pre-existing test file (idempotent — once
            // gone, the violation persists every round, so the step never clears).
            let _ = std::fs::remove_file(self.root.join(&self.test_rel));
            self.current = [
                SessionEvent::TextDelta("Build done, all green.".into()),
                SessionEvent::TurnDone {
                    status: TurnStatus::Completed,
                    usage: None,
                },
            ]
            .into_iter()
            .collect();
            Ok(())
        }
        async fn next_event(&mut self) -> Option<SessionEvent> {
            self.current.pop_front()
        }
        async fn respond(
            &mut self,
            _req_id: &str,
            _decision: ApprovalDecision,
        ) -> Result<(), SessionError> {
            Ok(())
        }
        async fn interrupt(&mut self) -> Result<(), SessionError> {
            Ok(())
        }
        async fn end(&mut self) -> Result<(), SessionError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_integrity_guard_blocks_a_gaming_step_and_is_bounded() {
        // UD-QA-001: a Build step whose doer DELETES a pre-existing test to fake a
        // pass must NOT be accepted, even though real impl source is on disk (so the
        // source-present floor passes). The guard flips the otherwise-passing verdict
        // to blocked, folds a file-naming finding into the re-drive directive, and is
        // bounded by the SAME fix-round counter — never an open grind.
        let tmp = tempfile::TempDir::new().unwrap();
        seed_source(tmp.path()); // app.ts (impl) → source-present floor passes
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(
            tmp.path().join("src/app.test.ts"),
            "it('adds', () => { expect(add(1,2)).toEqual(3); });\n",
        )
        .unwrap();
        let (events, _rec) = sink();
        let mut sess = GamingSession::new(tmp.path(), "src/app.test.ts");
        let sent = sess.sent_handle();
        let mut o = opts(tmp.path());
        o.requirement = "做一个完整的产品".to_string();
        let route = build_route();
        // The step's declared acceptance is SourcePresent — which WOULD pass over the
        // remaining impl source; only the integrity guard can block it.
        let step = one_failing_build_plan().steps.into_iter().next().unwrap();

        let outcome = drive_build_step(
            &mut sess,
            &o,
            &events,
            &route,
            &step,
            "", // no plan-progress recitation in this single-step unit test
            0,  // leaf step → base fix budget (MAX_STEP_FIX_ROUNDS), no rigor bonus
            std::time::Instant::now() + Duration::from_secs(3_600),
            &mut std::collections::HashSet::new(),
        )
        .await;

        assert!(
            !outcome.accepted,
            "a step that games tests must NOT be accepted even with impl source present"
        );
        // Bounded: round 0 + MAX_STEP_FIX_ROUNDS re-drives = 3 turns, never infinite.
        let directives = sent.lock().unwrap().clone();
        assert_eq!(
            directives.len(),
            MAX_STEP_FIX_ROUNDS + 1,
            "the integrity-driven rework is bounded by the fix-round counter: {directives:?}"
        );
        // The re-drive directive carries the typed, file-naming evidence.
        assert!(
            directives[1].contains("test-integrity") && directives[1].contains("app.test.ts"),
            "the fix directive names the gamed file: {:?}",
            directives[1]
        );
    }

    #[tokio::test]
    async fn test_integrity_guard_leaves_an_ungamed_step_alone() {
        // The complement: a Build step whose doer leaves the tests intact (FakeSession
        // never touches the fs) must pass cleanly — the guard is silent on an un-gamed
        // suite, so a genuine build is unaffected.
        let tmp = tempfile::TempDir::new().unwrap();
        seed_source(tmp.path());
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(
            tmp.path().join("src/app.test.ts"),
            "it('adds', () => { expect(add(1,2)).toEqual(3); });\n",
        )
        .unwrap();
        let (events, _rec) = sink();
        let mut sess = FakeSession::new(vec![text_turn("Implemented it. Done.")], false, "");
        let mut o = opts(tmp.path());
        o.requirement = "做一个完整的产品".to_string();
        let route = build_route();
        let step = one_failing_build_plan().steps.into_iter().next().unwrap();

        let outcome = drive_build_step(
            &mut sess,
            &o,
            &events,
            &route,
            &step,
            "", // no plan-progress recitation in this single-step unit test
            0,
            std::time::Instant::now() + Duration::from_secs(3_600),
            &mut std::collections::HashSet::new(),
        )
        .await;
        assert!(
            outcome.accepted,
            "an un-gamed build (tests left intact) must pass — the guard is silent"
        );
    }

    #[tokio::test]
    async fn a_passing_build_step_rewards_the_recalled_lesson_trust() {
        // SELF-EVOLUTION (relocated onto the default path): a Build step whose
        // acceptance PASSES must lift the trust of the lessons that were recalled into
        // it — the previously-dead feedback the runner-only path used to strand. Seed
        // one non-pitfall lesson (a quality failure), drive a step that passes on round
        // 0 (source is present), and assert the recalled lesson's trust rose. The
        // update is a pure SIDE EFFECT of the pass verdict — the step still accepts.
        let tmp = tempfile::TempDir::new().unwrap();
        seed_source(tmp.path()); // source present → SourcePresent acceptance passes
        let mut o = opts(tmp.path());
        o.requirement = "做一个登录系统".to_string();
        // Seed one recallable non-pitfall lesson at neutral trust.
        crate::lessons::capture_quality_failures(
            tmp.path(),
            &[crate::phases::QualityCheck {
                name: "coverage".to_string(),
                category: "quality".to_string(),
                description: "test".to_string(),
                status: "failed".to_string(),
                score: 20,
                details: "coverage below the bar for the login system".to_string(),
                weight: 2.0,
            }],
            "demo",
            &o.requirement,
        );
        let trust_of = |t: &std::path::Path| {
            crate::lessons::read_raw_lessons(t, "quality-failures.jsonl")
                .into_iter()
                .next()
                .map(|l| l.trust())
        };
        let before = trust_of(tmp.path()).unwrap();
        assert!(
            (before - crate::lessons::NEUTRAL_TRUST).abs() < f32::EPSILON,
            "the lesson seeds at neutral trust"
        );

        let (events, _rec) = sink();
        let mut sess = FakeSession::new(
            vec![text_turn("Implemented the login system. Done.")],
            false,
            "",
        );
        let route = build_route();
        let step = one_failing_build_plan().steps.into_iter().next().unwrap();
        let outcome = drive_build_step(
            &mut sess,
            &o,
            &events,
            &route,
            &step,
            "",
            0,
            std::time::Instant::now() + Duration::from_secs(3_600),
            &mut std::collections::HashSet::new(),
        )
        .await;
        assert!(outcome.accepted, "the step passes (source present)");
        assert!(
            trust_of(tmp.path()).unwrap() > before,
            "a passing step lifts the recalled lesson's trust (the relocated reward)"
        );
    }

    #[tokio::test]
    async fn first_step_dead_summon_resets_the_step_to_pending_before_bailing() {
        // MEDIUM #2 (strand fix): when the FIRST Build step can't drive (a dead summon
        // on the very first doer turn), `drive_plan_steps` returns None to fall back to
        // the single end-to-end turn. The just-marked Active step MUST be reset to
        // Pending (not left wedged Active) so a resume reads a clean plan. Drive
        // `drive_plan_steps` directly so we can inspect the plan after the None bail.
        let tmp = tempfile::TempDir::new().unwrap();
        seed_source(tmp.path());
        let (events, rec) = sink();
        // The first (and only) step's doer turn has a text delta but NO TurnDone → the
        // session drains to None mid-turn → summon returns done=false (didn't drive).
        let turns = vec![vec![SessionEvent::TextDelta("partial, no TurnDone".into())]];
        let mut sess = FakeSession::new(turns, false, "");
        let mut o = opts(tmp.path());
        o.requirement = "做一个完整的产品".to_string();
        let route = build_route();
        let mut plan = one_failing_build_plan(); // 1 Build step, id "a"

        let outcome = drive_plan_steps(
            &mut sess,
            &o,
            &events,
            &route,
            &mut plan,
            IdleBudget::new(Duration::from_millis(200), Duration::from_millis(200)),
            std::time::Instant::now() + Duration::from_secs(3_600),
        )
        .await;
        // The step-driver bailed (None) so the caller runs the single end-to-end turn.
        assert!(
            outcome.is_none(),
            "a first-step dead summon bails to the single-turn fallback"
        );
        // The just-marked Active step was RESET to Pending — never left wedged Active.
        assert_eq!(
            plan.steps[0].status,
            crate::plan_state::StepStatus::Pending,
            "the stranded first step is reset to Pending for a clean resume"
        );
        // A Pending status event was emitted for the reset (so the TUI un-sticks it).
        assert!(
            rec.count(
                |e| matches!(e, EngineEvent::PlanStepStatus { status, .. } if status == "pending")
            ) >= 1,
            "the reset-to-Pending transition is surfaced"
        );
    }

    #[tokio::test]
    async fn empty_team_review_step_is_a_neutral_skip_not_progress() {
        // HIGH #1: a standalone Review step whose route convened NO team (0 seats) did
        // zero real reviewing — it must be a NEUTRAL skip, NOT counted as progress that
        // ticks the step Done. `drive_review_step` returns made_progress=false for it.
        use crate::plan_state::{AcceptanceSpec, PlanStep, StepKind, StepStatus};
        let tmp = tempfile::TempDir::new().unwrap();
        seed_source(tmp.path());
        let (events, _rec) = sink();
        let mut sess = FakeSession::new(vec![], true, r#"{"accepts": true, "blocking": []}"#);
        let o = opts(tmp.path());
        let mut route = build_route();
        route.team = vec![]; // empty team → no seat actually reviews
        let step = PlanStep {
            id: "review".into(),
            title: "Cross-review".into(),
            seat: crate::critics::Seat::QaEngineer,
            kind: StepKind::Review,
            depends_on: vec![],
            acceptance: AcceptanceSpec::ReviewClean,
            evidence: Vec::new(),
            status: StepStatus::Active,
        };
        let outcome = drive_review_step(
            &mut sess,
            &o,
            &events,
            &route,
            &step,
            std::time::Instant::now() + Duration::from_secs(3_600),
        )
        .await;
        assert!(
            outcome.accepted,
            "an empty-team review accepts (nothing to block)"
        );
        assert!(
            !outcome.made_progress,
            "an empty-team review is a neutral skip, NOT real progress that marks Done"
        );
        assert!(!outcome.drove, "no review team actually convened (0 seats)");
    }

    #[tokio::test]
    async fn review_residual_corroboration_hits_a_floor_signal_and_is_silent_when_clean() {
        // FLOOR-INTEGRITY (the corroboration primitive): the residual-review floor check
        // is DETERMINISTIC + fail-open. A real governance violation on disk corroborates
        // a residual review finding; a genuinely clean floor (source present, no
        // violation, no doc gaps) corroborates NOTHING — so a bare critic opinion is
        // never smuggled through as if it were a floor signal.
        let tmp = tempfile::TempDir::new().unwrap();
        let (events, _rec) = sink();
        let route = build_route();

        // Clean floor: a plain source file, no governance violation, no PRD/architecture
        // docs → every deterministic check is a neutral pass → NOT corroborated (empty).
        seed_source(tmp.path());
        let o = opts(tmp.path());
        let clean = review_residual_floor_corroboration(&o, &events, &route).await;
        assert!(
            clean.is_empty(),
            "a clean floor corroborates nothing — a bare opinion stays advisory: {clean:?}"
        );

        // Governance violation on disk (emoji-as-icon in a UI source file) → the content
        // scan hits → the residual is CORROBORATED (non-empty, floor-tagged).
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(
            tmp.path().join("src/Btn.tsx"),
            "export const B = () => <button>🔍</button>;\n",
        )
        .unwrap();
        let hit = review_residual_floor_corroboration(&o, &events, &route).await;
        assert!(
            hit.iter().any(|l| l.contains("[governance]")),
            "an on-disk governance violation corroborates the residual: {hit:?}"
        );
    }

    #[tokio::test]
    async fn corroborated_residual_review_finding_marks_the_step_not_clean() {
        // FLOOR-INTEGRITY: after a review step's fix turn the team STILL flags a blocking
        // finding AND UmaDev's own deterministic floor (a governance violation on disk)
        // independently bites → the residual is CORROBORATED. The step must NOT tick
        // Done-clean: `made_progress=false` marks it Blocked (which folds into the final
        // gate's `all steps Done` clean signal so it can't vanish), while `accepted`
        // STAYS true so the circuit breaker (`drove && !accepted`) is untouched — a
        // corroborated residual never trips loop control, and critics stay advisory.
        use crate::plan_state::{AcceptanceSpec, PlanStep, StepKind, StepStatus};
        let tmp = tempfile::TempDir::new().unwrap();
        // Source present (source-present passes) BUT it carries a governance violation, so
        // the corroboration comes from the content scan, not an empty-tree floor.
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(
            tmp.path().join("src/Btn.tsx"),
            "export const B = () => <button>🔍</button>;\n",
        )
        .unwrap();
        let (events, _rec) = sink();
        // can_fork=true + a blocking verdict → the review team raises a finding on the
        // first pass AND on the recheck after the fix turn (the residual).
        let mut sess = FakeSession::new(
            vec![text_turn("fixed what I could")],
            true,
            r#"{"accepts": false, "blocking": ["按钮缺 loading 态"]}"#,
        );
        let o = opts(tmp.path());
        let route = build_route(); // team = [FrontendEngineer] → a seat actually reviews
        let step = PlanStep {
            id: "review".into(),
            title: "Cross-review".into(),
            seat: crate::critics::Seat::QaEngineer,
            kind: StepKind::Review,
            depends_on: vec![],
            acceptance: AcceptanceSpec::ReviewClean,
            evidence: Vec::new(),
            status: StepStatus::Active,
        };
        let outcome = drive_review_step(
            &mut sess,
            &o,
            &events,
            &route,
            &step,
            std::time::Instant::now() + Duration::from_secs(3_600),
        )
        .await;
        assert!(
            !outcome.made_progress,
            "a floor-corroborated residual must NOT tick Done-clean (it folds into the final gate)"
        );
        assert!(
            outcome.accepted,
            "`accepted` stays true so the circuit breaker is untouched — critics stay advisory"
        );
        assert!(
            !outcome.gap_evidence.is_empty(),
            "the corroborating floor lines are carried so they can't vanish"
        );
    }

    #[tokio::test]
    async fn uncorroborated_residual_review_finding_stays_advisory() {
        // The complement (invariant 2 + fail-open): a residual blocking finding with a
        // CLEAN deterministic floor (source present, no governance violation, no doc
        // gaps) is a bare critic OPINION. It must NOT hard-fail the step — `made_progress`
        // stays true (the step proceeds, today's behaviour). This also IS the fail-open
        // case: a neutral / uncomputable floor degrades to today's behaviour, never a
        // spurious fail.
        use crate::plan_state::{AcceptanceSpec, PlanStep, StepKind, StepStatus};
        let tmp = tempfile::TempDir::new().unwrap();
        seed_source(tmp.path()); // clean source → source-present passes, no violation
        let (events, _rec) = sink();
        let mut sess = FakeSession::new(
            vec![text_turn("fixed what I could")],
            true,
            r#"{"accepts": false, "blocking": ["按钮缺 loading 态"]}"#,
        );
        let o = opts(tmp.path());
        let route = build_route();
        let step = PlanStep {
            id: "review".into(),
            title: "Cross-review".into(),
            seat: crate::critics::Seat::QaEngineer,
            kind: StepKind::Review,
            depends_on: vec![],
            acceptance: AcceptanceSpec::ReviewClean,
            evidence: Vec::new(),
            status: StepStatus::Active,
        };
        let outcome = drive_review_step(
            &mut sess,
            &o,
            &events,
            &route,
            &step,
            std::time::Instant::now() + Duration::from_secs(3_600),
        )
        .await;
        assert!(
            outcome.made_progress,
            "an uncorroborated critic opinion stays advisory — the step proceeds (never hard-fails)"
        );
        assert!(outcome.accepted, "review steps still accept (advisory)");
        assert!(
            outcome.gap_evidence.is_empty(),
            "no floor corroboration → nothing to carry"
        );
    }

    #[tokio::test]
    async fn step_heartbeat_passes_through_and_a_fast_turn_emits_no_note() {
        // FIX #7: the heartbeat wrapper returns the wrapped future's value unchanged,
        // and a sub-interval (fast) turn emits NO heartbeat (the immediate first tick
        // is consumed) — so a quick step never spams the event stream.
        let (events, rec) = sink();
        let out = with_step_heartbeat(&events, "Quick step", async { 7u8 }).await;
        assert_eq!(
            out, 7,
            "the wrapped future's value passes through unchanged"
        );
        assert!(
            rec.count(|e| matches!(e, EngineEvent::Note(n) if n.contains("still building"))) == 0,
            "a sub-interval step emits no heartbeat (the immediate first tick is consumed)"
        );
    }

    #[tokio::test]
    async fn step_heartbeat_fires_on_a_turn_that_outlives_the_interval() {
        // FIX #7 (the positive case): a future that out-lives the heartbeat interval
        // yields at least one "still building" note — proof the heartbeat actually
        // fires for a genuinely long turn. Drives the explicit-interval variant with a
        // tiny real window (10ms) so the test stays fast without a paused-clock harness.
        let (events, rec) = sink();
        let slow = async {
            tokio::time::sleep(Duration::from_millis(60)).await;
            42u8
        };
        let out =
            with_step_heartbeat_every(&events, "Long step", Duration::from_millis(10), slow).await;
        assert_eq!(out, 42);
        assert!(
            rec.count(
                |e| matches!(e, EngineEvent::Note(n) if n.contains("Long step")
                && n.contains("still building"))
            ) >= 1,
            "a turn that outlives the heartbeat interval emits at least one progress note"
        );
    }

    #[tokio::test]
    async fn default_loop_records_usage_and_audit_and_lessons() {
        // Wave 2 deliverable 4: the DEFAULT single-turn loop records token usage,
        // the tool-call audit trail, and distils pitfalls — for every base, not just
        // claude in the legacy runner.
        let tmp = tempfile::TempDir::new().unwrap();
        seed_source(tmp.path());
        let (events, rec) = sink();
        // A turn that calls a tool (audited), a FAILED tool (a pitfall), and ends.
        let turns = vec![vec![
            SessionEvent::TextDelta("Implemented the feature. Done.".into()),
            SessionEvent::ToolCall {
                name: "Write".into(),
                input: serde_json::json!({"file_path": "src/app.ts"}),
            },
            SessionEvent::ToolResult {
                ok: false,
                summary: "npm run build failed: TS2304 cannot find name 'Foo'".into(),
            },
            SessionEvent::TurnDone {
                status: TurnStatus::Completed,
                usage: None,
            },
        ]];
        let mut sess = FakeSession::new(turns, false, "");
        let mut o = opts(tmp.path());
        o.backend = "codex".to_string(); // a non-claude base: audit must still record
                                         // Usage is written to ~/.umadev (HOME), so just assert the audit + lessons
                                         // side effects that land under the project root (deterministic, isolated).
        let outcome = drive_director_loop(&mut sess, &o, &events, "GO".into()).await;
        assert!(matches!(outcome, DirectorLoopOutcome::Done { .. }));

        // Audit trail recorded the tool call (UD-EVID-002) under the project root.
        let audit = tmp
            .path()
            .join(".umadev")
            .join("audit")
            .join("tool-calls.jsonl");
        let trail = std::fs::read_to_string(&audit).unwrap_or_default();
        assert!(
            trail.contains("Write") && trail.contains("src/app.ts"),
            "the tool call was recorded to the audit trail: {trail:?}"
        );

        // A `[learned]` note fired — the failed tool call was distilled into lessons.
        assert!(
            rec.events()
                .iter()
                .any(|e| matches!(e, EngineEvent::Note(n) if n.contains("[learned]"))),
            "the failed tool call was captured as a development pitfall"
        );
    }

    // ── Architecture unification: a CHAT-build's post-build QC earns the same
    //    flagship QC the `/run` path runs (governance/slop scan + team + bounded
    //    rework + capture), via `run_post_build_qc`. ──

    /// The behaviour-derived `Build`/`Light`/`Fast` route a chat-build carries — the
    /// EXACT shape the TUI's `reactive_build_route()` builds when the base writes its
    /// first file. `Light`/`Fast` means the QC takes the lean tier (source-present +
    /// governance scan, then settle), mirroring a real chat "做个落地页".
    fn chat_build_route() -> RoutePlan {
        use crate::router::{Budget, Depth, RouteClass};
        use crate::TaskKind;
        RoutePlan {
            class: RouteClass::Build,
            kind: TaskKind::Light,
            depth: Depth::Fast,
            team: Vec::new(),
            scope: Vec::new(),
            needs_clarify: None,
            est_budget: Budget::for_route(RouteClass::Build, Depth::Fast),
            confidence: 0.6,
        }
    }

    #[tokio::test]
    async fn post_build_qc_folds_a_design_slop_violation_into_a_fix_turn() {
        // A chat-build whose base wrote a UI file with emoji-as-icon (design slop)
        // must get the SAME governance scan the `/run` path runs — folded into a
        // bounded fix turn, exactly like a `/run` finding. This is the headline of the
        // unification: chat "做个落地页" now auto-gets the design/slop floor.
        let tmp = tempfile::TempDir::new().unwrap();
        // A real UI source file with an emoji used as a functional icon (a button
        // label) — `governance_scan` (the same emoji/slop detector) flags it.
        std::fs::write(
            tmp.path().join("App.tsx"),
            "export const Btn = () => <button>🚀 Launch</button>;",
        )
        .unwrap();
        let (events, _rec) = sink();
        // Turn 1 is the build reply (the base already claimed it built); turn 2 is the
        // fix turn (it "removes" the emoji — the scripted fake doesn't rewrite the file,
        // but we only assert the fix directive carried the governance finding).
        let turns = vec![text_turn(
            "Removed the emoji icon, used a Lucide icon. Done.",
        )];
        let mut sess = FakeSession::new(turns, true, r#"{"accepts": true, "blocking": []}"#);
        let sent = sess.sent_handle();
        let o = opts(tmp.path());
        let route = chat_build_route();

        let _ = run_post_build_qc(
            &mut sess,
            &o,
            &events,
            &route,
            "Built the landing page end to end. Done.",
        )
        .await;
        let sent = sent.lock().unwrap();
        assert!(
            sent.iter()
                .any(|d| d.contains("[governance]") && d.contains("must be fixed")),
            "the design-slop (emoji) finding was fed back as a fix directive: {sent:?}"
        );
    }

    #[tokio::test]
    async fn post_build_qc_on_a_clean_build_drives_no_fix_turn() {
        // A clean chat-build (real source, no governance violation, lean goal) must
        // settle with ZERO fix turns — the QC ran but found nothing, so the chat-build
        // is not slowed by needless rework.
        let tmp = tempfile::TempDir::new().unwrap();
        // A clean, slop-free, non-UI source module — `seed_source` writes exactly the
        // file the existing clean-build tests rely on (no emoji, no hardcoded color, no
        // root-component / ErrorBoundary rule), so the governance scan is genuinely clean.
        seed_source(tmp.path());
        let (events, _rec) = sink();
        let mut sess = FakeSession::new(vec![], true, r#"{"accepts": true, "blocking": []}"#);
        let sent = sess.sent_handle();
        let mut o = opts(tmp.path());
        // A lean goal → the lean tier short-circuits after the governance scan (clean).
        o.requirement = "做一个简单的纯前端落地页单页".to_string();
        let route = chat_build_route();

        let reply = run_post_build_qc(
            &mut sess,
            &o,
            &events,
            &route,
            "Built the clean landing page. Done.",
        )
        .await;
        assert!(
            reply.trim().is_empty(),
            "a clean post-build QC returns an empty reply (no fix turn ran): {reply:?}"
        );
        assert_eq!(
            sent.lock().unwrap().len(),
            0,
            "a clean chat-build drives no fix turn — chat stays fast"
        );
    }

    #[tokio::test]
    async fn post_build_qc_with_no_source_feeds_the_honesty_floor_back() {
        // A chat turn that claimed a build but wrote ZERO source: the source-present
        // honesty floor (always run, every tier) catches it and folds it into a fix
        // directive — the same decisive finding the `/run` path produces.
        let tmp = tempfile::TempDir::new().unwrap();
        let (events, _rec) = sink();
        let turns = vec![text_turn("Now actually created the files. Done.")];
        let mut sess = FakeSession::new(turns, false, "");
        let sent = sess.sent_handle();
        let o = opts(tmp.path());
        let route = chat_build_route();

        let _ = run_post_build_qc(
            &mut sess,
            &o,
            &events,
            &route,
            "Built it end to end. Done. (but wrote nothing)",
        )
        .await;
        assert!(
            sent.lock()
                .unwrap()
                .iter()
                .any(|d| d.contains("source-present") && d.contains("must be fixed")),
            "the no-source honesty finding was fed back as a fix directive"
        );
    }

    #[tokio::test]
    async fn post_build_qc_is_fail_open_on_a_dead_session() {
        // A session that dies on the fix turn must NOT panic — `run_post_build_qc`
        // settles fail-open (returns the empty/partial reply), never wedging the chat.
        let tmp = tempfile::TempDir::new().unwrap();
        // A governance violation so QC is NOT clean → it will try a fix turn.
        std::fs::write(
            tmp.path().join("App.tsx"),
            "export const Btn = () => <button>🚀 Go</button>;",
        )
        .unwrap();
        let (events, _rec) = sink();
        // The fix turn's batch has a text delta but NO TurnDone → next_event drains to
        // None mid-turn (a dead session). `run_post_build_qc` must settle, not panic.
        let turns = vec![vec![SessionEvent::TextDelta("partial fix".to_string())]];
        let mut sess = FakeSession::new(turns, true, r#"{"accepts": true, "blocking": []}"#);
        let o = opts(tmp.path());
        let route = chat_build_route();

        // Just reaching here without a panic is the assertion (fail-open). The reply is
        // whatever landed before the session died (empty in this scripted case).
        let _reply = run_post_build_qc(&mut sess, &o, &events, &route, "Built it. Done.").await;
    }

    #[test]
    fn post_build_rework_context_is_fail_open_on_an_empty_project() {
        // No knowledge dir + no lessons file → an empty prefix (never a panic). The
        // fix directive then degrades to the byte-for-byte plain directive.
        // Isolate HOME/UMADEV_KNOWLEDGE_DIR so a corpus staged to ~/.umadev/knowledge
        // (the bundled-knowledge home fallback) can't make this "empty" project recall.
        let _no_corpus = crate::test_support::NoBundledCorpus::new();
        let tmp = tempfile::TempDir::new().unwrap();
        let o = opts(tmp.path());
        let prefix = post_build_rework_context(&o);
        assert!(
            prefix.is_empty(),
            "an empty project recalls no knowledge/lessons → empty prefix: {prefix:?}"
        );
    }

    // ── Cross-session RESUME (`/continue` on a fresh session) ──

    /// Build a [`crate::plan_state::PlanStep`] for the resume tests.
    fn resume_step(
        id: &str,
        title: &str,
        deps: &[&str],
        status: crate::plan_state::StepStatus,
    ) -> crate::plan_state::PlanStep {
        use crate::plan_state::{AcceptanceSpec, PlanStep, StepKind};
        PlanStep {
            id: id.into(),
            title: title.into(),
            seat: crate::critics::Seat::FrontendEngineer,
            kind: StepKind::Build,
            depends_on: deps.iter().map(|d| (*d).to_string()).collect(),
            acceptance: AcceptanceSpec::SourcePresent,
            evidence: Vec::new(),
            status,
        }
    }

    #[test]
    fn plan_progress_recitation_is_bounded_and_honest() {
        // PLAN RECITATION lock test: the compact per-step "where we are in the plan"
        // line must (a) state the honest position, (b) name only the NEXT up-to-two
        // upcoming steps, and (c) be empty for a trivial plan — so a long step-by-step
        // run keeps the base anchored to the whole plan without bloating the directive.
        use crate::plan_state::{Plan, StepStatus};

        let plan = Plan {
            steps: vec![
                resume_step("s1", "scaffold the project", &[], StepStatus::Done),
                resume_step("s2", "build the auth route", &["s1"], StepStatus::Active),
                resume_step("s3", "build the dashboard", &["s2"], StepStatus::Pending),
                resume_step("s4", "wire the payments flow", &["s3"], StepStatus::Pending),
                resume_step("s5", "add the settings page", &["s4"], StepStatus::Pending),
            ],
            risks: vec![],
            open_questions: vec![],
        };

        let recit = plan_progress_recitation(&plan, "s2");
        // Honest position — the current (Active) step is not yet counted complete.
        assert!(
            recit.contains("1 of 5 plan steps complete"),
            "recites the honest position: {recit}"
        );
        // Names the NEXT (up to two) upcoming steps…
        assert!(
            recit.contains("build the dashboard"),
            "names the next step: {recit}"
        );
        assert!(
            recit.contains("wire the payments flow"),
            "names the 2nd next step: {recit}"
        );
        // …but is BOUNDED to two — the third pending step is NOT listed.
        assert!(
            !recit.contains("add the settings page"),
            "recitation is bounded to two upcoming steps: {recit}"
        );
        // …and never re-lists the already-done step.
        assert!(
            !recit.contains("scaffold the project"),
            "omits the done step: {recit}"
        );

        // The LAST step recites only its position (no upcoming) — still bounded.
        let last = plan_progress_recitation(&plan, "s5");
        assert!(
            last.contains("final step"),
            "the last step recites its position with no upcoming: {last}"
        );

        // FAIL-OPEN: a trivial single-step plan emits nothing (the goal frame suffices).
        let solo = Plan {
            steps: vec![resume_step("only", "do the thing", &[], StepStatus::Active)],
            risks: vec![],
            open_questions: vec![],
        };
        assert!(
            plan_progress_recitation(&solo, "only").is_empty(),
            "a single-step plan needs no progress recitation"
        );
    }

    #[tokio::test]
    async fn resume_drives_only_the_remaining_steps_not_the_done_ones() {
        // The resume entry loads a persisted plan with some Done + some Pending steps
        // and drives ONLY the remaining ones — the already-Done step is never re-run.
        use crate::plan_state::{Plan, StepStatus};
        let tmp = tempfile::TempDir::new().unwrap();
        // Source on disk so the remaining Build step's source-present acceptance passes
        // (it ticks Done, not Blocked) — the resume must COMPLETE the remaining work.
        seed_source(tmp.path());
        let (events, rec) = sink();

        // Persist a plan: `alpha` already DONE, `beta` PENDING (depends on alpha). A
        // resume must skip `alpha` entirely and drive only `beta`.
        let persisted = Plan {
            steps: vec![
                resume_step("alpha", "ALPHA scaffold the project", &[], StepStatus::Done),
                resume_step(
                    "beta",
                    "BETA build the remaining feature",
                    &["alpha"],
                    StepStatus::Pending,
                ),
            ],
            risks: vec![],
            open_questions: vec![],
        };
        plan_state::save(&persisted, tmp.path()).expect("persist the plan");

        let mut sess = FakeSession::new(vec![text_turn("Built BETA. Done.")], false, "");
        let sent = sess.sent_handle();
        let mut o = opts(tmp.path());
        o.requirement = "做一个完整的产品".to_string();
        let route = build_route();

        let outcome = drive_director_loop_resume(&mut sess, &o, &events, &route).await;
        assert!(
            matches!(outcome, Some(DirectorLoopOutcome::Done { .. })),
            "a resumable plan drives to a Done outcome"
        );

        // ONLY the remaining step drove — no directive ever mentioned the Done one.
        let sent = sent.lock().unwrap();
        assert!(
            sent.iter()
                .any(|d| d.contains("BETA build the remaining feature")),
            "the remaining (Pending) step was driven: {sent:?}"
        );
        assert!(
            !sent
                .iter()
                .any(|d| d.contains("ALPHA scaffold the project")),
            "the already-Done step was NOT re-driven: {sent:?}"
        );
        // Piece #3: the step directive RESTATES the original requirement (the goal
        // frame), so the base knows the overall product even on a fresh-session
        // resume — not just the bare step title.
        assert!(
            sent.iter()
                .any(|d| d.contains("做一个完整的产品") && d.contains("Overall goal")),
            "the resumed step directive restates the original goal, not just the step \
             title: {sent:?}"
        );
        // Plan recitation is threaded into the live directive: the driven step carries
        // the compact "where we are in the plan" line (1 of 2 done — beta is last).
        assert!(
            sent.iter()
                .any(|d| d.contains("Plan progress") && d.contains("1 of 2 plan steps complete")),
            "the step directive recites the plan position so the base stays anchored: {sent:?}"
        );

        // The persisted plan now has both steps Done (alpha preserved, beta completed).
        let after = plan_state::load(tmp.path()).expect("the plan is still on disk");
        let by = |id: &str| after.steps.iter().find(|s| s.id == id).unwrap().status;
        assert_eq!(
            by("alpha"),
            StepStatus::Done,
            "the prior Done step stays Done"
        );
        assert_eq!(
            by("beta"),
            StepStatus::Done,
            "the remaining step is completed"
        );

        // The checklist was re-rendered (PlanPosted) so the TUI shows the resume —
        // and the re-post carries the PERSISTED statuses (alpha already done), so
        // the panel renders "1/2" with alpha checked instead of resetting to
        // all-pending / 0 done (user-reported after /continue).
        let reposted = rec
            .events()
            .into_iter()
            .find_map(|e| match e {
                EngineEvent::PlanPosted {
                    statuses,
                    done,
                    total,
                    ..
                } => Some((statuses, done, total)),
                _ => None,
            })
            .expect("the checklist is re-rendered on resume");
        assert_eq!(
            reposted,
            (vec!["done".to_string(), "pending".to_string()], 1, 2),
            "the resume re-post carries the persisted per-step truth"
        );
    }

    #[tokio::test]
    async fn resume_is_none_when_no_resumable_plan_exists() {
        // Fail-open: an absent plan → the resume entry returns None so the caller falls
        // back to a fresh run (never a crash, never a phantom resume).
        let tmp = tempfile::TempDir::new().unwrap();
        let (events, _rec) = sink();
        let mut sess = FakeSession::new(vec![], false, "");
        let o = opts(tmp.path());
        let route = build_route();
        let outcome = drive_director_loop_resume(&mut sess, &o, &events, &route).await;
        assert!(
            outcome.is_none(),
            "no persisted plan → no resume (caller fails open to a fresh run)"
        );
    }

    #[test]
    fn has_resumable_run_detects_incomplete_done_and_absent() {
        // `has_resumable_run` is true for an incomplete persisted plan and false for a
        // fully-Done / absent one (no workflow-state written in these temp dirs).
        use crate::plan_state::{Plan, StepStatus};

        // (a) Absent plan + absent state → not resumable.
        let absent = tempfile::TempDir::new().unwrap();
        assert!(
            !has_resumable_run(absent.path()),
            "no plan / no state → not resumable"
        );

        // (b) A persisted plan with a Pending step → resumable.
        let incomplete = tempfile::TempDir::new().unwrap();
        let p = Plan {
            steps: vec![
                resume_step("a", "a", &[], StepStatus::Done),
                resume_step("b", "b", &["a"], StepStatus::Pending),
            ],
            risks: vec![],
            open_questions: vec![],
        };
        plan_state::save(&p, incomplete.path()).unwrap();
        assert!(
            has_resumable_run(incomplete.path()),
            "an incomplete persisted plan is resumable"
        );

        // (c) A persisted plan with EVERY step Done (+ no state) → not resumable.
        let done = tempfile::TempDir::new().unwrap();
        let p = Plan {
            steps: vec![
                resume_step("a", "a", &[], StepStatus::Done),
                resume_step("b", "b", &["a"], StepStatus::Done),
            ],
            risks: vec![],
            open_questions: vec![],
        };
        plan_state::save(&p, done.path()).unwrap();
        assert!(
            !has_resumable_run(done.path()),
            "a fully-Done plan with no state is not resumable"
        );
    }

    #[test]
    fn load_resumable_plan_resets_an_interrupted_active_step_to_pending() {
        // A step persisted as Active (the TUI closed mid-step) must be reset to Pending
        // on load so `ready_steps` surfaces it again — otherwise the interrupted step is
        // stranded (never re-driven). Done steps are preserved.
        use crate::plan_state::{Plan, StepStatus};
        let tmp = tempfile::TempDir::new().unwrap();
        let p = Plan {
            steps: vec![
                resume_step("a", "a", &[], StepStatus::Done),
                resume_step("b", "b", &[], StepStatus::Active),
            ],
            risks: vec![],
            open_questions: vec![],
        };
        plan_state::save(&p, tmp.path()).unwrap();
        let loaded =
            load_resumable_plan(tmp.path()).expect("an Active step makes the plan resumable");
        let by = |id: &str| loaded.steps.iter().find(|s| s.id == id).unwrap().status;
        assert_eq!(by("a"), StepStatus::Done, "the Done step is preserved");
        assert_eq!(
            by("b"),
            StepStatus::Pending,
            "the interrupted Active step is reset to Pending for a clean re-drive"
        );
        let ready: Vec<String> = loaded.ready_steps().iter().map(|s| s.id.clone()).collect();
        assert_eq!(ready, vec!["b"], "the reset step is ready again");
    }

    #[test]
    fn stale_upstream_doc_reopens_steps_on_resume() {
        // Item C end-to-end: a plan built against prd v1 is fully Done; the PRD then
        // changes; on resume the frontend step (reads Prd) + its downstream re-open so
        // the director re-derives against the changed upstream instead of trusting a
        // now-poisoned result.
        use crate::plan_state::{Plan, StepStatus};
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("output")).unwrap();
        std::fs::write(root.join("output").join("app-prd.md"), "prd v1").unwrap();
        record_artifact_versions(root);
        let mut plan = Plan {
            steps: vec![
                resume_step("fe", "frontend", &[], StepStatus::Done),
                resume_step("qa", "qa", &["fe"], StepStatus::Done),
            ],
            risks: vec![],
            open_questions: vec![],
        };
        invalidate_stale_steps(root, &mut plan);
        assert!(plan.steps.iter().all(|s| s.status == StepStatus::Done));
        std::fs::write(root.join("output").join("app-prd.md"), "prd v2 CHANGED").unwrap();
        invalidate_stale_steps(root, &mut plan);
        let by = |id: &str| plan.steps.iter().find(|s| s.id == id).unwrap().status;
        assert_eq!(
            by("fe"),
            StepStatus::Pending,
            "frontend re-opens on a prd change"
        );
        assert_eq!(by("qa"), StepStatus::Pending, "its downstream re-opens too");
    }

    // ── BOUNDED RE-PLAN of a blocked subtree (attempt_replan_blocked_subtree) ──

    /// scaffold(Done) → api(Blocked) → ui(Pending): a blocked step that STRANDS a
    /// dependent (`ui`). `mk` mirrors the inline PlanStep builder the other tests use.
    fn blocked_subtree_plan() -> Plan {
        use crate::plan_state::{AcceptanceSpec, PlanStep, StepKind};
        let mk = |id: &str, deps: &[&str], status: StepStatus| PlanStep {
            id: id.into(),
            title: format!("Build the {id}"),
            seat: crate::critics::Seat::BackendEngineer,
            kind: StepKind::Build,
            depends_on: deps.iter().map(|d| (*d).to_string()).collect(),
            acceptance: AcceptanceSpec::SourcePresent,
            evidence: Vec::new(),
            status,
        };
        Plan {
            steps: vec![
                mk("scaffold", &[], StepStatus::Done),
                mk("api", &["scaffold"], StepStatus::Blocked),
                mk("ui", &["api"], StepStatus::Pending),
            ],
            risks: vec![],
            open_questions: vec![],
        }
    }

    /// A brain re-plan reply: a fresh route {api2 → ui2} around the blocked `api`.
    const REPLAN_SUBDAG: &str = r#"{"steps":[
        {"id":"api2","title":"alt api","seat":"backend-engineer","kind":"build",
         "depends_on":["scaffold"],"acceptance":"source-present"},
        {"id":"ui2","title":"alt ui","seat":"frontend-engineer","kind":"build",
         "depends_on":["api2"],"acceptance":"source-present"}]}"#;

    #[tokio::test]
    async fn replan_triggers_once_merges_a_validated_subdag_and_is_bounded() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (events, _rec) = sink();
        // The fork's judge turn emits the replacement sub-DAG JSON.
        let mut sess = FakeSession::new(vec![], true, REPLAN_SUBDAG);
        let o = opts(tmp.path());
        let mut plan = blocked_subtree_plan();
        let mut replanned = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(3_600);

        // First block WITH stranded dependents → ONE re-plan; the sub-DAG is merged
        // (routed through normalized(): dedup / dangling strip / cycle-break / floors).
        let merged = attempt_replan_blocked_subtree(
            &mut sess,
            &o,
            &events,
            &mut plan,
            "api",
            "Build the api",
            &["source-present: no source files on disk".to_string()],
            &mut replanned,
            deadline,
        )
        .await;
        assert!(
            merged,
            "a blocked step with stranded dependents re-plans once"
        );
        assert!(replanned, "the single-attempt budget is consumed");
        let ids: Vec<&str> = plan.steps.iter().map(|s| s.id.as_str()).collect();
        assert!(
            ids.contains(&"api2") && ids.contains(&"ui2"),
            "the fresh route is spliced in: {ids:?}"
        );
        assert!(
            !ids.contains(&"api") && !ids.contains(&"ui"),
            "the blocked subtree is replaced: {ids:?}"
        );
        // The surviving Done step KEEPS its status (normalized's reset was undone).
        assert_eq!(
            plan.steps
                .iter()
                .find(|s| s.id == "scaffold")
                .unwrap()
                .status,
            StepStatus::Done
        );

        // BOUND: a SECOND block does NOT re-plan again (the flag is already consumed).
        let before = plan.clone();
        let again = attempt_replan_blocked_subtree(
            &mut sess,
            &o,
            &events,
            &mut plan,
            "api2",
            "alt api",
            &[],
            &mut replanned,
            deadline,
        )
        .await;
        assert!(!again, "at most ONE re-plan per run");
        assert_eq!(plan, before, "a second attempt leaves the plan unchanged");
    }

    #[tokio::test]
    async fn replan_falls_back_when_the_consult_fails() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (events, _rec) = sink();
        // can_fork == false → the consult can't open → judge_json None → fallback.
        let mut sess = FakeSession::new(vec![], false, REPLAN_SUBDAG);
        let o = opts(tmp.path());
        let mut plan = blocked_subtree_plan();
        let before = plan.clone();
        let mut replanned = false;
        let merged = attempt_replan_blocked_subtree(
            &mut sess,
            &o,
            &events,
            &mut plan,
            "api",
            "Build the api",
            &[],
            &mut replanned,
            std::time::Instant::now() + Duration::from_secs(3_600),
        )
        .await;
        assert!(!merged, "a failed consult falls back to the honest strand");
        assert_eq!(plan, before, "the plan is unchanged when the consult fails");
        assert!(
            replanned,
            "the attempt is still consumed so a failed consult can never retry (no loop)"
        );
    }

    #[tokio::test]
    async fn replan_falls_back_on_an_unparseable_or_noop_subdag() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (events, _rec) = sink();
        // Unparseable fork reply → parse_brain_steps empty → fallback.
        let mut sess = FakeSession::new(vec![], true, "not json at all");
        let o = opts(tmp.path());
        let mut plan = blocked_subtree_plan();
        let before = plan.clone();
        let mut replanned = false;
        let merged = attempt_replan_blocked_subtree(
            &mut sess,
            &o,
            &events,
            &mut plan,
            "api",
            "Build the api",
            &[],
            &mut replanned,
            std::time::Instant::now() + Duration::from_secs(3_600),
        )
        .await;
        assert!(!merged, "an unparseable sub-DAG falls back");
        assert_eq!(plan, before);

        // A no-op sub-DAG that re-emits ONLY existing ids (no new route) also falls back.
        let mut sess2 = FakeSession::new(
            vec![],
            true,
            r#"{"steps":[{"id":"ui","title":"x","seat":"frontend-engineer","kind":"build","acceptance":"source-present"}]}"#,
        );
        let mut plan2 = blocked_subtree_plan();
        let before2 = plan2.clone();
        let mut replanned2 = false;
        let merged2 = attempt_replan_blocked_subtree(
            &mut sess2,
            &o,
            &events,
            &mut plan2,
            "api",
            "Build the api",
            &[],
            &mut replanned2,
            std::time::Instant::now() + Duration::from_secs(3_600),
        )
        .await;
        assert!(
            !merged2,
            "a sub-DAG adding no NEW id changes nothing → fallback"
        );
        assert_eq!(plan2, before2);
    }

    #[tokio::test]
    async fn replan_does_not_trigger_for_a_blocked_leaf_with_no_dependents() {
        use crate::plan_state::{AcceptanceSpec, PlanStep, StepKind};
        let tmp = tempfile::TempDir::new().unwrap();
        let (events, _rec) = sink();
        // A valid sub-DAG is available on the fork, but the blocked step is a LEAF
        // (nothing depends on it) → nothing to recover → no consult, no change.
        let mut sess = FakeSession::new(vec![], true, REPLAN_SUBDAG);
        let o = opts(tmp.path());
        let mut plan = Plan {
            steps: vec![
                PlanStep {
                    id: "scaffold".into(),
                    title: "scaffold".into(),
                    seat: crate::critics::Seat::FrontendEngineer,
                    kind: StepKind::Build,
                    depends_on: vec![],
                    acceptance: AcceptanceSpec::SourcePresent,
                    evidence: Vec::new(),
                    status: StepStatus::Done,
                },
                PlanStep {
                    id: "leaf".into(),
                    title: "a leaf".into(),
                    seat: crate::critics::Seat::FrontendEngineer,
                    kind: StepKind::Build,
                    depends_on: vec!["scaffold".into()],
                    acceptance: AcceptanceSpec::SourcePresent,
                    evidence: Vec::new(),
                    status: StepStatus::Blocked,
                },
            ],
            risks: vec![],
            open_questions: vec![],
        };
        let before = plan.clone();
        let mut replanned = false;
        let merged = attempt_replan_blocked_subtree(
            &mut sess,
            &o,
            &events,
            &mut plan,
            "leaf",
            "a leaf",
            &[],
            &mut replanned,
            std::time::Instant::now() + Duration::from_secs(3_600),
        )
        .await;
        assert!(
            !merged,
            "a blocked leaf has nothing to recover → no re-plan"
        );
        assert!(
            !replanned,
            "no stranded dependents → the single-attempt budget is NOT spent"
        );
        assert_eq!(plan, before, "the plan is unchanged for a leaf block");
    }
}
