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

use umadev_runtime::{ApprovalDecision, BaseSession, SessionEvent, StreamEvent, TurnStatus};

use crate::director::{self, ReviewResult, VerifyKind, VerifyResult};
use crate::events::{EngineEvent, EventSink};
use crate::plan_state::{self, Plan, StepStatus};
use crate::router::RoutePlan;
use crate::runner::RunOptions;
use crate::trust::requires_confirmation;

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
/// one simply times out and the pump settles regardless.
const INTERRUPT_TIMEOUT_SECS: u64 = 5;

/// The result of ONE idle-guarded `next_event()` wait — the shared primitive
/// every main-session event pump uses so the "stops emitting but never exits"
/// hang can NEVER wedge a pump (the P0-3 / P1-11 zero-stall fix).
pub(crate) enum IdleEvent {
    /// The base emitted an event (the idle timer is reset by the caller looping).
    Event(SessionEvent),
    /// `next_event()` returned `None` — the session ended (process dead / EOF).
    /// The pump treats this as a failed turn (fail-open, no panic).
    SessionEnded,
    /// No event arrived within the idle window — the base is hung. The watchdog
    /// has already issued a best-effort (bounded) `interrupt()`; the pump settles
    /// the turn as a failure so `thinking` clears rather than blocking forever.
    IdleTimedOut,
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
        Ok(None) => IdleEvent::SessionEnded,
        Err(_) => {
            // No event within the idle window → the base is hung. Best-effort
            // interrupt to release the child, but bound the interrupt itself so a
            // dead pipe can't wedge it (the watchdog must always make progress).
            let _ = tokio::time::timeout(
                Duration::from_secs(INTERRUPT_TIMEOUT_SECS),
                session.interrupt(),
            )
            .await;
            IdleEvent::IdleTimedOut
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

    // 2. Synthesise + persist + post the owned plan. Fail-open at every step: a
    //    `None` plan (offline / no fork / unparseable) simply means no checklist —
    //    the build loop below runs exactly as it does today. Plan synthesis runs on
    //    a READ-ONLY fork (single-writer preserved); it never writes the workspace.
    let plan = match route {
        Some(r) => synthesize_and_post_plan(session, options, events, r).await,
        None => None,
    };

    // Read the idle watchdog window + the wall-clock build budget ONCE at the
    // boundary (not per-wait), so a mid-run env flip can't race the in-flight turns.
    // The deadline is a GRACEFUL ceiling — the loop winds down (final gate on what's
    // built) rather than aborting mid-write.
    drive_director_loop_with_idle(
        session,
        options,
        events,
        first_directive,
        plan,
        route,
        idle_timeout(),
        std::time::Instant::now() + run_budget(),
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
) -> Option<Plan> {
    // A plan is warranted whenever there's a BUILD to make visible — every Build
    // route, even a lean single-page one, gets a (proportionally short) plan so the
    // user SEES the director think, not just a deliberate/deep one. A fast chat /
    // explain / quick-edit needs no DAG (and would just pay a fork round-trip for
    // nothing).
    if !(matches!(route.class, crate::router::RouteClass::Build) || route.depth.is_deliberate()) {
        return None;
    }
    let plan = plan_state::synthesize_plan(session, options, &options.requirement, route).await?;
    // Persist best-effort; a failed write is ignored (fail-open — never blocks).
    let _ = plan_state::save(&plan, &options.project_root);
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
        let turn = match drive_one_turn(session, options, events, next_directive, idle).await {
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
            return DirectorLoopOutcome::Done { reply: last_reply };
        }

        // 6. Fold the QC findings into ONE fix directive and feed it back over the
        //    USB channel for another build pass → re-QC.
        next_directive = qc.fix_directive();
    }

    // Loop fell through (exhausted the bounded rounds) — persist the plan's final
    // state for resume; reality is the caller's hard-gate.
    persist_plan(&plan, options);
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
        let step = plan
            .steps
            .iter()
            .find(|s| s.id == step_id)
            .cloned()
            .expect("ready id resolves");
        plan.mark(&step_id, StepStatus::Active);
        events.emit(EngineEvent::plan_step_status(
            step_id.clone(),
            step.title.clone(),
            StepStatus::Active,
        ));
        persist_plan_ref(plan, options);

        let (accepted, reply, drove) = match step.kind {
            plan_state::StepKind::Build => {
                drive_build_step(session, options, events, route, &step).await
            }
            plan_state::StepKind::Review => {
                drive_review_step(session, options, events, route, &step).await
            }
        };
        if !reply.is_empty() {
            last_reply = reply;
        }

        // Fail-open bail: if the FIRST step is a Build that could not drive a single
        // turn (a dead session on the very first doer turn), the base can't be
        // scheduled — return None so the caller runs the single end-to-end turn
        // rather than silently marking a plan "done" over an empty build. A first
        // Review step that no-ops (an empty team) does NOT bail: there's simply
        // nothing to review yet, and the next (Build) step still gets its chance.
        if transitions == 1 && step.kind == plan_state::StepKind::Build && !drove {
            return None;
        }

        let status = if accepted {
            StepStatus::Done
        } else {
            // Bounded: a step that exhausted its fix budget is Blocked (honest), so
            // it no longer gates dependents but the plan records the gap. The final
            // QC gate + the caller's hard-gate still decide overall reality.
            StepStatus::Blocked
        };
        plan.mark(&step_id, status);
        events.emit(EngineEvent::plan_step_status(step_id, step.title, status));
        persist_plan_ref(plan, options);
    }

