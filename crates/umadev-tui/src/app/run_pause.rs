//! Director run-pause settlement for budget and operational stops.

use super::{App, ChatRole, TaskStatus};

impl App {
    /// A DIRECTOR build parked because its wall-clock budget was exhausted while
    /// resumable steps remained (Stage 1/2) — the terminal `RunPausedAtBudget`
    /// decision's recorder. Mirrors [`Self::record_run_paused_at_gate`] but for a
    /// PAUSE with no gate: it clears the in-flight "thinking…" state and the live
    /// counters (so the timer stops and the status reads `[paused]`, NEVER
    /// `[aborted]`), arms [`Self::budget_paused`] so `run_state` reads
    /// [`super::RunState::PausedAtBudget`], keeps the plan panel visible in a FROZEN
    /// (interrupted) form so the user can see what was saved, and pushes the
    /// `/continue` resume hint carrying `done/total`.
    ///
    /// This deliberately does NOT route through [`Self::mark_block_aborted`]: a budget
    /// pause is a resumable settle, not an honest hard abort — the plan is intact on
    /// disk and `/continue` re-drives only the remaining steps.
    pub(crate) fn record_run_paused_at_budget(&mut self, done: usize, total: usize) {
        self.stream_compacted = None;
        self.operational_pause_reason = None;
        // An away user should hear that the run parked (same as the abort/deliver
        // paths). Arm before the timers are cleared, gated on how long it had run.
        self.arm_completion_bell(self.run_started_at.or(self.thinking_started));
        self.thinking = false;
        self.thinking_started = None;
        self.agentic_in_flight = false;
        self.tool_in_progress = false;
        self.long_op_in_progress = false;
        self.stream_text_active = false;
        self.stream_tool_batch = None;
        self.collapse_thinking_block();
        self.reset_stream_md_cache();
        // The writer session is gone; what remains is the parked plan on disk.
        self.director_run_in_flight = false;
        self.budget_paused = true;
        // Clear the pipeline-live flag AND settle the run's registry task so it is no
        // longer `Running`. A leftover `run_started`/`Running` task keeps
        // `has_active_run()` — hence `has_interruptible_work()` — TRUE on a run that has
        // actually PARKED, which made `/continue` answer "a run is still in flight" and
        // do nothing, ESC arm a phantom interrupt, and `/codex` refuse as busy. Settle
        // it `Stopped` (a resumable pause, not `Failed`/`Done`); a `/continue`
        // re-registers the resumed run. (`is_pipeline_active()` already excludes a
        // budget pause; this also frees `active_task()`.)
        self.run_started = false;
        self.mark_active_task(TaskStatus::Stopped);
        // Stop every live counter so the status bar reflects a real paused state.
        self.run_started_at = None;
        self.phase_started_at = None;
        self.last_output_at = None;
        self.transient_status = None;
        // Keep the plan panel: drop the LIVE panel, then bring the saved plan back in
        // a FROZEN (interrupted) form so the user sees the completed / remaining steps
        // and that `/continue` resumes them. Fail-open (no readable plan → empty).
        self.clear_live_panels();
        self.rehydrate_frozen_plan_now();
        // The one-line resume hint carrying where the run parked (done/total steps).
        self.push(
            ChatRole::System,
            umadev_i18n::tf(
                self.lang,
                "run.budget_pause_resume_hint",
                &[&done.to_string(), &total.to_string()],
            ),
        );
        // A parked run fires no gate/completion, so drain any queued steer (same as
        // the abort path) so its "queued N" chip can't stay falsely lit forever.
        if !self.queued_steer.is_empty() {
            let text = self.queued_steer.drain(..).collect::<Vec<_>>().join("\n");
            self.push(
                ChatRole::System,
                umadev_i18n::tf(self.lang, "run.queued_dropped", &[&text]),
            );
        }
        self.refresh_status();
    }

    /// Settle a resumable pause caused by a typed reviewer/host outage. This
    /// mirrors the budget pause lifecycle but reports the real cause and never
    /// marks the run aborted, degraded, or delivered.
    pub(crate) fn record_run_paused_at_operational(
        &mut self,
        reason: String,
        done: usize,
        total: usize,
    ) {
        self.stream_compacted = None;
        self.arm_completion_bell(self.run_started_at.or(self.thinking_started));
        self.thinking = false;
        self.thinking_started = None;
        self.agentic_in_flight = false;
        self.tool_in_progress = false;
        self.long_op_in_progress = false;
        self.stream_text_active = false;
        self.stream_tool_batch = None;
        self.collapse_thinking_block();
        self.reset_stream_md_cache();
        self.director_run_in_flight = false;
        self.budget_paused = true;
        self.operational_pause_reason = Some(reason.clone());
        self.run_started = false;
        self.mark_active_task(TaskStatus::Stopped);
        self.run_started_at = None;
        self.phase_started_at = None;
        self.last_output_at = None;
        self.transient_status = None;
        self.clear_live_panels();
        self.rehydrate_frozen_plan_now();
        self.push(
            ChatRole::System,
            umadev_i18n::tf(
                self.lang,
                "run.operational_pause_resume_hint",
                &[&reason, &done.to_string(), &total.to_string()],
            ),
        );
        if !self.queued_steer.is_empty() {
            let text = self.queued_steer.drain(..).collect::<Vec<_>>().join("\n");
            self.push(
                ChatRole::System,
                umadev_i18n::tf(self.lang, "run.queued_dropped", &[&text]),
            );
        }
        self.refresh_status();
    }
}
