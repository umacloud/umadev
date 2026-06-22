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
use crate::trust::{requires_confirmation, TrustMode};

/// The hard ceiling on rework rounds at any single review node. The critic team
/// is ADVISORY: it may fold blocking findings into ONE rework directive and
/// re-review, but the loop is bounded so a base that can't satisfy a seat (or a
/// flapping verdict) can NEVER spin forever. After this many rounds the node
/// proceeds regardless — the deterministic floor + the user gate are the real
/// stop signals, never a critic. Kept small (the docs/preview teams already cost
/// N advisory base calls per round) so the wall-clock stays bounded.
const MAX_REWORK_ROUNDS: usize = 2;

/// Read the gradual-rollout switch for the continuous-session path.
///
/// The single-shot path is the DEFAULT; this path is opt-in via
/// `UMADEV_CONTINUOUS=1` so it can be A/B-tested and reverted without a code
/// change. Read once at the app boundary (CLI / TUI), the same way
/// [`crate::runner::strict_coverage_from_env`] is, so a run sees a stable
/// snapshot rather than a live process-global env read mid-run.
#[must_use]
pub fn continuous_enabled_from_env() -> bool {
    matches!(
        std::env::var("UMADEV_CONTINUOUS").as_deref(),
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

/// Drive ONE block of the 9-phase pipeline over a single live [`BaseSession`],
/// stopping at the first confirmation gate (or at delivery / a hard stop).
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

        // Quality is a REVIEW node too (not a confirm gate): after the quality
        // phase runs the real build/test/lint, the QA / security / backend /
        // DevOps seats review the delivered code on read-only forks, and any
        // blocking findings drive a bounded rework on the main session before
        // delivery. Advisory + fail-open; never blocks the run.
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

    events.emit(EngineEvent::BlockCompleted {
        final_phase: phases.last().copied().unwrap_or(Phase::Delivery),
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
            SessionEvent::TurnDone { status } => return finish_turn(events, phase, status),
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
fn govern_tool_call(
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    policy: &umadev_governance::Policy,
    phase: Phase,
    name: &str,
    input: &serde_json::Value,
) {
    let (target, decision) = evaluate_tool_call(policy, name, input);

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
        let decision = umadev_governance::scan_content_with_policy(path, content, policy);
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
fn finish_turn(events: &Arc<dyn EventSink>, phase: Phase, status: TurnStatus) -> PhaseResult {
    match status {
        TurnStatus::Completed => PhaseResult::Done,
        TurnStatus::Truncated => {
            // The base hit a turn/budget ceiling with partial work. Accept what
            // exists and move on (the hard gate / verify floor catches an
            // empty result downstream) — but flag it so the block boundary can
            // warn the output may be incomplete.
            events.emit(EngineEvent::Note(umadev_i18n::tlf(
                "continuous.phase_truncated",
                &[phase.id()],
            )));
            PhaseResult::Done
        }
        TurnStatus::Interrupted => PhaseResult::Failed(format!("{} interrupted", phase.id())),
        TurnStatus::Failed(reason) => PhaseResult::Failed(reason),
    }
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
/// plan. Two regimes:
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
    phases_for_block(start_after)
        .iter()
        .copied()
        .filter(|p| plan.includes(*p))
        .collect()
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
enum ReviewKind {
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
fn team_for(kind: ReviewKind, requirement: &str) -> Vec<Box<dyn RoleCritic>> {
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
async fn run_review_team(
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

    // PARALLEL: fork one read-only session per seat up front (each `fork()` is a
    // quick `&mut` borrow that returns an OWNED, independent session), then drive
    // every critic concurrently — the reviews hold only their own forks, never
    // the main session. `fork()` is independent per call, so the N reviews never
    // collide and never touch the main writer (single-writer invariant). A fork
    // failure is per-seat fail-open: that seat consults nothing and ACCEPTS.
    let mut forks = Vec::with_capacity(team.len());
    for _ in team {
        forks.push(session.fork().await);
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
async fn drive_rework_turn(
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
struct Blackboard {
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
    fn read(options: &RunOptions, kind: ReviewKind) -> Self {
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
        Self {
            prd,
            architecture,
            uiux,
            code,
            // The deterministic floors are surfaced as CONTEXT to the QA /
            // security seats (so their semantic pass focuses on what a static
            // check can't see). Empty for the docs / preview nodes.
            qa_floor: String::new(),
            security_floor: String::new(),
        }
    }

    /// Borrow the blackboard as the critic-facing [`CriticArtifacts`].
    fn artifacts<'a>(&'a self, requirement: &'a str) -> CriticArtifacts<'a> {
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
struct ForkConsult {
    /// The read-only fork, or the error that prevented opening one. `Mutex` so
    /// the `&self` `judge` can drive the `&mut` session.
    fork: tokio::sync::Mutex<Result<Box<dyn BaseSession>, SessionError>>,
}

impl ForkConsult {
    fn new(fork: Result<Box<dyn BaseSession>, SessionError>) -> Self {
        Self {
            fork: tokio::sync::Mutex::new(fork),
        }
    }

    /// Best-effort close the underlying fork session.
    async fn end(&self) {
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
            }
        }
        fn dying() -> Self {
            let mut s = Self::new(vec![]);
            s.die = true;
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
            "Bash",
            &serde_json::json!({ "command": "rm -rf /" }),
        );
        assert_eq!(cmd, "rm -rf /");
        assert!(decision.block, "rm -rf must block");
    }

    #[tokio::test]
    async fn read_tool_is_observe_only_and_passes() {
        let policy = umadev_governance::Policy::default();
        let (_t, decision) =
            evaluate_tool_call(&policy, "Read", &serde_json::json!({ "file_path": "a.rs" }));
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
        // spec, frontend, backend, quality — four lean turns, all clean.
        let mut session =
            FakeBaseSession::new(vec![vec![done()], vec![done()], vec![done()], vec![done()]]);
        let sent = session.sent_handle();

        // A Light plan is GATELESS → it drives the WHOLE lean list in one block
        // from Research start, runs to completion, and NEVER pauses at a gate.
        let outcome = run_block(&mut session, &options, &events, Phase::Research).await;
        assert_eq!(outcome, RunOutcome::Completed);

        let sent = sent.lock().unwrap();
        // spec + frontend + backend + quality (no research, no docs).
        assert_eq!(
            sent.len(),
            4,
            "lean plan: spec/frontend/backend/quality only"
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
}
