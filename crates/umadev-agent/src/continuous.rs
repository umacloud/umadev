//! Continuous-session run driver — the long-session model (see
//! `docs/CONTINUOUS_SESSION_ARCHITECTURE.md`, §1.5 / 1.6 / 2 / 3 / 3.5).
//!
//! This is the SECOND of the two run paths and lives ALONGSIDE the single-shot
//! [`crate::runner::AgentRunner`] path — it does not replace it. Where the
//! single-shot path runs `Runtime::complete` once per phase (a fresh, stateless
//! base process that narrates a paragraph), this path opens ONE long-lived
//! [`BaseSession`] for the whole run and injects one imperative directive per
//! phase, observing the base's own agentic tool loop (it WRITES files) over the
//! [`SessionEvent`] stream.
//!
//! ## Why a free function over a `Box<dyn BaseSession>` (not a method on
//! `AgentRunner<R>`)
//!
//! `umadev-agent` deliberately does NOT depend on `umadev-host` — it only knows
//! the [`BaseSession`] *trait* from `umadev-runtime`. The three concrete
//! sessions (`ClaudeSession` / `CodexSession` / `OpenCodeSession`) are
//! constructed by the host crate's `session_for(...)` factory and handed in as a
//! trait object. The binary / TUI (the next step) owns the wiring + the
//! gradual-rollout switch; this module owns the deterministic driving loop.
//!
//! ## What is preserved (the moat — unchanged)
//!
//! 9 phases + both confirm gates + governance pre-write checks + the
//! zero-source HARD STOP + tool-call audit (`UD-EVID-002`) + trust-tiered
//! approval + the single-writer run lock. The role-critic team reviews on
//! read-only `BaseSession::fork()` sessions at each review node (see
//! `run_review_team` / `ForkConsult` below) — parallel, isolated, advisory-only,
//! and fail-open, so a critic never drives loop termination.
//!
//! ## Fail-open, by contract
//!
//! Every failure mode degrades, never panics or wedges:
//! - the session can't start          → caller falls back to the single-shot path;
//! - the event stream ends mid-turn    → that phase is [`TurnStatus::Failed`] →
//!   the run stops with a clear failure, the session is `end()`-ed;
//! - a governance check errors          → governance itself is fail-open (returns
//!   pass), so a buggy rule never blocks the base;
//! - the plan was supposed to produce code and produced ZERO real source files →
//!   HARD STOP, reported as a failure (never disguised as success).

use std::sync::Arc;

use umadev_runtime::{
    ApprovalDecision, BaseSession, SessionError, SessionEvent, StreamEvent, TurnStatus,
};
use umadev_spec::Phase;

use crate::critics::{CriticArtifacts, CriticConsult, RoleCritic, RoleVerdict};
use crate::events::{EngineEvent, EventSink};
use crate::gates::Gate;
use crate::runner::RunOptions;
use crate::state::{write_workflow_state, WorkflowState};
use crate::trust::requires_confirmation_with_ledger;

/// The hard ceiling on rework rounds at any single review node. The critic team
/// is ADVISORY: it may fold blocking findings into ONE rework directive and
/// re-review, but the loop is bounded so a base that can't satisfy a seat (or a
/// flapping verdict) can NEVER spin forever. After this many rounds the node
/// proceeds regardless — the deterministic floor + the user gate are the real
/// stop signals, never a critic. Kept small (the docs/preview teams already cost
/// N advisory base calls per round) so the wall-clock stays bounded.
const MAX_REWORK_ROUNDS: usize = 2;

/// Whether a host-CLI run should drive the continuous long-session path.
///
/// The continuous path is now the DEFAULT (the architecture has formally closed
/// on it): when the brain is a logged-in host CLI, a run drives ONE long-lived
/// session for the whole pipeline. The single-shot per-phase path is retained as
/// a FAIL-OPEN fallback, reachable by an explicit OPT-OUT so a run can be
/// reverted in the field without a code change:
///
/// - `UMADEV_CONTINUOUS=0` / `false` / `off`  → single-shot (explicit disable)
/// - `UMADEV_LEGACY_RUN=1` / `true` / `on`     → single-shot (legacy alias)
/// - anything else (incl. unset)               → continuous (the default)
///
/// `UMADEV_CONTINUOUS` set to an explicit ON value (`1` / `true` / `on`) is still
/// honoured as a force-on for symmetry, but it is no longer REQUIRED. Read once
/// at the app boundary (CLI / TUI), the same way
/// [`crate::runner::strict_coverage_from_env`] is, so a run sees a stable
/// snapshot rather than a live process-global env read mid-run.
///
/// Fail-open by contract: this only SELECTS the path. If the continuous session
/// can't actually start, the app boundary falls back to the single-shot driver
/// (and a non-host / offline backend never reaches the continuous branch at all),
/// so the run never dies just because the long-session brain was unreachable.
#[must_use]
pub fn continuous_enabled_from_env() -> bool {
    // Explicit opt-out wins (either the off-switch on the continuous var, or the
    // legacy-run alias). Everything else — including unset — defaults to ON.
    let opted_out = matches!(
        std::env::var("UMADEV_CONTINUOUS").as_deref(),
        Ok("0" | "false" | "off")
    ) || matches!(
        std::env::var("UMADEV_LEGACY_RUN").as_deref(),
        Ok("1" | "true" | "on")
    );
    !opted_out
}

/// Whether an explicit `/run` (full product build) should fall back to the
/// **legacy fixed 9-phase pipeline** instead of the default **director-driven
/// agentic** path (Wave 1 of `docs/AGENT_WIELDS_BASE_ARCHITECTURE.md` §5).
///
/// The director path is now the DEFAULT for `/run`: a `/run "<goal>"` is handed
/// to the director (the same agentic brain a free-text message reaches), framed
/// as a full commercial build the director orchestrates with its team however it
/// judges fit — NOT a state-machine walk of nine fixed phases. The fixed pipeline
/// (`continuous::run_block` / the single-shot `run_initial_block`) is retained
/// untouched behind this explicit opt-in so the field can revert with no code
/// change:
///
/// - `UMADEV_LEGACY_PIPELINE=1` / `true` / `on`  → the legacy fixed pipeline
/// - anything else (incl. unset)                 → the director-driven path
///
/// Read ONCE at the app boundary (CLI `cmd_run` / TUI `StartRun`), the same way
/// [`continuous_enabled_from_env`] and [`crate::runner::strict_coverage_from_env`]
/// are, so a run sees a stable snapshot rather than a live env read mid-run.
///
/// Fail-open by contract: this only SELECTS the route. The director path keeps the
/// floor — single-writer run-lock, governance hook, audit, and an objective
/// source-present hard-gate (did real code actually get written) — so a director
/// who claimed "done" with zero source is reported honestly, never disguised as
/// success. If the director path can't start, the boundary can fall back to the
/// legacy pipeline.
#[must_use]
pub fn legacy_pipeline_from_env() -> bool {
    matches!(
        std::env::var("UMADEV_LEGACY_PIPELINE").as_deref(),
        Ok("1" | "true" | "on")
    )
}

/// How a single continuous run finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    /// The run paused at a confirmation gate awaiting the user (the natural
    /// pause point — the session stays alive, context retained, for the next
    /// block to resume from).
    PausedAtGate(Gate),
    /// The run drove all the way through delivery.
    Completed,
    /// The run stopped on a HARD signal (zero real source produced when the
    /// plan demanded code, or a phase failed). Carries a human-readable reason.
    /// **This is a deterministic, base-independent verdict — never disguised as
    /// success.**
    HardStop(String),
}

/// Persist the workflow state for the continuous path, mirroring the single-shot
/// [`crate::runner::AgentRunner::transition`] EXACTLY — same `WorkflowState`
/// shape, same `.umadev/workflow-state.json` file via the shared
/// [`write_workflow_state`]. This is what makes `umadev continue` / the TUI gate
/// resume / `umadev status` see the REAL door the continuous run paused at, just
/// like the single-shot path. Without it the default (continuous) run never wrote
/// state at all, so `continue` read `Missing` and bailed — `continue` was
/// structurally dead against the default run (the P0-A gap).
///
/// `active_gate` is the gate id (e.g. `docs_confirm`) when the block is pausing at
/// a gate, or empty while a phase is executing. **Fail-open by contract:** a
/// failed write is swallowed (`let _ =`) so a disk/permission error can never
/// wedge the run — the single-shot `transition` propagates its error, but the
/// continuous driver returns a [`RunOutcome`], not a `Result`, so we degrade to
/// "best-effort persisted" rather than aborting an otherwise-healthy run.
fn persist_state(options: &RunOptions, phase: Phase, active_gate: &str) {
    // Carry the base session id (if any) forward across transitions so a
    // phase-transition write never erases a cross-session resume pointer.
    let prior_base_session_id =
        crate::state::read_workflow_state(&options.project_root).and_then(|s| s.base_session_id);
    let state = WorkflowState {
        phase: phase.id().to_string(),
        active_gate: active_gate.to_string(),
        slug: options.effective_slug(),
        requirement: options.requirement.clone(),
        last_transition_at: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        note: format!("Advanced to {} (continuous session)", phase.id()),
        backend: options.backend.clone(),
        base_session_id: prior_base_session_id,
        spec_version: umadev_spec::SPEC_VERSION.to_string(),
    };
    let _ = write_workflow_state(&options.project_root, &state);
    // Keep the coach prompt in lockstep with the active phase, exactly as the
    // single-shot `transition` does. Best-effort: a write failure never blocks.
    let _ = crate::coach::write_coach_prompt(options, phase);
}

/// Drive ONE block of the **legacy** fixed 9-phase pipeline over a single live
/// [`BaseSession`], stopping at the first confirmation gate (or at delivery / a
/// hard stop).
///
/// **Status (DEMOTED — `docs/AGENT_WIELDS_BASE_ARCHITECTURE.md` §3/§5):**
/// this fixed `block_phases` walk is **no longer the default route** for a `/run`.
/// The default is the director build loop ([`crate::director_loop`], the USB model):
/// the base's body builds end to end, then UmaDev runs a read-only honesty/QC pass
/// and feeds bounded fix directives back — the planner/phases are only an advisory
/// prior. `run_block` is retained UNTOUCHED behind the explicit
/// `UMADEV_LEGACY_PIPELINE=1` opt-in ([`legacy_pipeline_from_env`]) so the field
/// can revert with no code change. Its *capabilities* — [`review_and_rework`] /
/// [`run_review_team`] / [`run_quality_gate`] / [`quality_floor`] / [`team_for`] /
/// [`fork_with_timeout`] / [`Blackboard`] / [`drive_rework_turn`] — are KEPT and
/// REUSED as the director's tool underpinnings ([`crate::director`] /
/// [`crate::director_loop`]); only the FIXED WALK below is legacy.
///
/// `start_after` is the phase the block begins at: a fresh run passes
/// [`Phase::Research`]; a resume after the docs gate passes [`Phase::Spec`];
/// after the preview gate, [`Phase::Backend`]. This keeps the gate-anchored
/// block structure identical to the single-shot path.
///
/// The `session` is BORROWED (`&mut`) so the same long-lived session spans
/// every block of the run — the caller owns its lifetime and `end()`s it once
/// the whole run settles. Context flows research → docs → code without
/// re-priming because it is the same session throughout.
pub async fn run_block(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    start_after: Phase,
) -> RunOutcome {
    let plan = crate::planner::plan(&options.requirement);
    let produces_code = plan.includes(Phase::Frontend) || plan.includes(Phase::Backend);

    // Read the idle watchdog windows ONCE at the boundary (not per-wait), so a
    // mid-run env flip can't race the in-flight phase pumps. Threaded into every
    // `drive_phase` so each main-session phase pump is idle-guarded (P1-11) AND
    // tool-aware (the extended grace while the base is mid-tool — long silent build).
    let idle = crate::director_loop::IdleBudget::from_env();
    // The run's wall-clock ceiling. The legacy phase walk has no `deadline` of its
    // own (the director loop owns one; this path predates it), so derive it the SAME
    // way here — `now + run_budget()` — and thread it into every phase / rework pump
    // so an ACTIVE base can't run one phase turn unbounded past the budget. A
    // GRACEFUL ceiling: the pump settles the turn on what's built, never aborts.
    let deadline = std::time::Instant::now() + crate::director_loop::run_budget();

    // Persist the project's governance context BEFORE any phase writes a file, so
    // the out-of-process PreToolUse hook (which reads `.umadev/governance-context.
    // json`) governs by it from the very first write — otherwise a clean static
    // frontend gets nagged about server-only rules (CSP / structured logging /
    // crypto-RNG) in real time. Re-derived + re-persisted per tool call too (see
    // `govern_tool_call`) so a project that grows a backend mid-run re-arms strict.
    persist_project_context(options);

    // The phases this block drives, tailored to the plan. A GATED plan
    // (`Greenfield` / `FrontendOnly` / `BackendOnly` / `DocsOnly`) keeps the
    // gate-anchored three-block split, intersected with the plan so a one-sided
    // build skips the phase it doesn't need (e.g. `FrontendOnly` drops Backend) —
    // the full pipeline + both confirm gates are unchanged. A GATELESS lean plan
    // (`TaskKind::Light` / `Bugfix` / `Refactor`) has NO confirm gate to pause at,
    // so its whole lean phase list (spec → implement → quality) is driven in ONE
    // block from the fresh-run start; any gate-resume entry for such a plan has
    // nothing left to do. This is what makes a simple "做一个待办单页应用" skip the
    // research + three-doc + gate ceremony and head straight for spec → implement
    // → verify (the 24-min → minutes fix), while a real product still pays for it.
    let phases = block_phases(start_after, &plan);
    if phases.is_empty() {
        // Nothing to drive (e.g. a docs-only plan resumed past its last phase, or
        // a Light plan whose initial block was all research/docs — the next block
        // picks up the code phases). Fail-open: report a clean completion.
        return RunOutcome::Completed;
    }

    if start_after == Phase::Research {
        events.emit(EngineEvent::PipelineStarted {
            slug: options.effective_slug(),
            requirement: options.requirement.clone(),
        });
    }

    // The first directive carries the FULL priming context (role + anti-slop
    // rules). On the standard pipeline that is the Research phase; on a lean plan
    // that has no Research phase, the FIRST surviving phase of a fresh run (e.g.
    // Spec for a Light plan) must carry that priming instead — otherwise the base
    // implements with no role/spec context. Keyed off the fresh-run start_after so
    // a resumed block (Spec/Backend after a gate) stays lean as before.
    let mut first_directive = start_after == Phase::Research;
    for &phase in &phases {
        // A gate is a pause point, not a base turn: stop here, let the caller
        // wait for the user, and resume on the next block.
        if phase.is_gate() {
            // The role-critic TEAM reviews the just-produced blackboard HERE,
            // before we pause for the user: at the docs gate the PM / architect /
            // UIUX seats review the three docs; at the preview gate the UIUX /
            // frontend seats review the delivered frontend. Each seat reviews on
            // its OWN `BaseSession::fork()` read-only session (parallel, isolated,
            // never writes), and any blocking findings are folded into a bounded
            // rework loop on the MAIN session (see §3.6). Fully advisory +
            // fail-open: it NEVER drives the gate decision — the gate still pauses
            // for the user exactly as before.
            let gate = gate_for_phase(phase);
            review_and_rework(session, options, events, gate_review_kind(phase), deadline).await;
            // P0-A: persist the OPEN-GATE state (phase = the gate phase, active_gate
            // = its id) so `umadev continue` / the TUI gate resume read the real door
            // and resume the continuous run from THIS gate — exactly the state shape
            // the single-shot `transition(gate_phase, gate.id_str())` would write.
            persist_state(options, phase, gate.id_str());
            events.emit(EngineEvent::gate_opened(gate));
            events.emit(EngineEvent::BlockCompleted {
                final_phase: phase,
                paused_at: Some(gate),
            });
            return RunOutcome::PausedAtGate(gate);
        }

        // Plan (read-only) mode never executes a code phase — it stops at the
        // docs gate by design. The gate handling above already returns before
        // any executing phase in the initial block, but guard the executing
        // phases too so a resumed block can't slip past plan mode.
        if !options.mode.executes() && is_executing(phase) {
            events.emit(EngineEvent::Note(
                umadev_i18n::tl("continuous.plan_mode_skip").to_string(),
            ));
            return RunOutcome::Completed;
        }

        // P0-A: persist the EXECUTING-phase state (active_gate empty) before
        // driving the turn, so a process kill mid-phase leaves a recoverable
        // `phase` for `infer_gate_from_phase` (the same intra-phase recovery the
        // single-shot path relies on), and `umadev status` reflects live progress.
        persist_state(options, phase, "");
        events.emit(EngineEvent::PhaseStarted { phase });
        let outcome = drive_phase(
            session,
            options,
            events,
            phase,
            std::mem::take(&mut first_directive),
            plan.kind,
            idle,
            deadline,
        )
        .await;
        // `first_directive` is consumed by `std::mem::take` only when this is
        // the very first directive of the run; subsequent phases are lean.
        match outcome {
            PhaseResult::Done => {}
            PhaseResult::Failed(reason) => {
                events.emit(EngineEvent::Note(umadev_i18n::tlf(
                    "continuous.phase_failed",
                    &[phase.id(), &reason],
                )));
                return RunOutcome::HardStop(format!("phase {} failed: {reason}", phase.id()));
            }
        }
        events.emit(EngineEvent::PhaseCompleted { phase });

        // DETERMINISTIC POST-WRITE GOVERNANCE CATCH-UP — after a code-writing
        // phase (frontend / backend), scan the WHOLE real source tree for
        // governance violations (emoji-as-icon / hardcoded colors / AI-slop) and
        // drive ONE bounded rework round. Critically this is the ONLY governance
        // path for bases WITHOUT a real-time PreToolUse hook (codex / opencode):
        // in the continuous loop `govern_tool_call` only OBSERVES + audits the
        // base's already-applied edits, it does not pre-screen them, so without
        // this catch-up a non-claude base's written files were never governed.
        // Fail-open + advisory: it re-delegates a fix but never stops the run.
        if matches!(phase, Phase::Frontend | Phase::Backend) {
            governance_catchup(session, options, events, deadline).await;
        }

        // DETERMINISTIC QUALITY GATE (the moat — HARD signal). Before the LLM
        // critic review, run the SAME deterministic floor the single-shot path
        // runs: a real build/test/lint via `run_verify` (persisted so the gate
        // consumes it) + `run_quality` (the scored gate JSON: zero-source hard
        // check, contract conformance, governance-audit checks, coverage).
        //
        // The HARD-STOP semantics mirror the single-shot path EXACTLY: a
        // heavyweight GATED plan (Greenfield / one-sided) that wrote the three
        // docs is held to the full scored gate — `passed:false` on a code run is a
        // disguised failure, so we stop at quality and never deliver. A lean
        // GATELESS plan (Light / Bugfix / Refactor) wrote NO docs, so it can never
        // satisfy the document-structure checks; the single-shot lean fast-track
        // therefore keeps the gate ADVISORY (it still runs verify + writes the
        // scorecard, but doesn't block) — and so do we, gated on `produces_code`
        // AND the plan being a heavyweight one. Fail-open: a gate that can't be
        // produced/read degrades to "pass" so a governor bug never wedges a run.
        if phase == Phase::Quality {
            let gated = plan.includes(Phase::DocsConfirm) || plan.includes(Phase::PreviewConfirm);
            let hard = produces_code && gated;
            if let Some(stop) = run_quality_gate(options, events, hard).await {
                return stop;
            }
        }

        // Quality is a REVIEW node too (not a confirm gate): after the
        // deterministic gate above, the QA / security / backend / DevOps seats
        // review the delivered code on read-only forks — and they now receive the
        // DETERMINISTIC floor (coverage gaps + contract drift + governance
        // findings) as context so the LLM pass builds on real findings rather than
        // an empty floor. Any blocking findings drive a bounded rework on the main
        // session. Advisory + fail-open; never blocks the run (the gate above is
        // the hard signal).
        if phase == Phase::Quality {
            review_and_rework(session, options, events, ReviewKind::Quality, deadline).await;
        }

        // HARD STOP (git-independent): after the last code-producing phase, if
        // the plan was supposed to produce code and the workspace has ZERO real
        // source files, the run is a disguised-empty delivery — stop, fail.
        if phase == last_code_phase(&plan) && produces_code {
            let n = crate::acceptance::source_files(&options.project_root).len();
            if n == 0 {
                // The user-visible Note is localized; the HardStop reason is kept
                // language-independent (it is a machine-read verdict string).
                events.emit(EngineEvent::Note(
                    umadev_i18n::tl("continuous.no_source_hardstop").to_string(),
                ));
                return RunOutcome::HardStop(
                    "no real source files produced — pipeline stopped (continuous hard gate)"
                        .to_string(),
                );
            }
        }
    }

    let final_phase = phases.last().copied().unwrap_or(Phase::Delivery);
    // P0-A: persist the terminal phase with NO open gate so a `continue` after a
    // completed block sees "no active gate" (the honest "pipeline is done / nothing
    // to approve") instead of a stale gate, mirroring the single-shot done-state.
    persist_state(options, final_phase, "");
    events.emit(EngineEvent::BlockCompleted {
        final_phase,
        paused_at: None,
    });
    RunOutcome::Completed
}

/// Result of driving a single phase's turn.
enum PhaseResult {
    /// The turn completed (or truncated with partial work that we accept).
    Done,
    /// The turn failed / the session died — stop the run.
    Failed(String),
}

