//! Frozen plan-panel rehydration after a transient (rate-limit) abort.
//!
//! When a base hits its rate limit mid-run the director loop terminates as a
//! failure and the TUI clears its live panels. Without this the saved plan DAG
//! (persisted per-step to `.umadev/plan.json`) becomes invisible — the checklist
//! "disappears and never comes back" unless the user happens to type `/continue`.
//! This module brings the checklist back in a FROZEN, interrupted state so the
//! user can see what was saved and that `/continue` resumes it, WITHOUT the stale
//! "· running" spinner that `clear_live_panels` exists to kill.

use super::{App, PlanStepRow};

/// The status string a persisted plan step renders as in a FROZEN (interrupted)
/// checklist after a transient abort: a step that was [`StepStatus::Active`] when
/// the run stopped becomes `"paused"` (shown statically, never the live
/// "· running" spinner); every other status keeps its persisted meaning (`done`
/// stays done, `pending` pending, `blocked` blocked). Pure.
#[must_use]
pub(super) fn frozen_step_status(status: umadev_agent::plan_state::StepStatus) -> String {
    use umadev_agent::plan_state::StepStatus;
    match status {
        StepStatus::Active => "paused".to_string(),
        other => other.as_str().to_string(),
    }
}

impl App {
    /// Repopulate the plan checklist in a FROZEN, interrupted state after a
    /// **transient** abort left a resumable plan on disk — the real fix for "the
    /// plan panel disappears and never comes back" when a base hits its rate limit
    /// mid-run. Called right after [`App::clear_live_panels`] in
    /// [`App::mark_block_aborted`], so a genuine hard failure keeps today's cleared
    /// panel while a rate-limit / overloaded / network blip brings the saved plan
    /// back where the user was watching it.
    ///
    /// The rehydrated rows render STATICALLY, never as a live run:
    /// - the step that was ACTIVE at the abort is mapped to `"paused"` (see
    ///   [`frozen_step_status`]) — the "· running" header suffix and the working
    ///   roster status both key off the literal `"active"`, so remapping it is what
    ///   keeps the stale spinner `clear_live_panels` exists to kill from coming
    ///   back; done steps stay done, pending stay pending;
    /// - [`App::plan_frozen`] is set so the renderer draws an "interrupted —
    ///   /continue to resume" header instead of the live plan title; and
    /// - NO run task is registered and NO "posted N steps" memo is pushed — this is
    ///   a read of saved state, not the start of a build.
    ///
    /// A subsequent genuine plan post ([`App::apply_plan_posted`], including the
    /// `/continue` resume re-post) fully replaces these rows and clears the frozen
    /// flag, so a stale frozen panel never bleeds into a fresh run.
    ///
    /// The team-review/critics panel is deliberately NOT restored: critic verdicts
    /// are not persisted, so there is no saved data to rehydrate — bringing it back
    /// would need a data-model change and is out of scope for this fix.
    ///
    /// Fail-open at every edge: a non-transient reason, no resumable plan, or an
    /// unreadable `plan.json` all leave the panel exactly as `clear_live_panels`
    /// left it (empty), never a fabricated panel; never panics.
    pub(super) fn rehydrate_frozen_plan_if_transient(&mut self, reason: &str) {
        let failure = umadev_agent::base_error::classify(None, None, Some(reason.trim()));
        if !umadev_agent::base_error::is_transient(&failure) {
            return;
        }
        // Both gates the fix specifies: the plan must still be resumable AND load.
        if !umadev_agent::has_resumable_director_plan(&self.project_root) {
            return;
        }
        let Some(plan) = umadev_agent::plan_state::load(&self.project_root) else {
            return;
        };
        let rows: Vec<PlanStepRow> = plan
            .steps
            .iter()
            .map(|step| PlanStepRow {
                id: step.id.clone(),
                title: step.title.clone(),
                status: frozen_step_status(step.status),
                seat: step.seat.role_id().to_string(),
            })
            .collect();
        if rows.is_empty() {
            return;
        }
        self.plan_steps = rows;
        self.plan_collapsed = false;
        self.plan_frozen = true;
    }
}
