use super::{
    clear_operational_review_checkpoint, drive_one_turn_with_memories, emit_blocker_assessments,
    ensure_final_review_retry_step, load_operational_review_checkpoint,
    operational_review_checkpoint_for_plan, plan_state, quality_evidence, record_artifact_versions,
    render_project_learned_reference, run_auto_qc, run_budget, run_critic_review_only,
    save_operational_review_checkpoint, source_tree_snapshot, Arc, BaseSession, EngineEvent,
    EventSink, IdleBudget, KnowledgeDigest, OperationalReviewCheckpoint, Phase, Plan, RoutePlan,
    RunOptions, SentReceiptGuard, SourceTreeSnapshot, StepStatus, TurnOutcome, MAX_QC_ROUNDS,
};

/// The final whole-build QC gate run once a step-driven plan has walked its DAG —
/// the SAME `run_auto_qc` pass the single-turn loop ends on, folded into ONE
/// bounded fix turn so a step-driven build is held to the identical objective floor.
/// Returns the fix turn's reply (empty when QC was already clean). Bounded by
/// `MAX_QC_ROUNDS`; fail-open throughout.
///
/// Wall-clock ceiling (graceful): the read-only QC READ ALWAYS runs (every iteration),
/// so the build is ALWAYS held to the objective floor even at the budget; only the
/// minute-level FIX TURN it would trigger is skipped once the deadline is spent (the
/// doc'd "hard ceiling" — the build could otherwise run several fix turns over budget
/// here). A residual finding is returned as a dirty outcome and becomes an honest
/// director failure; it is never delegated to a narrower source-only caller check.
/// The outcome of `run_final_gate`: the final fix-turn reply PLUS whether the gate
/// settled CLEAN. H1: the step-driven caller must AND `clean` into its finalize
/// decision — a build whose steps all ticked Done but whose final cross-cutting gate
/// (coverage / contract / runtime-proof / governance / fork review) stayed DIRTY must
/// NOT be finalized as a clean delivery (which would ship a full proof-pack/scorecard
/// disguising an incomplete build as success).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostBuildQcOutcome {
    /// The last fix-turn's reply (empty when QC was already clean / no fix ran).
    pub reply: String,
    /// `true` only when the QC read came back clean within the bounded rounds;
    /// `false` when the gate settled with residual blocking findings (budget /
    /// deadline / dead session).
    pub clean: bool,
    /// The last objective blocking findings. Empty on a clean gate; retained on
    /// every dirty settle so the director's terminal failure carries evidence.
    pub blocking: Vec<String>,
    /// Host-owned review outage evidence. Non-empty means the gate could not
    /// settle and must be retried; it is never a product blocker.
    pub operational_unavailable: Vec<String>,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_final_gate(
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
    // A persisted final-review checkpoint may prove that every deterministic QC
    // floor already passed for the exact current QC-input fingerprint (source,
    // tests, manifests/locks, tool configuration, Docker and CI inputs). In that
    // one case round 0 retries only the unavailable reviewer.
    review_only_first_round: bool,
    // Explicit `/run` owns a pre-run baseline. Chat-originated builds do not.
    attribute_to_director_run: bool,
) -> PostBuildQcOutcome {
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
        let qc = if round == 0 && review_only_first_round {
            events.emit(EngineEvent::Note(
                "team · source unchanged since the parked QC receipt — retrying only the final review"
                    .to_string(),
            ));
            run_critic_review_only(session, options, events, Some(route)).await
        } else {
            run_auto_qc(
                session,
                options,
                events,
                Some(route),
                Some(verify_signal.as_str()),
                verify_ran_build_tool,
                attribute_to_director_run,
            )
            .await
        };
        if let Some(receipt) = pending_memory_receipt.take() {
            let outcome = if qc.is_clean() {
                TurnOutcome::Pass
            } else if qc.has_operational_failure() {
                TurnOutcome::Unknown
            } else {
                TurnOutcome::Fail
            };
            let _ = receipt.settle(outcome);
        }
        if qc.is_clean() {
            return PostBuildQcOutcome {
                reply: last_reply,
                clean: true,
                blocking: Vec::new(),
                operational_unavailable: Vec::new(),
            };
        }
        if qc.has_operational_failure() {
            events.emit(EngineEvent::Note(if qc.blocking.is_empty() {
                quality_evidence::operational_stop_note(&qc.operational)
            } else {
                quality_evidence::operational_mixed_note(&qc.operational)
            }));
            return PostBuildQcOutcome {
                reply: last_reply,
                clean: false,
                blocking: qc.blocking,
                operational_unavailable: qc.operational,
            };
        }
        if round == 0 && review_only_first_round {
            // `/continue` resumed one exact, previously unavailable review boundary.
            // Its authority is review-only: a newly available semantic verdict may
            // settle that run, but it must not silently reopen the writer, edit source,
            // run another QC cycle, or manufacture a new review. The user can start a
            // fresh writable `/run` with the retained findings when they want repair.
            last_blocking = qc.residual_evidence();
            events.emit(EngineEvent::Note(
                "team · resumed final review returned actionable findings — the parked run is \
                 closed without reopening source writes; use /run to repair explicitly"
                    .to_string(),
            ));
            return PostBuildQcOutcome {
                reply: last_reply,
                clean: false,
                blocking: last_blocking,
                operational_unavailable: Vec::new(),
            };
        }
        last_blocking = qc.residual_evidence();
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
            return PostBuildQcOutcome {
                reply: last_reply,
                clean: false,
                blocking: last_blocking,
                operational_unavailable: Vec::new(),
            };
        }
        if round + 1 >= MAX_QC_ROUNDS {
            events.emit(EngineEvent::Note(
                "team · final QC reached its fix-round budget — stopping incomplete with residual evidence"
                    .to_string(),
            ));
            return PostBuildQcOutcome {
                reply: last_reply,
                clean: false,
                blocking: last_blocking,
                operational_unavailable: Vec::new(),
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
            return PostBuildQcOutcome {
                reply: last_reply,
                clean: false,
                blocking: last_blocking,
                operational_unavailable: Vec::new(),
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
                return PostBuildQcOutcome {
                    reply: last_reply,
                    clean: false,
                    blocking: last_blocking,
                    operational_unavailable: Vec::new(),
                }
            }
        }
    }
    PostBuildQcOutcome {
        reply: last_reply,
        clean: false,
        blocking: last_blocking,
        operational_unavailable: Vec::new(),
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
/// clean). Required-review transport failures are returned as typed operational
/// evidence; callers must pause/resume that exact boundary rather than report success,
/// repair unrelated source, or silently repeat the review. The wall-clock budget
/// bounds semantic fix turns exactly like `/run`.
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
) -> PostBuildQcOutcome {
    // The wall-clock budget bounds the EXTRA fix turns (graceful ceiling), exactly as
    // the `/run` loop reads it — a chat-build's post-QC rework can never run unbounded.
    let deadline = std::time::Instant::now() + run_budget();
    events.emit(EngineEvent::Note(
        "team · 构建执行已结束，尚未验收 — 正在运行设计/质量扫描 + 团队评审(和 /run 同一套验收)"
            .to_string(),
    ));
    // Recall the commercial-engineering knowledge digest + the project's prior pitfalls
    // ONCE, to front-load onto every fix directive (deliverable 3). The chat session
    // opened firmware-light (no JIT knowledge), so this is where a chat-build's fix gets
    // the standards + memory. Fail-open: empty recall = the byte-for-byte plain directive.
    let context = post_build_rework_context(options);
    // Seed corroboration `false`: this entry has only the seed REPLY text, not an
    // observed run, so round 0 runs UmaDev's own build/test read rather than trusting
    // the seed's prose "it's green" — narration alone must not skip. The caller consumes
    // the complete typed result, including dirty and operational states.
    run_final_gate(
        session, options, events, route, seed_reply, deadline, &context, false, false, false,
    )
    .await
}