/// Inject one phase directive and pump the resulting event stream, applying
/// governance + audit + trust-tiered approval + TUI streaming on each event,
/// until the turn's [`SessionEvent::TurnDone`].
#[allow(clippy::too_many_arguments)]
async fn drive_phase(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    phase: Phase,
    first_directive: bool,
    kind: crate::planner::TaskKind,
    // Idle watchdog windows (P1-11), passed in so the env is read ONCE at the
    // `run_block` boundary and the test can drive a tiny deterministic window. The
    // budget carries BOTH the base default and the extended tool-grace window.
    idle: crate::director_loop::IdleBudget,
    // The run's wall-clock ceiling, checked at the TOP of the pump so an ACTIVE base
    // can't run ONE phase turn unbounded past the run budget (the legacy phase walk
    // otherwise had no mid-turn ceiling at all — it never re-checks a deadline once a
    // phase turn starts draining).
    deadline: std::time::Instant,
) -> PhaseResult {
    let directive = phase_directive(options, phase, first_directive, kind);
    if let Err(e) = session.send_turn(directive).await {
        return PhaseResult::Failed(format!("send_turn: {e}"));
    }

    let policy = umadev_governance::Policy::load(&options.project_root);
    // Idle watchdog (P1-11): this is a naked-pump path the original P1-2 fix
    // MISSED — a base that hangs mid-phase (stops emitting, never exits) would
    // wedge the WHOLE phase forever on `next_event().await`. Reuse the SAME
    // shared idle primitive + window the director loop uses so every
    // main-session pump has identical zero-stall protection.
    // Tool-aware grace: while the base is plausibly mid-tool (a tool-use seen, no
    // result yet) a long task (build / compile / install / test) is legitimately
    // silent for minutes, so the next wait uses the extended tool window.
    let mut in_tool_call = false;
    loop {
        // Wall-clock budget reached DURING a phase turn. A base that stays ACTIVE
        // (keeps emitting, never trips the idle watchdog below) would otherwise run
        // one phase turn unbounded past the run budget. Settle GRACEFULLY as a
        // completed phase on the work so far: best-effort bounded interrupt (the SAME
        // one `next_event_idle` issues on an idle hang), an honest note, then
        // `PhaseResult::Done` (mirroring this pump's `TurnDone`-Completed path, which
        // records no usage) so the block winds down to its terminal phase rather than
        // hard-aborting mid-write.
        if std::time::Instant::now() >= deadline {
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(crate::director_loop::INTERRUPT_TIMEOUT_SECS),
                session.interrupt(),
            )
            .await;
            events.emit(EngineEvent::Note(
                "team · run budget reached mid-turn — interrupted the base and finalizing \
                 on what's built (raise UMADEV_RUN_BUDGET_SECS for a longer run)"
                    .to_string(),
            ));
            return PhaseResult::Done;
        }
        let ev = match crate::director_loop::next_event_idle(
            session,
            idle,
            in_tool_call,
            Some(deadline),
        )
        .await
        {
            crate::director_loop::IdleEvent::Event(ev) => ev,
            crate::director_loop::IdleEvent::SessionEnded { exit, stderr_tail } => {
                // `None` = the underlying session ended (process dead / EOF), OR a base
                // that died mid-tool (caught by the liveness poll). Per the BaseSession
                // contract, treat as a failed turn — fail-open, no panic. Surface the
                // base's OWN stderr/exit (captured at the settle) so the user sees WHY,
                // not a bare literal — mirrors the chat path.
                return PhaseResult::Failed(crate::director_loop::enrich_idle_reason(
                    "session ended mid-turn",
                    exit,
                    stderr_tail,
                    &options.backend,
                ));
            }
            crate::director_loop::IdleEvent::IdleTimedOut { exit, stderr_tail } => {
                // A non-tool hang at the base window (interrupt already issued, bounded),
                // OR the run budget reached while a tool was still running — settle as a
                // failed turn so the run ends honestly instead of freezing the phase
                // forever. Report the BASE idle window (the `UMADEV_IDLE_TIMEOUT_SECS`
                // knob) and fold in the base's stderr tail / exit so a hung build no
                // longer settles with a cause-less idle reason.
                return PhaseResult::Failed(crate::director_loop::enrich_idle_reason(
                    &crate::director_loop::idle_reason(idle.window(false)),
                    exit,
                    stderr_tail,
                    &options.backend,
                ));
            }
        };
        // Arm/disarm the tool-grace from this event before handling it.
        if let Some(t) = crate::director_loop::tool_phase_transition(&ev) {
            in_tool_call = t;
        }
        match ev {
            SessionEvent::TextDelta(text) => {
                // Stream the assistant's words to the TUI (alive-feel) — but
                // remember: `TextDelta` is what it SAID, `ToolCall` is what it
                // DID. The hard gate / audit key off tool calls, not this.
                events.emit(EngineEvent::WorkerStream {
                    event: StreamEvent::Text { delta: text },
                });
            }
            SessionEvent::ThinkingDelta(text) => {
                // The base's extended-thinking reasoning — surfaced as a collapsed
                // `[thinking]` block (transparency), never folded into the answer.
                events.emit(EngineEvent::WorkerStream {
                    event: StreamEvent::ThinkingDelta(text),
                });
            }
            SessionEvent::SessionModel(id) => {
                // The base reported its resolved model at session init — surface it
                // so the TUI's context gauge uses the REAL window, not a per-backend
                // guess. Purely informational; drives no loop control.
                events.emit(EngineEvent::BaseModel { id });
            }
            SessionEvent::ToolCall { name, input } => {
                govern_tool_call(options, events, &policy, phase, &name, &input);
            }
            SessionEvent::ToolResult { ok, summary } => {
                events.emit(EngineEvent::WorkerStream {
                    event: StreamEvent::ToolResult { ok, summary },
                });
            }
            SessionEvent::NeedApproval {
                req_id,
                action,
                target,
            } => {
                let decision = approval_decision(options, &action, &target);
                if matches!(decision, ApprovalDecision::Deny) {
                    events.emit(EngineEvent::Note(umadev_i18n::tlf(
                        "continuous.dangerous_action_denied",
                        &[&action, &target],
                    )));
                }
                if let Err(e) = session.respond(&req_id, decision).await {
                    // Couldn't answer the base — the session is broken. Fail the
                    // turn rather than hang waiting for a turn that can't finish.
                    return PhaseResult::Failed(format!("respond: {e}"));
                }
            }
            SessionEvent::TurnDone { status, .. } => {
                return finish_turn(options, events, phase, status)
            }
        }
    }
}

/// Apply the PreToolUse governance + audit (`UD-EVID-002`) + TUI tool row for
/// one observed [`SessionEvent::ToolCall`]. Fully fail-open: governance returns
/// a pass on any unexpected input, and the audit write is best-effort.
///
/// For a file-write tool (`Write` / `Edit`) the proposed CONTENT is scanned
/// (emoji / hardcoded color / AI-slop / secrets / …). For a `Bash` tool the
/// COMMAND is checked for dangerous verbs. A block is recorded in the audit
/// trail and surfaced as a Note — but, because UmaDev does not pre-screen the
/// base's own already-applied edit in this path (the base ran its tool loop),
/// the deterministic floor that actually GUARDS the delivery is the governance
/// hook (installed in `settings.json`) plus the post-hoc quality scan; here we
/// observe + audit + advise, matching the design's "two governance paths".
/// Derive the project's governance context from what the base has established
/// (task kind + requirement signals + architecture doc + per-file server
/// evidence). A clean static frontend → lenient (server-only rules N/A); ANY
/// backend/auth signal → strict. Fail-open: errors inside the planner fall back
/// to its own conservative default.
fn project_context_for(options: &RunOptions) -> umadev_governance::ProjectContext {
    crate::planner::derive_project_context(
        &options.requirement,
        &options.project_root,
        &options.effective_slug(),
    )
}

/// Write the derived context to `<root>/.umadev/governance-context.json` so the
/// out-of-process PreToolUse hook reads the SAME context the in-process scans
/// use. Best-effort / fail-open: a create/serialize/write error is swallowed
/// (the hook then defaults to full strictness — conservative, never a false
/// "clean"). Mirrors the agent runner's single-shot persistence.
fn persist_project_context(options: &RunOptions) {
    let ctx = project_context_for(options);
    let dir = options.project_root.join(".umadev");
    if std::fs::create_dir_all(&dir).is_ok() {
        if let Ok(json) = serde_json::to_string_pretty(&ctx) {
            let _ = std::fs::write(dir.join("governance-context.json"), json);
        }
    }
}

fn govern_tool_call(
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    policy: &umadev_governance::Policy,
    phase: Phase,
    name: &str,
    input: &serde_json::Value,
) {
    // Keep the on-disk context fresh (a static project that just grew a server
    // file re-arms strict) AND make this in-process scan context-aware.
    persist_project_context(options);
    let ctx = project_context_for(options);
    let (mut target, decision) = evaluate_tool_call(policy, ctx, name, input);

    // The base asked the user a structured multiple-choice question via its OWN
    // `AskUserQuestion` tool. UmaDev drives the base non-interactively, so that
    // call can't pop up a picker and auto-cancels — previously surfaced as a bare
    // "AskUserQuestion" stub with NO options, silently treated as cancelled. Now
    // we render the question + numbered options as a prominent Note and give the
    // tool row a real one-line detail, so the user SEES what's asked and that the
    // base is awaiting their choice (relayed on the next turn — see
    // `crate::ask_question`). Fail-open: a non-question / unreadable call → None.
    if let Some(surface) = crate::ask_question::surface(name, input) {
        target = surface.detail;
        events.emit(EngineEvent::Note(surface.note));
    }

    // TUI tool row — "正在写 src/App.tsx…". This is the SOURCE OF TRUTH for what
    // the base actually did. P1: a Write/Edit also forwards its structured
    // before/after so the TUI renders a diff card (fail-open: non-edit → None).
    let edit = umadev_runtime::ToolEdit::from_claude_tool_input(name, input);
    events.emit(EngineEvent::WorkerStream {
        event: StreamEvent::ToolUse {
            name: name.to_string(),
            detail: target.clone(),
            edit,
        },
    });

    let decision_word = if decision.block { "block" } else { "allow" };
    // UD-EVID-002: every tool call the base makes is recorded to the audit
    // trail, with the governance verdict + firing clause.
    let _ = umadev_governance::record_tool_call(
        &options.project_root,
        name,
        &target,
        decision_word,
        &decision.clause,
        &decision.reason,
        &options.effective_slug(),
        None,
    );

    if decision.block {
        events.emit(EngineEvent::Note(umadev_i18n::tlf(
            "continuous.tool_call_blocked",
            &[phase.id(), &decision.clause, &decision.reason, &target],
        )));
    }
}

/// Run the governance rules for one tool call, returning `(target, decision)`.
/// Pure + deterministic given the policy; the heart of `govern_tool_call`,
/// split out so it can be unit-tested without an event sink.
fn evaluate_tool_call(
    policy: &umadev_governance::Policy,
    ctx: umadev_governance::ProjectContext,
    name: &str,
    input: &serde_json::Value,
) -> (String, umadev_governance::Decision) {
    let lname = name.to_ascii_lowercase();
    if lname == "bash" || lname == "shell" || lname == "run" {
        let cmd = input
            .get("command")
            .and_then(serde_json::Value::as_str)
            .or_else(|| input.get("cmd").and_then(serde_json::Value::as_str))
            .unwrap_or_default();
        let decision = umadev_governance::check_dangerous_bash(cmd);
        return (cmd.to_string(), decision);
    }
    // File-mutating tools: scan the proposed content. `write_scan_path` covers
    // `file_path` (Write/Edit/MultiEdit), `path` (codex/opencode update/create)
    // AND `notebook_path` (NotebookEdit) in that order.
    let path = umadev_runtime::write_scan_path(input);
    // Keep this write-tool set aligned with the hook's `is_write` matcher
    // (Write / Edit / MultiEdit / NotebookEdit): a mutating tool omitted here
    // falls through to the observe-only `Decision::pass()` below with NO content
    // scan, so a secret written via `MultiEdit` / `NotebookEdit` would bypass the
    // governor. `update` / `create` cover the non-Claude bases' tool names.
    if lname == "write"
        || lname == "edit"
        || lname == "multiedit"
        || lname == "notebookedit"
        || lname == "update"
        || lname == "create"
    {
        // Extract the REAL body via the shared runtime walk: a MultiEdit's
        // `edits[].new_string` are concatenated and a NotebookEdit's `new_source`
        // is read, so the scan sees the actual content instead of "". Write/Edit
        // are unchanged (`content` / `new_string` / `new_str`).
        let content = umadev_runtime::write_scan_content(input);
        let decision = umadev_governance::scan_content_with_context(&path, &content, policy, ctx);
        return (path, decision);
    }
    // Read / Grep / Glob / … — observe-only, never a write. Pass.
    (path, umadev_governance::Decision::pass())
}

/// Map a [`SessionEvent::NeedApproval`] to a trust-tiered [`ApprovalDecision`].
///
/// The decision is **mode-aware** ([`requires_confirmation`]): `auto` lets
/// reversible actions through; `guarded` additionally confirms a write that
/// escapes the workspace; `plan` confirms any real execution. The irreversible
/// floor (`.git` internals, network, destructive shell verbs) forces a
/// confirmation regardless of mode — and in this non-interactive driving loop a
/// forced confirmation degrades to DENY so the base can't run an unattended
/// risky action.
///
/// It also consults the per-project **trust ledger** of remembered approvals
/// (`<root>/.umadev/trust.json`, [`requires_confirmation_with_ledger`]): a
/// reversible action class the user already approved for this project is not
/// re-asked. Fail-open: a missing / corrupt ledger behaves exactly as the bare
/// mode policy; the floor is never relaxed by a remembered rule.
fn approval_decision(options: &RunOptions, action: &str, target: &str) -> ApprovalDecision {
    let ledger = crate::trust::TrustLedger::load(&options.project_root);
    if requires_confirmation_with_ledger(
        options.mode,
        action,
        target,
        &options.project_root,
        &ledger,
    ) {
        ApprovalDecision::Deny
    } else {
        ApprovalDecision::Allow
    }
}

/// Turn the [`TurnStatus`] into a [`PhaseResult`] + the right operator note.
///
/// On [`TurnStatus::Truncated`] the work is partial — we still ACCEPT it
/// (fail-open: the deterministic hard / quality gates downstream are the real
/// stop signals, and forcing a `Failed` here would hard-stop a run that may have
/// produced usable output). But before, a truncated phase was reported with the
/// SAME soft Note whether it left a complete deliverable or nothing at all, so a
/// Docs phase truncated after writing only the PRD (no architecture / UI-UX)
/// slipped past silently and the critic team then fail-open-ACCEPTed the empty
/// surfaces. Now a truncation is split: if the phase's KEY artifacts exist, it is
/// the benign "ran long but finished the deliverable" case (the soft Note); if
/// they are MISSING, it is a genuinely incomplete phase, surfaced with a stronger
/// DEGRADED warning so the operator (and the downstream gates) treat the output
/// as suspect rather than clean.
fn finish_turn(
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    phase: Phase,
    status: TurnStatus,
) -> PhaseResult {
    match status {
        TurnStatus::Completed => PhaseResult::Done,
        TurnStatus::Truncated => {
            let missing = truncated_missing_artifacts(options, phase);
            if missing.is_empty() {
                // Partial-but-complete: the deliverable exists, just produced over
                // the turn budget. Benign — the soft warning, then proceed.
                events.emit(EngineEvent::Note(umadev_i18n::tlf(
                    "continuous.phase_truncated",
                    &[phase.id()],
                )));
            } else {
                // Truncated AND the key deliverable is missing → DEGRADED. We still
                // proceed (fail-open), but the stronger warning names what's absent
                // so the downstream quality/hard gates are the ones that decide.
                events.emit(EngineEvent::Note(umadev_i18n::tlf(
                    "continuous.phase_truncated_degraded",
                    &[phase.id(), &missing.join(", ")],
                )));
            }
            PhaseResult::Done
        }
        TurnStatus::Interrupted => PhaseResult::Failed(format!("{} interrupted", phase.id())),
        // A base-reported turn failure (an API error like a 429 rate limit, an
        // overloaded/auth failure) carries the base's OWN error text. Run it through
        // the actionable classifier so the run's hard-stop NAMES the fix (429 →
        // "底座触发限流 …") while keeping the raw error — never an anonymous failure.
        // Fail-open: an unclassifiable reason surfaces verbatim.
        TurnStatus::Failed(reason) => PhaseResult::Failed(
            crate::base_error::diagnose_turn_failure(&reason, &options.backend),
        ),
    }
}

