use super::{
    clear_operational_review_checkpoint, ensure_final_review_retry_step_in_plan,
    next_final_review_checkpoint, next_step_review_checkpoint, plan_state,
    record_artifact_versions, save_operational_review_checkpoint, Arc, DirectorLoopOutcome,
    EngineEvent, EventSink, OperationalReviewCheckpoint, OperationalReviewEvidence, Plan,
    RoutePlan, RunOptions, StepStatus,
};

fn evidence_summary(items: &[String]) -> String {
    if items.is_empty() {
        return "required reviewer did not return a trustworthy result".to_string();
    }
    items
        .iter()
        .take(4)
        .map(|item| item.chars().take(240).collect::<String>())
        .collect::<Vec<_>>()
        .join("; ")
}

fn paused_circuit_reason(
    boundary: &str,
    checkpoint: &OperationalReviewCheckpoint,
    evidence: &str,
) -> String {
    format!(
        "{boundary} remained unavailable after {} bounded review boundaries (each included one fresh-session retry): {evidence}. Automatic review retries are paused; the saved run remains resumable and no reviewer outage was sent to source repair",
        checkpoint.effective_outages()
    )
}

pub(super) fn block_open_steps(plan: &mut Plan, events: &Arc<dyn EventSink>) {
    for step in &mut plan.steps {
        if matches!(step.status, StepStatus::Pending | StepStatus::Active) {
            step.status = StepStatus::Blocked;
            events.emit(EngineEvent::plan_step_status(
                step.id.clone(),
                step.title.clone(),
                StepStatus::Blocked,
            ));
        }
    }
}

/// Persist and pause one unavailable step-review boundary.
///
/// The first unavailable boundary parks the exact review cursor. A second
/// consecutive boundary opens the automatic retry circuit; it still remains a
/// PAUSE, and a later explicit `/continue` re-arms that same cursor without
/// replaying source work.
pub(super) struct StepReviewOutage<'a> {
    pub options: &'a RunOptions,
    pub events: &'a Arc<dyn EventSink>,
    pub plan: &'a mut Plan,
    pub task_tracker: &'a mut crate::plan_tasks::PlanTaskTracker,
    pub step: &'a crate::plan_state::PlanStep,
    pub route: &'a RoutePlan,
    pub base_agents: &'a crate::bg_agents::BaseAgentObservation,
    pub semantic_blocking: &'a [String],
    pub operational_unavailable: &'a [String],
    pub prior: Option<&'a OperationalReviewCheckpoint>,
}

pub(super) fn handle_step_review_outage(context: StepReviewOutage<'_>) -> DirectorLoopOutcome {
    let StepReviewOutage {
        options,
        events,
        plan,
        task_tracker,
        step,
        route,
        base_agents,
        semantic_blocking,
        operational_unavailable,
        prior,
    } = context;
    let typed_evidence = OperationalReviewEvidence::new(semantic_blocking, operational_unavailable);
    let checkpoint = next_step_review_checkpoint(
        prior,
        step.id.clone(),
        crate::freshness::workspace_qc_fingerprint(&options.project_root),
        Some(route.team.clone()),
        step.kind == plan_state::StepKind::Build,
        typed_evidence,
    );
    let evidence = evidence_summary(&checkpoint.evidence().operational_unavailable);
    let boundary = format!("required review at step `{}`", step.title);
    let semantic = if checkpoint.evidence().semantic_blocking.is_empty() {
        String::new()
    } else {
        format!(
            "; {} semantic finding(s) retained separately",
            checkpoint.evidence().semantic_blocking.len()
        )
    };
    let circuit_open = checkpoint.circuit_open();
    let reason = if circuit_open {
        paused_circuit_reason(&boundary, &checkpoint, &evidence)
    } else {
        format!("{boundary} unavailable: {evidence}{semantic}")
    };
    let saved = save_operational_review_checkpoint(&options.project_root, &checkpoint).is_ok()
        && plan_state::save(plan, &options.project_root).is_ok();
    if saved {
        let blockers = checkpoint.evidence().ledger_blockers();
        if let Err(error) = task_tracker.settle_base_agents(
            step,
            base_agents,
            StepStatus::Blocked,
            true,
            &reason,
            &blockers,
        ) {
            events.emit(EngineEvent::Note(format!(
                "team · reviewer child ledger could not settle its outage ({error}); the typed review checkpoint remains resumable"
            )));
        }
        if let Err(error) = task_tracker.wait_for_user(&reason) {
            events.emit(EngineEvent::Note(format!(
                "team · plan task ledger could not append its pause ({error}); the typed review checkpoint remains resumable"
            )));
        }
        record_artifact_versions(&options.project_root);
        let (done, total) = plan.progress();
        let retry = if circuit_open {
            "automatic retries are paused; type /continue to retry this same saved review when the reviewer service is available"
        } else {
            "the automatic fresh-session retry also failed; type /continue to retry this same saved review, or enter a new requirement"
        };
        events.emit(EngineEvent::Note(format!(
            "team · {reason} — plan paused without source rework; {retry}"
        )));
        return DirectorLoopOutcome::PausedAtOperational {
            reason,
            done,
            total,
        };
    }

    clear_operational_review_checkpoint(&options.project_root);
    let reason = format!("{reason}; the resumable checkpoint could not be persisted");
    block_open_steps(plan, events);
    let _ = plan_state::save(plan, &options.project_root);
    let _ = task_tracker.finish(false, &reason, vec![reason.clone()]);
    DirectorLoopOutcome::Failed(reason)
}

