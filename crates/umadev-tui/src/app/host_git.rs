//! Host-owned Git operation routing and trust-mode boundary checks.

use super::{Action, App, ChatRole, SubmittedTurn};

/// One serialized resident-session input waiting for the sole base writer.
/// Host Git and native commands remain typed so no later drain/promotion can
/// reinterpret them as model chat or Director steering.
#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) enum ResidentDispatch {
    RoutedChat(String),
    NativeCommand(String),
    HostGitCommit(String),
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum QueuedResidentKind {
    RoutedChat,
    NativeCommand,
    HostGitCommit,
}

impl App {
    /// Settle only the transient UI owned by a host Git transaction.
    ///
    /// This deliberately does not touch Director/gate/budget/operational pause
    /// state, the active task or plan, the product requirement, or resident
    /// session/hand-back identity. A commit success, local refusal, approval
    /// denial, and transaction error all have the same non-destructive boundary.
    pub(crate) fn record_host_git_done(&mut self, result: Result<String, String>) {
        self.arm_completion_bell(self.thinking_started);
        self.host_git_in_flight = false;
        self.cancelling = false;
        self.thinking = false;
        self.thinking_started = None;
        self.agentic_in_flight = false;
        self.tool_in_progress = false;
        self.stream_text_active = false;
        self.stream_tool_batch = None;
        self.collapse_thinking_block();
        self.reset_stream_md_cache();
        self.last_dispatched_chat = None;
        self.pending_route_input = None;
        let (status, reply) = match result {
            Ok(reply) => ("completed", reply),
            Err(reply) => ("failed", reply),
        };
        self.push(ChatRole::System, reply.clone());
        self.record_turn(
            "assistant",
            format!("[control: host Git transaction {status}]\n{reply}"),
        );
        self.persist_chat();
        self.refresh_status();
    }

    /// Mark an in-flight host transaction as stopping without disturbing the
    /// product run parked behind it.
    pub(crate) fn begin_host_git_cancelling(&mut self) {
        self.cancelling = true;
        self.thinking = true;
        self.thinking_started = Some(std::time::Instant::now());
        self.push(
            ChatRole::System,
            umadev_i18n::t(self.lang, "status.stopping"),
        );
    }

    /// Settle an aborted host transaction after its rollback guard has dropped.
    pub(crate) fn record_host_git_cancelled(&mut self) {
        self.host_git_in_flight = false;
        self.cancelling = false;
        self.thinking = false;
        self.thinking_started = None;
        self.agentic_in_flight = false;
        self.tool_in_progress = false;
        self.stream_text_active = false;
        self.stream_tool_batch = None;
        self.collapse_thinking_block();
        self.reset_stream_md_cache();
        self.last_dispatched_chat = None;
        self.pending_route_input = None;
        let note = umadev_i18n::t(self.lang, "intent.git_commit_cancelled").to_string();
        self.push(ChatRole::System, note.clone());
        self.record_turn(
            "assistant",
            format!("{note}\n[control: host Git transaction cancelled; product run unchanged]"),
        );
        self.persist_chat();
        self.refresh_status();
    }

    /// Intercept every Git operation before any live vendor-input surface.
    /// Unsafe shapes fail locally; a pure commit runs immediately while idle or
    /// waits in UmaDev's own FIFO for the current writer to settle.
    pub(super) fn submit_host_git_operation(&mut self, turn: SubmittedTurn) -> Action {
        let text = turn.text.clone();
        self.push(ChatRole::You, text.clone());
        let pure = !turn.has_attachments()
            && umadev_agent::parse_host_git_commit_request(&text)
                .is_some_and(|request| request.verifier.is_none());
        if !pure {
            self.push(
                ChatRole::UmaDev,
                umadev_i18n::t(self.lang, "intent.git_commit_host_boundary"),
            );
            return Action::None;
        }
        if self.reject_director_execution_in_plan() {
            return Action::None;
        }
        let lane_open = !self.thinking
            && !self.agentic_in_flight
            && (!self.is_pipeline_active()
                || self.active_gate.is_some()
                || self.director_gate_paused);
        if lane_open {
            return self.route_host_git_operation(&text).unwrap_or(Action::None);
        }
        self.align_queued_dispatch_kinds();
        self.queued_chat.push_back(text);
        self.queued_turn_inputs.push_back(turn);
        self.queued_dispatch_kinds
            .push_back(QueuedResidentKind::HostGitCommit);
        self.push(ChatRole::System, umadev_i18n::t(self.lang, "chat.queued"));
        self.refresh_status();
        Action::None
    }

