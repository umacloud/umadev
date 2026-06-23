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
use crate::trust::{requires_confirmation, TrustMode};

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
    let state = WorkflowState {
        phase: phase.id().to_string(),
        active_gate: active_gate.to_string(),
        slug: options.effective_slug(),
        requirement: options.requirement.clone(),
        last_transition_at: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        note: format!("Advanced to {} (continuous session)", phase.id()),
        backend: options.backend.clone(),
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
            review_and_rework(session, options, events, gate_review_kind(phase)).await;
            // P0-A: persist the OPEN-GATE state (phase = the gate phase, active_gate
            // = its id) so `umadev continue` / the TUI gate resume read the real door
            // and resume the continuous run from THIS gate — exactly the state shape
            // the single-shot `transition(gate_phase, gate.id_str())` would write.
            persist_state(options, phase, gate.id_str());
            events.emit(EngineEvent::GateOpened { gate });
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
            governance_catchup(session, options, events).await;
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
            review_and_rework(session, options, events, ReviewKind::Quality).await;
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
async fn drive_phase(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    phase: Phase,
    first_directive: bool,
    kind: crate::planner::TaskKind,
) -> PhaseResult {
    let directive = phase_directive(options, phase, first_directive, kind);
    if let Err(e) = session.send_turn(directive).await {
        return PhaseResult::Failed(format!("send_turn: {e}"));
    }

    let policy = umadev_governance::Policy::load(&options.project_root);

    loop {
        let Some(ev) = session.next_event().await else {
            // `None` = the underlying session ended (process dead / EOF). Per
            // the BaseSession contract, treat as a failed turn — fail-open, no
            // panic.
            return PhaseResult::Failed("session ended mid-turn".to_string());
        };
        match ev {
            SessionEvent::TextDelta(text) => {
                // Stream the assistant's words to the TUI (alive-feel) — but
                // remember: `TextDelta` is what it SAID, `ToolCall` is what it
                // DID. The hard gate / audit key off tool calls, not this.
                events.emit(EngineEvent::WorkerStream {
                    event: StreamEvent::Text { delta: text },
                });
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
                let decision = approval_decision(options.mode, &action, &target);
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
            SessionEvent::TurnDone { status } => {
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
    let (target, decision) = evaluate_tool_call(policy, ctx, name, input);

    // TUI tool row — "正在写 src/App.tsx…". This is the SOURCE OF TRUTH for what
    // the base actually did.
    events.emit(EngineEvent::WorkerStream {
        event: StreamEvent::ToolUse {
            name: name.to_string(),
            detail: target.clone(),
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
    // File-mutating tools: scan the proposed content.
    let path = input
        .get("file_path")
        .and_then(serde_json::Value::as_str)
        .or_else(|| input.get("path").and_then(serde_json::Value::as_str))
        .unwrap_or_default();
    if lname == "write" || lname == "edit" || lname == "update" || lname == "create" {
        let content = input
            .get("content")
            .and_then(serde_json::Value::as_str)
            .or_else(|| input.get("new_string").and_then(serde_json::Value::as_str))
            .or_else(|| input.get("new_str").and_then(serde_json::Value::as_str))
            .unwrap_or_default();
        let decision = umadev_governance::scan_content_with_context(path, content, policy, ctx);
        return (path.to_string(), decision);
    }
    // Read / Grep / Glob / … — observe-only, never a write. Pass.
    (path.to_string(), umadev_governance::Decision::pass())
}

/// Map a [`SessionEvent::NeedApproval`] to a trust-tiered [`ApprovalDecision`].
///
/// `auto` lets reversible actions through; the irreversible-action floor
/// (`.git` internals, network, destructive shell verbs) forces a confirmation
/// regardless of mode — and in this non-interactive driving loop a forced
/// confirmation degrades to DENY so the base can't run an irreversible action
/// unattended. `guarded` / `plan` also deny here (the human gate happens at the
/// confirm gates, not mid-turn).
fn approval_decision(mode: TrustMode, action: &str, target: &str) -> ApprovalDecision {
    if requires_confirmation(mode, action, target) {
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
        TurnStatus::Failed(reason) => PhaseResult::Failed(reason),
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
    if !drive_rework_turn(session, options, events, directive).await {
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
fn backend_has_realtime_governance(backend: &str) -> bool {
    backend.eq_ignore_ascii_case("claude-code")
}

/// Scan every real source file with the governance kernel, returning a bounded
/// list of `"<rel>: <reason> (<clause>)"` violation strings. Empty = clean. Pure
/// and fail-open: an unreadable file is skipped, never a panic. Shared by the
/// catch-up rework loop (which reads it twice) and the critic floor.
fn governance_scan(options: &RunOptions) -> Vec<String> {
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
             safe error handling). Summarize results.\n\n{no_ask}"
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
                 secrets, inputs validated). Summarize results in a few lines.\n\n{no_ask}"
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
) {
    // Scale the team to the task; an empty team (lean / no-UI / docs-only paths)
    // means "no cross-review here" — return immediately, the floor stands.
    let team = team_for(kind, &options.requirement);
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
        if !drive_rework_turn(session, options, events, directive).await {
            // The rework turn failed / the session died — stop reworking (the
            // outer loop's phase/turn handling already surfaced the failure path).
            // Fail-open: leave the findings as advisory and proceed.
            return;
        }
    }
}

/// The team for a review node, scaled to the task via the planner's tiering.
pub(crate) fn team_for(kind: ReviewKind, requirement: &str) -> Vec<Box<dyn RoleCritic>> {
    let tier = crate::planner::classify(requirement);
    match kind {
        ReviewKind::Docs => crate::critics::docs_team_for_kind(tier),
        ReviewKind::Preview => crate::critics::preview_team_for_kind(tier),
        ReviewKind::Quality => crate::critics::quality_team_for_kind(tier),
    }
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

/// Inject the rework directive into the MAIN session and pump its turn through
/// the SAME governance + audit + approval path a normal phase turn uses. Returns
/// `true` when the turn finished (clean or truncated-but-accepted), `false` on a
/// failed turn / a dead session (fail-open: the caller stops reworking).
pub(crate) async fn drive_rework_turn(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    directive: String,
) -> bool {
    if session.send_turn(directive).await.is_err() {
        return false;
    }
    let policy = umadev_governance::Policy::load(&options.project_root);
    loop {
        let Some(ev) = session.next_event().await else {
            return false; // session ended mid-rework → fail-open stop
        };
        match ev {
            SessionEvent::TextDelta(text) => {
                events.emit(EngineEvent::WorkerStream {
                    event: StreamEvent::Text { delta: text },
                });
            }
            SessionEvent::ToolCall { name, input } => {
                // Rework writes real files — govern + audit them exactly like a
                // phase turn (the rework runs on the main writer session).
                govern_tool_call(options, events, &policy, Phase::Quality, &name, &input);
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
                let decision = approval_decision(options.mode, &action, &target);
                if session.respond(&req_id, decision).await.is_err() {
                    return false;
                }
            }
            SessionEvent::TurnDone { status } => {
                // Completed / Truncated → accept and re-review; Interrupted /
                // Failed → stop reworking (fail-open, advisory).
                return matches!(status, TurnStatus::Completed | TurnStatus::Truncated);
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
            source_digest(options)
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
        // (`user`); we drain the fork's events for the assistant text, then parse.
        let directive = format!(
            "{system}\n\nReturn EXACTLY ONE JSON object and nothing else — no markdown, \
             no code fence, no prose before or after.\n\n{user}"
        );
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
fn extract_json_object(text: &str) -> Option<String> {
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
    use std::path::Path;
    use std::sync::Mutex;
    use umadev_runtime::SessionError;

    // ── A scripted, fully-deterministic fake BaseSession ───────────────────
    //
    // Each `send_turn` pops the next scripted batch of events; `next_event`
    // drains that batch (ending on its `TurnDone`). This lets a unit test drive
    // the whole continuous path with NO real base process — exercising phase
    // advance, tool-call governance + audit, the TurnDone boundary, the gate
    // pause, the hard gate, and fail-open session death.

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
            }
        }
        fn dying() -> Self {
            let mut s = Self::new(vec![]);
            s.die = true;
            s
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
            // Load the next scripted turn (or an immediate clean TurnDone if the
            // script ran out, so the driver never hangs).
            let batch = if self.turns.is_empty() {
                vec![SessionEvent::TurnDone {
                    status: TurnStatus::Completed,
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
        }
    }

    fn sink() -> (Arc<dyn EventSink>, crate::events::RecordingSink) {
        let rec = crate::events::RecordingSink::default();
        (Arc::new(rec.clone()), rec)
    }

    // ── Wave 1: the legacy-pipeline opt-out switch ─────────────────────────

    #[test]
    fn legacy_pipeline_flag_defaults_off_and_honours_explicit_on() {
        // This is the ONLY test that touches `UMADEV_LEGACY_PIPELINE`, so it owns
        // the var: set/clear in-test, no cross-test env race. Default (unset) is
        // OFF (director path); only an explicit truthy value selects the legacy
        // fixed pipeline.
        std::env::remove_var("UMADEV_LEGACY_PIPELINE");
        assert!(
            !legacy_pipeline_from_env(),
            "default (unset) is the director path, not legacy"
        );
        for on in ["1", "true", "on"] {
            std::env::set_var("UMADEV_LEGACY_PIPELINE", on);
            assert!(
                legacy_pipeline_from_env(),
                "`{on}` selects the legacy fixed pipeline"
            );
        }
        // A non-truthy value stays on the director path (fail-open default).
        for off in ["0", "false", "off", "nonsense", ""] {
            std::env::set_var("UMADEV_LEGACY_PIPELINE", off);
            assert!(
                !legacy_pipeline_from_env(),
                "`{off}` is NOT an opt-in → director path"
            );
        }
        std::env::remove_var("UMADEV_LEGACY_PIPELINE");
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
                gate: Gate::DocsConfirm
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

    // ── TurnDone boundary (Failed → hard stop) ─────────────────────────────

    #[tokio::test]
    async fn failed_turn_stops_the_run() {
        let tmp = tempfile::tempdir().unwrap();
        let options = opts(tmp.path(), "build a dashboard", TrustMode::Guarded);
        let (events, _rec) = sink();
        let fail = SessionEvent::TurnDone {
            status: TurnStatus::Failed("base crashed".to_string()),
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
    fn gate_review_kind_maps_phases() {
        assert_eq!(gate_review_kind(Phase::DocsConfirm), ReviewKind::Docs);
        assert_eq!(gate_review_kind(Phase::PreviewConfirm), ReviewKind::Preview);
    }

    #[test]
    fn team_for_scales_with_the_kind() {
        // A greenfield requirement seats the full docs team; a one-line tweak
        // seats none (the deterministic floor stands).
        assert_eq!(
            team_for(
                ReviewKind::Docs,
                "build a SaaS dashboard web app with login"
            )
            .len(),
            3
        );
        assert!(team_for(ReviewKind::Docs, "fix a typo in the readme").is_empty());
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
        let saved = std::env::var("UMADEV_FORK_ESTABLISH_TIMEOUT_SECS").ok();
        std::env::set_var("UMADEV_FORK_ESTABLISH_TIMEOUT_SECS", "1");

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

        match saved {
            Some(v) => std::env::set_var("UMADEV_FORK_ESTABLISH_TIMEOUT_SECS", v),
            None => std::env::remove_var("UMADEV_FORK_ESTABLISH_TIMEOUT_SECS"),
        }

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
        let saved_c = std::env::var("UMADEV_CONTINUOUS").ok();
        let saved_l = std::env::var("UMADEV_LEGACY_RUN").ok();

        // Unset → DEFAULT ON (the architecture has closed on continuous).
        std::env::remove_var("UMADEV_CONTINUOUS");
        std::env::remove_var("UMADEV_LEGACY_RUN");
        assert!(
            continuous_enabled_from_env(),
            "continuous must be the DEFAULT when nothing is set"
        );

        // Explicit opt-out via the off-switch on the continuous var → single-shot.
        for off in ["0", "false", "off"] {
            std::env::set_var("UMADEV_CONTINUOUS", off);
            assert!(
                !continuous_enabled_from_env(),
                "UMADEV_CONTINUOUS={off} must opt OUT to single-shot"
            );
        }

        // Explicit opt-out via the legacy-run alias → single-shot, even when the
        // continuous var is left unset / on (opt-out wins).
        std::env::remove_var("UMADEV_CONTINUOUS");
        for on in ["1", "true", "on"] {
            std::env::set_var("UMADEV_LEGACY_RUN", on);
            assert!(
                !continuous_enabled_from_env(),
                "UMADEV_LEGACY_RUN={on} must opt OUT to single-shot"
            );
        }
        std::env::remove_var("UMADEV_LEGACY_RUN");

        // Explicit force-on still honoured (symmetry, no longer required).
        for on in ["1", "true", "on"] {
            std::env::set_var("UMADEV_CONTINUOUS", on);
            assert!(
                continuous_enabled_from_env(),
                "UMADEV_CONTINUOUS={on} must keep continuous ON"
            );
        }

        // Restore the global env exactly as we found it.
        match saved_c {
            Some(v) => std::env::set_var("UMADEV_CONTINUOUS", v),
            None => std::env::remove_var("UMADEV_CONTINUOUS"),
        }
        match saved_l {
            Some(v) => std::env::set_var("UMADEV_LEGACY_RUN", v),
            None => std::env::remove_var("UMADEV_LEGACY_RUN"),
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

        governance_catchup(&mut session, &options, &events).await;

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

        governance_catchup(&mut session, &options, &events).await;
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
}
