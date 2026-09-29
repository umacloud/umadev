//! Input while a confirmation gate is open: the structured picker, approval,
//! cancellation, read-only questions, clarification answers, and revisions.

use super::{Action, App, ChatRole, Gate, GateDecision, SubmittedTurn};

impl App {
    /// Route one line submitted while `gate` is open.
    pub(super) fn submit_at_gate(&mut self, gate: Gate, turn: SubmittedTurn) -> Action {
        let text = turn.text.clone();
        if turn.has_attachments() {
            self.queue_chat_turn(turn);
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "input.steer.gate_deferred"),
            );
            self.refresh_status();
            return Action::None;
        }
        // Picking "Request changes" / "Add more" asked the user to describe the
        // change, so this line IS the revision, whatever its wording.
        let revision_requested = self.gate_revision_pending.take() == Some(gate);
        // A question at a gate asks for a model answer; it is not consent and
        // must never be reinterpreted as `Action::Revise`. The Director writer
        // is already parked at this point, so answer on a fresh read-only
        // surface while keeping the gate open.
        if !revision_requested
            && matches!(
                umadev_agent::classify_running_input(&text),
                umadev_agent::RunningInputDisposition::Query
            )
        {
            return self.begin_gate_query(text);
        }
        if self.reject_director_execution_in_plan() {
            return Action::None;
        }
        // ClarifyGate: non-"c" text is an answer (append to
        // answers file); "c" submits all answers + continues.
        if gate == Gate::ClarifyGate {
            if matches!(text.trim(), "c" | "C") {
                self.active_gate = None;
                self.gate_choice = None;
                self.push(
                    ChatRole::UmaDev,
                    umadev_i18n::t(self.lang, "gate.clarify_saved").to_string(),
                );
                return Action::Continue(gate);
            }
            if !umadev_agent::is_explicit_clarification_answer(&text) {
                self.queue_chat_turn(turn.clone());
                self.push(ChatRole::System, umadev_i18n::t(self.lang, "run.deferred"));
                self.refresh_status();
                return Action::None;
            }
            match self.append_clarify_answer(&text) {
                Ok(()) => self.push(
                    ChatRole::UmaDev,
                    umadev_i18n::t(self.lang, "gate.clarify_recorded").to_string(),
                ),
                // Persist failed — don't claim "recorded"; the resume path
                // would lose this answer. Tell the user the write failed.
                Err(e) => self.push(
                    ChatRole::System,
                    umadev_i18n::tf(self.lang, "gate.clarify_write_failed", &[&e.to_string()]),
                ),
            }
            return Action::None;
        }
        // A2#2: run the free text through the SAME `classify_reply` the CLI
        // gate surfaces use, so "确认" / "通过" / "approve" / "ok" / "lgtm"
        // APPROVES the gate instead of being mistaken for a revision that
        // re-runs the whole producing block (the reported trap). The literal
        // `c` shortcut stays first (classify_reply would read it as a
        // revision); "取消" / "cancel" cancels; everything else revises.
        let approved = matches!(text.trim(), "c" | "C")
            || matches!(
                umadev_agent::classify_reply(&text),
                umadev_agent::GateOutcome::Approved
            );
        if approved {
            self.active_gate = None;
            self.gate_choice = None;
            let what = match gate {
                Gate::DocsConfirm => umadev_i18n::t(self.lang, "gate.confirmed_docs"),
                Gate::PreviewConfirm => umadev_i18n::t(self.lang, "gate.confirmed_preview"),
                Gate::ClarifyGate => umadev_i18n::t(self.lang, "gate.confirmed_generic"),
            };
            self.push(ChatRole::UmaDev, format!("[ok] {what}"));
            // A manual approval also builds trust for this gate.
            self.record_trust_pass(gate.id_str());
            return Action::Continue(gate);
        }
        if matches!(
            umadev_agent::classify_reply(&text),
            umadev_agent::GateOutcome::Cancelled
        ) {
            // An explicit cancel at the gate — same path as the picker's
            // Cancel option (the run is torn down, never a revision spawn).
            self.active_gate = None;
            self.gate_choice = None;
            return Action::Cancel;
        }
        if !revision_requested {
            // At a docs/preview gate, a clear correction or an edit request for
            // the artifact under review is a revision, as the gate card
            // promises. A later task is deferred for the model, and anything
            // else is answered read-only instead of re-running this block.
            let disposition = umadev_agent::classify_running_input(&text);
            if matches!(disposition, umadev_agent::RunningInputDisposition::Deferred)
                && umadev_agent::is_explicit_later_work(&text)
            {
                self.queue_chat_turn(turn.clone());
                self.push(ChatRole::System, umadev_i18n::t(self.lang, "run.deferred"));
                self.refresh_status();
                return Action::None;
            }
            if !matches!(disposition, umadev_agent::RunningInputDisposition::Steer)
                && !umadev_agent::is_gate_revision_feedback(&text)
            {
                return self.begin_gate_query(text);
            }
        }
        // A revision request resets this gate's trust streak.
        self.record_trust_revision(gate.id_str());
        self.push(
            ChatRole::UmaDev,
            umadev_i18n::tf(self.lang, "gate.revision_received", &[&text]),
        );
        Action::Revise(text)
    }

    /// Confirm the picker option at `idx`, mapping the chosen [`GateDecision`]
    /// onto the EXISTING gate flow (no new decision machinery):
    /// - `Approve` → clear the gate + record trust, drive [`Action::Continue`]
    ///   (exactly what typing `c` does);
    /// - `Revise` / `AddMore` → keep the gate open and ask for specifics; the
    ///   next line submitted at this gate is the revision ([`Action::Revise`]);
    /// - `Cancel` → [`Action::Cancel`].
    ///
    /// **Fail-open:** an out-of-range index, no active picker, or no active gate
    /// → [`Action::None`] (the gate is left untouched).
    pub(super) fn gate_choice_pick(&mut self, idx: usize) -> Action {
        if self.gate_query_in_flight {
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "gate.query.busy"),
            );
            self.refresh_status();
            return Action::None;
        }
        let Some(option) = self
            .gate_choice
            .as_ref()
            .and_then(|c| c.options.get(idx))
            .cloned()
        else {
            return Action::None;
        };
        let Some(gate) = self.active_gate else {
            return Action::None;
        };
        if self.reject_director_execution_in_plan() {
            return Action::None;
        }
        // Echo the chosen option so the transcript records the decision (the
        // picker panel itself is transient). Localize the label key via `t()`,
        // which returns a literal verbatim and a known key localized.
        let label = umadev_i18n::t(self.lang, &option.label).to_string();
        self.push(ChatRole::You, label);
        // The picker is consumed regardless of the branch; the gate stays open
        // only for a revise/add-more free-text follow-up (re-checked below).
        self.gate_choice = None;
        self.gate_choice_sel = 0;
        match option.decision {
            GateDecision::Approve => {
                self.active_gate = None;
                let what = match gate {
                    Gate::DocsConfirm => umadev_i18n::t(self.lang, "gate.confirmed_docs"),
                    Gate::PreviewConfirm => umadev_i18n::t(self.lang, "gate.confirmed_preview"),
                    Gate::ClarifyGate => umadev_i18n::t(self.lang, "gate.confirmed_generic"),
                };
                self.push(ChatRole::UmaDev, format!("[ok] {what}"));
                // A picker approval builds trust for this gate, like a manual `c`.
                self.record_trust_pass(gate.id_str());
                Action::Continue(gate)
            }
            GateDecision::Revise | GateDecision::AddMore => {
                // A revise needs specifics → keep the gate open and ask for
                // them; the user's next line is the revision. A picked revise
                // resets this gate's trust streak.
                self.record_trust_revision(gate.id_str());
                self.gate_revision_pending = Some(gate);
                let prompt_key = if matches!(option.decision, GateDecision::AddMore) {
                    "gate.choice.add_more.prompt"
                } else {
                    "gate.choice.revise.prompt"
                };
                self.push(
                    ChatRole::UmaDev,
                    umadev_i18n::t(self.lang, prompt_key).to_string(),
                );
                Action::None
            }
            GateDecision::Cancel => {
                self.active_gate = None;
                Action::Cancel
            }
        }
    }
}