    /// Drain only a host commit at the FIFO head.
    ///
    /// A Director pause releases the writer but deliberately keeps ordinary
    /// deferred chat parked. This narrow peek lets a front-most host transaction
    /// proceed without overtaking an earlier conversation turn.
    pub(crate) fn take_front_host_git_commit(&mut self) -> Option<String> {
        self.align_queued_dispatch_kinds();
        if self.queued_dispatch_kinds.front() != Some(&QueuedResidentKind::HostGitCommit) {
            return None;
        }
        match self.take_next_queued_dispatch()? {
            ResidentDispatch::HostGitCommit(text) => Some(text),
            ResidentDispatch::RoutedChat(_) | ResidentDispatch::NativeCommand(_) => None,
        }
    }

    /// Refuse a Git operation recovered from resident/durable run state.
    ///
    /// A fresh explicit commit is a host transaction, but a requirement loaded
    /// by `/continue`, `/redo`, `/tasks resume`, or a programmatic replay is not
    /// fresh authority. Replaying it would commit again without a new user
    /// instruction. All restoration entries call this before creating a task,
    /// opening a base/session, or writing workflow state.
    pub(crate) fn reject_replayed_host_git_operation(&mut self, request: &str) -> bool {
        if !umadev_agent::request_has_git_commit_operation(request) {
            return false;
        }
        self.push(
            ChatRole::UmaDev,
            umadev_i18n::t(self.lang, "intent.git_commit_host_boundary"),
        );
        true
    }

    /// Refuse a workspace-mutating host action while the session is in Plan
    /// mode. Returns `true` when the action was consumed. Keeping this check on
    /// the UI thread means shell helpers, project commands, Director starts, and
    /// resume paths all settle before they can spawn a process or persist state.
    /// Ordinary conversation and genuinely read-only inspection stay available.
    pub(super) fn reject_workspace_mutation_in_plan(&mut self) -> bool {
        if self.effective_trust_mode() != umadev_agent::TrustMode::Plan {
            return false;
        }
        self.push(
            ChatRole::UmaDev,
            umadev_i18n::t(self.lang, "continuous.plan_mode_skip").to_string(),
        );
        self.push(
            ChatRole::UmaDev,
            umadev_i18n::t(self.lang, "mode.plan.gate").to_string(),
        );
        true
    }

    /// Back-compatible name for Director-specific callers. Director execution
    /// is one member of the same workspace-mutation boundary enforced above.
    pub(crate) fn reject_director_execution_in_plan(&mut self) -> bool {
        self.reject_workspace_mutation_in_plan()
    }

    /// Route a Git commit operation to the resident host transaction, or reject
    /// an unsafe compound locally. `None` means the text is not a Git operation.
    ///
    /// This helper is shared by `/run`, `/goal`, and `/quick`; none of those
    /// aliases may turn a commit into product work merely because their normal
    /// path happens to be Director- or Light-owned.
    pub(super) fn route_host_git_operation(&mut self, request: &str) -> Option<Action> {
        if !umadev_agent::request_has_git_commit_operation(request) {
            return None;
        }
        match umadev_agent::parse_host_git_commit_request(request) {
            Some(host_request) if host_request.verifier.is_none() => {
                let request = request.to_string();
                self.record_user_turn(&request);
                self.last_dispatched_chat = Some(request.clone());
                self.pending_route_input = Some(SubmittedTurn::text(request.clone()));
                Some(Action::Route(request))
            }
            _ => {
                self.push(
                    ChatRole::UmaDev,
                    umadev_i18n::t(self.lang, "intent.git_commit_host_boundary"),
                );
                Some(Action::None)
            }
        }
    }
}