/// Durable resume coordinates for a resident chat build whose required final
/// reviewer was operationally unavailable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostBuildOperationalPause {
    /// Bounded reader-facing reason for the pause.
    pub reason: String,
    /// Completed steps in the minimal persisted resume plan.
    pub done: usize,
    /// Total steps in the minimal persisted resume plan.
    pub total: usize,
    /// Compatibility signal that the bounded automatic-retry circuit is open.
    /// This never makes the cursor terminal to an explicit `/continue`.
    pub terminal: bool,
}

/// Cancel and close a persisted operational-review boundary before a fresh run
/// supersedes it or the user explicitly cancels.
///
/// The checkpoint is removed last. Until the exact resident entry (when any),
/// director ledger, and plan cursor are durably terminal, the recovery pointer
/// remains in place so a partial I/O failure cannot manufacture an orphaned
/// `Waiting` writer.
pub fn cancel_operational_review_pause(
    project_root: &std::path::Path,
    detail: &str,
) -> Result<bool, String> {
    let persisted_plan = plan_state::load(project_root);
    let checkpoint = load_operational_review_checkpoint(project_root).or_else(|| {
        persisted_plan
            .as_ref()
            .and_then(|plan| operational_review_checkpoint_for_plan(project_root, plan))
    });
    let Some(checkpoint) = checkpoint else {
        return crate::continuous::cancel_legacy_operational_review_pause(project_root);
    };

    if let Some(plan) = persisted_plan.as_ref() {
        let state = crate::state::read_workflow_state(project_root);
        if let Some(state) = state.filter(|state| {
            !state.backend.trim().is_empty() && !state.requirement.trim().is_empty()
        }) {
            let tracker = crate::plan_tasks::PlanTaskTracker::open_existing(
                project_root,
                &state.backend,
                &state.requirement,
                plan,
            )
            .map_err(|error| format!("could not reopen paused plan ledger: {error}"))?;
            if let Some(mut tracker) = tracker {
                tracker
                    .finish(false, detail, vec![detail.to_string()])
                    .map_err(|error| format!("could not cancel paused plan ledger: {error}"))?;
            }
        } else if !matches!(
            &checkpoint,
            OperationalReviewCheckpoint::FinalGateReview {
                entry_task_run_id: Some(_),
                ..
            }
        ) {
            return Err(
                "paused plan has no backend/requirement identity for ledger settlement".to_string(),
            );
        }
    }

    if let OperationalReviewCheckpoint::FinalGateReview {
        entry_task_run_id: Some(run_id),
        ..
    } = &checkpoint
    {
        crate::task_lifecycle::EntryTaskTracker::cancel_exact(project_root, run_id, detail)
            .map_err(|error| format!("could not cancel resident task {run_id}: {error}"))?;
    }

    if let Some(mut plan) = persisted_plan {
        for step in &mut plan.steps {
            if matches!(step.status, StepStatus::Pending | StepStatus::Active) {
                step.status = StepStatus::Blocked;
            }
        }
        plan_state::save(&plan, project_root)
            .map_err(|error| format!("could not close paused plan cursor: {error}"))?;
    }
    // An explicit cancellation closes the REPAIR channel too: the blocked steps
    // this cursor-close just wrote must stay terminal on a later /continue —
    // repair-resume reopens acceptance-blocked work, never a run the user
    // deliberately cancelled. A fresh plan synthesis clears the marker.
    super::resume::mark_plan_repair_closed(project_root, detail);
    crate::continuous::cancel_legacy_operational_review_pause(project_root)?;
    clear_operational_review_checkpoint(project_root);
    Ok(true)
}

