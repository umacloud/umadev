//! TUI-local tasks: `!cmd`, UmaDev helper commands, and a confirmed `/deploy`.
//!
//! None of them touches the resident base session, so each settles through its
//! own terminal result, and a cancel stops only the task (see
//! [`crate::local_command`]).

use super::{local_command_call_id, App, ChatRole, MessageBody, ToolStatus};
use crate::local_command::{LocalCommandResult, LocalTaskStop};

impl App {
    /// Settle the exact local-command row without relying on transcript
    /// adjacency: users can queue input or receive status notices while the
    /// child is running, so "last row wins" would update the wrong message.
    pub(crate) fn record_local_command_done(&mut self, result: LocalCommandResult) {
        let call_id = local_command_call_id(result.request.presentation);
        let target = self.history.iter().rposition(|message| {
            message.role == ChatRole::Host
                && matches!(
                    &message.kind,
                    MessageBody::Tool(tool)
                        if tool.status == ToolStatus::Running
                            && tool.call_id.as_deref() == Some(call_id)
                )
        });
        if let Some(tool) = target
            .and_then(|index| self.history.get_mut(index))
            .and_then(|message| match &mut message.kind {
                MessageBody::Tool(tool) => Some(tool),
                _ => None,
            })
        {
            tool.status = if result.ok {
                ToolStatus::Ok
            } else {
                ToolStatus::Fail
            };
            tool.result = Some(result.output);
            tool.progress = None;
            tool.collapsed = result.ok;
        } else {
            self.push_local_command_row(&result.request.display, result.ok, result.output);
        }
        self.local_task = None;
        self.thinking = false;
        self.thinking_started = None;
        self.last_output_at = Some(std::time::Instant::now());
        self.tool_in_progress = false;
        self.transient_status = None;
        self.refresh_status();
    }

    /// Settle a tracked deploy and release its single-task guard.
    pub(crate) fn record_deploy_done(&mut self, succeeded: bool) {
        self.local_task = None;
        self.stream_compacted = None;
        self.arm_completion_bell(self.thinking_started);
        self.thinking = false;
        self.thinking_started = None;
        self.agentic_in_flight = false;
        self.tool_in_progress = false;
        self.record_turn(
            "assistant",
            format!(
                "[control: deploy task settled — {}]",
                if succeeded {
                    "deployed"
                } else {
                    "not deployed"
                }
            ),
        );
        self.persist_chat();
        self.refresh_status();
    }

    /// Own the stop handle of the local task that is starting.
    pub(crate) fn register_local_task(&mut self) -> tokio::sync::oneshot::Receiver<()> {
        let (stop, stopped) = LocalTaskStop::new();
        self.local_task = Some(stop);
        stopped
    }

    /// Esc/Ctrl+C/`/cancel` while a local task runs: stop just that task. It
    /// then settles through its own result; the resident base session, the
    /// native resume id and the conversation memory stay untouched. Returns
    /// whether a local task owned the request.
    pub(crate) fn stop_local_task(&mut self) -> bool {
        let Some(stop) = self.local_task.as_ref() else {
            return false;
        };
        if stop.request() {
            self.push(
                ChatRole::System,
                umadev_i18n::t(self.lang, "status.stopping"),
            );
        }
        true
    }
}