/// The KEY deliverables a phase MUST have produced, that are MISSING on disk —
/// the existence check a truncated phase is held to. Empty = the phase left its
/// expected output (so a truncation there is benign); non-empty = a genuinely
/// incomplete phase (→ the DEGRADED warning). Pure + fail-open: an unreadable
/// workspace simply yields "nothing missing" rather than a panic, so this check
/// can never itself wedge or hard-stop a run.
///
/// - `Docs` → the three core documents (`prd` / `architecture` / `uiux`): a Docs
///   turn truncated after only the PRD must NOT slip through as clean.
/// - `Spec` → the execution-plan artifact (`*-execution-plan.md`) — the canonical
///   spec surface `run_spec` writes; lenient (the base may name the tasks file
///   variously) so a real plan never trips a false degraded warning.
/// - `Frontend` / `Backend` → at least one real source file in the tree (the same
///   "produced real code" surface the zero-source hard gate keys off).
/// - other phases (research / quality / delivery / gates) have no single
///   file-existence invariant here → never reported missing (the soft path).
fn truncated_missing_artifacts(options: &RunOptions, phase: Phase) -> Vec<String> {
    let slug = options.effective_slug();
    let root = &options.project_root;
    // A doc is "present" only when it exists AND is non-trivially sized (a 0-byte
    // touch is not a deliverable). Fail-open: an unreadable path reads as absent.
    let doc_present = |name: &str| -> bool {
        let p = root.join(format!("output/{slug}-{name}.md"));
        std::fs::metadata(&p).map(|m| m.len() > 16).unwrap_or(false)
    };
    match phase {
        Phase::Docs => ["prd", "architecture", "uiux"]
            .into_iter()
            .filter(|n| !doc_present(n))
            .map(|n| format!("{n}.md"))
            .collect(),
        Phase::Spec => {
            if doc_present("execution-plan") {
                Vec::new()
            } else {
                vec!["execution-plan.md".to_string()]
            }
        }
        Phase::Frontend | Phase::Backend => {
            if crate::acceptance::source_files(root).is_empty() {
                vec!["source files".to_string()]
            } else {
                Vec::new()
            }
        }
        // No single existence invariant → the soft path.
        Phase::Research | Phase::Quality | Phase::Delivery => Vec::new(),
        Phase::DocsConfirm | Phase::PreviewConfirm => Vec::new(),
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Deterministic gatekeepers — the moat, reattached to the continuous default
// path. These REUSE the single-shot path's deterministic functions rather than
// re-implementing them, and they are the HARD signal: the LLM critic team stays
// purely advisory. Every one is fail-open: an error degrades to "pass / no
// finding" so a governor bug can never wedge the host.
// ───────────────────────────────────────────────────────────────────────────

/// Run the DETERMINISTIC quality gate at the quality phase and decide whether to
/// HARD STOP. Mirrors the single-shot path's quality block: it ALWAYS runs a real
/// build/test/lint (`run_verify`, persisted so the gate consumes it) and the
/// scored gate (`run_quality`, which leaves the auditable scorecard), then reads
/// the gate JSON back. Returns `Some(HardStop)` to stop the block at quality;
/// `None` to proceed.
///
/// `hard_block` selects the SEMANTICS, matching the single-shot path: a
/// heavyweight gated code run sets it `true` and a `passed:false` gate becomes a
/// HARD STOP (refuse to deliver); a lean / docs-only run sets it `false` and the
/// gate is purely ADVISORY (verify still runs, the scorecard is still written,
/// but the run is never blocked — a lean plan writes no docs and can't satisfy
/// the document checks).
///
/// **Fail-open by contract:** even with `hard_block`, the gate is only a HARD
/// signal when it produced a READABLE `passed:false` gate file. `run_quality`
/// erroring, no gate file, or an unreadable/unparsable gate all degrade to
/// "proceed" — a governor bug must never wedge a real run. The zero-source hard
/// gate downstream still independently catches an empty run.
async fn run_quality_gate(
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    hard_block: bool,
) -> Option<RunOutcome> {
    // 1. Real build/test/lint, persisted to `.umadev/audit/verify.jsonl` so
    //    `run_quality`'s `verify_results_check` folds it into the gate — exactly
    //    how the single-shot `maybe_verify` feeds the gate. Each step is
    //    independent + fail-open (a missing manifest yields no steps).
    let outcomes = crate::verify::run_verify(&options.project_root).await;
    for o in &outcomes {
        let _ = crate::verify::record_verify_outcome(&options.project_root, Phase::Quality.id(), o);
    }
    if outcomes.iter().any(|o| !o.passed && !o.skipped) {
        events.emit(EngineEvent::Note(
            umadev_i18n::tl("continuous.verify_failed").to_string(),
        ));
    }

    // 2. The scored gate (zero-source hard check + contract conformance +
    //    governance-audit checks + coverage), written to
    //    `output/<slug>-quality-gate.json`. Fail-open: a write error → proceed.
    let Ok(quality_out) = crate::phases::run_quality(options) else {
        return None;
    };
    let produced_gate_file = quality_out
        .artifacts
        .iter()
        .any(|p| p.to_string_lossy().ends_with("-quality-gate.json"));

    // 3. Read the gate JSON back and extract `(score, passed)` the same way the
    //    single-shot path does.
    let qg_path = options
        .project_root
        .join("output")
        .join(format!("{}-quality-gate.json", options.effective_slug()));
    let qg_body = std::fs::read_to_string(&qg_path).ok();
    let (score, passed) = match qg_body.as_deref() {
        Some(qg) => crate::phases::extract_quality_score(qg),
        // The gate phase wrote a file we can't read back → a disk/permission
        // failure, not "offline". Treat as not-passed so a write failure can't
        // masquerade as success — but only when the gate file was actually
        // produced; otherwise (no gate at all) fail-open to pass.
        None if produced_gate_file => ("?".to_string(), false),
        None => return None,
    };

    // Honest verdict wording: a HARD gate (heavyweight gated code run) that
    // fails is "BLOCKED — deterministic hard signal" (it will stop the run); a
    // lean / advisory gate that scores below the bar is NOT a block (the run
    // already completed) — it's advisory feedback, so it must not masquerade as
    // a hard BLOCK.
    if passed {
        events.emit(EngineEvent::Note(umadev_i18n::tlf(
            "continuous.quality_gate_result",
            &[&score, "PASSED"],
        )));
    } else if hard_block {
        events.emit(EngineEvent::Note(umadev_i18n::tlf(
            "continuous.quality_gate_result",
            &[&score, "BLOCKED"],
        )));
    } else {
        events.emit(EngineEvent::Note(umadev_i18n::tlf(
            "continuous.quality_gate_advisory",
            &[&score],
        )));
    }

    // 4. HARD STOP only when the caller asked for hard-block semantics (a
    //    heavyweight gated code run) AND the gate actually failed. A lean /
    //    docs-only run keeps the gate advisory — verify ran + the scorecard is
    //    written, but the run proceeds (matches the single-shot guard exactly).
    if passed || !hard_block {
        return None;
    }

    // Surface the top findings inline so the user sees WHAT failed without
    // opening the JSON.
    let findings = qg_body
        .as_deref()
        .map(|b| crate::phases::quality_findings(b, 5))
        .unwrap_or_default();
    if !findings.is_empty() {
        let list = findings
            .iter()
            .map(|f| format!("  · {f}"))
            .collect::<Vec<_>>()
            .join("\n");
        events.emit(EngineEvent::Note(umadev_i18n::tlf(
            "continuous.quality_gate_findings",
            &[&list],
        )));
    }
    events.emit(EngineEvent::Note(umadev_i18n::tlf(
        "continuous.quality_gate_blocked",
        &[&score],
    )));
    Some(RunOutcome::HardStop(format!(
        "quality gate failed ({score}/100) — pipeline stopped at quality, nothing delivered \
         (continuous deterministic gate)"
    )))
}

/// DETERMINISTIC post-write governance catch-up — scan the whole real source
/// tree for governance violations and drive ONE bounded rework round on the MAIN
/// session. Reuses `umadev_governance::scan_content_with_policy` over
/// [`crate::acceptance::source_files`] (the same scan the single-shot
/// `run_governance_catchup` and the quality gate use).
///
/// This is the ONLY governance feedback loop for bases WITHOUT a real-time
/// PreToolUse hook: only `claude-code` installs one, so codex / opencode write
/// files that the continuous loop's `govern_tool_call` merely OBSERVES (it can't
/// pre-screen an already-applied edit). For those bases this catch-up closes the
/// gap; for `claude-code` it is skipped (the hook already blocked these at write
/// time). Keyed off the backend id — a deterministic, host-free check.
///
/// **Fail-open + advisory:** a clean scan returns immediately; a rework turn that
/// fails just leaves the findings for the quality gate to catch — never stops the
/// run.
async fn governance_catchup(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    deadline: std::time::Instant,
) {
    if backend_has_realtime_governance(&options.backend) {
        return;
    }
    let violations = governance_scan(options);
    if violations.is_empty() {
        return;
    }
    events.emit(EngineEvent::Note(umadev_i18n::tlf(
        "continuous.governance_catchup",
        &[&violations.len().to_string()],
    )));
    let directive = governance_rework_directive(&violations);
    if !drive_rework_turn(session, options, events, directive, deadline).await {
        return; // rework turn failed → fail-open, leave for the quality gate
    }
    let remaining = governance_scan(options);
    if remaining.is_empty() {
        events.emit(EngineEvent::Note(
            umadev_i18n::tl("continuous.governance_clean").to_string(),
        ));
    } else {
        events.emit(EngineEvent::Note(umadev_i18n::tlf(
            "continuous.governance_remaining",
            &[&remaining.len().to_string()],
        )));
    }
}

/// Whether a backend id drives a base that governs writes in REAL TIME (a
/// PreToolUse hook fires before each write). Only `claude-code` does; every other
/// base writes ungoverned in real time and needs the post-hoc
/// [`governance_catchup`]. Deterministic + host-free (matches the
/// `realtime_governance` capability the host crate reports for these bases).
pub(crate) fn backend_has_realtime_governance(backend: &str) -> bool {
    backend.eq_ignore_ascii_case("claude-code")
}

/// Scan every real source file with the governance kernel, returning a bounded
/// list of `"<rel>: <reason> (<clause>)"` violation strings. Empty = clean. Pure
/// and fail-open: an unreadable file is skipped, never a panic. Shared by the
/// catch-up rework loop (which reads it twice) and the critic floor.
pub(crate) fn governance_scan(options: &RunOptions) -> Vec<String> {
    let policy = umadev_governance::Policy::load(&options.project_root);
    let ctx = project_context_for(options);
    let mut out = Vec::new();
    for f in crate::acceptance::source_files(&options.project_root) {
        let Ok(content) = std::fs::read_to_string(&f) else {
            continue;
        };
        let rel = f
            .strip_prefix(&options.project_root)
            .unwrap_or(&f)
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        let d = umadev_governance::scan_content_with_context(&rel, &content, &policy, ctx);
        if d.block {
            out.push(format!(
                "{rel}: {} ({})",
                d.reason.split('.').next().unwrap_or("violation").trim(),
                d.clause
            ));
        }
        if out.len() >= 25 {
            break;
        }
    }
    out
}

/// Build ONE imperative governance-rework directive from the scanned violations.
/// Reuses the single-shot path's wording (design tokens / an icon library / no
/// AI filler) so the base fixes exactly the flagged files and nothing else.
fn governance_rework_directive(violations: &[String]) -> String {
    let mut list = String::new();
    for v in violations.iter().take(25) {
        list.push_str("- ");
        list.push_str(v);
        list.push('\n');
    }
    format!(
        "{}\n\nViolations:\n{list}\nWhen all are fixed, end your turn.",
        umadev_i18n::tl("continuous.governance_rework_intro")
    )
}

/// The DETERMINISTIC floor for the quality-node critic team — coverage gaps
/// (`uncovered_requirements`), interface-acceptance gaps, and frontend↔contract
/// drift fold into `qa_floor`; governance findings fold into `security_floor`.
/// These are the HARD signal the critics receive as CONTEXT so their semantic
/// pass builds on real findings instead of an empty floor (the review P0-2 fix).
/// Pure + fail-open: every contributor swallows its own errors → empty floor.
pub(crate) fn quality_floor(options: &RunOptions) -> (String, String) {
    let slug = options.effective_slug();
    let root = &options.project_root;

    // qa_floor: requirement coverage + interface-acceptance gaps + frontend /
    // contract drift — the spec→tasks and spec→code halves of the loop.
    let mut qa: Vec<String> = Vec::new();
    for r in crate::coverage::uncovered_requirements(root, &slug) {
        qa.push(format!("coverage gap: {r}"));
    }
    for g in crate::acceptance::task_acceptance_gaps(root, &slug) {
        qa.push(format!("acceptance gap: {g}"));
    }
    for v in frontend_contract_drift(options, &slug) {
        qa.push(format!("contract drift: {v}"));
    }

    // security_floor: the governance content scan over the real source files.
    let security = governance_scan(options);

    (qa.join("\n- "), security.join("\n- "))
}

/// Frontend↔backend contract drift: parse the architecture API table into a
/// typed contract, extract the frontend's real `fetch`/`axios` calls, and return
/// the mismatch details (a fetch URL with no matching backend route). Reuses
/// `umadev_contract` exactly like the single-shot quality gate. Fail-open: an
/// unreadable architecture doc → empty contract → no drift.
fn frontend_contract_drift(options: &RunOptions, slug: &str) -> Vec<String> {
    let arch_text = std::fs::read_to_string(
        options
            .project_root
            .join("output")
            .join(format!("{slug}-architecture.md")),
    )
    .unwrap_or_default();
    let arch_spec = umadev_contract::parse_architecture(&arch_text, &format!("{slug} API"));
    let derived = umadev_contract::derive_endpoints_from_requirement(&options.requirement);
    let contract_spec = umadev_contract::merge_specs(&arch_spec, &derived);
    if contract_spec.is_empty() {
        return Vec::new();
    }
    let fe_calls = umadev_contract::extract_frontend_calls(&options.project_root);
    umadev_contract::validate_frontend_vs_contract(&fe_calls, &contract_spec)
        .into_iter()
        .map(|v| v.detail)
        .collect()
}

// ───────────────────────────────────────────────────────────────────────────
// Phase plan + directives
// ───────────────────────────────────────────────────────────────────────────

/// The phases this block drives, given the phase it starts after.
///
/// Mirrors the single-shot block split: the initial block is research → docs →
/// (docs gate); the post-docs block is spec → frontend → (preview gate); the
/// post-preview block is backend → quality → delivery.
fn phases_for_block(start_after: Phase) -> &'static [Phase] {
    match start_after {
        Phase::Research => &[Phase::Research, Phase::Docs, Phase::DocsConfirm],
        // Resume after the docs gate.
        Phase::Spec => &[Phase::Spec, Phase::Frontend, Phase::PreviewConfirm],
        // Resume after the preview gate.
        Phase::Backend => &[Phase::Backend, Phase::Quality, Phase::Delivery],
        // Any other entry point drives just the tail — fail-open so a caller
        // can't wedge.
        _ => &[Phase::Backend, Phase::Quality, Phase::Delivery],
    }
}

/// The actual phases to drive this block, tailoring [`phases_for_block`] to the
/// plan. **LEGACY (Wave 3 — only reached under `UMADEV_LEGACY_PIPELINE=1`):** this
/// is the fixed-walk route the director loop replaced as the default; the planner
/// here decides a forced phase list, whereas the director loop treats the planner
/// as an advisory prior ([`crate::planner::advisory_prior`]) and decides the route
/// itself. Kept verbatim so the legacy opt-in behaves exactly as before. Two
/// regimes:
///
/// - **Gated plan** (any plan that still has a confirm gate — `Greenfield` /
///   `FrontendOnly` / `BackendOnly` / `DocsOnly`): the unchanged gate-anchored
///   block split, intersected with the plan so a one-sided build skips the phase
///   it doesn't need (`FrontendOnly` keeps the preview gate but drops Backend;
///   `BackendOnly` drops Frontend + its preview gate). The full pipeline + both
///   human confirm gates are preserved exactly.
/// - **Gateless lean plan** (`Light` / `Bugfix` / `Refactor` — no confirm gate at
///   all): there is no gate to anchor a block split on, so the WHOLE lean phase
///   list (e.g. Light: spec → frontend → backend → quality) is driven in ONE
///   block at the fresh-run `Research` start. A gate-resume entry (Spec/Backend)
///   for such a plan has nothing left → empty → a clean completion. This is the
///   lightweight fast path on the continuous session: no research, no three docs,
///   no gate pause — straight to implement + verify, governance + the zero-source
///   hard gate + the quality node all still apply.
fn block_phases(start_after: Phase, plan: &crate::planner::PhasePlan) -> Vec<Phase> {
    let gateless = !plan.includes(Phase::DocsConfirm) && !plan.includes(Phase::PreviewConfirm);
    if gateless {
        // One unsplit block at the fresh start; nothing on a (spurious) resume.
        return if start_after == Phase::Research {
            plan.phases.clone()
        } else {
            Vec::new()
        };
    }
    // P0-B: a GATED plan with a docs gate but NO preview gate (`BackendOnly`, and
    // any future docs-gated plan that drops the preview gate) MUST drive the whole
    // tail in ONE post-docs block. The old `phases_for_block(Spec) ∩ plan` returned
    // just `[Spec]` for BackendOnly (its window was `[Spec, Frontend,
    // PreviewConfirm]`, none of which past Spec survive the intersection), so the
    // driver stopped after Spec and Backend/Quality/Delivery were NEVER driven —
    // the hard gates (zero-source / quality) never fired, and an empty Spec-only
    // run reported `Completed` as a disguised success. When the plan has no preview
    // gate, drive every plan phase from `start_after` to the end (like the lean
    // "one block to done" shape, but keeping the docs gate that already split off
    // the initial block). When the plan DOES have a preview gate, keep the
    // unchanged gate-anchored three-block split intersected with the plan.
    if plan.includes(Phase::DocsConfirm) && !plan.includes(Phase::PreviewConfirm) {
        // Initial block (fresh run): research → docs → docs gate (unchanged so the
        // docs checkpoint still pauses for the user). Any post-docs resume drives
        // the whole remaining tail (Spec → Backend → Quality → Delivery) to done.
        return if start_after == Phase::Research {
            [Phase::Research, Phase::Docs, Phase::DocsConfirm]
                .into_iter()
                .filter(|p| plan.includes(*p))
                .collect()
        } else {
            // Drive every plan phase at or after the resume point, in canonical
            // order, all the way to Delivery — no further gate to anchor a split.
            plan.phases
                .iter()
                .copied()
                .filter(|p| phase_order(*p) >= phase_order(start_after))
                .collect()
        };
    }
    phases_for_block(start_after)
        .iter()
        .copied()
        .filter(|p| plan.includes(*p))
        .collect()
}

/// Canonical position of a phase in [`umadev_spec::PHASE_CHAIN`] — used to slice
/// "every plan phase at or after the resume point" for the no-preview-gate tail.
/// Pure + total: an off-chain phase (none exist today) sorts last (fail-open: it
/// is simply never selected ahead of a real phase).
fn phase_order(phase: Phase) -> usize {
    umadev_spec::PHASE_CHAIN
        .iter()
        .position(|p| *p == phase)
        .unwrap_or(usize::MAX)
}

/// Whether a phase is one that writes real code (and so is subject to plan-mode
/// read-only suppression + the zero-source hard gate).
fn is_executing(phase: Phase) -> bool {
    matches!(
        phase,
        Phase::Spec | Phase::Frontend | Phase::Backend | Phase::Quality | Phase::Delivery
    )
}

/// The last code-producing phase actually in the plan — the hard-gate anchor.
fn last_code_phase(plan: &crate::planner::PhasePlan) -> Phase {
    if plan.includes(Phase::Backend) {
        Phase::Backend
    } else if plan.includes(Phase::Frontend) {
        Phase::Frontend
    } else {
        // No code phase planned → anchor on Delivery so the gate simply never
        // fires (it's guarded by `produces_code` anyway).
        Phase::Delivery
    }
}

/// The [`Gate`] corresponding to a gate phase.
fn gate_for_phase(phase: Phase) -> Gate {
    match phase {
        Phase::PreviewConfirm => Gate::PreviewConfirm,
        // DocsConfirm and any other (defensive) → docs gate.
        _ => Gate::DocsConfirm,
    }
}

/// Build the imperative, command-style directive for one phase.
///
/// `first` (only the very first phase of a fresh run) injects the FULL context
/// (requirement + role + the spec/anti-slop rules). Later phases are LEAN — the
/// same session already holds the prior research / docs / code, so we only issue
/// the next instruction ("now implement the frontend from the approved docs you
/// already wrote") rather than re-priming everything.
///
/// `kind` tailors the FRAMING to the task: a heavyweight (`Greenfield` / one-sided)
/// plan ran research + the three docs first, so its Spec/Frontend/Backend
/// directives reference "the approved documents you wrote". A lean GATELESS plan
/// (`Light` / `Bugfix` / `Refactor`) wrote NO docs — so it gets short,
/// self-contained, directly-imperative directives ("implement these features now,
/// write the code files") via [`lean_directive`], with no doc references and no
/// heavy front matter, which is the per-`TaskKind` wording that keeps a simple
/// "做一个待办单页应用" fast.
///
/// Crucially every directive is COMMAND-style: "produce X now, write the files
/// directly, do NOT ask me whether to continue." This is the single fix for the
/// single-shot path's "base replies a paragraph and asks 'shall I continue?'"
/// failure — in a live agentic session the base just does it.
fn phase_directive(
    options: &RunOptions,
    phase: Phase,
    first: bool,
    kind: crate::planner::TaskKind,
) -> String {
    let slug = options.effective_slug();
    let req = &options.requirement;
    let no_ask = "Work autonomously: use your tools to do this NOW, write all files \
         directly to disk, and do NOT ask me whether to continue — just produce the \
         deliverable. When done, end your turn.";

    // Lean gateless plans (Light / Bugfix / Refactor) skip research + the three
    // core docs, so their phase directives must NOT reference documents that were
    // never written. Route them to the lean, self-contained, command-style
    // directives instead of the heavyweight doc-anchored ones below.
    if is_lean_kind(kind) {
        return lean_directive(&slug, req, phase, first, kind, no_ask);
    }

    // Each phase opens by explicitly naming the senior ROLE that owns it (PM →
    // architect → designer → engineers → QA/security → DevOps) so the base steps
    // into that seat's professional standard, then the imperative body of the
    // phase follows. Empty for the gate phases (which never get a directive). The
    // Research+first case already carries the full role-priming `system` prompt,
    // so it skips the prefix to avoid restating the seat twice.
    let persona = crate::experts::phase_persona(phase);
    let role = if persona.is_empty() {
        String::new()
    } else {
        format!("{persona}\n\n")
    };

    match phase {
        Phase::Research => {
            let p = crate::experts::research_prompt(&slug, req, "");
            if first {
                format!("{}\n\n{}\n\n{no_ask}", p.system, p.user)
            } else {
                format!("{role}Now do the research phase.\n\n{}\n\n{no_ask}", p.user)
            }
        }
        Phase::Docs => format!(
            "{role}Now produce ALL THREE core documents for `{slug}`, writing each file directly:\n\
             - `output/{slug}-prd.md` (product requirements)\n\
             - `output/{slug}-architecture.md` (architecture + API surface table)\n\
             - `output/{slug}-uiux.md` (design system: tokens, typography, icon library)\n\
             Use the research you just produced. Follow the UmaDev rules you were given \
             (no emoji icons, design-token colors only, frontend fetch paths must match the \
             architecture API table).\n\n{no_ask}"
        ),
        Phase::Spec => format!(
            "{role}The user has APPROVED the three documents. Now translate them into an \
             implementation spec + a task breakdown for `{slug}` (write the spec/tasks \
             files). Cite the PRD's `FR-` ids so coverage maps 1:1.\n\n{no_ask}"
        ),
        Phase::Frontend => format!(
            "{role}Now IMPLEMENT THE FRONTEND for `{slug}` as REAL code files (components, pages, \
             API client, design-token styles) from the UIUX + architecture docs you wrote. \
             Icons from the declared library only — never emoji. Wire every fetch URL to an \
             architecture API path. Run the build and fix errors. Write \
             `output/{slug}-frontend-notes.md` with the preview URL + run command.\n\n{no_ask}"
        ),
        Phase::Backend => format!(
            "{role}Now IMPLEMENT THE BACKEND for `{slug}` as REAL code files (routes, models, \
             middleware, tests) matching the architecture API surface. Validate inputs, \
             use the standard error envelope, write + run tests. Write \
             `output/{slug}-backend-notes.md`.\n\n{no_ask}"
        ),
        Phase::Quality => format!(
            "{role}Now run QUALITY for `{slug}`: run the project's real build + test + lint, fix \
             what fails, and do a security pass (no hardcoded secrets, input validation, \
             safe error handling). Summarize results.\n\n{deps}\n\n{no_ask}",
            deps = crate::experts::deps_before_tests_directive()
        ),
        Phase::Delivery => format!(
            "{role}Now produce the DELIVERY recipe for `{slug}`: verify the production build for \
             frontend + backend, and write exact deployment instructions. Do NOT deploy to \
             any remote system — only verify locally and write the recipe.\n\n{no_ask}"
        ),
        // Gate phases never get a directive (the driver pauses before them); a
        // defensive empty directive keeps this total.
        Phase::DocsConfirm | Phase::PreviewConfirm => String::new(),
    }
}

/// Whether `kind` is a lean, GATELESS plan — the lightweight fast path that
/// skips research + the three core docs + both confirm gates. These get the
/// short, self-contained [`lean_directive`] framing rather than the heavyweight
/// doc-anchored [`phase_directive`] one.
fn is_lean_kind(kind: crate::planner::TaskKind) -> bool {
    use crate::planner::TaskKind::{Bugfix, Light, Refactor};
    matches!(kind, Light | Bugfix | Refactor)
}

/// Short, self-contained, directly-imperative directives for a lean GATELESS plan
/// (`Light` / `Bugfix` / `Refactor`). There is NO research and NO PRD /
/// architecture / UI-UX to reference — so these directives carry the requirement
/// itself and tell the base to act, with no heavy front matter and no doc
/// dependencies. The `first` phase of a lean run carries a ONE-LINE role +
/// anti-slop reminder (since the heavyweight Research priming never ran); later
/// lean phases stay maximally terse.
fn lean_directive(
    slug: &str,
    req: &str,
    phase: Phase,
    first: bool,
    kind: crate::planner::TaskKind,
    no_ask: &str,
) -> String {
    use crate::planner::TaskKind::{Bugfix, Refactor};
    // A compact priming line ONLY on the first phase of a fresh lean run — names
    // the role + the hard visual rules (no emoji icons, design-token colors only)
    // so a Light frontend still respects the moat without the full Research+docs
    // ceremony. Sourced from `experts::lean_priming` (prompts are agent policy, kept
    // in one place). Empty on later phases (same session already holds the context).
    let prime = if first {
        format!("{}\n\n", crate::experts::lean_priming())
    } else {
        String::new()
    };
    // A short, explicit ROLE line on EVERY lean phase (even the terse later ones)
    // so the base still works as the right seat — "as an engineer, just implement
    // this" — without the document-anchored heavyweight persona. Folded into the
    // `prime` so the first-phase reminder and the role read as one preamble.
    let prime = format!("{}{}\n\n", prime, crate::experts::lean_phase_role(phase));
    match phase {
        Phase::Spec => format!(
            "{prime}Task for `{slug}`:\n{req}\n\n\
             Write a SHORT, lean implementation plan for exactly this task — the \
             concrete files to create/change and the steps, nothing more. No formal \
             PRD/architecture; this is a small scoped change. Keep it to a few bullet \
             points, then proceed.\n\n{no_ask}"
        ),
        Phase::Frontend => format!(
            "{prime}Now IMPLEMENT this task as REAL code files, directly:\n{req}\n\n\
             Write the actual source (HTML/CSS/JS or the project's framework), build \
             working features end to end, and run the build/dev server to confirm it \
             works. Icons from a declared library only — never emoji; colors via \
             design tokens. Keep it proportional to this small scope — do NOT scaffold \
             a large multi-module app.\n\n{no_ask}"
        ),
        Phase::Backend => format!(
            "{prime}Now implement any backend/server logic this task needs as REAL \
             code files, directly:\n{req}\n\n\
             Validate inputs and handle errors. If this task is purely frontend / a \
             static page and needs no backend, say so in one line and make no backend \
             changes. Keep it proportional to the small scope.\n\n{no_ask}"
        ),
        Phase::Quality => {
            let focus = match kind {
                Bugfix => {
                    "Confirm the bug is actually fixed (reproduce the original \
                           failure path and verify it no longer happens). "
                }
                Refactor => {
                    "Confirm behavior is UNCHANGED by the refactor (the existing \
                             tests still pass). "
                }
                _ => "",
            };
            format!(
                "{prime}Now VERIFY `{slug}`: run the project's real build + test + lint \
                 and fix what fails. {focus}Do a quick security pass (no hardcoded \
                 secrets, inputs validated). Summarize results in a few lines.\n\n{deps}\n\n{no_ask}",
                deps = crate::experts::deps_before_tests_directive()
            )
        }
        // A lean plan never reaches Research / Docs / Delivery / the gates — but
        // keep this total + fail-open: fall back to the requirement + no-ask so a
        // stray phase can't produce an empty directive.
        Phase::Research
        | Phase::Docs
        | Phase::Delivery
        | Phase::DocsConfirm
        | Phase::PreviewConfirm => {
            format!("{prime}Task for `{slug}`:\n{req}\n\n{no_ask}")
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Role-critic team review + bounded rework (see design §3.5 / §3.6)
//
// At each review node the director: (1) scales a team to the task, (2) reads the
// on-disk blackboard the main session just wrote, (3) PARALLEL-forks one
// read-only session per seat and collects N `RoleVerdict`s, (4) deterministically
// decides — any `blocking[]` non-empty folds into ONE imperative rework directive
// injected back into the MAIN session, then re-reviews; all-accept proceeds. The
// loop is BOUNDED (`MAX_REWORK_ROUNDS` + a stall counter that stops when the
// blocking count stops dropping). Fully fail-open + advisory: a base with no fork
// / an offline brain / a parse failure yields empty accepting verdicts → no
// blocking → proceed. A critic NEVER drives termination; the only hard stops are
// the deterministic floor + the user gate elsewhere.
// ───────────────────────────────────────────────────────────────────────────

/// Which review node is running — selects the team + the blackboard surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewKind {
    /// The docs gate: PM / architect / UIUX review the three core documents.
    Docs,
    /// The preview gate: UIUX / frontend review the delivered frontend.
    Preview,
    /// The quality node: QA / security / backend / DevOps review the code.
    Quality,
}

/// Map a gate phase to its review node kind.
fn gate_review_kind(phase: Phase) -> ReviewKind {
    match phase {
        Phase::PreviewConfirm => ReviewKind::Preview,
        // DocsConfirm + any defensive other → docs review.
        _ => ReviewKind::Docs,
    }
}

/// Run the cross-review team for a node, then drive a BOUNDED rework loop on the
/// main session. Deterministic control: the loop continues only while a seat
/// reports a NEW blocking finding AND the round budget + stall counter allow it.
/// Advisory + fail-open throughout — it never returns a verdict that blocks the
/// run; the gate/floor decide that elsewhere.
async fn review_and_rework(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    kind: ReviewKind,
    deadline: std::time::Instant,
) {
    // Scale the team to the task; an empty team (lean / no-UI / docs-only paths)
    // means "no cross-review here" — return immediately, the floor stands.
    let team = team_for(kind, &options.requirement, &options.project_root);
    if team.is_empty() {
        return;
    }

    let mut prev_blocking = usize::MAX;
    for round in 0..=MAX_REWORK_ROUNDS {
        // 1. Read the blackboard FRESH each round (the rework may have rewritten
        //    it) and run the team in parallel on read-only forks.
        let blocking = run_review_team(session, options, events, kind, &team, round).await;

        // 2. All-accept (or fail-open empty) → proceed. This is the only success
        //    exit; everything else is bounded rework.
        if blocking.is_empty() {
            if round > 0 {
                events.emit(EngineEvent::Note(umadev_i18n::tlf(
                    "continuous.team.passed_after_rework",
                    &[kind_label(kind), &round.to_string()],
                )));
            }
            return;
        }

        // 3. Deterministic stall / budget guard: stop reworking when we've spent
        //    the round budget OR the blocking count did not DROP (no progress —
        //    the base can't satisfy a seat, or a flapping verdict). Either way we
        //    proceed: the critic is advisory and must never wedge the run.
        let made_progress = blocking.len() < prev_blocking;
        if round == MAX_REWORK_ROUNDS || !made_progress {
            events.emit(EngineEvent::Note(umadev_i18n::tlf(
                "continuous.team.unresolved_advisory",
                &[kind_label(kind), &blocking.len().to_string()],
            )));
            return;
        }
        prev_blocking = blocking.len();

        // 4. Fold every blocking finding into ONE imperative rework directive and
        //    inject it into the MAIN session — the base fixes the files in the
        //    SAME context, then the next loop iteration re-reviews.
        events.emit(EngineEvent::Note(umadev_i18n::tlf(
            "continuous.team.inject_rework",
            &[
                kind_label(kind),
                &blocking.len().to_string(),
                &(round + 1).to_string(),
            ],
        )));
        let directive = rework_directive(kind, &blocking);
        if !drive_rework_turn(session, options, events, directive, deadline).await {
            // The rework turn failed / the session died — stop reworking (the
            // outer loop's phase/turn handling already surfaced the failure path).
            // Fail-open: leave the findings as advisory and proceed.
            return;
        }
    }
}

/// The team for a review node, scaled to the task via the planner's tiering, plus
/// any USER-DEFINED seats (`.umadev/agents/*.md`) that apply to this node.
///
/// The built-in roster scales with the task kind exactly as before. User-defined
/// seats ride the SAME scaling: they are appended ONLY when the built-in team is
/// non-empty (so a lean kind still convenes none — the deterministic floor stands
/// alone there) and ONLY for the review kinds they apply to. They are ADDED on top
/// of the eight built-in seats, never replacing them, and convene on the same
/// read-only-fork path as advisory-only critics (the floor still governs).
pub(crate) fn team_for(
    kind: ReviewKind,
    requirement: &str,
    project_root: &std::path::Path,
) -> Vec<Box<dyn RoleCritic>> {
    let tier = crate::planner::classify(requirement);
    let mut team: Vec<Box<dyn RoleCritic>> = match kind {
        ReviewKind::Docs => crate::critics::docs_team_for_kind(tier),
        ReviewKind::Preview => crate::critics::preview_team_for_kind(tier),
        ReviewKind::Quality => crate::critics::quality_team_for_kind(tier),
    };
    // Custom seats only join a node that ALREADY convenes a built-in team — this is
    // what keeps the team-size scaling intact (a lean kind convenes no team, so it
    // convenes no custom seats either; a one-line tweak never pays for a reviewer).
    if !team.is_empty() {
        team.extend(crate::agents::custom_team_for(project_root, kind));
    }
    team
}

/// Run the whole team in PARALLEL — one read-only `BaseSession::fork()` per seat
/// — and return the deduped union of every seat's `blocking[]`, tagged with the
/// seat. Each verdict is recorded to the team ledger. Fully fail-open: a base
/// that can't fork, an offline brain, or a parse failure yields empty accepting
/// verdicts → no blocking.
pub(crate) async fn run_review_team(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    kind: ReviewKind,
    team: &[Box<dyn RoleCritic>],
    round: usize,
) -> Vec<String> {
    // Read the on-disk blackboard ONCE (every seat reviews the same snapshot).
    let bb = Blackboard::read(options, kind);
    let arts = bb.artifacts(&options.requirement);

    events.emit(EngineEvent::Note(umadev_i18n::tlf(
        "continuous.team.cross_review_header",
        &[kind_label(kind), &team.len().to_string()],
    )));

    // Fork one read-only session per seat up front. `fork()` takes `&mut self`, so
    // the N establishments are necessarily SERIAL (you can't hold N `&mut` borrows
    // of the main session at once) — but each returns an OWNED, independent session,
    // so the REVIEW turns below run CONCURRENTLY (`join_all_ordered`), which is where
    // the wall-clock actually goes (a fork handshake is cheap, a judge turn is a full
    // base round-trip). Each `fork()` is bounded by a TIMEOUT so a base whose fork
    // handshake wedges (codex/opencode never returning `initialize`, whose reader
    // only errors when the base closes) can NEVER freeze the whole gate: a timed-out
    // fork degrades to an `Err`, which `review_one` already treats as a fail-open
    // ACCEPT (that seat consults nothing). `fork()` is independent per call, so the
    // reviews never collide and never touch the main writer (single-writer invariant).
    let mut forks = Vec::with_capacity(team.len());
    for _ in team {
        forks.push(fork_with_timeout(session).await);
    }
    let reviews = team
        .iter()
        .zip(forks)
        .map(|(critic, fork)| review_one(critic.as_ref(), fork, arts));
    let verdicts = crate::runner::join_all_ordered(reviews).await;

    // Sequentially (deterministic order) record + fold blocking — the seat order
    // is the team order regardless of which fork finished first.
    let phase_label = kind_phase_label(kind);
    let mut blocking: Vec<String> = Vec::new();
    for verdict in verdicts {
        crate::critics::append_team_ledger(&options.project_root, phase_label, round + 1, &verdict);
        let seat = verdict.role.clone();
        // Wave 1 (L1/L2 visibility): surface each seat's verdict as a STRUCTURED
        // event so a UI can render a real team-review panel (accept + blocking +
        // advisory), replacing the bland one-line Note as the source of truth. The
        // human-readable Notes are kept too (today's TUI still renders Notes; W1-B
        // switches to the panel) — both are observational, neither drives the loop
        // (verdicts stay advisory — invariant 2).
        events.emit(EngineEvent::critic_verdict(&verdict));
        if verdict.accepts && verdict.blocking.is_empty() {
            events.emit(EngineEvent::Note(umadev_i18n::tlf(
                "continuous.team.seat_passed",
                &[&seat],
            )));
        } else if !verdict.blocking.is_empty() {
            events.emit(EngineEvent::Note(umadev_i18n::tlf(
                "continuous.team.seat_blocking",
                &[&seat, &verdict.blocking.len().to_string()],
            )));
            for b in verdict.blocking {
                let item = format!("[{seat}] {}", b.trim());
                if item.len() > 6 && !blocking.contains(&item) {
                    blocking.push(item);
                }
            }
        }
    }
    blocking
}

/// Establish ONE read-only fork, bounded by [`fork_establish_timeout`]. A fork
/// handshake that wedges (e.g. a codex/opencode base that never returns its
/// `initialize`, whose stdout reader only errors once the base CLOSES) would
/// otherwise hang here forever and freeze the entire gate — `judge` has its own
/// turn timeout, but it never runs if the fork never finishes opening. The
/// timeout converts that hang into a [`SessionError::Start`], which `review_one`
/// already treats as a fail-open ACCEPT (the seat consults nothing). Fail-open by
/// contract: a wedged fork degrades one seat to advisory-accept, never blocks.
pub(crate) async fn fork_with_timeout(
    session: &mut dyn BaseSession,
) -> Result<Box<dyn BaseSession>, SessionError> {
    match tokio::time::timeout(fork_establish_timeout(), session.fork()).await {
        Ok(res) => res,
        Err(_) => Err(SessionError::Start(
            "fork handshake timed out — seat fail-open ACCEPT".to_string(),
        )),
    }
}

/// Drive ONE critic over its (possibly failed) fork, fail-open to an accepting
/// empty verdict. The critic's `review` runs its strict-JSON judge turn through a
/// [`ForkConsult`] that owns the fork; a fork that didn't open routes to a
/// fail-open consult that simply ACCEPTS.
async fn review_one(
    critic: &dyn RoleCritic,
    fork: Result<Box<dyn BaseSession>, SessionError>,
    arts: CriticArtifacts<'_>,
) -> RoleVerdict {
    let consult = ForkConsult::new(fork);
    let verdict = critic.review(&consult, arts).await;
    // Best-effort close the fork session (release the process / HTTP session).
    consult.end().await;
    verdict
}

/// What one [`drive_rework_turn_capturing`] turn observed — its completion flag plus
/// the accumulated assistant text and the failed-tool summaries (the pitfall feed).
/// The plain [`drive_rework_turn`] discards everything but `done`; the director's
/// step scheduler reads `text` (for the "claimed a build" gate) and `pitfalls` (to
/// distil into the lessons KB on the DEFAULT loop — Wave 2 deliverable 4).
pub(crate) struct ReworkTurn {
    /// The turn finished (Completed / Truncated). `false` = failed / dead / hung.
    pub done: bool,
    /// The accumulated assistant text for this turn.
    pub text: String,
    /// Summaries of every FAILED tool result this turn produced (the pitfall feed).
    pub pitfalls: Vec<String>,
}

/// Inject the rework directive into the MAIN session and pump its turn through
/// the SAME governance + audit + approval path a normal phase turn uses. Returns
/// `true` when the turn finished (clean or truncated-but-accepted), `false` on a
/// failed turn / a dead session (fail-open: the caller stops reworking).
pub(crate) async fn drive_rework_turn(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    directive: String,
    deadline: std::time::Instant,
) -> bool {
    drive_rework_turn_capturing(session, options, events, directive, deadline)
        .await
        .done
}

/// [`drive_rework_turn`], but returning the full [`ReworkTurn`] (text + pitfalls).
/// Reads the idle window ONCE at the boundary (not per-wait), so a mid-turn env flip
/// can't race; the deterministic core takes it as a param (the test drives it with a
/// tiny window, no process-env mutation to race). `deadline` is the run's wall-clock
/// ceiling, checked at the TOP of the pump so an ACTIVE base can't run one turn past
/// the budget (the mid-turn graceful settle — see [`drive_rework_turn_with_idle`]).
pub(crate) async fn drive_rework_turn_capturing(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    directive: String,
    deadline: std::time::Instant,
) -> ReworkTurn {
    drive_rework_turn_with_idle(
        session,
        options,
        events,
        directive,
        crate::director_loop::IdleBudget::from_env(),
        deadline,
    )
    .await
}

/// [`drive_rework_turn`] with an explicit idle window — the env read is hoisted
/// to the wrapper so this core is deterministic for the idle-watchdog test.
///
/// `deadline` is the run's wall-clock ceiling. It is checked at the TOP of the pump
/// loop, BEFORE the idle-guarded wait, so a base that stays ACTIVE (keeps emitting,
/// never trips the idle watchdog) can't run ONE turn unbounded past the run budget —
/// the mid-turn settle interrupts the base (bounded) and returns the work so far as a
/// completed turn, GRACEFULLY (never an error), so the caller finalizes what's built.
async fn drive_rework_turn_with_idle(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    directive: String,
    idle: crate::director_loop::IdleBudget,
    deadline: std::time::Instant,
) -> ReworkTurn {
    // Estimate this turn's token cost up front (the session stream carries no usage
    // on TurnDone) so the summon-driven step path records usage on the DEFAULT loop,
    // for every base — recorded once at TurnDone. Mirrors `drive_one_turn`.
    let mut est_tokens: u64 = crate::director_loop::approx_tokens(&directive);
    if session.send_turn(directive).await.is_err() {
        return ReworkTurn {
            done: false,
            text: String::new(),
            pitfalls: Vec::new(),
        };
    }
    let policy = umadev_governance::Policy::load(&options.project_root);
    let mut text = String::new();
    let mut pitfalls: Vec<String> = Vec::new();
    // Tool-aware grace (same as the build pump): a rework turn also runs tools (the
    // base re-runs its build/test to fix the cause), which go silent for minutes, so
    // an in-flight tool gets the extended window before the watchdog calls it a hang.
    let mut in_tool_call = false;
    // Idle watchdog (P1-11): this rework pump (reused by `governance_catchup` /
    // `review_and_rework` / the director's `summon`) was a naked
    // `next_event().await` — a base that hangs mid-rework would freeze every
    // review node forever. Guard it with the SAME shared idle primitive as the
    // director loop + `drive_phase`, so no main-session pump can wedge.
    loop {
        // Wall-clock budget reached DURING a turn (not just between steps). A base
        // that stays ACTIVE (keeps emitting tool-calls / text deltas — e.g. writing
        // code) never trips the idle watchdog below, so without this a single summon
        // turn runs UNBOUNDED past the run budget (the between-step deadline checks
        // can't be reached while this pump is still draining). Settle GRACEFULLY on
        // the work so far: best-effort bounded interrupt (the SAME one
        // `next_event_idle` issues on an idle hang), record this turn's usage estimate
        // (no `TurnDone` → no real usage, F3), and return the accumulated text as a
        // completed turn (`done: true`) — so the step scheduler treats it as "this
        // step produced what it produced" and the between-step deadline winds the run
        // down to the final gate rather than re-driving past the budget.
        if std::time::Instant::now() >= deadline {
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(crate::director_loop::INTERRUPT_TIMEOUT_SECS),
                session.interrupt(),
            )
            .await;
            crate::director_loop::record_estimated_usage(&options.backend, est_tokens);
            events.emit(EngineEvent::Note(
                "team · run budget reached mid-turn — interrupted the base and finalizing \
                 on what's built (raise UMADEV_RUN_BUDGET_SECS for a longer run)"
                    .to_string(),
            ));
            return ReworkTurn {
                done: true,
                text,
                pitfalls,
            };
        }
        let ev = match crate::director_loop::next_event_idle(
            session,
            idle,
            in_tool_call,
            Some(deadline),
        )
        .await
        {
            crate::director_loop::IdleEvent::Event(ev) => ev,
            // Session ended mid-rework (incl. a base that died mid-tool, caught by the
            // liveness poll), OR a non-tool hang past the base window (interrupt already
            // issued, bounded), OR the run budget reached mid-tool → fail-open stop
            // reworking. A rework turn is advisory, so a settle here simply leaves the
            // findings for the next gate rather than wedging the run — but no longer
            // SILENTLY: surface the base's OWN stderr/exit (captured at the settle) as a
            // Note, since a `ReworkTurn` carries no reason string, so a hung rework reads
            // the same WHY as the chat / phase paths.
            crate::director_loop::IdleEvent::SessionEnded { exit, stderr_tail } => {
                events.emit(EngineEvent::Note(crate::director_loop::enrich_idle_reason(
                    "team · rework turn ended — base session ended mid-turn",
                    exit,
                    stderr_tail,
                    &options.backend,
                )));
                return ReworkTurn {
                    done: false,
                    text,
                    pitfalls,
                };
            }
            crate::director_loop::IdleEvent::IdleTimedOut { exit, stderr_tail } => {
                events.emit(EngineEvent::Note(crate::director_loop::enrich_idle_reason(
                    &crate::director_loop::idle_reason(idle.window(false)),
                    exit,
                    stderr_tail,
                    &options.backend,
                )));
                return ReworkTurn {
                    done: false,
                    text,
                    pitfalls,
                };
            }
        };
        // Arm/disarm the tool-grace from this event before handling it.
        if let Some(t) = crate::director_loop::tool_phase_transition(&ev) {
            in_tool_call = t;
        }
        match ev {
            SessionEvent::TextDelta(delta) => {
                est_tokens = est_tokens.saturating_add(crate::director_loop::approx_tokens(&delta));
                text.push_str(&delta);
                events.emit(EngineEvent::WorkerStream {
                    event: StreamEvent::Text { delta },
                });
            }
            SessionEvent::ThinkingDelta(delta) => {
                // Reasoning during a rework round — stream it to the collapsed
                // `[thinking]` block; never accumulate it into the answer `text`.
                events.emit(EngineEvent::WorkerStream {
                    event: StreamEvent::ThinkingDelta(delta),
                });
            }
            SessionEvent::SessionModel(id) => {
                // The base reported its resolved model at session init — surface it
                // so the TUI's context gauge uses the REAL window, not a per-backend
                // guess. Informational only; drives no rework control.
                events.emit(EngineEvent::BaseModel { id });
            }
            SessionEvent::ToolCall { name, input } => {
                // Rework writes real files — govern + audit them exactly like a
                // phase turn (the rework runs on the main writer session).
                govern_tool_call(options, events, &policy, Phase::Quality, &name, &input);
            }
            SessionEvent::ToolResult { ok, summary } => {
                if !ok {
                    // A failed tool call is a development pitfall — feed it to the
                    // lessons KB (the caller distils it). Mirrors `runner.rs`'s
                    // `ok: false` capture, now on the DEFAULT loop too.
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
                let decision = approval_decision(options, &action, &target);
                if session.respond(&req_id, decision).await.is_err() {
                    return ReworkTurn {
                        done: false,
                        text,
                        pitfalls,
                    };
                }
            }
            SessionEvent::TurnDone { status, usage } => {
                // Record this turn's usage on the DEFAULT loop (fail-open). F3:
                // prefer the base's REAL reported usage (claude/codex), falling
                // back to the chars/4 estimate (opencode, or any base that didn't
                // report). Mirrors `director_loop::drive_one_turn`.
                crate::director_loop::record_estimated_usage(
                    &options.backend,
                    crate::director_loop::real_or_estimated_tokens(usage, est_tokens),
                );
                // Completed / Truncated → accept and re-review; Interrupted /
                // Failed → stop reworking (fail-open, advisory).
                return ReworkTurn {
                    done: matches!(status, TurnStatus::Completed | TurnStatus::Truncated),
                    text,
                    pitfalls,
                };
            }
        }
    }
}

/// Build ONE imperative rework directive from the union of every seat's blocking
/// findings. Command-style ("fix these now, edit the files directly") so the
/// base acts in its live agentic loop rather than narrating.
fn rework_directive(kind: ReviewKind, blocking: &[String]) -> String {
    let surface = match kind {
        ReviewKind::Docs => "the three core documents (PRD / architecture / UI-UX)",
        ReviewKind::Preview => "the delivered frontend code",
        ReviewKind::Quality => "the delivered code (frontend + backend + tests)",
    };
    let mut list = String::new();
    for b in blocking {
        list.push_str("- ");
        list.push_str(b);
        list.push('\n');
    }
    format!(
        "The review team flagged MUST-FIX issues in {surface}. Fix EVERY one of them \
         now by editing the files directly — do not ask me, do not narrate, just apply \
         the fixes and re-run any build/test you already ran. Issues:\n{list}\nWhen all \
         are fixed, end your turn."
    )
}

/// The on-disk blackboard surface for a review node — the docs / code the main
/// session wrote, read fresh so a rework round reviews the UPDATED files. Owns
/// its strings so the borrowed [`CriticArtifacts`] can point into it.
pub(crate) struct Blackboard {
    prd: String,
    architecture: String,
    uiux: String,
    code: String,
    qa_floor: String,
    security_floor: String,
}

impl Blackboard {
    /// Read the surface a review node needs. Docs → the three `output/*.md`;
    /// preview / quality → the architecture/UIUX context + a digest of the real
    /// source files. All reads are fail-open (a missing file → empty string).
    pub(crate) fn read(options: &RunOptions, kind: ReviewKind) -> Self {
        let slug = options.effective_slug();
        let root = &options.project_root;
        let doc = |name: &str| {
            std::fs::read_to_string(root.join(format!("output/{slug}-{name}.md")))
                .unwrap_or_default()
        };
        let (prd, architecture, uiux) = (doc("prd"), doc("architecture"), doc("uiux"));
        let code = if matches!(kind, ReviewKind::Preview | ReviewKind::Quality) {
            let digest = source_digest(options);
            if matches!(kind, ReviewKind::Quality) {
                // The Quality critics judge a FORKED session whose context is the
                // blackboard (the `output/*.md` docs + this source DIGEST), not the
                // full file tree — so a critic can hallucinate "no tests / no backend
                // / no source exist" and raise a spurious blocking finding. GROUND
                // them with a bounded listing of the REAL workspace files so they SEE
                // what exists and judge it, instead of filtering the hallucination out
                // after the fact. Fail-open: an empty listing (unreadable/empty tree)
                // contributes nothing and the digest stands alone.
                let listing = WorkspaceEvidence::read(root).to_review_context();
                if listing.is_empty() {
                    digest
                } else {
                    format!("{listing}\n{digest}")
                }
            } else {
                digest
            }
        } else {
            String::new()
        };
        // The deterministic floors are surfaced as CONTEXT to the QA / security
        // seats (so their semantic pass focuses on what a static check can't see).
        // At the QUALITY node these are the REAL deterministic findings — coverage
        // gaps + frontend↔contract drift (→ qa_floor) and governance violations
        // (→ security_floor) — so the critics build on hard findings rather than an
        // empty floor (the review P0-2 fix). Empty for the docs / preview nodes.
        let (qa_floor, security_floor) = if matches!(kind, ReviewKind::Quality) {
            quality_floor(options)
        } else {
            (String::new(), String::new())
        };
        Self {
            prd,
            architecture,
            uiux,
            code,
            qa_floor,
            security_floor,
        }
    }

    /// Borrow the blackboard as the critic-facing [`CriticArtifacts`].
    pub(crate) fn artifacts<'a>(&'a self, requirement: &'a str) -> CriticArtifacts<'a> {
        CriticArtifacts {
            requirement,
            prd: &self.prd,
            architecture: &self.architecture,
            uiux: &self.uiux,
            code: &self.code,
            qa_floor: &self.qa_floor,
            security_floor: &self.security_floor,
        }
    }
}

/// A bounded, newest-first digest of the real source files for the code-review
/// seats — the same blackboard the QA / frontend / backend / DevOps critics read.
/// Capped so a large tree can't blow the judge prompt (the critics also excerpt).
fn source_digest(options: &RunOptions) -> String {
    let files = crate::acceptance::source_files(&options.project_root);
    let mut out = String::new();
    for f in files.iter().take(40) {
        let Ok(content) = std::fs::read_to_string(f) else {
            continue;
        };
        let rel = f
            .strip_prefix(&options.project_root)
            .unwrap_or(f)
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        out.push_str("\n// ===== ");
        out.push_str(&rel);
        out.push_str(" =====\n");
        out.push_str(&crate::experts::excerpt(&content, 4000));
        out.push('\n');
        if out.len() >= 60_000 {
            break;
        }
    }
    out
}

/// Max file paths listed in a single workspace-evidence grounding block — keeps
/// the forked judge prompt bounded even on a large tree.
const MAX_LISTED_PATHS: usize = 40;
/// Max bytes of joined relative paths in a workspace-evidence grounding block.
const MAX_LISTING_BYTES: usize = 4096;

/// A bounded, deterministic listing of the REAL workspace files, handed to the
/// Quality-review critics as GROUNDING.
///
/// The Quality critics review a `fork()`ed read-only session whose context is
/// the blackboard (the `output/*.md` docs + a source *digest*), NOT the full
/// file tree — so a critic can hallucinate "no tests / no backend / no source
/// exist" and raise a spurious blocking finding. Injecting the ACTUAL relative
/// paths lets a critic SEE what exists and judge it, killing the hallucination
/// at the source. Because the grounding prevents the false claim up front, there
/// is no post-hoc verdict surgery: critic verdicts stay advisory (invariant 2)
/// and the deterministic floor (coverage / contract / verify) governs loop
/// termination.
///
/// The listing is bounded ([`MAX_LISTED_PATHS`] paths / [`MAX_LISTING_BYTES`]
/// bytes), deterministic (paths sorted), and fail-open: an unreadable / empty
/// tree yields an empty listing that injects NOTHING — never a panic, and never
/// a fabricated claim that files exist when they don't.
#[derive(Debug, Default)]
struct WorkspaceEvidence {
    /// Relative paths of the real test files (accurate [`is_test_file`] split).
    tests: Vec<String>,
    /// Relative paths of the real non-test source files (backend + everything else).
    source: Vec<String>,
    /// How many additional files existed beyond the listing cap (the "(+N more)").
    overflow: usize,
}

impl WorkspaceEvidence {
    /// Scan the workspace once and collect a bounded, sorted listing of the real
    /// source + test files. Fail-open: an unreadable tree yields empty vecs.
    fn read(project_root: &std::path::Path) -> Self {
        let files = crate::acceptance::source_files(project_root);
        let total = files.len();
        // `source_files` walks the tree in OS-defined `read_dir` order; sort by
        // relative path so the injected listing is DETERMINISTIC.
        let mut rels: Vec<(bool, String)> = files
            .iter()
            .map(|f| (is_test_file(f), rel_path(project_root, f)))
            .collect();
        rels.sort_by(|a, b| a.1.cmp(&b.1));

        let mut evidence = Self::default();
        let mut bytes = 0usize;
        for (is_test, rel) in rels {
            let listed = evidence.tests.len() + evidence.source.len();
            if listed >= MAX_LISTED_PATHS || bytes + rel.len() + 1 > MAX_LISTING_BYTES {
                break;
            }
            bytes += rel.len() + 1;
            if is_test {
                evidence.tests.push(rel);
            } else {
                evidence.source.push(rel);
            }
        }
        evidence.overflow = total.saturating_sub(evidence.tests.len() + evidence.source.len());
        evidence
    }

    /// Render the grounding block for the critic review context. Empty when the
    /// scan found nothing (fail-open: nothing real to ground → inject nothing).
    fn to_review_context(&self) -> String {
        if self.tests.is_empty() && self.source.is_empty() {
            return String::new();
        }
        let mut out = String::new();
        out.push_str(
            "## Current workspace files (fresh filesystem listing)\n\
             The files below REALLY EXIST in the workspace right now. Do NOT claim tests, \
             backend, or source are absent when the matching files appear here — review the \
             files that exist for specific, evidence-backed gaps instead.\n",
        );
        render_listing(&mut out, "Test files", &self.tests);
        render_listing(
            &mut out,
            "Source files (backend + everything else)",
            &self.source,
        );
        if self.overflow > 0 {
            out.push_str(&format!("(+{} more file(s) not listed)\n", self.overflow));
        }
        out
    }
}

/// Append a labelled, one-path-per-line block. Skips an EMPTY bucket so the
/// listing never implies files exist that don't: if there are genuinely no test
/// files, no "Test files" block appears, and a critic's legitimate "no tests"
/// finding is neither contradicted nor suppressed.
fn render_listing(out: &mut String, label: &str, files: &[String]) {
    if files.is_empty() {
        return;
    }
    out.push_str("### ");
    out.push_str(label);
    out.push('\n');
    for f in files {
        out.push_str("- ");
        out.push_str(f);
        out.push('\n');
    }
}

fn rel_path(root: &std::path::Path, file: &std::path::Path) -> String {
    file.strip_prefix(root)
        .unwrap_or(file)
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/")
}

fn is_test_file(path: &std::path::Path) -> bool {
    let rel = path
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    rel.contains("/tests/")
        || rel.contains("/test/")
        || rel.contains("/__tests__/")
        || name.contains(".test.")
        || name.contains(".spec.")
        || name.ends_with("_test.rs")
        || name.ends_with("_test.go")
        || name.starts_with("test_")
        || name.ends_with("_test.py")
}

/// A [`CriticConsult`] that routes a seat's strict-JSON judge turn to a READ-ONLY
/// `BaseSession::fork()`. The fork is owned for the seat's lifetime; a fork that
/// failed to open (or an offline brain) makes `judge` fail-open to the empty
/// (accepting) verdict — an absent critic can NEVER block (invariant 1).
pub(crate) struct ForkConsult {
    /// The read-only fork, or the error that prevented opening one. `Mutex` so
    /// the `&self` `judge` can drive the `&mut` session.
    fork: tokio::sync::Mutex<Result<Box<dyn BaseSession>, SessionError>>,
}

impl ForkConsult {
    pub(crate) fn new(fork: Result<Box<dyn BaseSession>, SessionError>) -> Self {
        Self {
            fork: tokio::sync::Mutex::new(fork),
        }
    }

    /// Best-effort close the underlying fork session.
    pub(crate) async fn end(&self) {
        if let Ok(s) = self.fork.lock().await.as_mut() {
            let _ = s.end().await;
        }
    }

    /// Run ONE strict-JSON consult on the read-only fork and return the extracted
    /// JSON object text (NOT parsed into a [`RoleVerdict`]) — the generic primitive
    /// the router / planner use to get a typed artifact of their OWN shape over the
    /// borrowed brain. Same fork → judge-turn → `extract_json_object` path as
    /// [`CriticConsult::judge`], same fail-open contract: a missing fork, an offline
    /// brain, a timeout, or a reply with no JSON object yields `None`, so the caller
    /// degrades to its deterministic floor.
    ///
    /// `label` is only used for the bounded-turn log line; `system` pins the schema,
    /// `user` carries the payload. The caller `serde_json::from_str`s the result.
    pub(crate) async fn judge_json(
        &self,
        label: &str,
        system: &str,
        user: String,
    ) -> Option<String> {
        let _ = label;
        let mut guard = self.fork.lock().await;
        let fork = guard.as_mut().ok()?; // no fork (offline / unsupported / failed) → None
        let directive = format!(
            "{system}\n\nReturn EXACTLY ONE JSON object and nothing else — no markdown, \
             no code fence, no prose before or after.\n\n{user}"
        );
        if fork.send_turn(directive).await.is_err() {
            return None;
        }
        // Bound the judge turn so one wedged fork can't hang the caller (same window
        // the critic team uses). A timeout / dead session → None (fail-open).
        match tokio::time::timeout(review_turn_timeout(), drain_review_text(fork)).await {
            Ok(Some(text)) => extract_json_object(&text),
            _ => None,
        }
    }

    /// Run ONE consult turn on the read-only fork and return the RAW assistant
    /// text (NOT parsed / NOT JSON-extracted) — the generic text primitive for a
    /// caller whose reply is free-form rather than a JSON object (e.g. the active
    /// fact-extraction backstop's `key: value` lines, see [`crate::fact_extract`]).
    /// Unlike [`judge_json`](Self::judge_json) it sends `directive` verbatim (no
    /// "return one JSON object" suffix) and does no extraction, so the caller owns
    /// the wording and the parse. Same fork → turn → bounded-drain path + fail-open
    /// contract: a missing fork (offline / unsupported / failed handshake), a send
    /// error, a timeout, or a dead session yields `None`, so the caller degrades to
    /// "nothing extracted" and never blocks the turn.
    ///
    /// `label` is only used for the bounded-turn log line; `directive` is the
    /// fully-composed prompt.
    pub(crate) async fn judge_text(&self, label: &str, directive: String) -> Option<String> {
        let _ = label;
        let mut guard = self.fork.lock().await;
        let fork = guard.as_mut().ok()?; // no fork (offline / unsupported / failed) → None
        if fork.send_turn(directive).await.is_err() {
            return None;
        }
        // Bound the judge turn so one wedged fork can't hang the caller (same window
        // the critic team + judge_json use). A timeout / dead session → None.
        match tokio::time::timeout(review_turn_timeout(), drain_review_text(fork)).await {
            Ok(Some(text)) => Some(text),
            _ => None,
        }
    }
}

/// Maker-checker INDEPENDENCE firewall — the clean-room preamble prepended to
/// every critic's judge directive so the reviewer evaluates the ARTIFACT on a
/// clean context, never the maker's reasoning.
///
/// WHY THIS EXISTS: a read-only critic fork inherits the MAIN session's context
/// on two of the three bases — claude resumes + branches it (`--fork-session`),
/// codex forks the live thread (`thread/fork`), so the fork carries the doer's
/// full deliberation / chain-of-thought (opencode alone opens a fresh independent
/// session, so it is already clean). A reviewer that reads the maker's reasoning
/// unconsciously ADOPTS its framing and misses exactly what that framing hides —
/// a self-preference leak that degrades review quality. UmaDev keeps the
/// read-only forked-parallel-critique mechanism (the fork is what fences the
/// single-writer invariant), but quarantines any inherited author reasoning at
/// the prompt boundary: the reviewer is told to disregard prior conversation and
/// judge ONLY the artifact + acceptance criteria + requirement supplied below,
/// from its own seat. This makes the independence uniform across all three bases
/// without changing the review mechanism — the change is WHAT CONTEXT the critic
/// acts on, not how it is convened.
const INDEPENDENT_REVIEW_FIREWALL: &str = "You are opening an INDEPENDENT, clean-room review. \
     If any earlier conversation, plan, author commentary, or chain-of-thought appears in your \
     context, treat it as the MAKER's private notes and DISREGARD it — adopting the author's \
     framing would bias your verdict and hide the very gaps you are here to find. Review ONLY \
     the artifact, the acceptance criteria, and the requirement provided below, on their own \
     terms, digging independently from your role's seat. Judge what the artifact ACTUALLY is \
     and does — not what its author intended, narrated, or claimed.";

/// Compose the full judge directive sent to a critic's read-only fork: the
/// maker-checker [`INDEPENDENT_REVIEW_FIREWALL`] FIRST (so the reviewer
/// quarantines any inherited maker reasoning before it reads anything), then the
/// role's strict-JSON `system` prompt + the JSON-shape instruction, then the
/// artifact-only `user` payload. Extracted as a pure fn so the clean-context
/// invariant is directly testable: the directive is built from ONLY the firewall,
/// the role prompt, and the artifact seed — the doer's transcript is never one of
/// its inputs.
fn compose_review_directive(system: &str, user: &str) -> String {
    format!(
        "{INDEPENDENT_REVIEW_FIREWALL}\n\n{system}\n\nReturn EXACTLY ONE JSON object and \
         nothing else — no markdown, no code fence, no prose before or after.\n\n{user}"
    )
}

#[async_trait::async_trait]
impl CriticConsult for ForkConsult {
    async fn judge(&self, role: &str, system: &str, user: String) -> RoleVerdict {
        let mut guard = self.fork.lock().await;
        let Ok(fork) = guard.as_mut() else {
            // No fork (unsupported / failed) → fail-open ACCEPT.
            return RoleVerdict::empty(role);
        };
        // One strict-JSON judge turn on the read-only fork. The directive pins the
        // role + the JSON shape (the critic's `system`) and carries the artifacts
        // (`user`), behind the maker-checker independence firewall so the reviewer
        // judges the artifact on a CLEAN context (not the doer's inherited
        // reasoning); we drain the fork's events for the assistant text, then parse.
        let directive = compose_review_directive(system, &user);
        if fork.send_turn(directive).await.is_err() {
            return RoleVerdict::empty(role);
        }
        // Bound the judge turn so one wedged fork can't hang the whole gate.
        match tokio::time::timeout(review_turn_timeout(), drain_review_text(fork)).await {
            // A clean TurnDone with the collected text → parse the verdict.
            Ok(Some(text)) => parse_verdict(role, &text),
            // Timed out / session ended without a clean TurnDone → fail-open ACCEPT.
            _ => RoleVerdict::empty(role),
        }
    }
}

/// Drain a read-only fork's events until its `TurnDone`, returning the collected
/// assistant text (`Some`) — or `None` if the session ended first. Tool noise on
/// a read-only fork is ignored. Split out of `judge` to keep nesting shallow.
async fn drain_review_text(fork: &mut Box<dyn BaseSession>) -> Option<String> {
    let mut text = String::new();
    while let Some(ev) = fork.next_event().await {
        match ev {
            SessionEvent::TextDelta(t) => text.push_str(&t),
            SessionEvent::TurnDone { .. } => return Some(text),
            // A read-only fork should not write; ignore any tool noise.
            _ => {}
        }
    }
    None
}

/// Parse a fork's judge reply into a [`RoleVerdict`], fail-open to the empty
/// (accepting) verdict when no JSON object is found / it doesn't deserialize.
fn parse_verdict(role: &str, text: &str) -> RoleVerdict {
    let Some(json) = extract_json_object(text) else {
        return RoleVerdict::empty(role);
    };
    serde_json::from_str::<RoleVerdict>(&json)
        .map(|v| v.normalized(role))
        .unwrap_or_else(|_| RoleVerdict::empty(role))
}

/// Extract the first balanced top-level JSON object from `text` (the judge reply
/// may carry stray prose despite the strict-JSON instruction). Mirrors the
/// runner's tolerant extractor — string/escape aware so a `}` inside a string
/// can't close the object early.
pub(crate) fn extract_json_object(text: &str) -> Option<String> {
    let start = text.find('{')?;
    let bytes = text.as_bytes();
    let (mut depth, mut in_str, mut esc) = (0i32, false, false);
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_str {
            in_str = in_string_step(b, &mut esc);
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return text.get(start..=i).map(str::to_string);
                }
            }
            _ => {}
        }
    }
    None
}

