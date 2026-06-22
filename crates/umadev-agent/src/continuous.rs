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
//! approval + the single-writer run lock. Critics route through the existing
//! consult mechanism (a fork-based read-only critic is a follow-up — see the
//! `TODO(fork-critic)` below).
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

use umadev_runtime::{ApprovalDecision, BaseSession, SessionEvent, StreamEvent, TurnStatus};
use umadev_spec::Phase;

use crate::events::{EngineEvent, EventSink};
use crate::gates::Gate;
use crate::runner::RunOptions;
use crate::trust::{requires_confirmation, TrustMode};

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

    let phases = phases_for_block(start_after);
    if phases.is_empty() {
        // Nothing to drive (e.g. a docs-only plan resumed past its last phase).
        return RunOutcome::Completed;
    }

    if start_after == Phase::Research {
        events.emit(EngineEvent::PipelineStarted {
            slug: options.effective_slug(),
            requirement: options.requirement.clone(),
        });
    }

    let mut first_directive = start_after == Phase::Research;
    for &phase in phases {
        // A gate is a pause point, not a base turn: stop here, let the caller
        // wait for the user, and resume on the next block.
        if phase.is_gate() {
            // TODO(fork-critic): the role-critic TEAM (PM / architect / UIUX at
            // the docs gate; UIUX / frontend at the preview gate) belongs HERE —
            // routed through a `BaseSession::fork()` read-only session so each
            // seat reviews the main line's output in isolation (see design
            // §3.5). `BaseSession` has no `fork()` yet, so until it lands the
            // existing `ForkedConsult` consult path (`runner.rs`) carries the
            // critic team; this driver gets the main loop right first. Critics
            // are advisory + fail-open and NEVER drive loop termination, so the
            // gate semantics above are already correct without them.
            let gate = gate_for_phase(phase);
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
                "[plan] 计划模式(只读):不执行 spec/前端/后端,未写入真实代码。  \
                 [plan] Plan mode (read-only): spec/frontend/backend not executed."
                    .to_string(),
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
        )
        .await;
        // `first_directive` is consumed by `std::mem::take` only when this is
        // the very first directive of the run; subsequent phases are lean.
        match outcome {
            PhaseResult::Done => {}
            PhaseResult::Failed(reason) => {
                events.emit(EngineEvent::Note(format!(
                    "[fail] {} 阶段失败:{reason} —— 持续会话路径停止。",
                    phase.id()
                )));
                return RunOutcome::HardStop(format!("phase {} failed: {reason}", phase.id()));
            }
        }
        events.emit(EngineEvent::PhaseCompleted { phase });

        // HARD STOP (git-independent): after the last code-producing phase, if
        // the plan was supposed to produce code and the workspace has ZERO real
        // source files, the run is a disguised-empty delivery — stop, fail.
        if phase == last_code_phase(&plan) && produces_code {
            let n = crate::acceptance::source_files(&options.project_root).len();
            if n == 0 {
                let msg = "[fail] 未产出真实代码 — 流水线停止,未交付(持续会话路径硬门)。  \
                     [fail] No real source files produced — pipeline stopped, nothing delivered."
                    .to_string();
                events.emit(EngineEvent::Note(msg.clone()));
                return RunOutcome::HardStop(msg);
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
) -> PhaseResult {
    let directive = phase_directive(options, phase, first_directive);
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
                    events.emit(EngineEvent::Note(format!(
                        "[gate] 危险动作需用户确认,本回合按拒绝处理:{action} → {target}"
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
        events.emit(EngineEvent::Note(format!(
            "[gov] {} 阶段:底座工具调用触发 {} —— {} (target: {target})",
            phase.id(),
            decision.clause,
            decision.reason,
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
            events.emit(EngineEvent::Note(format!(
                "[warn] {} 阶段被截断(底座到达回合上限),保留已产出,继续。",
                phase.id()
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
/// Crucially every directive is COMMAND-style: "produce X now, write the files
/// directly, do NOT ask me whether to continue." This is the single fix for the
/// single-shot path's "base replies a paragraph and asks 'shall I continue?'"
/// failure — in a live agentic session the base just does it.
fn phase_directive(options: &RunOptions, phase: Phase, first: bool) -> String {
    let slug = options.effective_slug();
    let req = &options.requirement;
    let no_ask = "Work autonomously: use your tools to do this NOW, write all files \
         directly to disk, and do NOT ask me whether to continue — just produce the \
         deliverable. When done, end your turn.";

    match phase {
        Phase::Research => {
            let p = crate::experts::research_prompt(&slug, req, "");
            if first {
                format!("{}\n\n{}\n\n{no_ask}", p.system, p.user)
            } else {
                format!("Now do the research phase.\n\n{}\n\n{no_ask}", p.user)
            }
        }
        Phase::Docs => format!(
            "Now produce ALL THREE core documents for `{slug}`, writing each file directly:\n\
             - `output/{slug}-prd.md` (product requirements)\n\
             - `output/{slug}-architecture.md` (architecture + API surface table)\n\
             - `output/{slug}-uiux.md` (design system: tokens, typography, icon library)\n\
             Use the research you just produced. Follow the UmaDev rules you were given \
             (no emoji icons, design-token colors only, frontend fetch paths must match the \
             architecture API table).\n\n{no_ask}"
        ),
        Phase::Spec => format!(
            "The user has APPROVED the three documents. Now translate them into an \
             implementation spec + a task breakdown for `{slug}` (write the spec/tasks \
             files). Cite the PRD's `FR-` ids so coverage maps 1:1.\n\n{no_ask}"
        ),
        Phase::Frontend => format!(
            "Now IMPLEMENT THE FRONTEND for `{slug}` as REAL code files (components, pages, \
             API client, design-token styles) from the UIUX + architecture docs you wrote. \
             Icons from the declared library only — never emoji. Wire every fetch URL to an \
             architecture API path. Run the build and fix errors. Write \
             `output/{slug}-frontend-notes.md` with the preview URL + run command.\n\n{no_ask}"
        ),
        Phase::Backend => format!(
            "Now IMPLEMENT THE BACKEND for `{slug}` as REAL code files (routes, models, \
             middleware, tests) matching the architecture API surface. Validate inputs, \
             use the standard error envelope, write + run tests. Write \
             `output/{slug}-backend-notes.md`.\n\n{no_ask}"
        ),
        Phase::Quality => format!(
            "Now run QUALITY for `{slug}`: run the project's real build + test + lint, fix \
             what fails, and do a security pass (no hardcoded secrets, input validation, \
             safe error handling). Summarize results.\n\n{no_ask}"
        ),
        Phase::Delivery => format!(
            "Now produce the DELIVERY recipe for `{slug}`: verify the production build for \
             frontend + backend, and write exact deployment instructions. Do NOT deploy to \
             any remote system — only verify locally and write the recipe.\n\n{no_ask}"
        ),
        // Gate phases never get a directive (the driver pauses before them); a
        // defensive empty directive keeps this total.
        Phase::DocsConfirm | Phase::PreviewConfirm => String::new(),
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
    }

    impl FakeBaseSession {
        fn new(turns: Vec<Vec<SessionEvent>>) -> Self {
            Self {
                turns,
                current: std::collections::VecDeque::new(),
                sent: Arc::new(Mutex::new(Vec::new())),
                responded: Arc::new(Mutex::new(Vec::new())),
                die: false,
            }
        }
        fn dying() -> Self {
            let mut s = Self::new(vec![]);
            s.die = true;
            s
        }
        fn sent_handle(&self) -> Arc<Mutex<Vec<String>>> {
            Arc::clone(&self.sent)
        }
        fn responded_handle(&self) -> Arc<Mutex<Vec<(String, ApprovalDecision)>>> {
            Arc::clone(&self.responded)
        }
    }

    #[async_trait::async_trait]
    impl BaseSession for FakeBaseSession {
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
}