/// Persist and pause an unavailable final whole-build review.
pub(super) struct FinalReviewOutage<'a> {
    pub options: &'a RunOptions,
    pub events: &'a Arc<dyn EventSink>,
    pub plan: &'a mut Plan,
    pub task_tracker: &'a mut crate::plan_tasks::PlanTaskTracker,
    pub route: &'a RoutePlan,
    pub semantic_blocking: &'a [String],
    pub operational_unavailable: &'a [String],
    pub prior: Option<&'a OperationalReviewCheckpoint>,
    pub resident_task: Option<&'a mut crate::task_lifecycle::EntryTaskTracker>,
    pub checkpoint_entry_task_run_id: Option<&'a str>,
}

pub(super) fn handle_final_review_outage(context: FinalReviewOutage<'_>) -> DirectorLoopOutcome {
    let FinalReviewOutage {
        options,
        events,
        plan,
        task_tracker,
        route,
        semantic_blocking,
        operational_unavailable,
        prior,
        mut resident_task,
        checkpoint_entry_task_run_id,
    } = context;
    let typed_evidence = OperationalReviewEvidence::new(semantic_blocking, operational_unavailable);
    ensure_final_review_retry_step_in_plan(plan, Some(route), events);
    let entry_task_run_id = resident_task
        .as_ref()
        .map(|task| task.run_id().to_string())
        .or_else(|| checkpoint_entry_task_run_id.map(str::to_string));
    let checkpoint = next_final_review_checkpoint(
        prior,
        crate::freshness::workspace_qc_fingerprint(&options.project_root),
        Some(route.team.clone()),
        entry_task_run_id,
        typed_evidence,
    );
    let evidence = evidence_summary(&checkpoint.evidence().operational_unavailable);
    let semantic = if semantic_blocking.is_empty() {
        String::new()
    } else {
        format!("; {} semantic finding(s) retained", semantic_blocking.len())
    };
    let circuit_open = checkpoint.circuit_open();
    let reason = if circuit_open {
        paused_circuit_reason("final quality review", &checkpoint, &evidence)
    } else {
        format!("final quality review unavailable: {evidence}{semantic}")
    };
    let saved = save_operational_review_checkpoint(&options.project_root, &checkpoint).is_ok()
        && plan_state::save(plan, &options.project_root).is_ok();
    if saved {
        if let Some(task) = resident_task.as_mut() {
            if let Err(error) = task.wait(&reason) {
                events.emit(EngineEvent::Note(format!(
                    "team · resident task ledger could not append its pause ({error}); the typed review checkpoint remains resumable"
                )));
            }
        }
        if let Err(error) = task_tracker.wait_for_user(&reason) {
            events.emit(EngineEvent::Note(format!(
                "team · plan task ledger could not append its pause ({error}); the typed review checkpoint remains resumable"
            )));
        }
        record_artifact_versions(&options.project_root);
        let (done, total) = plan.progress();
        let retry = if circuit_open {
            "automatic retries are paused; type /continue to retry this same saved review when the reviewer service is available"
        } else {
            "the automatic fresh-session retry also failed; type /continue to retry this same saved review, or enter a new requirement"
        };
        events.emit(EngineEvent::Note(format!(
            "team · {reason} — plan paused without source rework; {retry}"
        )));
        return DirectorLoopOutcome::PausedAtOperational {
            reason,
            done,
            total,
        };
    }

    clear_operational_review_checkpoint(&options.project_root);
    let reason = format!("{reason}; the typed review checkpoint could not be persisted");
    block_open_steps(plan, events);
    let mut blockers = semantic_blocking.to_vec();
    blockers.extend_from_slice(operational_unavailable);
    if blockers.is_empty() {
        blockers.push(reason.clone());
    }
    if let Some(task) = resident_task.as_mut() {
        let _ = task.fail(&reason, blockers.clone());
    }
    let _ = task_tracker.finish(false, &reason, blockers);
    let _ = plan_state::save(plan, &options.project_root);
    events.emit(EngineEvent::Note(format!("team · {reason}")));
    DirectorLoopOutcome::Failed(reason)
}