/// Persist the exact post-build review boundary before the TUI publishes an
/// operational pause.
///
/// This is deliberately separate from [`run_post_build_qc`]: the caller first
/// validates its workspace execution postcondition, so a reviewer outage can
/// never hide an out-of-scope write. Once called, either the plan, checkpoint,
/// and workflow state all become durable or the prior plan/state are restored
/// best-effort and an error is returned. The caller must fail closed on error.
pub fn checkpoint_post_build_review_pause(
    options: &RunOptions,
    route: &RoutePlan,
    qc: &PostBuildQcOutcome,
    events: &Arc<dyn EventSink>,
    entry_task_run_id: Option<&str>,
    base_session_id: Option<&str>,
    base_resume_identity: Option<&umadev_runtime::BaseResumeIdentity>,
) -> Result<PostBuildOperationalPause, String> {
    if qc.operational_unavailable.is_empty() {
        return Err("post-build QC has no operational review outage".to_string());
    }

    let evidence = qc
        .operational_unavailable
        .iter()
        .take(4)
        .map(|item| item.chars().take(240).collect::<String>())
        .collect::<Vec<_>>()
        .join("; ");
    let semantic = if qc.blocking.is_empty() {
        String::new()
    } else {
        format!("; {} semantic finding(s) retained", qc.blocking.len())
    };
    let reason = format!("final quality review unavailable: {evidence}{semantic}");

    let prior_plan = plan_state::load(&options.project_root);
    let prior_state = crate::state::read_workflow_state(&options.project_root);
    let mut plan = Some(Plan {
        steps: Vec::new(),
        risks: Vec::new(),
        open_questions: Vec::new(),
    });
    ensure_final_review_retry_step(&mut plan, Some(route), events);
    let plan = plan.expect("the helper always creates a final-review cursor");
    let checkpoint = super::next_final_review_checkpoint(
        None,
        crate::freshness::workspace_qc_fingerprint(&options.project_root),
        Some(route.team.clone()),
        entry_task_run_id.map(str::to_string),
        super::OperationalReviewEvidence::new(&qc.blocking, &qc.operational_unavailable),
    );
    let plan_path = plan_state::save(&plan, &options.project_root)
        .map_err(|error| format!("could not persist post-build review plan: {error}"))?;

    if let Err(error) = save_operational_review_checkpoint(&options.project_root, &checkpoint) {
        if let Some(prior) = prior_plan.as_ref() {
            let _ = plan_state::save(prior, &options.project_root);
        } else {
            let _ = umadev_state::fs::remove_regular_file(&plan_path);
        }
        return Err(format!(
            "could not persist post-build review checkpoint: {error}"
        ));
    }

    let mut state = prior_state
        .clone()
        .unwrap_or_else(|| crate::state::WorkflowState::new(Phase::Quality));
    state.phase = Phase::Quality.id().to_string();
    state.active_gate.clear();
    state.slug = options.effective_slug();
    state.requirement.clone_from(&options.requirement);
    state.last_transition_at = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    state.note = if checkpoint.circuit_open() {
        "Operational review circuit open after bounded automatic retry".to_string()
    } else {
        "Paused at final quality review (operational outage)".to_string()
    };
    state.backend.clone_from(&options.backend);
    state.base_session_id = base_session_id.map(str::to_string);
    state.base_resume_identity = base_resume_identity.cloned();
    state.permission_profile = Some(options.mode.base_permissions());
    if let Err(error) = crate::state::write_workflow_state(&options.project_root, &state) {
        clear_operational_review_checkpoint(&options.project_root);
        if let Some(prior) = prior_plan.as_ref() {
            let _ = plan_state::save(prior, &options.project_root);
        } else {
            let _ = umadev_state::fs::remove_regular_file(&plan_path);
        }
        if let Some(prior) = prior_state.as_ref() {
            let _ = crate::state::write_workflow_state(&options.project_root, prior);
        }
        return Err(format!(
            "could not persist post-build review workflow state: {error}"
        ));
    }

    record_artifact_versions(&options.project_root);
    let (done, total) = plan.progress();
    Ok(PostBuildOperationalPause {
        reason,
        done,
        total,
        terminal: checkpoint.circuit_open(),
    })
}

/// Build the CONTEXT prefix front-loaded onto a chat-build's post-QC fix directives —
/// the recalled commercial-engineering knowledge digest (`agentic_knowledge_digest`)
/// plus the project's prior pitfalls (`relevant_lessons_for_prompt`). The chat session
/// opens firmware-LIGHT (no JIT knowledge layer — that's the latency-saving default),
/// so a fix turn would otherwise repair blind; this restores the standards + memory at
/// the one point it matters (fixing real findings), without paying the full firmware
/// cost on every chat message. Pure + fully fail-open: each contributor swallows its
/// own errors into an empty string (the plain directive), never a panic or a block.
pub(super) fn post_build_rework_context(options: &RunOptions) -> KnowledgeDigest {
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