    // Final whole-build QC gate — the SAME objective pass the single-turn loop runs
    // as its last word (source-present hard floor + content governance + optional
    // review), so a step-driven build is held to the identical floor. Advisory: it
    // does not re-drive here (each step was already verified); it folds any residual
    // finding into ONE last fix turn, bounded, then settles. This guarantees a
    // step-driven build is never held to a WEAKER bar than the single-turn build.
    let final_reply = run_final_gate(session, options, events, route, &last_reply).await;
    if !final_reply.is_empty() {
        last_reply = final_reply;
    }

    // Persist the plan's terminal state for resume.
    persist_plan_ref(plan, options);
    // Wave 4 (§L4 / G8): a step-driven (always deliberate) build leaves the FULL
    // shareable delivery — core docs + proof-pack + scorecard. Fail-open inside.
    director::finalize(options, events, Some(route));
    Some(DirectorLoopOutcome::Done { reply: last_reply })
}

/// Drive ONE Build step: `summon` the step's seat serially on the main session with
/// a focused directive (recalled pitfalls injected), then verify against the step's
/// `acceptance` on the deterministic floor. A failing acceptance folds its evidence
/// into a bounded fix re-drive ([`MAX_STEP_FIX_ROUNDS`]). Returns
/// `(accepted, last_reply, drove_at_least_one_turn)`.
async fn drive_build_step(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    route: &RoutePlan,
    step: &plan_state::PlanStep,
) -> (bool, String, bool) {
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
        // `instruction` carries the focused task on round 0 and is rewritten with
        // the failing acceptance evidence on each re-drive (see the loop tail).
        let summoned = director::summon(
            session,
            options,
            events,
            seat_id,
            &instruction,
            director::SummonMode::Serial,
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
        if verdict.accepted {
            return (true, last_reply, drove);
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
    (false, last_reply, drove)
}

/// Drive ONE Review step: fork the cross-review team (read-only) over the current
/// blackboard. A review step is "accepted" when no seat raises a blocking finding;
/// blocking findings fold into ONE bounded fix turn on the MAIN session (the doer
/// repairs), then we re-read. Returns `(accepted, last_reply, drove)`.
async fn drive_review_step(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    route: &RoutePlan,
    step: &plan_state::PlanStep,
) -> (bool, String, bool) {
    let _ = step;
    // Wave 2 deliverable 3: size the review team from the ROUTE's seats (the seats
    // the router already chose for this turn), not from a re-derived requirement
    // classification. An empty route team → no cross-review (the floor stands).
    let review = director::review_with_seats(session, options, events, &route.team).await;
    if !review.has_blocking() {
        return (true, String::new(), review.seats > 0);
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
    let drove = crate::continuous::drive_rework_turn(session, options, events, directive).await;
    // A review step is advisory: after one bounded repair we accept it (the final
    // QC gate + hard-gate still own reality). This keeps the schedule moving while
    // never letting an LLM verdict drive termination.
    (true, String::new(), drove)
}

/// The outcome of verifying one step against its declared acceptance.
struct StepVerdict {
    /// Whether the step's deterministic acceptance check passed (or was a neutral
    /// skip — an unavailable check is NOT a failure, fail-open).
    accepted: bool,
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
            acceptance_from_verify(director::verify(options, events, VerifyKind::BuildTest).await)
        }
        A::Contract => {
            let src = director::verify(options, events, VerifyKind::SourcePresent).await;
            if src.available && !src.passed {
                return acceptance_from_verify(src);
            }
            acceptance_from_verify(director::verify(options, events, VerifyKind::Contract).await)
        }
        A::ReviewClean => {
            // Route-team-aware (deliverable 3): the review seats come from the route.
            let review = director::review_with_seats(session, options, events, &route.team).await;
            StepVerdict {
                // Advisory: a review-clean step is accepted unless a seat blocks —
                // and even then the final deterministic gate, not this verdict, owns
                // overall termination. No team convened ⇒ accept (nothing to review).
                accepted: !review.has_blocking(),
                evidence: review.blocking.clone(),
            }
        }
        A::TurnSettled => {
            // The weakest criterion: the work turn settled. Still honour the
            // source-present honesty floor for a Build step so "claimed done, wrote
            // nothing" never slips through.
            if step.kind == plan_state::StepKind::Build {
                return acceptance_from_verify(
                    director::verify(options, events, VerifyKind::SourcePresent).await,
                );
            }
            StepVerdict {
                accepted: true,
                evidence: Vec::new(),
            }
        }
    }
}

/// Fold a [`VerifyResult`] into a [`StepVerdict`]: an unavailable (skipped) check is
/// a NEUTRAL accept (fail-open — never a false failure); a passed check accepts; a
/// real failure rejects, carrying the evidence for the fix directive.
fn acceptance_from_verify(r: VerifyResult) -> StepVerdict {
    StepVerdict {
        accepted: !r.available || r.passed,
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
async fn run_final_gate(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    route: &RoutePlan,
    seed_reply: &str,
) -> String {
    let mut last_reply = String::new();
    // The incremental-verify signal seeds from the LAST step's reply (the steps just
    // ran the build/test); each fix round below then carries its own turn's reply.
    let mut verify_signal = seed_reply.to_string();
    for round in 0..MAX_QC_ROUNDS {
        // The final gate sizes its review team from the ROUTE (deliverable 3). Pass
        // the freshest reply so the build/test read is skipped when the base already
        // ran it green (Wave 3 incremental verify).
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
        // Fold the residual findings into ONE fix turn on the main session.
        match drive_one_turn(session, options, events, qc.fix_directive(), idle_timeout()).await {
            Ok(t) => {
                verify_signal = t.text.clone();
                last_reply = t.text;
            }
            Err(_) => return last_reply, // a dead/hung session → settle (fail-open)
        }
    }
    last_reply
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

/// A short focus line appended to each step directive so the doer knows the overall
/// goal + the build's depth (proportional craft) without re-priming the whole
/// requirement every step (the base already holds it in the continuous session).
fn route_focus_line(route: &RoutePlan) -> String {
    format!(
        "this is one step of the overall build (depth: {}). Do THIS step's slice now \
         with real files on disk; the rest of the plan is handled separately.",
        route.depth.as_str()
    )
}

/// Persist a `&Plan` (the step-driver holds `&mut Plan`, not `Option<Plan>`).
/// Best-effort + fail-open: a write error is ignored, never blocks the schedule.
fn persist_plan_ref(plan: &Plan, options: &RunOptions) {
    let _ = plan_state::save(plan, &options.project_root);
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
            IdleEvent::SessionEnded => {
                // `None` = the session ended (process dead / EOF). Per the
                // BaseSession contract, treat as a failed turn — fail-open, no panic.
                return Err("base session ended mid-turn".to_string());
            }
            IdleEvent::IdleTimedOut => {
                // No event within the idle window → the base is hung. Settle as a
                // Failed outcome so the loop ends and `thinking` clears, rather than
                // blocking forever (the interrupt was already issued, bounded).
                return Err(idle_reason(idle));
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
                events.emit(EngineEvent::WorkerStream {
                    event: StreamEvent::ToolUse { name, detail },
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
                    return Err(format!("session respond: {e}"));
                }
            }
            SessionEvent::TurnDone { status } => match status {
                // Completed / Truncated → accept the turn (the deterministic floor
                // downstream is the real stop signal; forcing a fail here would
                // hard-stop a build that may have produced usable output).
                TurnStatus::Completed | TurnStatus::Truncated => {
                    // Wave 2 deliverable 4: record usage + distil pitfalls on the
                    // DEFAULT loop, for every base. Both fail-open.
                    record_turn_usage(options, est_tokens);
                    capture_turn_pitfalls(options, events, &pitfalls);
                    return Ok(TurnResult { text });
                }
                TurnStatus::Interrupted => return Err("director turn interrupted".to_string()),
                TurnStatus::Failed(reason) => return Err(reason),
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

/// Record one director turn's estimated token usage to `~/.umadev/usage.jsonl` so
/// `/usage` is real on the default loop for all three bases. Fail-open: a zero
/// estimate / a write error is a no-op. Mirrors [`crate::runner::record_usage`].
fn record_turn_usage(options: &RunOptions, est_tokens: u64) {
    record_estimated_usage(&options.backend, est_tokens);
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
        let mut body = String::new();
        for b in &self.blocking {
            body.push_str("- ");
            body.push_str(b);
            body.push('\n');
        }
        format!(
            "An objective check of what you just built surfaced problems that must be \
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
            },
        ]
    }

    /// Write a minimal real source file so the source-present floor passes and QC
    /// moves on to build/test + review (instead of stopping at the hard floor).
    fn seed_source(root: &std::path::Path) {
        std::fs::write(root.join("app.ts"), "export const x = 1;").unwrap();
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
        // Persisted terminal plan is all-Done.
        let loaded = crate::plan_state::load(tmp.path()).expect("plan persisted");
        assert!(loaded
            .steps
            .iter()
            .all(|s| s.status == crate::plan_state::StepStatus::Done));
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
        // focused directive (which would carry a step title + "one step of the
        // overall build").
        assert!(
            sent.iter().any(|d| d.contains("GO")),
            "the build ran the caller's single directive: {sent:?}"
        );
        assert!(
            !sent
                .iter()
                .any(|d| d.contains("one step of the overall build")),
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
}
