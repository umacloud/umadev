//! Typed outcomes crossing from spawned resident tasks to the TUI event loop.

use umadev_agent::Gate;
use umadev_runtime::{
    BaseResumeIdentity, PromptQueueMutation, PromptQueueSnapshot, SteerSemantics,
};

use crate::app::SubmittedTurn;
use crate::local_command::LocalCommandResult;

pub(super) const fn should_start_director(
    route_source: Option<umadev_agent::RouteSource>,
    uses_director_workflow: bool,
    has_typed_attachments: bool,
    execution_read_only: bool,
) -> bool {
    matches!(route_source, Some(umadev_agent::RouteSource::Brain))
        && uses_director_workflow
        && !has_typed_attachments
        && !execution_read_only
}

pub(super) const fn should_run_routed_qc(
    native_command: bool,
    route_source: Option<umadev_agent::RouteSource>,
    flagship_qc: bool,
    execution_read_only: bool,
) -> bool {
    !native_command
        && matches!(route_source, Some(umadev_agent::RouteSource::Brain))
        && flagship_qc
        && !execution_read_only
}

/// Terminal or interim signal from a model/host-routed turn.
#[derive(Debug, Clone, Eq, PartialEq)]
pub(super) enum RouteDecision {
    /// Complete native queue replacement from the base.
    PromptQueueSnapshot(PromptQueueSnapshot),
    /// A queued input frame reached the transport.
    PromptQueueInputWritten { text: String },
    /// Native queue delivery failed before a snapshot could accept it.
    PromptQueueInputRejected { turn: SubmittedTurn, note: String },
    /// A versioned queue mutation failed.
    PromptQueueMutationRejected {
        mutation: PromptQueueMutation,
        note: String,
    },
    /// A live steering method returned its delivery receipt.
    LiveInputAccepted {
        text: String,
        semantics: SteerSemantics,
    },
    /// A live typed input failed validation or protocol delivery.
    LiveInputRejected { turn: SubmittedTurn, note: String },
    /// Initial structured input failed before reaching the base.
    InputRejected { turn: SubmittedTurn, note: String },
    /// The user cancelled pre-session authentication.
    AuthCancelled { turn: SubmittedTurn, note: String },
    /// A natural-language turn crossed into Director ownership.
    DirectorStarted { requirement: String },
    /// A brain-driven streaming turn finished.
    AgenticDone {
        reply: String,
        director_build: bool,
        /// The base hit a hard limit (max turns / tokens / budget) mid-work. The reply is a
        /// PARTIAL result: the durable ledger records it as failed, so the visible settle must
        /// NOT show the ✅ completion card or mark the task Done — it settles as Stopped with
        /// the truncation caveat, matching the ledger instead of contradicting it.
        truncated: bool,
        base_session_id: Option<String>,
        base_resume_identity: Option<BaseResumeIdentity>,
    },
    /// A resident turn ended WITHOUT completing its work — the user/base
    /// interrupted it, or the base parked awaiting the user's typed answer.
    /// Settles bookkeeping exactly like [`RouteDecision::AgenticDone`] (thinking
    /// clears, session ids re-pin, the queue drains) but the transcript line is
    /// honest — these turns previously emitted an empty `AgenticDone`, which the
    /// settle path rendered as "[agentic] 完成。" while the ledger said cancelled
    /// (the reported 完成 ≠ delivered confusion).
    AgenticStopped {
        /// i18n key for the honest stop line
        /// (`agentic.interrupted` / `agentic.awaiting_answer`).
        message_key: &'static str,
        base_session_id: Option<String>,
        base_resume_identity: Option<BaseResumeIdentity>,
    },
    /// A host-owned Git transaction settled without touching resident state.
    HostGitDone {
        result: std::result::Result<String, String>,
    },
    /// A bounded TUI-owned shell/helper command settled.
    LocalCommandDone(LocalCommandResult),
    /// A Plan/read-only Director entry performed no execution.
    RunNotExecuted,
    /// A routed turn failed to produce a usable reply.
    Failed(String),
    /// A Director run parked at a confirmation gate.
    RunPausedAtGate { gate: Gate },
    /// A Director run parked at its wall-clock budget.
    RunPausedAtBudget { done: usize, total: usize },
    /// A Director run parked for an operational dependency.
    RunPausedAtOperational {
        reason: String,
        done: usize,
        total: usize,
    },
    /// A read-only question at a parked gate completed.
    GateQueryDone { epoch: u64, reply: String },
    /// A read-only question at a parked gate failed.
    GateQueryFailed { epoch: u64, note: String },
    /// A tracked deployment task settled.
    DeployDone { succeeded: bool },
}

impl RouteDecision {
    /// Whether the task publishing this outcome must be joined before the event
    /// loop applies it or starts another FIFO writer.
    pub(super) const fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::InputRejected { .. }
                | Self::AuthCancelled { .. }
                | Self::AgenticDone { .. }
                // A parked (awaiting-answer / base-interrupted) turn is ALSO terminal for the
                // publishing task: it `return`s right after sending AgenticStopped, dropping
                // the task that owns the workspace writer lock. Without joining it here, the
                // very next queued follow-up races that destructor — its RunLock::acquire hits
                // the still-held same-process guard and the queued message is spuriously
                // rejected with a writer-lock error. Joining also settles the handle so the
                // interrupted task is not detached un-awaited (Ctrl-C can still abort it).
                | Self::AgenticStopped { .. }
                | Self::HostGitDone { .. }
                | Self::LocalCommandDone(_)
                | Self::RunNotExecuted
                | Self::Failed(_)
                | Self::RunPausedAtGate { .. }
                | Self::RunPausedAtBudget { .. }
                | Self::RunPausedAtOperational { .. }
                | Self::GateQueryDone { .. }
                | Self::GateQueryFailed { .. }
                | Self::DeployDone { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_parked_turn_is_terminal_so_its_writer_lock_is_joined() {
        // AgenticStopped publishes a parked turn whose task already `return`ed, dropping the
        // workspace-writer-lock guard. It MUST be terminal so the event loop joins that task
        // before firing the next queued turn — otherwise the follow-up races the destructor
        // and is spuriously rejected with a writer-lock error.
        assert!(RouteDecision::AgenticStopped {
            message_key: "agentic.awaiting_answer",
            base_session_id: None,
            base_resume_identity: None,
        }
        .is_terminal());
        // A clean finish is (still) terminal; a mid-flight director start is NOT.
        assert!(RouteDecision::AgenticDone {
            reply: String::new(),
            director_build: false,
            truncated: false,
            base_session_id: None,
            base_resume_identity: None,
        }
        .is_terminal());
        assert!(!RouteDecision::DirectorStarted {
            requirement: String::new()
        }
        .is_terminal());
    }

    #[test]
    fn read_only_brain_build_never_transfers_to_director() {
        assert!(!should_start_director(
            Some(umadev_agent::RouteSource::Brain),
            true,
            false,
            true,
        ));
        assert!(should_start_director(
            Some(umadev_agent::RouteSource::Brain),
            true,
            false,
            false,
        ));
        assert!(!should_run_routed_qc(
            false,
            Some(umadev_agent::RouteSource::Brain),
            true,
            true,
        ));
    }
}
