use super::{
    clear_operational_review_checkpoint, ensure_final_review_retry_step_in_plan,
    next_final_review_checkpoint, next_step_review_checkpoint, plan_state,
    record_artifact_versions, save_operational_review_checkpoint, Arc, DirectorLoopOutcome,
    EngineEvent, EventSink, OperationalReviewCheckpoint, Plan, RoutePlan, RunOptions, StepStatus,
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

fn stop_reason(boundary: &str, checkpoint: &OperationalReviewCheckpoint, evidence: &str) -> String {
    format!(
        "{boundary} remained unavailable after {} bounded review boundaries (each included one fresh-session retry): {evidence}. This run is stopped incomplete; no reviewer outage was sent to source repair. Start a new /run or send a new requirement when the reviewer service is available",
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

/// Persist or terminally settle one unavailable step-review boundary.
///
/// The first outage parks one final `/continue` opportunity. The second outage
/// over the same QC fingerprint opens the durable circuit and closes all live
/// plan tasks, so another `/continue` cannot recreate the same wait forever.
pub(super) struct StepReviewOutage<'a> {
    pub options: &'a RunOptions,
    pub events: &'a Arc<dyn EventSink>,
    pub plan: &'a mut Plan,
    pub task_tracker: &'a mut crate::plan_tasks::PlanTaskTracker,
    pub step: &'a crate::plan_state::PlanStep,
    pub route: &'a RoutePlan,
    pub base_agents: &'a crate::bg_agents::BaseAgentObservation,
    pub gaps: &'a [String],
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
        gaps,
        prior,
    } = context;
    let evidence = evidence_summary(gaps);
    let checkpoint = next_step_review_checkpoint(
        prior,
        step.id.clone(),
        crate::freshness::workspace_qc_fingerprint(&options.project_root),
        Some(route.team.clone()),
        step.kind == plan_state::StepKind::Build,
    );
    let boundary = format!("required review at step `{}`", step.title);
    if checkpoint.circuit_open() {
        let reason = stop_reason(&boundary, &checkpoint, &evidence);
        let _ = save_operational_review_checkpoint(&options.project_root, &checkpoint);
        block_open_steps(plan, events);
        let blockers = if gaps.is_empty() {
            vec![evidence]
        } else {
            gaps.to_vec()
        };
        let _ = task_tracker.settle_base_agents(
            step,
            base_agents,
            StepStatus::Blocked,
            true,
            &reason,
            &blockers,
        );
        let _ =
            task_tracker.settle_step(step, StepStatus::Blocked, true, &reason, blockers.clone());
        let _ = task_tracker.finish(false, &reason, blockers);
        let _ = plan_state::save(plan, &options.project_root);
        record_artifact_versions(&options.project_root);
        events.emit(EngineEvent::Note(format!("team · {reason}")));
        return DirectorLoopOutcome::Failed(reason);
    }

    let reason = format!("{boundary} unavailable: {evidence}");
    let saved = save_operational_review_checkpoint(&options.project_root, &checkpoint).is_ok()
        && plan_state::save(plan, &options.project_root).is_ok();
    if saved && task_tracker.wait_for_user(&reason).is_ok() {
        record_artifact_versions(&options.project_root);
        let (done, total) = plan.progress();
        events.emit(EngineEvent::Note(format!(
            "team · {reason} — the automatic fresh-session retry also failed; plan paused without source rework. Type /continue for one final bounded retry, or enter a new requirement"
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

/// Persist or terminally settle an unavailable final whole-build review.
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
    let evidence = evidence_summary(operational_unavailable);
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
    );
    if checkpoint.circuit_open() {
        let reason = stop_reason("final quality review", &checkpoint, &evidence);
        let _ = save_operational_review_checkpoint(&options.project_root, &checkpoint);
        block_open_steps(plan, events);
        let mut blockers = semantic_blocking.to_vec();
        blockers.extend_from_slice(operational_unavailable);
        if blockers.is_empty() {
            blockers.push(evidence);
        }
        if let Some(task) = resident_task.as_mut() {
            let _ = task.fail(&reason, blockers.clone());
        }
        let _ = task_tracker.finish(false, &reason, blockers);
        let _ = plan_state::save(plan, &options.project_root);
        record_artifact_versions(&options.project_root);
        events.emit(EngineEvent::Note(format!("team · {reason}")));
        return DirectorLoopOutcome::Failed(reason);
    }

    let semantic = if semantic_blocking.is_empty() {
        String::new()
    } else {
        format!("; {} semantic finding(s) retained", semantic_blocking.len())
    };
    let reason = format!("final quality review unavailable: {evidence}{semantic}");
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
        events.emit(EngineEvent::Note(format!(
            "team · {reason} — the automatic fresh-session retry also failed; plan paused without source rework. Type /continue for one final bounded retry, or enter a new requirement"
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