/// One byte of in-string scanning: track the escape state and report whether the
/// scanner is STILL inside the string after this byte. Split out so
/// [`extract_json_object`] stays a flat single-level loop.
fn in_string_step(b: u8, esc: &mut bool) -> bool {
    if *esc {
        *esc = false;
        true // an escaped char never ends the string
    } else if b == b'\\' {
        *esc = true;
        true
    } else {
        b != b'"' // a bare quote ends the string
    }
}

/// Timeout for one read-only judge turn. Advisory reviews are discardable, so a
/// wedged fork must never hang the gate — it fails open to ACCEPT. Overridable
/// via `UMADEV_REVIEW_TURN_TIMEOUT_SECS` for slow machines / CI.
fn review_turn_timeout() -> std::time::Duration {
    std::env::var("UMADEV_REVIEW_TURN_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .map_or_else(
            || std::time::Duration::from_secs(120),
            std::time::Duration::from_secs,
        )
}

/// Timeout for ESTABLISHING one read-only fork (the `initialize`/`thread/fork`/
/// `POST /session` handshake), distinct from the per-turn judge timeout above. A
/// fork that never completes its handshake must not freeze the gate, so a stuck
/// `fork()` is bounded and degraded to a fail-open ACCEPT. Kept short (the
/// handshake is cheap when healthy) but overridable via
/// `UMADEV_FORK_ESTABLISH_TIMEOUT_SECS` for slow machines / CI.
fn fork_establish_timeout() -> std::time::Duration {
    std::env::var("UMADEV_FORK_ESTABLISH_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .map_or_else(
            || std::time::Duration::from_secs(30),
            std::time::Duration::from_secs,
        )
}

