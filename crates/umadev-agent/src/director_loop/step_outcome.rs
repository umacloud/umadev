use super::{
    persist_plan_ref, plan_state, record_artifact_versions, Arc, DirectorLoopOutcome, EngineEvent,
    EventSink, Plan, RunOptions, StepStatus,
};

/// The observable result of driving one plan step. The flags are independent
/// observations, not an enum: acceptance, a driven turn, and real progress can
/// differ when a check is neutral, a reviewer is unavailable, or a base dies.
#[allow(clippy::struct_excessive_bools)]
pub(super) struct StepOutcome {
    pub(super) accepted: bool,
    pub(super) reply: String,
    pub(super) drove: bool,
    pub(super) made_progress: bool,
    pub(super) unavailable: bool,
    pub(super) base_agents: crate::bg_agents::BaseAgentObservation,
    /// Semantic evidence eligible for an explicit source-repair run.
    pub(super) gap_evidence: Vec<String>,
    /// Host/reviewer availability evidence that must never trigger source edits.
    pub(super) operational_unavailable: Vec<String>,
    /// The base failed the step's turn for a reason no re-drive in this run can
    /// fix (see [`crate::base_error::TurnFailure::stops_the_run`]): the diagnosed
    /// reason the scheduler ends the run with, instead of verifying an unchanged
    /// tree and burning fix rounds on the same failure.
    pub(super) base_failure: Option<String>,
}

impl StepOutcome {
    /// A step stopped because the base itself failed its turn for a run-stopping
    /// reason (an exhausted quota, an expired login, a transient outage that
    /// outlasted the bounded backoff). Nothing is verified or accepted.
    pub(super) fn stopped_by_base(
        reply: String,
        drove: bool,
        base_agents: crate::bg_agents::BaseAgentObservation,
        reason: String,
    ) -> Self {
        Self {
            accepted: false,
            reply,
            drove,
            made_progress: false,
            unavailable: false,
            base_agents,
            gap_evidence: Vec::new(),
            operational_unavailable: Vec::new(),
            base_failure: Some(reason),
        }
    }
}

/// End the schedule on a base failure re-driving cannot fix in this run.
///
/// The step goes back to `Pending` and the task ledger is parked (not finished),
/// so `/continue` re-drives exactly this step once the quota resets, the login is
/// restored, or the outage clears; the run ends `Failed` on the diagnosed reason,
/// which is what the caller's resume hint classifies.
pub(super) fn stop_schedule_on_base_failure(
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    plan: &mut Plan,
    task_tracker: &mut crate::plan_tasks::PlanTaskTracker,
    step: &plan_state::PlanStep,
    base_agents: &crate::bg_agents::BaseAgentObservation,
    reason: String,
) -> DirectorLoopOutcome {
    let blockers = vec![reason.clone()];
    // Settled as unavailable (a retryable attempt), never as a verified failure.
    let _ = task_tracker.settle_base_agents(
        step,
        base_agents,
        StepStatus::Blocked,
        true,
        &reason,
        &blockers,
    );
    let _ = task_tracker.settle_step(step, StepStatus::Blocked, true, &reason, blockers);
    let _ = task_tracker.wait_for_user(&reason);
    plan.mark(&step.id, StepStatus::Pending);
    events.emit(EngineEvent::plan_step_status(
        step.id.clone(),
        step.title.clone(),
        StepStatus::Pending,
    ));
    persist_plan_ref(plan, options);
    record_artifact_versions(&options.project_root);
    events.emit(EngineEvent::Note(umadev_i18n::tlf(
        "director.base_failure_stop",
        &[&step.title],
    )));
    DirectorLoopOutcome::Failed(reason)
}
