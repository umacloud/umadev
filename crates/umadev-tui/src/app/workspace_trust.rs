//! Asking whether the user trusts the project, and applying the answer.
//!
//! The decision itself lives in [`umadev_agent::workspace_trust`], outside the
//! project. Here the TUI asks for it the first time a project is opened (a
//! picker in the gate-choice panel), offers `/trust` to change it later, and
//! publishes it to the host so every base launched in the project loads, or
//! ignores, the project's own vendor configuration accordingly.

use std::path::Path;

use umadev_agent::{GateChoice, GateChoiceOption, GateDecision, TrustMode};

use super::{Action, App, ChatRole};

/// The picker's question key. It also tells the trust picker apart from a run
/// gate's picker in the shared gate-choice panel.
const QUESTION: &str = "workspace_trust.question";

/// Read the user's decision for `project_root` and publish it to the host.
/// Undecided and unreadable both leave the project untrusted.
pub(super) fn load(project_root: &Path) -> Option<bool> {
    let decision = umadev_agent::workspace_trust::decision(project_root);
    umadev_host::project_config::set_project_trusted(project_root, decision == Some(true));
    decision
}

impl App {
    /// Whether the user trusts this project.
    pub(crate) fn workspace_trusted(&self) -> bool {
        self.workspace_trust == Some(true)
    }

    /// Ask the trust question on a project the user has not decided about.
    pub(super) fn ask_workspace_trust_if_undecided(&mut self) {
        if self.workspace_trust.is_none() {
            self.ask_workspace_trust();
        }
    }

    /// Show the trust picker: Trust first, so a plain Enter on the question the
    /// user was just shown does what its first line says.
    fn ask_workspace_trust(&mut self) {
        let option = |label: &str, decision| GateChoiceOption {
            label: label.to_string(),
            decision,
        };
        self.gate_choice = Some(GateChoice {
            question: QUESTION.to_string(),
            options: vec![
                option("workspace_trust.trust", GateDecision::Approve),
                option("workspace_trust.distrust", GateDecision::Cancel),
            ],
        });
        self.gate_choice_sel = 0;
    }

    /// `/trust`: say where the project stands and ask again.
    pub(super) fn slash_trust(&mut self) -> Action {
        let state = if self.workspace_trusted() {
            "workspace_trust.trusted"
        } else {
            "workspace_trust.untrusted"
        };
        self.push(
            ChatRole::UmaDev,
            umadev_i18n::t(self.lang, state).to_string(),
        );
        self.ask_workspace_trust();
        Action::None
    }

    /// The trust picker's answer, when `idx` picks from it; `None` leaves a run
    /// gate's picker to the gate.
    pub(super) fn pick_workspace_trust(&mut self, idx: usize) -> Option<Action> {
        let choice = self
            .gate_choice
            .as_ref()
            .filter(|c| c.question == QUESTION)?;
        let option = choice.options.get(idx)?.clone();
        if self.has_interruptible_work() || self.thinking {
            // The live base keeps the configuration it started with; changing
            // trust under it would say one thing while the base does another.
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "chat.busy_cancel_first"),
            );
            return Some(Action::None);
        }
        self.gate_choice = None;
        self.gate_choice_sel = 0;
        self.push(
            ChatRole::You,
            umadev_i18n::t(self.lang, &option.label).to_string(),
        );
        Some(self.decide_workspace_trust(option.decision == GateDecision::Approve))
    }

    /// Record and apply the user's decision. A base already running keeps the
    /// configuration it started with, so the resident session is restarted.
    fn decide_workspace_trust(&mut self, trusted: bool) -> Action {
        if let Err(e) = umadev_agent::workspace_trust::record(&self.project_root, trusted) {
            self.push(
                ChatRole::System,
                umadev_i18n::tf(self.lang, "workspace_trust.save_failed", &[&e.to_string()]),
            );
        }
        umadev_host::project_config::set_project_trusted(&self.project_root, trusted);
        self.workspace_trust = Some(trusted);
        if !trusted && self.trust_mode_override == Some(TrustMode::Auto) {
            self.set_trust_mode(TrustMode::Guarded);
        }
        let note = if trusted {
            "workspace_trust.trusted"
        } else {
            "workspace_trust.untrusted"
        };
        self.push(
            ChatRole::UmaDev,
            umadev_i18n::t(self.lang, note).to_string(),
        );
        self.refresh_status();
        Action::SandboxChanged
    }

    /// Why a switch to `next` must be refused, as an i18n key, or `None`.
    ///
    /// The ONE guard shared by `/mode`, `/manual`/`/auto` and Shift+Tab:
    /// - Auto needs a trusted project.
    /// - A mid-turn DOWNGRADE (e.g. Auto→Guarded) would flip the chip to a
    ///   tighter tier while the running base process keeps its wider launch
    ///   authority: the chip would say 手动审核 while writes still flow unasked.
    /// - Codex fixes its approval policy and sandbox when a worker starts, so
    ///   any change waits until it is idle.
    pub(super) fn mode_change_refusal(&self, next: TrustMode) -> Option<&'static str> {
        let busy = self.has_interruptible_work() || self.thinking;
        if umadev_agent::workspace_trust::cap_tier(next, self.workspace_trusted()) != next {
            Some("workspace_trust.auto_refused")
        } else if (busy && self.effective_trust_mode().is_downgrade_to(next))
            || self.codex_mode_change_requires_idle(next)
        {
            Some("chat.busy_cancel_first")
        } else {
            None
        }
    }
}

/// Trust `project_root` in the test process's scratch state directory, for
/// tests whose app must start in a project the user trusts.
#[cfg(test)]
pub(crate) fn trust_for_test(project_root: &Path) {
    super::tests::isolate_state_directory();
    umadev_agent::workspace_trust::record(project_root, true).expect("trust the test project");
}
