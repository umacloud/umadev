//! Director run-pause settlement for budget and operational stops.

use super::{Action, App, ChatRole, TaskStatus};

impl App {
    /// Whether an exact natural-language continuation has host-owned state to
    /// resume. The state guard prevents prose about a "continue" button from
    /// being stolen from the model.
    pub(super) fn has_run_resume_target(&self) -> bool {
        self.active_gate.is_some()
            // Durable state is authoritative across a completed in-memory turn,
            // TUI restart, and binary upgrade. `finished` describes only the last
            // resident block; using it as a filesystem-resume ceiling hid a newer
            // incomplete plan and made `/continue` claim that no pipeline started.
            || umadev_agent::has_resumable_director_plan(&self.project_root)
            || umadev_agent::has_resumable_run(&self.project_root)
            || umadev_agent::legacy_operational_review_circuit_reason(&self.project_root)
                .is_some()
    }

    /// Shared state transition for `/continue` and its exact natural-language
    /// aliases, kept outside semantic routing so a routing timeout cannot demote
    /// an existing run to read-only or synthesize a replacement plan.
    pub(super) fn continue_run_action(&mut self) -> Action {
        let replay_requirement = self.resume_run_requirement();
        if self.reject_replayed_host_git_operation(&replay_requirement) {
            return Action::None;
        }
        let has_resume_target = self.has_run_resume_target();
        // Re-arming a durable cursor is itself state-changing. Plan mode must stop
        // before that write, not merely before the later ResumeRun dispatch.
        if has_resume_target && self.effective_trust_mode() == umadev_agent::TrustMode::Plan {
            self.reject_director_execution_in_plan();
            return Action::None;
        }
        let rearmed_review =
            match umadev_agent::rearm_operational_review_for_explicit_retry(&self.project_root) {
                Ok(rearmed) => rearmed,
                Err(reason) => {
                    self.push(ChatRole::System, reason);
                    return Action::None;
                }
            };
        // Explicit cancellation remains terminal. Review *circuits* were re-armed
        // above because this command is fresh user authority to retry the same
        // cursor; a cancelled cursor must never be resurrected implicitly.
        if let Some(reason) =
            umadev_agent::legacy_operational_review_terminal_reason(&self.project_root)
        {
            self.push(ChatRole::System, reason);
            return Action::None;
        }
        if rearmed_review {
            // A stale presentation gate may coexist with the review receipt after
            // an interrupted event drain. The explicit retry owns the durable
            // review cursor, never that unrelated in-memory gate.
            self.active_gate = None;
            self.gate_choice = None;
            self.director_gate_paused = false;
            self.push_resume_separator();
            self.push(
                ChatRole::UmaDev,
                umadev_i18n::t(self.lang, "continue.resuming"),
            );
            Action::ResumeRun(replay_requirement)
        } else if let Some(gate) = self.active_gate.take() {
            self.push(
                ChatRole::UmaDev,
                umadev_i18n::tf(
                    self.lang,
                    "slash.gate_approved",
                    &[umadev_i18n::t(self.lang, gate.human_label_key())],
                ),
            );
            self.record_trust_pass(gate.id_str());
            Action::Continue(gate)
        } else if (self.budget_paused || self.aborted)
            && umadev_agent::has_resumable_run(&self.project_root)
        {
            if self.reject_director_execution_in_plan() {
                return Action::None;
            }
            self.push_resume_separator();
            self.push(
                ChatRole::UmaDev,
                umadev_i18n::t(self.lang, "continue.resuming"),
            );
            Action::ResumeRun(replay_requirement.clone())
        } else if self.has_interruptible_work() || self.thinking {
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "continue.running"),
            );
            Action::None
        } else if !self.run_started && has_resume_target {
            if self.reject_director_execution_in_plan() {
                return Action::None;
            }
            self.push_resume_separator();
            self.push(
                ChatRole::UmaDev,
                umadev_i18n::t(self.lang, "continue.resuming"),
            );
            Action::ResumeRun(replay_requirement)
        } else if !self.run_started
            && !self.finished
            && self.host_chat_session_active
            && self.chat_session_id.is_some()
        {
            self.push(
                ChatRole::UmaDev,
                umadev_i18n::t(self.lang, "continue.resuming_chat"),
            );
            Action::Route(umadev_i18n::t(self.lang, "continue.chat_directive").to_string())
        } else {
            let hint = if self.run_started && !self.finished {
                umadev_i18n::t(self.lang, "continue.running")
            } else if self.finished {
                umadev_i18n::t(self.lang, "continue.finished")
            } else {
                umadev_i18n::t(self.lang, "continue.not_started")
            };
            self.push(ChatRole::System, hint);
            Action::None
        }
    }

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