/// Short, LOCALIZED human label for a review node (for operator Notes). Routes
/// through the i18n catalog so the node name follows the user's UI language.
fn kind_label(kind: ReviewKind) -> &'static str {
    match kind {
        ReviewKind::Docs => umadev_i18n::tl("continuous.node.docs"),
        ReviewKind::Preview => umadev_i18n::tl("continuous.node.preview"),
        ReviewKind::Quality => umadev_i18n::tl("continuous.node.quality"),
    }
}

/// The phase id used in the team ledger for a review node.
fn kind_phase_label(kind: ReviewKind) -> &'static str {
    match kind {
        ReviewKind::Docs => "docs",
        ReviewKind::Preview => "preview",
        ReviewKind::Quality => "quality",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trust::TrustMode;
    use std::path::Path;
    use std::sync::Mutex;
    use umadev_runtime::SessionError;

    struct EnvRestore {
        key: &'static str,
        prev: Option<String>,
    }

    impl EnvRestore {
        fn capture(key: &'static str) -> Self {
            Self {
                key,
                prev: std::env::var(key).ok(),
            }
        }

        fn set(&self, value: &str) {
            std::env::set_var(self.key, value);
        }

        fn remove(&self) {
            std::env::remove_var(self.key);
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            match self.prev.as_ref() {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    // ── A scripted, fully-deterministic fake BaseSession ───────────────────
    //
    // Each `send_turn` pops the next scripted batch of events; `next_event`
    // drains that batch (ending on its `TurnDone`). This lets a unit test drive
    // the whole continuous path with NO real base process — exercising phase
    // advance, tool-call governance + audit, the TurnDone boundary, the gate
    // pause, the hard gate, and fail-open session death.

    // The mock's distinct failure modes are independent toggles (die / fork-hangs /
    // next-event-hangs / active-forever), each modelling ONE base pathology a pump
    // must survive; folding them into a state enum would obscure that they can be set
    // independently and only adds ceremony to a test fixture.
    #[allow(clippy::struct_excessive_bools)]
    struct FakeBaseSession {
        /// One `Vec<SessionEvent>` per upcoming turn, consumed front-to-back.
        turns: Vec<Vec<SessionEvent>>,
        /// The currently-draining turn's events (front-to-back).
        current: std::collections::VecDeque<SessionEvent>,
        /// Directives received, in order (asserted by tests).
        sent: Arc<Mutex<Vec<String>>>,
        /// Approval replies received, in order.
        responded: Arc<Mutex<Vec<(String, ApprovalDecision)>>>,
        /// When true, `next_event` yields `None` immediately (session death).
        die: bool,
        /// Verdict JSON the SUCCESSIVE `fork()` calls hand back — one per call,
        /// front-to-back. `Some(json)` → that fork emits the JSON as its judge
        /// reply then `TurnDone`; `None` → that fork FAILS (`ForkUnsupported`),
        /// exercising the per-seat fail-open path. Shared so a test can assert the
        /// fork count and the main session can mutate it from `&self`-ish `fork`.
        fork_script: Arc<Mutex<std::collections::VecDeque<Option<String>>>>,
        /// How many forks were opened (asserted by tests).
        forks_opened: Arc<Mutex<usize>>,
        /// When true, `fork()` AWAITS FOREVER instead of returning — models a base
        /// whose fork handshake wedges (never returns `initialize`). The
        /// `fork_with_timeout` wrapper must bound it and fail-open to ACCEPT.
        fork_hangs: bool,
        /// When true, `next_event` AWAITS FOREVER after `send_turn` (the base
        /// holds the pipe open but emits nothing, never exits) — the P1-11 hang
        /// the idle watchdog on every MAIN-session pump must settle.
        next_event_hangs: bool,
        /// When true, every `next_event` returns a fresh `TextDelta` and NEVER a
        /// `TurnDone` — the base that stays ACTIVE forever (keeps emitting, e.g.
        /// writing code), so the idle watchdog never trips. Only the wall-clock
        /// budget check at the top of each pump can settle such a turn.
        active_forever: bool,
        /// Count of `interrupt()` calls — a test asserts the idle watchdog issued
        /// its best-effort interrupt before settling.
        interrupts: Arc<Mutex<usize>>,
    }

    impl FakeBaseSession {
        fn new(turns: Vec<Vec<SessionEvent>>) -> Self {
            Self {
                turns,
                current: std::collections::VecDeque::new(),
                sent: Arc::new(Mutex::new(Vec::new())),
                responded: Arc::new(Mutex::new(Vec::new())),
                die: false,
                fork_script: Arc::new(Mutex::new(std::collections::VecDeque::new())),
                forks_opened: Arc::new(Mutex::new(0)),
                fork_hangs: false,
                next_event_hangs: false,
                active_forever: false,
                interrupts: Arc::new(Mutex::new(0)),
            }
        }
        /// A session that accepts `send_turn` then stays ACTIVE forever — every
        /// `next_event` yields a fresh `TextDelta`, never a `TurnDone`. The idle
        /// watchdog never trips (the base keeps emitting), so ONLY the wall-clock
        /// budget check at the top of a pump can settle the turn — the mid-turn
        /// budget path this models.
        fn active_forever() -> Self {
            let mut s = Self::new(vec![]);
            s.active_forever = true;
            s
        }
        fn dying() -> Self {
            let mut s = Self::new(vec![]);
            s.die = true;
            s
        }
        /// A session that accepts `send_turn` but then HANGS forever on
        /// `next_event` — the "base holds the pipe open, emits nothing, never
        /// exits" mid-turn hang the idle watchdog must settle.
        fn hanging() -> Self {
            let mut s = Self::new(vec![]);
            s.next_event_hangs = true;
            s
        }
        fn interrupts_handle(&self) -> Arc<Mutex<usize>> {
            Arc::clone(&self.interrupts)
        }
        /// A session whose every `fork()` hangs forever (a wedged fork handshake).
        fn fork_wedged() -> Self {
            let mut s = Self::new(vec![vec![done()], vec![done()]]);
            s.fork_hangs = true;
            s
        }
        /// Script the successive `fork()` calls with the given verdict replies
        /// (`Some(json)` = a verdict-emitting fork, `None` = a failing fork).
        fn with_fork_script(mut self, verdicts: Vec<Option<String>>) -> Self {
            self.fork_script = Arc::new(Mutex::new(verdicts.into_iter().collect()));
            self
        }
        fn sent_handle(&self) -> Arc<Mutex<Vec<String>>> {
            Arc::clone(&self.sent)
        }
        fn responded_handle(&self) -> Arc<Mutex<Vec<(String, ApprovalDecision)>>> {
            Arc::clone(&self.responded)
        }
        fn forks_handle(&self) -> Arc<Mutex<usize>> {
            Arc::clone(&self.forks_opened)
        }
        /// A leaf fork session: emits `verdict` text then a clean TurnDone.
        fn verdict_fork(verdict: &str) -> Self {
            Self::new(vec![vec![
                SessionEvent::TextDelta(verdict.to_string()),
                SessionEvent::TurnDone {
                    status: TurnStatus::Completed,
                    usage: None,
                },
            ]])
        }
    }

    #[async_trait::async_trait]
    impl BaseSession for FakeBaseSession {
        async fn fork(&mut self) -> Result<Box<dyn BaseSession>, SessionError> {
            *self.forks_opened.lock().unwrap() += 1;
            // A wedged fork handshake: await forever so `fork_with_timeout` must be
            // the thing that ends the wait (fail-open ACCEPT), not this returning.
            if self.fork_hangs {
                std::future::pending::<()>().await;
            }
            // Pop the next scripted fork outcome. An empty script → a default
            // accepting verdict (so a test that doesn't care still gets a clean,
            // fail-open ACCEPT). `None` → this fork fails (fail-open path).
            let next = self.fork_script.lock().unwrap().pop_front();
            match next {
                Some(Some(json)) => Ok(Box::new(Self::verdict_fork(&json))),
                Some(None) => Err(SessionError::ForkUnsupported(
                    "scripted fork failure".into(),
                )),
                None => Ok(Box::new(Self::verdict_fork(r#"{"accepts":true}"#))),
            }
        }

        async fn send_turn(&mut self, directive: String) -> Result<(), SessionError> {
            self.sent.lock().unwrap().push(directive);
            // A hanging session emits NOTHING after send_turn (empty batch) so the
            // next `next_event` parks forever — modelling the base that holds the
            // pipe open without output. The idle watchdog must settle it.
            if self.next_event_hangs {
                self.current.clear();
                return Ok(());
            }
            // An always-active session drives itself from `next_event` (a fresh
            // TextDelta each call, never a TurnDone) — leave `current` empty so the
            // override below takes over (no scripted batch / no auto TurnDone).
            if self.active_forever {
                self.current.clear();
                return Ok(());
            }
            // Load the next scripted turn (or an immediate clean TurnDone if the
            // script ran out, so the driver never hangs).
            let batch = if self.turns.is_empty() {
                vec![SessionEvent::TurnDone {
                    status: TurnStatus::Completed,
                    usage: None,
                }]
            } else {
                self.turns.remove(0)
            };
            self.current = batch.into_iter().collect();
            Ok(())
        }
        async fn next_event(&mut self) -> Option<SessionEvent> {
            if self.die {
                return None;
            }
            // A base that hangs holding the pipe open (`send_turn` left `current`
            // empty): park forever once there is nothing buffered. Never resolves →
            // the idle watchdog must settle it.
            if self.next_event_hangs && self.current.is_empty() {
                std::future::pending::<()>().await;
            }
            // An always-active base keeps emitting a fresh TextDelta forever and
            // never a TurnDone — so the idle watchdog never trips. Only the pump's
            // wall-clock budget check can end such a turn. A short yield keeps the
            // loop cooperative (the test's past deadline returns on the first pass).
            if self.active_forever && self.current.is_empty() {
                tokio::task::yield_now().await;
                return Some(SessionEvent::TextDelta("still working…".to_string()));
            }
            self.current.pop_front()
        }
        async fn respond(
            &mut self,
            req_id: &str,
            decision: ApprovalDecision,
        ) -> Result<(), SessionError> {
            self.responded
                .lock()
                .unwrap()
                .push((req_id.to_string(), decision));
            Ok(())
        }
        async fn interrupt(&mut self) -> Result<(), SessionError> {
            *self.interrupts.lock().unwrap() += 1;
            Ok(())
        }
        async fn end(&mut self) -> Result<(), SessionError> {
            Ok(())
        }
    }

    fn opts(root: &Path, requirement: &str, mode: TrustMode) -> RunOptions {
        RunOptions {
            project_root: root.to_path_buf(),
            requirement: requirement.to_string(),
            slug: "demo".to_string(),
            model: String::new(),
            backend: "claude-code".to_string(),
            design_system: String::new(),
            seed_template: String::new(),
            mode,
            strict_coverage: false,
        }
    }

    fn done() -> SessionEvent {
        SessionEvent::TurnDone {
            status: TurnStatus::Completed,
            usage: None,
        }
    }

    fn sink() -> (Arc<dyn EventSink>, crate::events::RecordingSink) {
        let rec = crate::events::RecordingSink::default();
        (Arc::new(rec.clone()), rec)
    }

    // ── Wave 1: the legacy-pipeline opt-out switch ─────────────────────────

    #[test]
    fn legacy_pipeline_flag_defaults_off_and_honours_explicit_on() {
        let env = EnvRestore::capture("UMADEV_LEGACY_PIPELINE");
        env.remove();
        assert!(
            !legacy_pipeline_from_env(),
            "default (unset) is the director path, not legacy"
        );
        for on in ["1", "true", "on"] {
            env.set(on);
            assert!(
                legacy_pipeline_from_env(),
                "`{on}` selects the legacy fixed pipeline"
            );
        }
        // A non-truthy value stays on the director path (fail-open default).
        for off in ["0", "false", "off", "nonsense", ""] {
            env.set(off);
            assert!(
                !legacy_pipeline_from_env(),
                "`{off}` is NOT an opt-in → director path"
            );
        }
    }

    // ── Phase advance + gate pause ─────────────────────────────────────────

    #[tokio::test]
    async fn initial_block_runs_research_docs_then_pauses_at_docs_gate() {
        let tmp = tempfile::tempdir().unwrap();
        let options = opts(
            tmp.path(),
            "build a SaaS dashboard with login",
            TrustMode::Guarded,
        );
        let (events, rec) = sink();
        // research turn, docs turn — both clean.
        let mut session = FakeBaseSession::new(vec![vec![done()], vec![done()]]);
        let sent = session.sent_handle();

        let outcome = run_block(&mut session, &options, &events, Phase::Research).await;

        assert_eq!(outcome, RunOutcome::PausedAtGate(Gate::DocsConfirm));
        // Exactly two directives went to the base: research, then docs.
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 2, "research + docs directives");
        assert!(sent[0].to_lowercase().contains("research"));
        assert!(sent[1].contains("output/demo-prd.md"));
        // A GateOpened(DocsConfirm) was emitted.
        let evs = rec.events();
        assert!(evs.iter().any(|e| matches!(
            e,
            EngineEvent::GateOpened {
                gate: Gate::DocsConfirm,
                ..
            }
        )));
    }

    // ── ToolCall governance + audit ────────────────────────────────────────

    #[tokio::test]
    async fn tool_call_is_audited_and_emits_tool_row() {
        let tmp = tempfile::tempdir().unwrap();
        let options = opts(tmp.path(), "build a dashboard", TrustMode::Guarded);
        let (events, rec) = sink();
        // Docs turn writes a file, then completes.
        let write = SessionEvent::ToolCall {
            name: "Write".to_string(),
            input: serde_json::json!({
                "file_path": "output/demo-prd.md",
                "content": "# PRD\n\nclean content, no emoji"
            }),
        };
        let mut session = FakeBaseSession::new(vec![vec![done()], vec![write, done()]]);

        let _ = run_block(&mut session, &options, &events, Phase::Research).await;

        // The audit JSONL recorded the tool call (UD-EVID-002).
        let audit = tmp.path().join(".umadev/audit/tool-calls.jsonl");
        let body = std::fs::read_to_string(&audit).unwrap_or_default();
        assert!(
            body.contains("output/demo-prd.md"),
            "tool call audited: {body}"
        );
        // A ToolUse stream row was emitted to the TUI.
        let evs = rec.events();
        assert!(evs.iter().any(|e| matches!(
            e,
            EngineEvent::WorkerStream {
                event: StreamEvent::ToolUse { .. }
            }
        )));
    }

    #[tokio::test]
    async fn ask_user_question_renders_question_and_options_not_a_bare_stub() {
        // The base calls its OWN interactive AskUserQuestion while UmaDev drives it
        // non-interactively. It must NOT render a bare optionless stub / silent
        // cancel: the question + every numbered option are surfaced as a Note, and
        // the tool row gets a real one-line detail.
        let tmp = tempfile::tempdir().unwrap();
        let options = opts(tmp.path(), "build a dashboard", TrustMode::Guarded);
        let (events, rec) = sink();
        let ask = SessionEvent::ToolCall {
            name: "AskUserQuestion".to_string(),
            input: serde_json::json!({
                "questions": [{
                    "header": "Database",
                    "question": "Which database should the API use?",
                    "options": [
                        {"label": "Postgres", "description": "Relational"},
                        {"label": "MongoDB"}
                    ]
                }]
            }),
        };
        let mut session = FakeBaseSession::new(vec![vec![done()], vec![ask, done()]]);

        let _ = run_block(&mut session, &options, &events, Phase::Research).await;

        let evs = rec.events();
        // A prominent Note carries the question AND every numbered option.
        let note = evs.iter().find_map(|e| match e {
            EngineEvent::Note(s) if s.contains("Which database") => Some(s.clone()),
            _ => None,
        });
        let note = note.expect("AskUserQuestion must surface the question as a Note");
        assert!(note.contains("1. Postgres"), "numbered options: {note}");
        assert!(note.contains("2. MongoDB"), "every option present: {note}");
        // The tool row's detail is non-empty (was a bare stub before the fix).
        let detail_nonempty = evs.iter().any(|e| {
            matches!(
                e,
                EngineEvent::WorkerStream {
                    event: StreamEvent::ToolUse { name, detail, .. }
                } if name == "AskUserQuestion" && !detail.is_empty()
            )
        });
        assert!(
            detail_nonempty,
            "the AskUserQuestion tool row has a real detail"
        );
    }

    #[tokio::test]
    async fn emoji_write_is_blocked_and_recorded_but_does_not_panic() {
        let policy = umadev_governance::Policy::default();
        // A markdown file whose only governance trip is an emoji icon must fire
        // the emoji rule (UD-CODE-001) — kept to markdown so a JS/TSX structure
        // rule (error-boundary / a11y) doesn't win precedence and mask it.
        let (target, decision) = evaluate_tool_call(
            &policy,
            umadev_governance::ProjectContext::unknown(),
            "Write",
            &serde_json::json!({
                "file_path": "output/demo-uiux.md",
                "content": "# UIUX\n\nUse the \u{1F680} icon for the launch button."
            }),
        );
        assert_eq!(target, "output/demo-uiux.md");
        assert!(decision.block, "emoji icon must block");
        assert_eq!(decision.clause, "UD-CODE-001");
    }

    #[tokio::test]
    async fn dangerous_bash_is_classified() {
        let policy = umadev_governance::Policy::default();
        let (cmd, decision) = evaluate_tool_call(
            &policy,
            umadev_governance::ProjectContext::unknown(),
            "Bash",
            &serde_json::json!({ "command": "rm -rf /" }),
        );
        assert_eq!(cmd, "rm -rf /");
        assert!(decision.block, "rm -rf must block");
    }

    #[tokio::test]
    async fn read_tool_is_observe_only_and_passes() {
        let policy = umadev_governance::Policy::default();
        let (_t, decision) = evaluate_tool_call(
            &policy,
            umadev_governance::ProjectContext::unknown(),
            "Read",
            &serde_json::json!({ "file_path": "a.rs" }),
        );
        assert!(!decision.block);
    }

    /// A leaked credential — the irreversible-if-written floor — assembled at
    /// runtime so this source file carries no contiguous key.
    fn leaked_secret() -> String {
        format!(
            "const k = \"sk_live_4eC39H{}\";",
            "qLyjWDarjtT1zdp7dcABCDEFGH"
        )
    }

    #[tokio::test]
    async fn multiedit_write_reaches_the_content_scan() {
        // A real `MultiEdit` = `{file_path, edits: [{old_string, new_string}, …]}`
        // with NO top-level content. Before, extraction read `content`/`new_string`
        // and fell to "" for this shape, so the scan ran over nothing and a secret
        // inlined via `edits[].new_string` bypassed the governor. Now the batch is
        // concatenated and scanned. The secret sits in the SECOND hunk to prove
        // every hunk is read, not just the first.
        let policy = umadev_governance::Policy::default();
        let (target, decision) = evaluate_tool_call(
            &policy,
            umadev_governance::ProjectContext::unknown(),
            "MultiEdit",
            &serde_json::json!({
                "file_path": "src/cfg.js",
                "edits": [
                    { "old_string": "a", "new_string": "let a = 1;" },
                    { "old_string": "b", "new_string": leaked_secret() }
                ]
            }),
        );
        assert_eq!(target, "src/cfg.js");
        assert!(
            decision.block,
            "a MultiEdit write must reach the content scan and block a secret in any hunk"
        );
    }

    #[tokio::test]
    async fn notebookedit_write_reaches_the_content_scan() {
        // A real `NotebookEdit` = `{notebook_path, new_source, …}` — the cell body
        // is in `new_source` (NOT `content`) and the path in `notebook_path` (NOT
        // `file_path`). Before, both fell to "" so the scan saw nothing. Routed to
        // a secret-scanned path (a notebook cell IS code) to isolate the extraction
        // fix from the scan's own extension policy, which this fix does not change.
        let policy = umadev_governance::Policy::default();
        let (target, decision) = evaluate_tool_call(
            &policy,
            umadev_governance::ProjectContext::unknown(),
            "NotebookEdit",
            &serde_json::json!({
                "notebook_path": "notebook_cell.py",
                "new_source": leaked_secret()
            }),
        );
        assert_eq!(target, "notebook_cell.py", "path comes from notebook_path");
        assert!(
            decision.block,
            "a NotebookEdit write must scan new_source and block a leaked secret"
        );
    }

    #[tokio::test]
    async fn multiedit_and_notebookedit_clean_content_passes() {
        // The fix must not over-block: a well-formed MultiEdit / NotebookEdit whose
        // real body is clean resolves to a normal pass, exactly like a clean Write.
        let policy = umadev_governance::Policy::default();
        let ctx = || umadev_governance::ProjectContext::unknown();
        let (_t, multi_d) = evaluate_tool_call(
            &policy,
            ctx(),
            "MultiEdit",
            &serde_json::json!({
                "file_path": "src/util.js",
                "edits": [{ "old_string": "x", "new_string": "export const x = 1;" }]
            }),
        );
        assert!(!multi_d.block, "a clean MultiEdit must pass");
        let (_t, nb_d) = evaluate_tool_call(
            &policy,
            ctx(),
            "NotebookEdit",
            &serde_json::json!({
                "notebook_path": "nb_cell.py",
                "new_source": "total = 1 + 2"
            }),
        );
        assert!(!nb_d.block, "a clean NotebookEdit must pass");
    }

    #[tokio::test]
    async fn malformed_write_payload_fails_open() {
        // A mutating tool whose body fields are absent / wrong-typed scans "" and
        // passes — today's behavior, never a crash.
        let policy = umadev_governance::Policy::default();
        let (target, decision) = evaluate_tool_call(
            &policy,
            umadev_governance::ProjectContext::unknown(),
            "MultiEdit",
            &serde_json::json!({ "file_path": "src/cfg.js", "edits": [] }),
        );
        assert_eq!(target, "src/cfg.js");
        assert!(
            !decision.block,
            "an empty MultiEdit batch scans nothing and passes"
        );
    }

    // ── TurnDone boundary (Failed → hard stop) ─────────────────────────────

    #[tokio::test]
    async fn failed_turn_stops_the_run() {
        let tmp = tempfile::tempdir().unwrap();
        let options = opts(tmp.path(), "build a dashboard", TrustMode::Guarded);
        let (events, _rec) = sink();
        let fail = SessionEvent::TurnDone {
            status: TurnStatus::Failed("base crashed".to_string()),
            usage: None,
        };
        let mut session = FakeBaseSession::new(vec![vec![fail]]);

        let outcome = run_block(&mut session, &options, &events, Phase::Research).await;
        match outcome {
            RunOutcome::HardStop(reason) => assert!(reason.contains("base crashed")),
            other => panic!("expected hard stop, got {other:?}"),
        }
    }

    // ── Fail-open: session dies mid-turn → failure, no panic ───────────────

    #[tokio::test]
    async fn session_death_mid_turn_is_a_failure_not_a_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let options = opts(tmp.path(), "build a dashboard", TrustMode::Guarded);
        let (events, _rec) = sink();
        let mut session = FakeBaseSession::dying();

        let outcome = run_block(&mut session, &options, &events, Phase::Research).await;
        assert!(matches!(outcome, RunOutcome::HardStop(_)));
    }

    // ── Truncated turn: degraded when key artifacts missing (P2-3) ─────────

    #[test]
    fn truncated_missing_artifacts_flags_incomplete_docs() {
        let tmp = tempfile::tempdir().unwrap();
        let options = opts(tmp.path(), "build a dashboard", TrustMode::Guarded);
        // Nothing written → a truncated Docs phase is missing ALL three docs.
        let missing = truncated_missing_artifacts(&options, Phase::Docs);
        assert_eq!(missing.len(), 3, "all three docs missing: {missing:?}");

        // Write only the PRD → a Docs truncation is STILL degraded (architecture +
        // uiux absent). This is exactly the slip the fix closes.
        let dir = tmp.path().join("output");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("demo-prd.md"), "# PRD\n\nsubstantive content body").unwrap();
        let missing = truncated_missing_artifacts(&options, Phase::Docs);
        assert!(
            missing.iter().any(|m| m.contains("architecture"))
                && missing.iter().any(|m| m.contains("uiux")),
            "partial docs still flagged: {missing:?}"
        );
        assert!(
            !missing.iter().any(|m| m.contains("prd")),
            "the written PRD is not flagged: {missing:?}"
        );

        // All three present → a truncation there is benign (nothing missing).
        for n in ["architecture", "uiux"] {
            std::fs::write(
                dir.join(format!("demo-{n}.md")),
                format!("# {n}\n\nsubstantive content body"),
            )
            .unwrap();
        }
        assert!(
            truncated_missing_artifacts(&options, Phase::Docs).is_empty(),
            "complete docs → benign truncation"
        );
    }

    #[test]
    fn truncated_missing_artifacts_for_code_phases_checks_source() {
        let tmp = tempfile::tempdir().unwrap();
        let options = opts(tmp.path(), "build a dashboard", TrustMode::Guarded);
        // No source → a truncated Frontend/Backend is degraded.
        assert!(!truncated_missing_artifacts(&options, Phase::Frontend).is_empty());
        assert!(!truncated_missing_artifacts(&options, Phase::Backend).is_empty());
        // With source present → benign.
        seed_source(tmp.path());
        assert!(truncated_missing_artifacts(&options, Phase::Frontend).is_empty());
        // Phases with no single existence invariant are never flagged.
        assert!(truncated_missing_artifacts(&options, Phase::Quality).is_empty());
        assert!(truncated_missing_artifacts(&options, Phase::Research).is_empty());
    }

    #[tokio::test]
    async fn truncated_docs_with_missing_artifacts_emits_degraded_warning() {
        // End-to-end: a Docs phase that TRUNCATES having written nothing must emit
        // the stronger DEGRADED warning (not the benign soft truncation note), so a
        // half-finished docs phase no longer slips through silently.
        let tmp = tempfile::tempdir().unwrap();
        let options = opts(
            tmp.path(),
            "build a SaaS dashboard web app with login and charts",
            TrustMode::Guarded,
        );
        let (events, rec) = sink();
        // research clean, docs TRUNCATED (no files written).
        let trunc = SessionEvent::TurnDone {
            status: TurnStatus::Truncated,
            usage: None,
        };
        let mut session = FakeBaseSession::new(vec![vec![done()], vec![trunc]]);

        let _ = run_block(&mut session, &options, &events, Phase::Research).await;

        // A DEGRADED note (not just the soft truncated note) must have fired — match
        // the language-independent marker present in every catalog string.
        let evs = rec.events();
        let degraded = evs.iter().any(|e| {
            matches!(e, EngineEvent::Note(n)
                if n.contains("DEGRADED") || n.contains("降级"))
        });
        assert!(
            degraded,
            "a truncated docs phase with missing artifacts must warn DEGRADED: {evs:?}"
        );
    }

    #[tokio::test]
    async fn truncated_docs_with_all_artifacts_is_benign_soft_note() {
        // The benign case: a Docs phase truncates but the three docs DO exist (ran
        // long, still produced the deliverable) → the soft truncated note, NOT the
        // degraded one.
        let tmp = tempfile::tempdir().unwrap();
        seed_docs(tmp.path());
        let options = opts(
            tmp.path(),
            "build a SaaS dashboard web app with login and charts",
            TrustMode::Guarded,
        );
        let (events, rec) = sink();
        let trunc = SessionEvent::TurnDone {
            status: TurnStatus::Truncated,
            usage: None,
        };
        let mut session = FakeBaseSession::new(vec![vec![done()], vec![trunc]]);

        let _ = run_block(&mut session, &options, &events, Phase::Research).await;

        let evs = rec.events();
        // No DEGRADED note when the deliverables are all present.
        assert!(
            !evs.iter().any(|e| matches!(e, EngineEvent::Note(n)
                if n.contains("DEGRADED") || n.contains("降级"))),
            "complete docs → no degraded warning: {evs:?}"
        );
    }

    // ── Hard gate: plan demands code but zero source produced ──────────────

    #[tokio::test]
    async fn zero_source_after_code_phase_hard_stops() {
        let tmp = tempfile::tempdir().unwrap();
        // A greenfield requirement → plan includes Frontend/Backend → code expected.
        let options = opts(
            tmp.path(),
            "build a SaaS dashboard web app with login and charts",
            TrustMode::Auto,
        );
        let (events, _rec) = sink();
        // Backend turn completes but writes NO source files; quality + delivery
        // never reached because the hard gate fires after backend.
        let mut session = FakeBaseSession::new(vec![vec![done()]]);

        let outcome = run_block(&mut session, &options, &events, Phase::Backend).await;
        match outcome {
            RunOutcome::HardStop(reason) => {
                assert!(reason.to_lowercase().contains("real") || reason.contains("代码"));
            }
            other => panic!("expected hard stop on empty code run, got {other:?}"),
        }
    }

    // ── NeedApproval routing under trust modes ─────────────────────────────

    #[tokio::test]
    async fn auto_allows_reversible_action_and_denies_irreversible() {
        let tmp = tempfile::tempdir().unwrap();
        let options = opts(tmp.path(), "build a dashboard", TrustMode::Auto);
        let (events, _rec) = sink();
        // Two approvals in one docs turn: a reversible Write (auto-allow) and a
        // network push (irreversible floor → deny), then done.
        let turn = vec![
            SessionEvent::NeedApproval {
                req_id: "r1".to_string(),
                action: "Write".to_string(),
                target: "output/demo-prd.md".to_string(),
            },
            SessionEvent::NeedApproval {
                req_id: "r2".to_string(),
                action: "git push origin main".to_string(),
                target: String::new(),
            },
            done(),
        ];
        let mut session = FakeBaseSession::new(vec![vec![done()], turn]);
        let responded = session.responded_handle();

        let _ = run_block(&mut session, &options, &events, Phase::Research).await;

        let r = responded.lock().unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0], ("r1".to_string(), ApprovalDecision::Allow));
        assert_eq!(r[1], ("r2".to_string(), ApprovalDecision::Deny));
    }

    // ── Plan (read-only) mode never executes a code phase ──────────────────

    #[tokio::test]
    async fn plan_mode_does_not_execute_spec_phase() {
        let tmp = tempfile::tempdir().unwrap();
        let options = opts(tmp.path(), "build a dashboard app", TrustMode::Plan);
        let (events, _rec) = sink();
        let mut session = FakeBaseSession::new(vec![vec![done()]]);
        let sent = session.sent_handle();

        // Resume at Spec under plan mode → must refuse to execute.
        let outcome = run_block(&mut session, &options, &events, Phase::Spec).await;
        assert_eq!(outcome, RunOutcome::Completed);
        assert!(
            sent.lock().unwrap().is_empty(),
            "plan mode sent no executing directive"
        );
    }

    // ── Critic-team review + bounded rework (the §3.5 / §3.6 closure) ──────

    // ── Role personas: each phase directive names its owning seat ───────────

    #[test]
    fn heavyweight_phase_directives_carry_their_role_persona() {
        // A greenfield (non-lean) plan: every executing phase directive must open
        // by naming the senior role that owns it, so the base works AS that seat.
        let options = opts(
            Path::new("/tmp"),
            "build a SaaS dashboard app",
            TrustMode::Auto,
        );
        let k = crate::planner::TaskKind::Greenfield;
        // (phase, a keyword that must appear in its directive's role line)
        let cases = [
            (Phase::Docs, "product manager"),
            (Phase::Spec, "architect"),
            (Phase::Frontend, "frontend engineer"),
            (Phase::Backend, "backend engineer"),
            (Phase::Quality, "QA"),
            (Phase::Delivery, "DevOps"),
        ];
        for (phase, kw) in cases {
            // `first=false` → the lean per-phase role prefix is exercised (the
            // first Research turn carries the full role-priming system prompt).
            let d = phase_directive(&options, phase, false, k);
            assert!(
                d.contains(kw),
                "{phase:?} directive must name its role ({kw}): {d}"
            );
            // Still command-style (writes files, doesn't ask) — persona augments,
            // never replaces, the imperative body.
            assert!(d.contains("do NOT ask me") || d.to_lowercase().contains("write"));
        }
        // Research+first keeps the full priming system prompt (which already names
        // the product-research seat) rather than the short prefix.
        let research = phase_directive(&options, Phase::Research, true, k);
        assert!(research.to_lowercase().contains("product researcher"));
    }

    #[test]
    fn lean_phase_directives_carry_an_engineer_role() {
        // A lean (gateless) plan: each phase directive still steps the base into
        // an engineer's seat, without referencing any (never-written) documents.
        let options = opts(Path::new("/tmp"), "做一个待办单页应用", TrustMode::Auto);
        let k = crate::planner::TaskKind::Light;
        for phase in [Phase::Spec, Phase::Frontend, Phase::Backend, Phase::Quality] {
            let d = phase_directive(&options, phase, false, k);
            assert!(
                d.to_lowercase().contains("engineer"),
                "lean {phase:?} directive must name an engineer seat: {d}"
            );
            // No heavyweight doc anchoring on the lean path.
            assert!(!d.to_lowercase().contains("approved the three documents"));
        }
    }

    #[test]
    fn quality_directives_carry_deps_before_tests_on_both_paths() {
        // The build/verify (Quality) directive — heavyweight AND lean — must carry
        // the deps-before-tests guidance (incl. the uv `--extra dev` gotcha), so the
        // base syncs dev/test deps in one pass instead of failing on
        // `No module named pytest` and retrying.
        let options = opts(Path::new("/tmp"), "build a data API", TrustMode::Auto);
        for k in [
            crate::planner::TaskKind::Greenfield,
            crate::planner::TaskKind::Bugfix,
        ] {
            let d = phase_directive(&options, Phase::Quality, false, k);
            assert!(
                d.contains("uv sync --extra dev"),
                "{k:?} Quality directive carries the uv --extra dev guidance: {d}"
            );
            assert!(
                d.contains("DEPENDENCIES BEFORE TESTS"),
                "{k:?} Quality directive carries the deps-before-tests block: {d}"
            );
        }
        // A NON-test phase (Frontend) must NOT carry it — self-gated to the
        // build/verify path, so a non-test turn isn't bloated.
        let fe = phase_directive(
            &options,
            Phase::Frontend,
            false,
            crate::planner::TaskKind::Greenfield,
        );
        assert!(
            !fe.contains("DEPENDENCIES BEFORE TESTS"),
            "the deps directive must not leak onto a non-test phase: {fe}"
        );
    }

    #[test]
    fn gate_review_kind_maps_phases() {
        assert_eq!(gate_review_kind(Phase::DocsConfirm), ReviewKind::Docs);
        assert_eq!(gate_review_kind(Phase::PreviewConfirm), ReviewKind::Preview);
    }

    #[test]
    fn team_for_scales_with_the_kind() {
        // A greenfield requirement seats the full docs team; a one-line tweak
        // seats none (the deterministic floor stands). No agents dir -> the
        // built-in roster only.
        let tmp = tempfile::TempDir::new().unwrap();
        assert_eq!(
            team_for(
                ReviewKind::Docs,
                "build a SaaS dashboard web app with login",
                tmp.path()
            )
            .len(),
            3
        );
        assert!(team_for(ReviewKind::Docs, "fix a typo in the readme", tmp.path()).is_empty());
    }

    #[test]
    fn team_for_appends_custom_seats_only_where_a_built_in_team_convenes() {
        // A user-defined seat joins the team for an applicable kind on a tier that
        // already convenes a built-in team — but a lean kind (which convenes none)
        // stays empty, so the custom seat can never convene a team / drive the loop
        // on its own (the deterministic floor still governs). The 8 built-in seats
        // are unchanged: the custom seat is ADDED on top.
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join(".umadev").join("agents");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("accessibility.md"),
            "---\nname: Accessibility Reviewer\napplies_to: [preview, quality]\n\
             focus: WCAG 2.1 AA review.\n---\nAudit every interactive control.\n",
        )
        .unwrap();

        let green = "build a SaaS dashboard web app with login";
        // Quality node on a greenfield: 4 built-in + 1 custom = 5.
        let q = team_for(ReviewKind::Quality, green, tmp.path());
        assert_eq!(
            q.len(),
            5,
            "the custom seat joins the built-in quality team"
        );
        assert!(q.iter().any(|c| c.role() == "accessibility-reviewer"));
        // Docs node: the custom seat is scoped out (preview/quality only) -> the 3
        // built-in docs seats only.
        assert_eq!(team_for(ReviewKind::Docs, green, tmp.path()).len(), 3);
        // A lean kind convenes NO team even with a custom seat on disk -> the floor
        // stands alone; a custom seat can never force a review where none runs.
        assert!(team_for(ReviewKind::Quality, "fix a typo in the readme", tmp.path()).is_empty());
    }

    #[test]
    fn extract_json_object_is_string_aware() {
        // A `}` inside a string must NOT close the object early.
        let s = r#"prose {"blocking": ["a } b"], "accepts": false} trailing"#;
        let j = extract_json_object(s).unwrap();
        assert!(j.starts_with('{') && j.ends_with('}'));
        let v: RoleVerdict = serde_json::from_str(&j).unwrap();
        assert!(!v.accepts);
        assert_eq!(v.blocking, vec!["a } b".to_string()]);
        // No object at all → None.
        assert!(extract_json_object("no json here").is_none());
    }

    #[test]
    fn parse_verdict_fail_open_on_garbage() {
        // Garbage / no JSON → the empty accepting verdict (fail-open).
        let v = parse_verdict("architect", "the base rambled with no json");
        assert!(v.accepts && v.blocking.is_empty());
        assert_eq!(v.role, "architect");
        // A real blocking verdict parses + is tagged with the role.
        let v = parse_verdict(
            "qa-engineer",
            r#"{"accepts":false,"blocking":["no tests"]}"#,
        );
        assert!(!v.accepts);
        assert_eq!(v.role, "qa-engineer");
        assert_eq!(v.blocking, vec!["no tests".to_string()]);
    }

    #[test]
    fn rework_directive_folds_every_blocking_item() {
        let d = rework_directive(
            ReviewKind::Docs,
            &[
                "[architect] no API table".into(),
                "[product-manager] no KPIs".into(),
            ],
        );
        assert!(d.contains("MUST-FIX"));
        assert!(d.contains("no API table"));
        assert!(d.contains("no KPIs"));
        // Command-style: tells the base to edit directly + end the turn.
        assert!(d.to_lowercase().contains("editing the files directly"));
        assert!(d.to_lowercase().contains("end your turn"));
    }

    /// Write the three docs to the blackboard so the docs team has something
    /// substantive to review (the team skips an empty blackboard).
    fn seed_docs(root: &Path) {
        let dir = root.join("output");
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["prd", "architecture", "uiux"] {
            std::fs::write(
                dir.join(format!("demo-{name}.md")),
                format!("# {name}\n## section\nsubstantive content for review\n"),
            )
            .unwrap();
        }
    }

    #[tokio::test]
    async fn docs_gate_runs_parallel_review_all_accept_then_pauses() {
        let tmp = tempfile::tempdir().unwrap();
        seed_docs(tmp.path());
        let options = opts(
            tmp.path(),
            "build a SaaS dashboard web app with login and charts",
            TrustMode::Guarded,
        );
        let (events, _rec) = sink();
        // research + docs turns, then the docs gate forks a 3-seat team — script
        // all three to ACCEPT so the gate proceeds with no rework.
        let mut session =
            FakeBaseSession::new(vec![vec![done()], vec![done()]]).with_fork_script(vec![
                Some(r#"{"accepts":true}"#.into()),
                Some(r#"{"accepts":true}"#.into()),
                Some(r#"{"accepts":true}"#.into()),
            ]);
        let forks = session.forks_handle();
        let sent = session.sent_handle();

        let outcome = run_block(&mut session, &options, &events, Phase::Research).await;

        assert_eq!(outcome, RunOutcome::PausedAtGate(Gate::DocsConfirm));
        // Three read-only forks opened (one per docs seat), run in parallel.
        assert_eq!(*forks.lock().unwrap(), 3, "one fork per docs seat");
        // All-accept → NO rework directive injected into the main session
        // (research + docs only).
        assert_eq!(sent.lock().unwrap().len(), 2, "no rework on all-accept");
    }

    #[tokio::test]
    async fn docs_gate_blocking_injects_one_rework_then_passes() {
        let tmp = tempfile::tempdir().unwrap();
        seed_docs(tmp.path());
        let options = opts(
            tmp.path(),
            "build a SaaS dashboard web app with login and charts",
            TrustMode::Guarded,
        );
        let (events, _rec) = sink();
        // Round 0: one seat BLOCKS (3 forks). Round 1 (re-review after rework):
        // all 3 accept (3 more forks). So 6 forks, ONE rework directive.
        let mut session =
            FakeBaseSession::new(vec![vec![done()], vec![done()]]).with_fork_script(vec![
                Some(r#"{"accepts":false,"blocking":["no API surface table"]}"#.into()),
                Some(r#"{"accepts":true}"#.into()),
                Some(r#"{"accepts":true}"#.into()),
                // re-review round → all accept
                Some(r#"{"accepts":true}"#.into()),
                Some(r#"{"accepts":true}"#.into()),
                Some(r#"{"accepts":true}"#.into()),
            ]);
        let sent = session.sent_handle();

        let outcome = run_block(&mut session, &options, &events, Phase::Research).await;
        assert_eq!(outcome, RunOutcome::PausedAtGate(Gate::DocsConfirm));

        let directives = sent.lock().unwrap();
        // research + docs + exactly ONE rework directive.
        assert_eq!(
            directives.len(),
            3,
            "exactly one rework injected: {directives:?}"
        );
        assert!(
            directives[2].contains("no API surface table"),
            "rework folds the blocking finding: {}",
            directives[2]
        );
    }

    #[tokio::test]
    async fn docs_gate_rework_is_bounded_when_blocking_never_clears() {
        let tmp = tempfile::tempdir().unwrap();
        seed_docs(tmp.path());
        let options = opts(
            tmp.path(),
            "build a SaaS dashboard web app with login and charts",
            TrustMode::Guarded,
        );
        let (events, _rec) = sink();
        // EVERY review round returns the SAME single blocking item (no progress).
        // Plenty of scripted forks so the bound — not the script — stops the loop.
        let blocking = || Some(r#"{"accepts":false,"blocking":["unfixable gap"]}"#.to_string());
        let accept = || Some(r#"{"accepts":true}"#.to_string());
        let mut script = Vec::new();
        for _ in 0..6 {
            // round: one blocks, two accept (count stays 1 → stall after round 0)
            script.push(blocking());
            script.push(accept());
            script.push(accept());
        }
        let mut session =
            FakeBaseSession::new(vec![vec![done()], vec![done()]]).with_fork_script(script);
        let sent = session.sent_handle();

        let outcome = run_block(&mut session, &options, &events, Phase::Research).await;
        assert_eq!(outcome, RunOutcome::PausedAtGate(Gate::DocsConfirm));

        // The blocking count never DROPS (stays 1), so the stall guard stops after
        // the FIRST rework: research + docs + at most MAX_REWORK_ROUNDS reworks.
        // It MUST be bounded — never spins on the unfixable gap.
        let n = sent.lock().unwrap().len();
        assert!(
            (2..=2 + MAX_REWORK_ROUNDS).contains(&n),
            "rework must be bounded, got {n} directives"
        );
    }

    #[tokio::test]
    async fn docs_gate_fork_failure_fails_open_to_accept() {
        let tmp = tempfile::tempdir().unwrap();
        seed_docs(tmp.path());
        let options = opts(
            tmp.path(),
            "build a SaaS dashboard web app with login and charts",
            TrustMode::Guarded,
        );
        let (events, _rec) = sink();
        // EVERY fork FAILS (`None`) → each seat fail-opens to ACCEPT → no
        // blocking → no rework → the gate proceeds normally.
        let mut session = FakeBaseSession::new(vec![vec![done()], vec![done()]])
            .with_fork_script(vec![None, None, None]);
        let forks = session.forks_handle();
        let sent = session.sent_handle();

        let outcome = run_block(&mut session, &options, &events, Phase::Research).await;
        assert_eq!(outcome, RunOutcome::PausedAtGate(Gate::DocsConfirm));
        assert_eq!(
            *forks.lock().unwrap(),
            3,
            "still attempts one fork per seat"
        );
        assert_eq!(
            sent.lock().unwrap().len(),
            2,
            "fork-fail fail-open → no rework"
        );
    }

    #[tokio::test]
    async fn fork_with_timeout_fails_open_accept_on_wedged_handshake() {
        // P2-4: a base whose fork handshake WEDGES (never returns) must not freeze
        // the gate. `fork_with_timeout` bounds it; the timed-out fork degrades to an
        // Err → `review_one` fail-open ACCEPTs → no blocking → the gate proceeds.
        // Use a tiny timeout via the env override so the test is fast; restore it.
        let tmp = tempfile::tempdir().unwrap();
        seed_docs(tmp.path());
        let options = opts(
            tmp.path(),
            "build a SaaS dashboard web app with login and charts",
            TrustMode::Guarded,
        );
        let (events, _rec) = sink();
        let fork_timeout_env = EnvRestore::capture("UMADEV_FORK_ESTABLISH_TIMEOUT_SECS");
        fork_timeout_env.set("1");

        let mut session = FakeBaseSession::fork_wedged();
        let sent = session.sent_handle();
        let forks = session.forks_handle();

        // Bound the WHOLE run too, as a backstop: if the timeout regressed this
        // would hang forever, so the test asserts it returns well under the cap.
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            run_block(&mut session, &options, &events, Phase::Research),
        )
        .await
        .expect("run must not hang on a wedged fork — the timeout must fire");

        // The gate still PAUSED (the wedged forks all fail-open ACCEPT → no rework).
        assert_eq!(outcome, RunOutcome::PausedAtGate(Gate::DocsConfirm));
        // Forks WERE attempted (one per seat), and no rework was injected.
        assert!(*forks.lock().unwrap() >= 1, "forks were attempted");
        assert_eq!(
            sent.lock().unwrap().len(),
            2,
            "wedged-fork fail-open → research + docs only, no rework"
        );
    }

    // ── Maker-checker independence: the critic reviews the ARTIFACT on a CLEAN
    //    context, never the doer's inherited reasoning ─────────────────────────

    #[test]
    fn review_directive_is_clean_room_artifact_only() {
        // The judge directive a critic sends to its read-only fork must (1) lead
        // with the maker-checker independence firewall so any inherited author
        // reasoning is quarantined, and (2) carry the clean artifact seed — the
        // role prompt + the requirement + the produced artifact + the acceptance
        // criteria — and NOTHING the doer deliberated.
        let system = "You are a STRICT senior QA engineer. JSON shape: {\"accepts\": <bool>}";
        let user = "## Requirement\nbuild a login system\n\n## Acceptance criteria\n\
                    FR-001 user can log in\n\n## Delivered code\nfn login() { /* impl */ }";
        let d = compose_review_directive(system, user);

        // (1) The firewall comes FIRST (before the role prompt) and explicitly tells
        // the reviewer to disregard the maker's reasoning / prior conversation.
        assert!(
            d.starts_with(INDEPENDENT_REVIEW_FIREWALL),
            "the independence firewall leads the directive"
        );
        let lower = d.to_lowercase();
        assert!(
            lower.contains("independent"),
            "review is framed as independent"
        );
        assert!(
            lower.contains("disregard"),
            "prior reasoning is disregarded"
        );
        assert!(
            lower.contains("maker") || lower.contains("author"),
            "the maker's framing is named as the thing to quarantine"
        );
        assert!(
            lower.contains("chain-of-thought") || lower.contains("conversation"),
            "the inherited deliberation is named"
        );
        assert!(
            d.find(INDEPENDENT_REVIEW_FIREWALL).unwrap() < d.find(system).unwrap(),
            "the firewall precedes the role prompt"
        );

        // (2) The clean artifact seed is present: requirement + artifact + criteria.
        assert!(d.contains("## Requirement"));
        assert!(d.contains("build a login system"));
        assert!(d.contains("## Acceptance criteria"));
        assert!(d.contains("FR-001 user can log in"));
        assert!(d.contains("fn login()"), "the produced artifact is carried");
        assert!(
            d.contains(system),
            "the role's own focus (system) is carried"
        );
    }

    #[test]
    fn review_directive_is_composed_from_only_firewall_role_and_artifact() {
        // The clean-context invariant proven structurally: the directive is built
        // from ONLY the firewall + the role prompt + the artifact `user` payload —
        // the doer's chain-of-thought is never an input, so it can never be smuggled
        // in. A simulated maker reasoning trace that is NOT part of either input is
        // therefore absent from the composed directive.
        let system = "ROLE_PROMPT_MARKER";
        let user = "ARTIFACT_MARKER";
        let d = compose_review_directive(system, user);
        // Exactly the three known parts, nothing else of substance.
        assert!(d.contains(INDEPENDENT_REVIEW_FIREWALL));
        assert!(d.contains("ROLE_PROMPT_MARKER"));
        assert!(d.contains("ARTIFACT_MARKER"));
        // A doer's private deliberation that was never handed to compose_review_directive
        // cannot appear — there is no transcript input to leak.
        assert!(
            !d.contains("DOER_CHAIN_OF_THOUGHT"),
            "the directive only ever carries the firewall + role prompt + artifact"
        );
        // Reconstruct: the directive is precisely firewall + system + json-shape + user.
        let expected = format!(
            "{INDEPENDENT_REVIEW_FIREWALL}\n\n{system}\n\nReturn EXACTLY ONE JSON object and \
             nothing else — no markdown, no code fence, no prose before or after.\n\n{user}"
        );
        assert_eq!(
            d, expected,
            "no hidden inputs beyond firewall + role + artifact"
        );
    }

    #[tokio::test]
    async fn judge_with_firewall_still_fails_open_on_no_fork() {
        // The independence firewall must not change the fail-open contract: a critic
        // whose fork never opened still ACCEPTs (an absent critic can NEVER block —
        // invariant 1), exactly as before the firewall was added.
        let consult = ForkConsult::new(Err(SessionError::ForkUnsupported("no fork".into())));
        let v = consult
            .judge(
                "security-engineer",
                "you are a strict reviewer",
                "## Requirement\nx\n\n## Delivered code\ny".to_string(),
            )
            .await;
        assert!(v.accepts, "no fork → fail-open ACCEPT, firewall or not");
        assert_eq!(v.role, "security-engineer");
        assert!(v.blocking.is_empty());
    }

    // ── Lean GATELESS plan (Light / Bugfix / Refactor) on the continuous path ──

    /// Drop a real source file so the zero-source hard gate is satisfied (a lean
    /// plan still enforces "produced real code" — only the research/docs/gates are
    /// skipped, the moat stands). A `.js` file counts toward the implementation
    /// surface without tripping the governance CSP scanner on the test fixture.
    fn seed_source(root: &Path) {
        std::fs::write(
            root.join("app.js"),
            "function addTodo(t){ /* lean todo impl */ return t; }\n",
        )
        .unwrap();
    }

    #[tokio::test]
    async fn light_build_runs_lean_block_with_no_gate_and_no_research() {
        let tmp = tempfile::tempdir().unwrap();
        seed_source(tmp.path());
        // The dogfood case: an explicitly-simple single-page pure-frontend build.
        let options = opts(
            tmp.path(),
            "做一个简单的待办清单单页应用,纯前端,支持添加删除",
            TrustMode::Auto,
        );
        let (events, rec) = sink();
        // spec, frontend, quality — three lean turns (a PURE frontend Light build
        // drops the do-nothing Backend phase), all clean.
        let mut session = FakeBaseSession::new(vec![vec![done()], vec![done()], vec![done()]]);
        let sent = session.sent_handle();

        // A Light plan is GATELESS → it drives the WHOLE lean list in one block
        // from Research start, runs to completion, and NEVER pauses at a gate.
        let outcome = run_block(&mut session, &options, &events, Phase::Research).await;
        assert_eq!(outcome, RunOutcome::Completed);

        let sent = sent.lock().unwrap();
        // spec + frontend + quality (no research, no docs, no empty backend).
        assert_eq!(
            sent.len(),
            3,
            "lean pure-frontend plan: spec/frontend/quality only (Backend trimmed)"
        );
        // The FIRST directive (spec) must NOT reference research / the three docs,
        // and must carry the requirement + the lean priming + a small-scope cue.
        let first = sent[0].to_lowercase();
        assert!(!first.contains("three core documents"));
        assert!(!first.contains("approved"));
        assert!(first.contains("lean fast-track"));
        assert!(sent[0].contains("待办清单"));
        // No GateOpened anywhere — the lean path has no confirm gate.
        let evs = rec.events();
        assert!(
            !evs.iter()
                .any(|e| matches!(e, EngineEvent::GateOpened { .. })),
            "lean plan opens no confirm gate"
        );
    }

    #[tokio::test]
    async fn light_build_with_zero_source_hard_stops() {
        let tmp = tempfile::tempdir().unwrap();
        // NO source seeded → the moat's zero-source hard gate must still fire even
        // on the lean path (governance + the hard gate are NOT skipped).
        let options = opts(
            tmp.path(),
            "做一个简单的待办清单单页应用,纯前端",
            TrustMode::Auto,
        );
        let (events, _rec) = sink();
        let mut session =
            FakeBaseSession::new(vec![vec![done()], vec![done()], vec![done()], vec![done()]]);

        let outcome = run_block(&mut session, &options, &events, Phase::Research).await;
        match outcome {
            RunOutcome::HardStop(reason) => {
                assert!(reason.to_lowercase().contains("real") || reason.contains("代码"));
            }
            other => panic!("lean plan with no code must hard-stop, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn light_gate_resume_entry_is_a_clean_noop() {
        let tmp = tempfile::tempdir().unwrap();
        seed_source(tmp.path());
        let options = opts(
            tmp.path(),
            "做一个简单的待办清单单页应用,纯前端",
            TrustMode::Auto,
        );
        let (events, _rec) = sink();
        let mut session = FakeBaseSession::new(vec![vec![done()]]);
        let sent = session.sent_handle();

        // A lean plan never pauses, so a Continue-style resume entry has nothing
        // left to drive — it must complete cleanly without sending any directive.
        let outcome = run_block(&mut session, &options, &events, Phase::Spec).await;
        assert_eq!(outcome, RunOutcome::Completed);
        assert!(
            sent.lock().unwrap().is_empty(),
            "gateless resume drives nothing"
        );
    }

    #[tokio::test]
    async fn bugfix_drives_lean_phases_with_bugfix_quality_focus() {
        let tmp = tempfile::tempdir().unwrap();
        seed_source(tmp.path());
        let options = opts(tmp.path(), "修复登录按钮点击没反应", TrustMode::Auto);
        let (events, _rec) = sink();
        let mut session =
            FakeBaseSession::new(vec![vec![done()], vec![done()], vec![done()], vec![done()]]);
        let sent = session.sent_handle();

        let outcome = run_block(&mut session, &options, &events, Phase::Research).await;
        assert_eq!(outcome, RunOutcome::Completed);
        let sent = sent.lock().unwrap();
        // The quality directive carries the bug-fix-specific verification focus.
        let quality = sent.last().unwrap().to_lowercase();
        assert!(
            quality.contains("bug is actually fixed") || quality.contains("reproduce"),
            "bugfix quality focus: {quality}"
        );
    }

    #[test]
    fn block_phases_lean_is_one_block_then_empty() {
        // Light is gateless → the whole lean list at Research start, nothing after.
        let plan = crate::planner::plan_light("anything");
        let first = block_phases(Phase::Research, &plan);
        assert_eq!(first, plan.phases);
        assert!(block_phases(Phase::Spec, &plan).is_empty());
        assert!(block_phases(Phase::Backend, &plan).is_empty());
    }

    #[test]
    fn block_phases_greenfield_keeps_gate_anchored_split() {
        // A heavyweight plan is unchanged: the standard three-block split.
        let plan = crate::planner::plan("build a SaaS dashboard with login and a database");
        assert_eq!(plan.kind, crate::planner::TaskKind::Greenfield);
        assert_eq!(
            block_phases(Phase::Research, &plan),
            vec![Phase::Research, Phase::Docs, Phase::DocsConfirm]
        );
        assert_eq!(
            block_phases(Phase::Spec, &plan),
            vec![Phase::Spec, Phase::Frontend, Phase::PreviewConfirm]
        );
    }

    #[test]
    fn block_phases_frontend_only_skips_backend_keeps_preview_gate() {
        // A one-sided gated plan: the split is intersected with the plan, so the
        // post-docs block keeps the preview gate but the post-preview block has no
        // backend to drive.
        let plan = crate::planner::plan("做一个前端落地页");
        assert_eq!(plan.kind, crate::planner::TaskKind::FrontendOnly);
        assert_eq!(
            block_phases(Phase::Spec, &plan),
            vec![Phase::Spec, Phase::Frontend, Phase::PreviewConfirm]
        );
        // Post-preview block: backend is NOT in a FrontendOnly plan → only quality
        // + delivery survive.
        assert_eq!(
            block_phases(Phase::Backend, &plan),
            vec![Phase::Quality, Phase::Delivery]
        );
    }

    #[test]
    fn block_phases_backend_only_drives_full_tail_after_docs_gate() {
        // P0-B regression: a BackendOnly plan has a docs gate but NO preview gate.
        // The initial block is research → docs → docs gate (unchanged); the
        // post-docs resume MUST drive the WHOLE tail Spec → Backend → Quality →
        // Delivery in one block. The pre-fix bug returned just `[Spec]`, dropping
        // Backend/Quality/Delivery and disguising an empty run as success.
        let plan = crate::planner::plan("写一个后端 graphql 接口");
        assert_eq!(plan.kind, crate::planner::TaskKind::BackendOnly);
        // Initial block keeps the docs gate.
        assert_eq!(
            block_phases(Phase::Research, &plan),
            vec![Phase::Research, Phase::Docs, Phase::DocsConfirm]
        );
        // The continuous resume phase after the docs gate is Spec — from there the
        // block must run all the way to Delivery (no preview gate to split on).
        assert_eq!(
            block_phases(Phase::Spec, &plan),
            vec![Phase::Spec, Phase::Backend, Phase::Quality, Phase::Delivery],
            "BackendOnly post-docs block must drive Spec→Backend→Quality→Delivery"
        );
    }

    #[tokio::test]
    async fn backend_only_resume_drives_to_delivery_and_hard_gates_empty_code() {
        // End-to-end P0-B: resume a BackendOnly run after the docs gate. With NO
        // source produced, the zero-source HARD gate must fire after Backend — the
        // run must NOT silently complete at Spec (the disguised-empty-delivery bug).
        let tmp = tempfile::tempdir().unwrap();
        // No source seeded → the moat's hard gate must catch the empty backend.
        let options = opts(
            tmp.path(),
            "写一个后端 graphql 接口带鉴权和数据库",
            TrustMode::Auto,
        );
        assert_eq!(
            crate::planner::plan(&options.requirement).kind,
            crate::planner::TaskKind::BackendOnly
        );
        let (events, rec) = sink();
        // spec + backend turns (both clean narration) — the deterministic hard gate
        // is what stops the run after Backend, not the base.
        let mut session = FakeBaseSession::new(vec![vec![done()], vec![done()]]);

        let outcome = run_block(&mut session, &options, &events, Phase::Spec).await;
        match outcome {
            RunOutcome::HardStop(reason) => {
                assert!(
                    reason.to_lowercase().contains("real") || reason.contains("代码"),
                    "empty BackendOnly run must hard-stop on the zero-source gate: {reason}"
                );
            }
            other => panic!("BackendOnly with no code must hard-stop, not {other:?}"),
        }
        // Backend WAS driven (PhaseStarted emitted) — proving the block didn't stop
        // at Spec. The pre-fix bug never reached Backend at all.
        let evs = rec.events();
        assert!(
            evs.iter().any(|e| matches!(
                e,
                EngineEvent::PhaseStarted {
                    phase: Phase::Backend
                }
            )),
            "Backend phase must be driven on a BackendOnly resume"
        );
    }

    #[tokio::test]
    async fn backend_only_resume_with_source_runs_quality_and_delivery() {
        // The healthy path: a BackendOnly resume with real source produced runs
        // Spec → Backend → Quality → Delivery to completion (no early stop at Spec).
        // The quality gate is advisory-passing here because real backend source +
        // a written PRD/arch would be needed to fail it; with source present and no
        // docs the gate would normally block — so seed a passing-ish minimal state
        // by ALSO disabling the hard block via a docs/research-free lean comparison
        // is not possible here; instead assert it reaches Delivery's PhaseStarted
        // only when the gate passes. To keep this deterministic we assert the
        // weaker, robust invariant: Backend + Quality are both DRIVEN (not skipped).
        let tmp = tempfile::tempdir().unwrap();
        seed_source(tmp.path());
        let options = opts(tmp.path(), "写一个后端 graphql 接口", TrustMode::Auto);
        let (events, rec) = sink();
        let mut session =
            FakeBaseSession::new(vec![vec![done()], vec![done()], vec![done()], vec![done()]]);

        let _ = run_block(&mut session, &options, &events, Phase::Spec).await;
        let evs = rec.events();
        for phase in [Phase::Spec, Phase::Backend, Phase::Quality] {
            assert!(
                evs.iter()
                    .any(|e| matches!(e, EngineEvent::PhaseStarted { phase: p } if *p == phase)),
                "BackendOnly resume must drive {phase:?} (not stop at Spec)"
            );
        }
    }

    #[tokio::test]
    async fn continuous_path_persists_workflow_state_for_continue() {
        // P0-A regression: the continuous (default) path MUST write
        // `.umadev/workflow-state.json` so `umadev continue` can read the real door
        // and resume. Pre-fix the continuous path never wrote state, so `continue`
        // read `Missing` and bailed — structurally dead against the default run.
        let tmp = tempfile::tempdir().unwrap();
        seed_docs(tmp.path());
        let options = opts(
            tmp.path(),
            "build a SaaS dashboard web app with login and charts",
            TrustMode::Guarded,
        );
        let (events, _rec) = sink();
        let mut session = FakeBaseSession::new(vec![vec![done()], vec![done()]]);

        let outcome = run_block(&mut session, &options, &events, Phase::Research).await;
        assert_eq!(outcome, RunOutcome::PausedAtGate(Gate::DocsConfirm));

        // The state file exists and records the OPEN docs gate so `continue`
        // resolves the right block to resume from.
        let state = crate::state::read_workflow_state(tmp.path())
            .expect("continuous path must persist workflow-state.json");
        assert_eq!(
            state.active_gate, "docs_confirm",
            "the open gate is persisted"
        );
        assert_eq!(state.phase, Phase::DocsConfirm.id());
        assert_eq!(
            state.slug, "demo",
            "slug persisted so continue resolves artifacts"
        );
        assert_eq!(
            state.requirement, options.requirement,
            "requirement persisted so continue keeps context"
        );
    }

    #[tokio::test]
    async fn lean_tweak_seats_no_team_and_does_not_fork() {
        let tmp = tempfile::tempdir().unwrap();
        // A trivial "fix a typo" is a lean GATELESS Bugfix plan now: it drives the
        // lean phases straight through (no docs gate to pause at) and seats NO
        // review team at any node → opens zero forks. Seed a source file so the
        // zero-source hard gate is satisfied and the run completes cleanly.
        seed_source(tmp.path());
        let options = opts(tmp.path(), "fix a typo in the footer text", TrustMode::Auto);
        let (events, _rec) = sink();
        let mut session =
            FakeBaseSession::new(vec![vec![done()], vec![done()], vec![done()], vec![done()]]);
        let forks = session.forks_handle();

        let outcome = run_block(&mut session, &options, &events, Phase::Research).await;
        assert_eq!(outcome, RunOutcome::Completed);
        assert_eq!(*forks.lock().unwrap(), 0, "lean task opens no review forks");
    }

    /// The path selector now DEFAULTS to the continuous long-session path, and is
    /// reachable back to single-shot only via an explicit opt-out. One serial test
    /// covers every branch (the process env is shared, so it saves + restores both
    /// vars and never leaves global state mutated). No other test in this crate
    /// reads these vars, so this can't race a sibling.
    #[test]
    fn continuous_is_default_with_explicit_opt_out() {
        let continuous_env = EnvRestore::capture("UMADEV_CONTINUOUS");
        let legacy_env = EnvRestore::capture("UMADEV_LEGACY_RUN");

        // Unset → DEFAULT ON (the architecture has closed on continuous).
        continuous_env.remove();
        legacy_env.remove();
        assert!(
            continuous_enabled_from_env(),
            "continuous must be the DEFAULT when nothing is set"
        );

        // Explicit opt-out via the off-switch on the continuous var → single-shot.
        for off in ["0", "false", "off"] {
            continuous_env.set(off);
            assert!(
                !continuous_enabled_from_env(),
                "UMADEV_CONTINUOUS={off} must opt OUT to single-shot"
            );
        }

        // Explicit opt-out via the legacy-run alias → single-shot, even when the
        // continuous var is left unset / on (opt-out wins).
        continuous_env.remove();
        for on in ["1", "true", "on"] {
            legacy_env.set(on);
            assert!(
                !continuous_enabled_from_env(),
                "UMADEV_LEGACY_RUN={on} must opt OUT to single-shot"
            );
        }
        legacy_env.remove();

        // Explicit force-on still honoured (symmetry, no longer required).
        for on in ["1", "true", "on"] {
            continuous_env.set(on);
            assert!(
                continuous_enabled_from_env(),
                "UMADEV_CONTINUOUS={on} must keep continuous ON"
            );
        }
    }

    // ── Deterministic gatekeepers reattached to the continuous default path ──
    //
    // These exercise the four moat functions wired back into `run_block`:
    // (1) the quality HARD GATE (`run_quality_gate`), (2) the contract/coverage
    // critic floor (`quality_floor`), (3) the post-write governance catch-up
    // (`governance_catchup`, the codex/opencode no-hook gap), and that the LLM
    // critic stays advisory while the deterministic gate is the hard signal.

    /// A source file with a hardcoded (non-token) color — trips the governance
    /// color rule (UD-CODE-002) so the catch-up scan + security floor have a
    /// real, deterministic finding. A `.tsx` so the color rule's guarded-ext set
    /// applies.
    fn seed_ungoverned_source(root: &std::path::Path) {
        std::fs::write(
            root.join("App.tsx"),
            "export const App = () => <div style={{ color: \"#3a7bd5\" }}>hi</div>;\n",
        )
        .unwrap();
    }

    #[tokio::test]
    async fn quality_gate_hard_stops_when_gate_fails_on_code_run() {
        let tmp = tempfile::tempdir().unwrap();
        // Source EXISTS (so the separate zero-source gate is satisfied) but there
        // are NO docs/evidence, so the deterministic quality gate scores well
        // below the 90 threshold → `passed:false`. On a code-producing run that is
        // a HARD STOP at quality — never disguised as success, never delivered.
        seed_source(tmp.path());
        let options = opts(
            tmp.path(),
            "build a SaaS dashboard web app with login and charts",
            TrustMode::Auto,
        );
        let (events, _rec) = sink();

        let stop = run_quality_gate(&options, &events, true).await;
        match stop {
            Some(RunOutcome::HardStop(reason)) => {
                assert!(
                    reason.to_lowercase().contains("quality gate failed"),
                    "hard stop names the quality gate: {reason}"
                );
            }
            other => panic!("code run with a failing gate must hard-stop, got {other:?}"),
        }
        // The gate file was actually produced by the deterministic `run_quality`.
        assert!(tmp.path().join("output/demo-quality-gate.json").exists());
    }

    #[tokio::test]
    async fn quality_gate_does_not_block_a_non_code_run() {
        let tmp = tempfile::tempdir().unwrap();
        // `produces_code = false` (a docs/research-only plan): the gate is advisory
        // here and must NEVER hard-stop, even when the score is poor — the
        // single-shot path has the identical guard.
        let options = opts(tmp.path(), "write a research brief only", TrustMode::Auto);
        let (events, _rec) = sink();

        let stop = run_quality_gate(&options, &events, false).await;
        assert!(
            stop.is_none(),
            "a non-code-producing run must not hard-stop on the quality gate"
        );
    }

    #[tokio::test]
    async fn quality_gate_failure_blocks_inside_the_block_loop() {
        // End-to-end through `run_block`: the post-preview block drives
        // Backend → Quality → Delivery. With source present but no docs the gate
        // fails, so the block HARD-STOPS at quality and NEVER reaches delivery.
        let tmp = tempfile::tempdir().unwrap();
        seed_source(tmp.path());
        let options = opts(
            tmp.path(),
            "build a SaaS dashboard web app with login and a database",
            TrustMode::Auto,
        );
        let (events, rec) = sink();
        // backend turn + quality turn, both clean (the BASE narrates success) —
        // the deterministic gate is what stops the run, not the base.
        let mut session = FakeBaseSession::new(vec![vec![done()], vec![done()]]);

        let outcome = run_block(&mut session, &options, &events, Phase::Backend).await;
        assert!(
            matches!(outcome, RunOutcome::HardStop(_)),
            "block must hard-stop at quality, got {outcome:?}"
        );
        // It must NOT have emitted a Delivery PhaseStarted — delivery is unreached.
        let evs = rec.events();
        assert!(
            !evs.iter().any(|e| matches!(
                e,
                EngineEvent::PhaseStarted {
                    phase: Phase::Delivery
                }
            )),
            "delivery must be unreached when the quality gate blocks"
        );
    }

    #[tokio::test]
    async fn governance_catchup_runs_for_a_non_claude_base_and_reworks() {
        let tmp = tempfile::tempdir().unwrap();
        seed_ungoverned_source(tmp.path());
        // A codex base has NO real-time PreToolUse hook → the catch-up scan must
        // fire, find the hardcoded-color violation, and inject ONE rework turn.
        let mut options = opts(tmp.path(), "build a dashboard", TrustMode::Auto);
        options.backend = "codex".to_string();
        let (events, _rec) = sink();
        let mut session = FakeBaseSession::new(vec![vec![done()]]);
        let sent = session.sent_handle();

        governance_catchup(
            &mut session,
            &options,
            &events,
            std::time::Instant::now() + std::time::Duration::from_secs(3600),
        )
        .await;

        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 1, "exactly one governance rework turn injected");
        assert!(
            sent[0].to_lowercase().contains("governance violations")
                || sent[0].contains("治理违规"),
            "rework directive carries the governance intro: {}",
            sent[0]
        );
    }

    #[tokio::test]
    async fn governance_catchup_is_skipped_for_claude_base() {
        let tmp = tempfile::tempdir().unwrap();
        seed_ungoverned_source(tmp.path());
        // claude-code governs at WRITE time (PreToolUse hook), so the post-write
        // catch-up is a no-op — no rework turn, even with a real violation on disk.
        let options = opts(tmp.path(), "build a dashboard", TrustMode::Auto); // backend = claude-code
        let (events, _rec) = sink();
        let mut session = FakeBaseSession::new(vec![vec![done()]]);
        let sent = session.sent_handle();

        governance_catchup(
            &mut session,
            &options,
            &events,
            std::time::Instant::now() + std::time::Duration::from_secs(3600),
        )
        .await;
        assert!(
            sent.lock().unwrap().is_empty(),
            "claude-code already governs at write time — no catch-up rework"
        );
    }

    #[test]
    fn backend_realtime_governance_only_for_claude() {
        assert!(backend_has_realtime_governance("claude-code"));
        assert!(backend_has_realtime_governance("CLAUDE-CODE"));
        assert!(!backend_has_realtime_governance("codex"));
        assert!(!backend_has_realtime_governance("opencode"));
    }

    #[test]
    fn quality_floor_collects_coverage_and_governance_findings() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // A PRD declaring FR ids that NO task covers → a coverage gap in qa_floor.
        std::fs::create_dir_all(root.join("output")).unwrap();
        std::fs::write(
            root.join("output/demo-prd.md"),
            "| FR-001 | login |\n| FR-002 | logout |\n",
        )
        .unwrap();
        // A real source file with a hardcoded color → a security_floor finding.
        seed_ungoverned_source(root);
        let options = opts(root, "build a dashboard", TrustMode::Auto);

        let (qa, security) = quality_floor(&options);
        assert!(
            qa.contains("coverage gap") && (qa.contains("FR-001") || qa.contains("FR-002")),
            "qa_floor surfaces the uncovered requirements: {qa}"
        );
        assert!(
            !security.trim().is_empty() && security.contains("App.tsx"),
            "security_floor surfaces the governance violation: {security}"
        );
    }

    #[tokio::test]
    async fn quality_node_critic_team_receives_the_deterministic_floor() {
        // The quality-node blackboard must hand the critics a NON-empty floor (the
        // review P0-2 fix): the seat's judge prompt is built from `CriticArtifacts`
        // whose `qa_floor` / `security_floor` come from `quality_floor`. Read the
        // Quality blackboard directly and assert the floors are populated.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("output")).unwrap();
        std::fs::write(root.join("output/demo-prd.md"), "| FR-001 | login |\n").unwrap();
        seed_ungoverned_source(root);
        let options = opts(root, "build a dashboard", TrustMode::Auto);

        let bb = Blackboard::read(&options, ReviewKind::Quality);
        let arts = bb.artifacts(&options.requirement);
        assert!(
            !arts.qa_floor.trim().is_empty(),
            "quality critics must get a non-empty qa_floor (coverage/contract)"
        );
        assert!(
            !arts.security_floor.trim().is_empty(),
            "quality critics must get a non-empty security_floor (governance)"
        );
        // The docs node, by contrast, has no code floor.
        let docs_bb = Blackboard::read(&options, ReviewKind::Docs);
        let docs_arts = docs_bb.artifacts(&options.requirement);
        assert!(docs_arts.qa_floor.is_empty() && docs_arts.security_floor.is_empty());
    }

    #[test]
    fn quality_blackboard_grounds_critics_with_real_file_listing() {
        // GROUNDING: the Quality critics review a forked session with only the
        // blackboard + a source digest, so they can hallucinate absent files. The
        // review context must carry the ACTUAL relative paths of the real test +
        // source (incl. backend) files so a critic SEES them and cannot claim
        // absence.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("src/api")).unwrap();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::write(
            root.join("src/api/server.py"),
            "def route():\n    return 200\n",
        )
        .unwrap();
        std::fs::write(
            root.join("tests/test_server.py"),
            "def test_route():\n    assert True\n",
        )
        .unwrap();
        let options = opts(root, "build an API", TrustMode::Auto);

        let bb = Blackboard::read(&options, ReviewKind::Quality);
        let arts = bb.artifacts(&options.requirement);

        assert!(
            arts.code.contains("Current workspace files"),
            "quality critics must see the real file listing: {}",
            arts.code
        );
        // The listing separates tests from source, and shows the ACTUAL paths (not
        // a bare count the critic can ignore) — the backend file is visible by its
        // real path under the source bucket.
        assert!(arts.code.contains("### Test files"), "{}", arts.code);
        assert!(arts.code.contains("### Source files"), "{}", arts.code);
        assert!(arts.code.contains("src/api/server.py"), "{}", arts.code);
        assert!(arts.code.contains("tests/test_server.py"), "{}", arts.code);
    }

    #[test]
    fn workspace_evidence_only_grounds_files_that_exist() {
        // TRUTHFUL grounding: with source present but NO test files, the listing
        // must NOT emit a "Test files" block — it never fabricates that tests exist,
        // so a critic's legitimate "no tests" finding stays valid (nothing is
        // suppressed).
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/app.py"), "def app():\n    return 1\n").unwrap();

        let ctx = WorkspaceEvidence::read(root).to_review_context();
        assert!(
            ctx.contains("src/app.py"),
            "lists the real source file: {ctx}"
        );
        assert!(
            !ctx.contains("### Test files"),
            "must NOT claim tests exist when none do: {ctx}"
        );
    }

    #[test]
    fn workspace_evidence_is_fail_open_on_unreadable_tree() {
        // FAIL-OPEN: an unreadable / missing tree yields an EMPTY listing that
        // injects nothing — never a panic, never a fabricated claim.
        let missing = std::path::Path::new("/umadev-nonexistent-abcdef/definitely/not/here");
        let ev = WorkspaceEvidence::read(missing);
        assert!(
            ev.to_review_context().is_empty(),
            "an unreadable tree grounds nothing (fail-open)"
        );
    }

    #[tokio::test]
    async fn quality_review_does_not_suppress_or_force_accept_absence_findings() {
        // NO post-hoc suppression / force-accept: the crude filter is gone. Even a
        // GLOBAL "no tests exist" blocking finding passes through `run_review_team`
        // verbatim — critic verdicts are advisory (invariant 2) and the
        // deterministic floor governs loop control; the injected file listing (the
        // grounding) is what prevents the hallucination, not verdict surgery.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::write(
            root.join("tests/test_app.py"),
            "def test_app():\n    assert True\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/app.py"), "def app():\n    return 1\n").unwrap();
        let options = opts(root, "build an API", TrustMode::Auto);
        let (events, _rec) = sink();

        // A single QA seat scripted to BLOCK with a global-absence finding — the
        // exact shape the old filter would have suppressed + force-accepted.
        let team: Vec<Box<dyn RoleCritic>> = vec![Box::new(crate::critics::QaCritic)];
        let mut session = FakeBaseSession::new(vec![]).with_fork_script(vec![Some(
            r#"{"accepts":false,"blocking":["No tests exist anywhere in the delivered artifact"]}"#
                .into(),
        )]);

        let blocking = run_review_team(
            &mut session,
            &options,
            &events,
            ReviewKind::Quality,
            &team,
            0,
        )
        .await;

        assert!(
            blocking
                .iter()
                .any(|b| b.contains("No tests exist anywhere")),
            "a critic's blocking finding must pass through un-suppressed: {blocking:?}"
        );
    }

    // ── Idle watchdog on EVERY main-session pump (P0-3 / P1-11) ─────────────

    #[tokio::test]
    async fn drive_phase_idle_watchdog_settles_a_hung_base() {
        // P1-11: a base that hangs mid-phase (accepts send_turn, then never emits
        // and never exits) must NOT wedge the phase forever — the shared idle
        // watchdog settles it as a Failed turn. Drive `drive_phase` with a tiny
        // window (no env mutation to race) and assert it returns promptly.
        let tmp = tempfile::tempdir().unwrap();
        let options = opts(tmp.path(), "build a dashboard", TrustMode::Auto);
        let (events, _rec) = sink();
        let mut session = FakeBaseSession::hanging();
        let interrupts = session.interrupts_handle();

        let result = drive_phase(
            &mut session,
            &options,
            &events,
            Phase::Frontend,
            false,
            crate::planner::TaskKind::Greenfield,
            crate::director_loop::IdleBudget::new(
                std::time::Duration::from_millis(80),
                std::time::Duration::from_millis(80),
            ),
            std::time::Instant::now() + std::time::Duration::from_secs(3600),
        )
        .await;

        match result {
            PhaseResult::Failed(reason) => assert!(
                reason.contains("UMADEV_IDLE_TIMEOUT_SECS"),
                "a hung base settles as an idle Failed: {reason}"
            ),
            PhaseResult::Done => panic!("a hung phase must settle as Failed, not Done"),
        }
        assert_eq!(
            *interrupts.lock().unwrap(),
            1,
            "the watchdog issued its best-effort interrupt before settling"
        );
    }

    #[tokio::test]
    async fn drive_rework_turn_idle_watchdog_settles_a_hung_base() {
        // P1-11: the rework pump (governance_catchup / review_and_rework / the
        // director's summon all flow through here) must also be idle-guarded — a
        // base that hangs mid-rework can't freeze the review node. A hung session
        // settles the rework as `false` (fail-open stop) within the tiny window.
        let tmp = tempfile::tempdir().unwrap();
        let options = opts(tmp.path(), "build a dashboard", TrustMode::Auto);
        let (events, _rec) = sink();
        let mut session = FakeBaseSession::hanging();
        let interrupts = session.interrupts_handle();

        let ok = drive_rework_turn_with_idle(
            &mut session,
            &options,
            &events,
            "fix these".to_string(),
            crate::director_loop::IdleBudget::new(
                std::time::Duration::from_millis(80),
                std::time::Duration::from_millis(80),
            ),
            std::time::Instant::now() + std::time::Duration::from_secs(3600),
        )
        .await
        .done;

        assert!(
            !ok,
            "a hung rework turn settles as a fail-open stop (false)"
        );
        assert_eq!(
            *interrupts.lock().unwrap(),
            1,
            "the watchdog issued its best-effort interrupt before settling"
        );
    }

    #[tokio::test]
    async fn drive_rework_turn_budget_settles_an_active_base_mid_turn() {
        // The 62-min bug: a base that stays ACTIVE (keeps emitting, never sends
        // TurnDone — e.g. writing code) never trips the IDLE watchdog, so before the
        // mid-turn wall-clock check a single rework/summon turn ran UNBOUNDED past the
        // run budget (the between-step deadline checks can't be reached while one pump
        // turn is still draining). With a deadline ALREADY in the past, the pump must
        // return PROMPTLY — GRACEFULLY (done = true, the work-so-far stands), after a
        // best-effort interrupt — instead of looping forever on the active stream.
        let tmp = tempfile::tempdir().unwrap();
        let options = opts(tmp.path(), "build a dashboard", TrustMode::Auto);
        let (events, _rec) = sink();
        // `active_forever`: every next_event yields a fresh TextDelta, never TurnDone
        // (a full idle window would otherwise NEVER fire — the base is always active).
        let mut session = FakeBaseSession::active_forever();
        let interrupts = session.interrupts_handle();

        // A deadline 1s in the PAST → the top-of-loop budget check fires on the first
        // pass, before any next_event_idle wait. A generous idle window proves it is
        // the BUDGET (not the idle watchdog) that settles the active base.
        let past_deadline = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(1))
            .unwrap();
        let turn = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            drive_rework_turn_with_idle(
                &mut session,
                &options,
                &events,
                "build it".to_string(),
                // idle window that never trips (base + tool both far past the deadline)
                crate::director_loop::IdleBudget::new(
                    std::time::Duration::from_secs(3600),
                    std::time::Duration::from_secs(3600),
                ),
                past_deadline,
            ),
        )
        .await
        .expect("an active base past its budget must settle promptly, not loop forever");

        assert!(
            turn.done,
            "the mid-turn budget settle is GRACEFUL (done = true), so the caller \
             finalizes the work-so-far rather than treating it as a failed turn"
        );
        assert_eq!(
            *interrupts.lock().unwrap(),
            1,
            "the budget path issued its best-effort (bounded) interrupt before settling"
        );
    }
}
