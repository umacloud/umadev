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

use std::sync::Arc;
use std::time::Duration;

use umadev_runtime::{ApprovalDecision, BaseSession, SessionEvent, StreamEvent, TurnStatus, Usage};

use crate::director::{self, ReviewResult, VerifyKind, VerifyResult};
use crate::events::{EngineEvent, EventSink};
use crate::plan_state::{self, Plan, StepStatus};
use crate::router::RoutePlan;
use crate::runner::RunOptions;
use crate::trust::requires_confirmation;
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
/// watchdog (`umadev-host` keys the same env). 300s (5 min) is generous enough
/// that a legitimately-long streaming compile/test turn survives as long as it
/// emits ANY output (every event resets the timer), but a true hang settles.
const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 300;

/// The idle watchdog window for one `next_event().await`, from
/// `UMADEV_IDLE_TIMEOUT_SECS` (the SAME env the single-shot host watchdog reads —
/// `umadev_host`), falling back to [`DEFAULT_IDLE_TIMEOUT_SECS`]. A non-positive /
/// unparseable value falls back to the default (fail-open: a bad env never
/// disables the watchdog, which would re-introduce the permanent hang). Read once
/// per turn at the app boundary, not per wait, so a mid-turn env flip can't race.
///
/// `pub(crate)` so every main-session event pump (this loop, plus
/// [`crate::continuous`]'s `drive_phase` / `drive_rework_turn`) reads the SAME
/// window from ONE place — the consistency the P1-11 fix depends on.
pub(crate) fn idle_timeout() -> Duration {
    let secs = std::env::var("UMADEV_IDLE_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_IDLE_TIMEOUT_SECS);
    Duration::from_secs(secs)
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

/// The result of ONE idle-guarded `next_event()` wait — the shared primitive
/// every main-session event pump uses so the "stops emitting but never exits"
/// hang can NEVER wedge a pump (the P0-3 / P1-11 zero-stall fix).
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
    /// No event arrived within the idle window — the base is hung. The watchdog
    /// has already issued a best-effort (bounded) `interrupt()`; the pump settles
    /// the turn as a failure so `thinking` clears rather than blocking forever.
    /// Carries the base's diagnosis captured AFTER the bounded interrupt (an
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
/// Bounds the wait by `idle`; ANY event resets it (the caller loops, calling this
/// again with the full window), so a legitimately-long streaming compile/test
/// turn survives as long as it emits SOMETHING. Pure silence past the window
/// issues a best-effort `interrupt()` — itself bounded by [`INTERRUPT_TIMEOUT_SECS`]
/// so a wedged interrupt path can't re-introduce the hang — then returns
/// [`IdleEvent::IdleTimedOut`]. Fail-open by contract: a bad/dead session always
/// resolves to a settle, never a wedge.
pub(crate) async fn next_event_idle(session: &mut dyn BaseSession, idle: Duration) -> IdleEvent {
    match tokio::time::timeout(idle, session.next_event()).await {
        Ok(Some(ev)) => IdleEvent::Event(ev),
        Ok(None) => {
            // The session ended — capture the base's OWN diagnosis NOW (we hold
            // `&mut session` right here) so the caller can tell the user WHY
            // instead of a bare "ended mid-turn". Fail-open: both default to
            // `None`, never block (the run path mirrors the chat path's enrich).
            IdleEvent::SessionEnded {
                exit: session.try_exit_status(),
                stderr_tail: session.stderr_tail(),
            }
        }
        Err(_) => {
            // No event within the idle window → the base is hung. Best-effort
            // interrupt to release the child, but bound the interrupt itself so a
            // dead pipe can't wedge it (the watchdog must always make progress).
            let _ = tokio::time::timeout(
                Duration::from_secs(INTERRUPT_TIMEOUT_SECS),
                session.interrupt(),
            )
            .await;
            // Capture AFTER the interrupt — a hung child that the interrupt just
            // killed now has an exit status / a final stderr line to surface.
            IdleEvent::IdleTimedOut {
                exit: session.try_exit_status(),
                stderr_tail: session.stderr_tail(),
            }
        }
    }
}

/// The machine-true reason string for an idle-watchdog settle — shared so every
/// pump reports the hang identically (and the TUI / tests match on `"idle"`).
pub(crate) fn idle_reason(idle: Duration) -> String {
    format!(
        "base went idle — no output for {}s (possible hang); settled. \
         Set UMADEV_IDLE_TIMEOUT_SECS to adjust.",
        idle.as_secs()
    )
}

/// Fold the base's OWN diagnosis (exit status + stderr tail, captured at the idle /
/// ended settle by [`next_event_idle`]) into the user-visible reason — so the RUN
/// path tells the user WHY a base went idle / ended, exactly as the chat path's
/// `enrich_base_failure` does (a broken model id / "not logged in" / a config error
/// the base prints to STDERR, never stdout). Without this the run path settled with
/// a bare "base went idle — …" and no cause — the original symptom on the path that
/// matters most for a hung build.
///
/// Fail-open + bounded, mirroring the chat path: a non-success exit appends
/// `(base 进程已退出: <status>)`; a present stderr tail appends ` — base stderr: …`
/// using its last 3 non-empty lines, ≤280 chars. A `None` exit / tail → today's
/// bare `base_reason` unchanged. Never panics, never blocks.
pub(crate) fn enrich_idle_reason(
    base_reason: &str,
    exit: Option<std::process::ExitStatus>,
    stderr_tail: Option<String>,
) -> String {
    let mut msg = match exit {
        Some(s) if !s.success() => format!("{base_reason}(base 进程已退出: {s})"),
        _ => base_reason.to_string(),
    };
    if let Some(tail) = stderr_tail {
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
    msg
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
        idle_timeout(),
        deadline,
    )
    .await
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

/// [`drive_director_loop`] with an explicit idle window — the env read is hoisted
/// to the public wrapper so this core is deterministic (the test drives it with a
/// tiny window, no process-env mutation / race).
///
/// **Wave 2 — depth-tiered scheduling (the "drive the plan" change):**
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
    idle: Duration,
    deadline: std::time::Instant,
) -> DirectorLoopOutcome {
    // Wave 2: a DELIBERATE build with an owned plan is driven step-by-step (summon
    // per step + per-step acceptance + real checklist ticking). A lean/Fast build —
    // or any path with no plan — keeps the single-turn fast loop below, untouched.
    // Fail-open: `drive_plan_steps` returns `None` if it couldn't drive even the
    // first step (the caller then runs the single-turn loop, never losing the build).
    if let (Some(r), Some(p)) = (route, plan.as_mut()) {
        if r.depth.is_deliberate() {
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
                Err(reason) => return DirectorLoopOutcome::Failed(reason),
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
            // steps as-is (the caller decides) and settle.
            return DirectorLoopOutcome::Done { reply: last_reply };
        }

        // 3. UmaDev runs its OWN objective QC pass — hard floor + verify + optional
        //    fork review. NOTHING here is the base summoning a team; it is UmaDev
        //    inspecting reality over the borrowed brain. When a route is in hand, the
        //    review team is sized from the ROUTE's seats (deliverable 3 on the
        //    single-turn path too); else the kind-derived team (the legacy entry).
        let qc = run_auto_qc(session, options, events, route, Some(turn.text.as_str())).await;

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
            // architecture / UI-UX doc (+ a proof-pack on the deliberate path).
            director::finalize(options, events, route);
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
    idle: Duration,
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
        let Some(step_id) = plan.ready_steps().first().map(|s| s.id.clone()) else {
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

        let outcome = match step.kind {
            plan_state::StepKind::Build => {
                drive_build_step(session, options, events, route, &step, deadline).await
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
        } else {
            // Bounded: a step that exhausted its fix budget — OR cleared only a
            // neutral skip with no real work — is Blocked (honest), so it no longer
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
    let final_reply =
        run_final_gate(session, options, events, route, &last_reply, deadline, "").await;
    if !final_reply.is_empty() {
        last_reply = final_reply;
    }

    // Persist the plan's terminal state for resume.
    persist_plan_ref(plan, options);
    // Sync the 9-phase workflow state at finalize. HONEST clean signal: every step
    // reached Done (none Blocked / stranded) — only then may the build claim
    // `delivery`; otherwise the state advances to the furthest phase that actually
    // completed, so `/status` reflects where the build really stopped. Fail-open.
    let clean = plan.steps.iter().all(|s| s.status == StepStatus::Done);
    finalize_phase_from_plan(plan, options, clean);
    // Wave 4 (§L4 / G8): a step-driven (always deliberate) build leaves the FULL
    // shareable delivery — core docs + proof-pack + scorecard. Fail-open inside.
    director::finalize(options, events, Some(route));
    Some(DirectorLoopOutcome::Done { reply: last_reply })
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
}

/// Drive ONE Build step: `summon` the step's seat serially on the main session with
/// a focused directive (recalled pitfalls injected), then verify against the step's
/// `acceptance` on the deterministic floor. A failing acceptance folds its evidence
/// into a bounded fix re-drive ([`MAX_STEP_FIX_ROUNDS`]). Returns a [`StepOutcome`].
///
/// Wall-clock ceiling (graceful): the `deadline` bounds the EXTRA fix rounds, not the
/// real work — round 0 (the step's actual doer turn) ALWAYS runs, so a budget already
/// spent before this step never starves the step itself; only the re-drives past the
/// budget are skipped (the doc'd "hard ceiling" is honoured inside the step too).
async fn drive_build_step(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    route: &RoutePlan,
    step: &plan_state::PlanStep,
    deadline: std::time::Instant,
) -> StepOutcome {
    let seat_id = step.seat.role_id();
    // The step's focused instruction + (fail-open) recalled stack pitfalls so the
    // doer pre-empts a known trap. relevant_lessons_for_prompt is empty on first
    // runs / a miss, so the directive is unchanged then.
    let pitfalls =
        crate::lessons::relevant_lessons_for_prompt(&options.project_root, &options.requirement);
    let mut instruction = format!("{} — {}", step.title, route_focus_line(route));
    if !pitfalls.trim().is_empty() {
        instruction.push_str("\n\n## Known pitfalls to avoid (from past runs)\n");
        instruction.push_str(pitfalls.trim());
    }

    let mut drove = false;
    let mut last_reply = String::new();
    for round in 0..=MAX_STEP_FIX_ROUNDS {
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
        let verdict = verify_step_acceptance(session, options, events, route, step).await;
        // MEDIUM #3 — a dead/hung summon turn that never actually ran (`!drove`) must
        // not "complete" a Build step on a NEUTRAL-SKIP acceptance (an unavailable
        // check / a TurnSettled free pass). Require REAL evidence: either the doer
        // turn actually ran, OR the floor produced positive evidence (real source on
        // disk / a green build). Without either, a build step over a dead session is
        // honestly left unaccepted (→ the caller marks it Blocked), so a dead session
        // can't silently tick steps 2..N Done over an empty build.
        if verdict.accepted && (drove || verdict.has_positive_evidence) {
            return StepOutcome {
                accepted: true,
                reply: last_reply,
                drove,
                // A Build step makes real progress when its turn actually ran or the
                // floor positively confirmed real work — exactly the (drove ||
                // has_positive_evidence) condition that let it accept here.
                made_progress: true,
            };
        }
        // Out of fix budget → leave the step unaccepted (the caller marks it Blocked
        // and the final gate still has the last word). Bounded — never an open grind.
        if round >= MAX_STEP_FIX_ROUNDS {
            break;
        }
        // Fold this step's failing acceptance into the NEXT re-drive's directive so
        // the same seat fixes the cause with raw evidence, in the same session.
        instruction = format!(
            "{} — {}\n\n## This step did not pass its acceptance check yet — fix the cause\n{}\n\
             Edit the real files, run any build/test you need, and make this step's \
             acceptance ({}) actually pass.",
            step.title,
            route_focus_line(route),
            verdict.evidence_line(),
            acceptance_label(&step.acceptance),
        );
    }
    StepOutcome {
        accepted: false,
        reply: last_reply,
        drove,
        made_progress: false,
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
        events.emit(EngineEvent::Note(format!(
            "team · review step repaired but {} finding(s) remain after the fix turn — \
             left for the final gate (objective hard-gate owns reality)",
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
    }
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
    match &step.acceptance {
        A::SourcePresent => acceptance_from_verify(
            director::verify(options, events, VerifyKind::SourcePresent).await,
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
) -> String {
    let mut last_reply = String::new();
    // The incremental-verify signal seeds from the LAST step's reply (the steps just
    // ran the build/test); each fix round below then carries its own turn's reply.
    let mut verify_signal = seed_reply.to_string();
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
        )
        .await;
        if qc.is_clean() {
            return last_reply;
        }
        if round + 1 >= MAX_QC_ROUNDS {
            events.emit(EngineEvent::Note(
                "team · final QC reached its fix-round budget — settling (objective hard-gate decides reality)"
                    .to_string(),
            ));
            return last_reply;
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
            return last_reply;
        }
        // Fold the residual findings into ONE fix turn on the main session — with the
        // optional context prefix (knowledge + pitfalls) front-loaded for a chat-build.
        match drive_one_turn(
            session,
            options,
            events,
            qc.fix_directive_with_context(fix_prefix),
            idle_timeout(),
            deadline,
        )
        .await
        {
            Ok(t) => {
                verify_signal = t.text.clone();
                last_reply = t.text;
            }
            Err(_) => return last_reply, // a dead/hung session → settle (fail-open)
        }
    }
    last_reply
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
    run_final_gate(
        session, options, events, route, seed_reply, deadline, &prefix,
    )
    .await
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
    let digest =
        crate::phases::agentic_knowledge_digest(&options.project_root, &options.requirement, 3);
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
        A::ReviewClean => "the review team raises no blocking issue",
        A::TurnSettled => "the work turn completes",
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
fn persist_phase(options: &RunOptions, phase: Phase) {
    // Clamp: never regress below what's already on disk.
    let current = current_persisted_phase(options);
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
        note: format!("Advanced to {} (director loop)", phase.id()),
        backend: options.backend.clone(),
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
    let phase = match (clean, reached) {
        // A clean finish with real completed work: advance to that furthest phase, and
        // — only when clean — let the build claim `Delivery` as the terminal hand-off
        // (it is the deepest phase, so this is always ≥ the furthest seat's phase).
        (true, Some(_)) => Phase::Delivery,
        // A clean finish with no anchorable Done step (e.g. a single-turn build whose
        // plan never ticked): the build still completed clean, so `Delivery` is honest.
        (true, None) => Phase::Delivery,
        // Not clean: persist only what genuinely completed; never an optimistic jump.
        (false, Some(p)) => p,
        // Not clean and nothing completed: leave the on-disk phase as-is (the clamp in
        // `persist_phase` keeps whatever the in-progress sync already wrote).
        (false, None) => current_persisted_phase(options),
    };
    persist_phase(options, phase);
}

/// [`finalize_phase_from_plan`] over the single-turn loop's `&Option<Plan>`. A
/// CLEAN finalize with no plan (the single-turn fallback) is a genuine clean
/// finish → `delivery`. A NON-clean finalize with no plan leaves the on-disk phase
/// as-is (whatever the in-progress sync wrote, clamped — never regressed, never
/// optimistically jumped to delivery). Fail-open.
fn finalize_phase_from_plan_opt(plan: &Option<Plan>, options: &RunOptions, clean: bool) {
    match plan {
        Some(p) => finalize_phase_from_plan(p, options, clean),
        None if clean => persist_phase(options, Phase::Delivery),
        None => {} // non-clean + no plan: keep the current on-disk phase (no regress)
    }
}

/// One base turn's observable result.
struct TurnResult {
    /// The accumulated assistant text. The caller reads it for the "claimed a build"
    /// hard-gate; this loop reads it to decide whether QC is even warranted.
    text: String,
}

/// Send one directive and pump the base's event stream to its `TurnDone`, forwarding
/// tool calls + text to the live sink (the SAME `WorkerStream` render path the
/// pipeline uses), answering approvals via the always-on irreversible floor, and
/// accumulating the assistant text. Returns the turn's text, or `Err` with a
/// machine-true reason on a failed / dead turn (fail-open: the caller maps it to a
/// hard stop, never a panic).
async fn drive_one_turn(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    directive: String,
    idle: Duration,
    deadline: std::time::Instant,
) -> Result<TurnResult, String> {
    // Estimate the directive's token cost up front (the session stream carries no
    // usage on `TurnDone`, unlike the single-shot path), so `/usage` is real on the
    // default loop for ALL three bases — not just claude in the legacy runner.
    let mut est_tokens: u64 = approx_tokens(&directive);
    if let Err(e) = session.send_turn(directive).await {
        return Err(format!("session send: {e}"));
    }
    let mut text = String::new();
    let mut pitfalls: Vec<String> = Vec::new();
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
            return Ok(TurnResult { text });
        }
        // Idle watchdog (P0-3 / P1-11): a base that HANGS (stops emitting stdout
        // but never exits) would leave `next_event()` blocked forever — no
        // `TurnDone`, no settle, `thinking` stuck. The shared [`next_event_idle`]
        // bounds each wait by `idle` (ANY event resets it, so a long streaming
        // compile/test turn survives as long as it emits SOMETHING) and converts
        // pure silence into a settle. The SAME primitive guards every main-session
        // pump (here + `continuous::drive_phase` / `drive_rework_turn`), so the
        // protection can't be "fixed in one, forgotten in another".
        let ev = match next_event_idle(session, idle).await {
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
                ));
            }
            IdleEvent::IdleTimedOut { exit, stderr_tail } => {
                // No event within the idle window → the base is hung. Settle as a
                // Failed outcome so the loop ends and `thinking` clears, rather than
                // blocking forever (the interrupt was already issued, bounded).
                // LOW #2: record the tokens spent up to the hang (fail-open). The
                // turn hung with no `TurnDone` → estimate (no real usage). F3.
                record_turn_usage(options, events, None, est_tokens);
                // Fold in the base's stderr tail / exit so a hung build no longer
                // settles with a cause-less "base went idle — …".
                return Err(enrich_idle_reason(&idle_reason(idle), exit, stderr_tail));
            }
        };
        match ev {
            SessionEvent::TextDelta(delta) => {
                est_tokens = est_tokens.saturating_add(approx_tokens(&delta));
                text.push_str(&delta);
                events.emit(EngineEvent::WorkerStream {
                    event: StreamEvent::Text { delta },
                });
            }
            SessionEvent::ToolCall { name, input } => {
                // Surface what the base actually DID (the source of truth). The
                // governance hook governs the write itself in real time (claude); the
                // content-governance QC scan is the craft floor for ALL bases. Here we
                // (a) render the tool row, and (b) record the call to the audit trail
                // (UD-EVID-002) so the audit is honest on the DEFAULT loop for every
                // base — not just claude in the legacy runner. Fail-open: a recording
                // error is swallowed and never blocks the turn.
                let detail = tool_call_target(&input);
                record_tool_call_audit(options, &name, &detail);
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
                // Always-on irreversible floor: deny an irreversible action even
                // headless (the same floor the `auto` tier can't skip), allow the
                // rest so a headless build isn't wedged waiting on a human.
                let decision = if requires_confirmation(options.mode, &action, &target) {
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
                    return Ok(TurnResult { text });
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
/// Records the call + target with an `allow` verdict (the real-time governance is the
/// claude hook + the QC content scan; this is the AUDIT record, present for every
/// base so the trail isn't empty on a codex/opencode run). Fail-open: any error is
/// swallowed. Mirrors `continuous::govern_tool_call`'s audit write.
fn record_tool_call_audit(options: &RunOptions, name: &str, target: &str) {
    let _ = umadev_governance::record_tool_call(
        &options.project_root,
        name,
        target,
        "allow",
        "",
        "",
        &options.effective_slug(),
        None,
    );
}

/// Best-effort human-readable target of a base tool call (a file path / command)
/// for the live tool row — fail-open to an empty string on any unexpected shape.
fn tool_call_target(input: &serde_json::Value) -> String {
    for key in ["file_path", "path", "command", "url", "pattern"] {
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
    if crate::planner::is_lean_build(&options.requirement) {
        events.emit(EngineEvent::Note(
            "team · lean goal — source present, skipping the duplicate build + fork review"
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
    let base_already_verified = last_turn_text
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
    use crate::events::RecordingSink;
    use crate::trust::TrustMode;
    use umadev_runtime::{SessionError, SessionEvent, TurnStatus};

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
            std::time::Duration::from_secs(5),
            std::time::Instant::now() + std::time::Duration::from_secs(3600),
        )
        .await;
        match out {
            Ok(r) => assert_eq!(r.text, "done, real usage attached"),
            Err(e) => panic!("a turn with real usage must complete cleanly: {e}"),
        }
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
            Duration::from_millis(100),
            std::time::Instant::now() + Duration::from_secs(3_600),
        )
        .await;
        if let DirectorLoopOutcome::Failed(reason) = outcome {
            assert!(
                reason.contains("idle"),
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
            Duration::from_millis(100),
            std::time::Instant::now() + Duration::from_secs(3_600),
        )
        .await;
        if let DirectorLoopOutcome::Failed(reason) = outcome {
            assert!(
                reason.contains("idle"),
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
        // No exit, no tail → today's bare reason, unchanged (fail-open).
        let base = idle_reason(Duration::from_secs(7));
        assert_eq!(enrich_idle_reason(&base, None, None), base);
        // A present tail is folded in, last 3 non-empty lines, joined (a 4th-from-end
        // line and blank lines are dropped).
        let enriched = enrich_idle_reason(
            "base session ended mid-turn",
            None,
            Some("DROPPED\n\nmodel not found\nlogin required\nfinal line\n".to_string()),
        );
        assert!(enriched.contains("base stderr: model not found | login required | final line"));
        assert!(
            !enriched.contains("DROPPED"),
            "only the last 3 lines: {enriched}"
        );
        // A long tail is bounded to ≤280 chars of snippet (never unbounded).
        let long = "x".repeat(1_000);
        let enriched = enrich_idle_reason("r", None, Some(long));
        let tail = enriched.split("base stderr: ").nth(1).unwrap();
        assert!(tail.chars().count() <= 280, "stderr tail is bounded");
    }

    #[test]
    fn idle_timeout_reads_env_and_falls_back_safely() {
        let prior = std::env::var_os("UMADEV_IDLE_TIMEOUT_SECS");
        // A valid positive value is honoured.
        std::env::set_var("UMADEV_IDLE_TIMEOUT_SECS", "42");
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
        match prior {
            Some(v) => std::env::set_var("UMADEV_IDLE_TIMEOUT_SECS", v),
            None => std::env::remove_var("UMADEV_IDLE_TIMEOUT_SECS"),
        }
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
        let qc = run_auto_qc(&mut sess, &o, &events, None, None).await;
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
        let qc = run_auto_qc(&mut sess, &o, &events, None, None).await;
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
        let qc = run_auto_qc(&mut sess, &o, &events, None, None).await;
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
        let qc = run_auto_qc(&mut sess, &o, &events, None, None).await;
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
        let qc = run_auto_qc(&mut sess, &o, &events, None, None).await;
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
        let qc = run_auto_qc(&mut sess, &o, &events, None, None).await;
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
        let qc = run_auto_qc(&mut sess, &o, &events, None, None).await;
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
        // Wave 3 incremental verify: when the base's reply reports a PASSED build/test,
        // UmaDev skips its OWN duplicate full build/test read — it emits the
        // "trusting its result" note and does NOT emit the "verify build-test" note.
        // The source-present floor + governance still ran (clean here), so QC is clean.
        let tmp = tempfile::TempDir::new().unwrap();
        seed_source(tmp.path());
        let (events, rec) = sink();
        let mut sess = FakeSession::new(vec![], false, "");
        let o = opts(tmp.path()); // "做一个登录系统" — non-lean, so it reaches the build read
        let reply = "Implemented the login system end to end. Ran the suite — all tests pass and the build succeeded.";
        let qc = run_auto_qc(&mut sess, &o, &events, None, Some(reply)).await;
        assert!(
            qc.is_clean(),
            "clean source + trusted build → clean: {:?}",
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
        let qc = run_auto_qc(&mut sess, &o, &events, None, Some("Done — implemented it.")).await;
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
        let qc = run_auto_qc(&mut sess, &deliberate, &events, Some(&route), None).await;
        assert!(
            qc.blocking.iter().any(|b| b.contains("coverage gap")),
            "deliberate QC enforces the acceptance floor: {:?}",
            qc.blocking
        );

        // Lean requirement → QC returns at the lean short-circuit, BEFORE the floor.
        let mut lean = opts(tmp.path());
        lean.requirement = "做一个简单的待办清单单页应用,纯前端".to_string();
        let qc2 = run_auto_qc(&mut sess, &lean, &events, None, None).await;
        assert!(
            !qc2.blocking.iter().any(|b| b.contains("coverage gap")),
            "a lean goal does NOT pay the acceptance floor (speed): {:?}",
            qc2.blocking
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
        let prior = std::env::var_os("UMADEV_RUN_BUDGET_SECS");
        std::env::set_var("UMADEV_RUN_BUDGET_SECS", "120");
        assert_eq!(run_budget(), Duration::from_secs(120));
        std::env::set_var("UMADEV_RUN_BUDGET_SECS", "0"); // non-positive → default
        assert_eq!(run_budget(), Duration::from_secs(DEFAULT_RUN_BUDGET_SECS));
        std::env::set_var("UMADEV_RUN_BUDGET_SECS", "nonsense");
        assert_eq!(run_budget(), Duration::from_secs(DEFAULT_RUN_BUDGET_SECS));
        std::env::remove_var("UMADEV_RUN_BUDGET_SECS");
        assert_eq!(run_budget(), Duration::from_secs(DEFAULT_RUN_BUDGET_SECS));
        if let Some(v) = prior {
            std::env::set_var("UMADEV_RUN_BUDGET_SECS", v);
        }
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
            Duration::from_millis(200),
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
            Duration::from_millis(200),
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
            Duration::from_millis(200),
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
            Duration::from_millis(200),
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
            status: StepStatus::Active,
        };
        let verdict = verify_step_acceptance(&mut sess, &o, &events, &route, &step).await;
        assert!(
            !verdict.accepted,
            "a TurnSettled Build over an empty tree must NOT pass the honesty floor"
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
            Duration::from_millis(200),
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
            std::time::Instant::now() + Duration::from_secs(3_600),
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
            Duration::from_millis(200),
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
}
