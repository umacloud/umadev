//! TUI-hosted interaction bridge for the director loop — the task-scoped channel
//! that lets the DEFAULT `/run` engine reach a live user when one exists.
//!
//! The director loop (`crate::director_loop`) is deliberately headless-safe: every
//! decision point (a base `NeedApproval`, a spec-MUST confirmation gate) has a
//! deterministic fail-open floor so a CLI / CI run is never wedged waiting on a
//! human. But when the loop runs INSIDE the TUI there IS a human — and the old
//! behaviour silently auto-denied approvals and drove straight through the two
//! spec-MUST gates (`UD-FLOW-002` / `UD-FLOW-003`) the product promises.
//!
//! This module carries the hosting UI's live hooks to the loop WITHOUT threading a
//! parameter through every pump signature: the host scopes a [`RunInteraction`]
//! around the whole drive via [`hosted`] (a tokio task-local), and the loop's
//! decision points consult it fail-open — an unscoped (headless) run reads `None`
//! everywhere and keeps today's behaviour byte-for-byte.
//!
//! Three hooks ride the scope:
//! - **`approval`** — an async callback the loop awaits when the trust floor says
//!   a base action needs confirmation; the TUI backs it with the SAME
//!   `await_user_approval` y/n pause the chat surface uses (bounded, fail-open
//!   deny). Headless keeps the deterministic deny.
//! - **`steer`** — a shared queue of user steering directives (`/plan skip|veto|
//!   add`, text typed mid-build). The loop drains it at each step boundary and
//!   folds the directives into the next doer step, so steering applies mid-run
//!   instead of evaporating.
//! - **`confirm_gates`** — whether the host can actually render + resume a
//!   confirmation gate. Only a hosted, non-auto run pauses at `docs_confirm` /
//!   `preview_confirm`; headless runs (which could never resume) drive through
//!   exactly as before.
//!
//! Everything here is fail-open by contract (a poisoned lock / missing scope
//! degrades to "no interaction"), and no new dependency is introduced (tokio's
//! `task_local!` is already in the tree).

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

/// The shared mid-run steering intake: the hosting UI pushes user directives in;
/// the director loop drains them at step boundaries ([`take_steer`]). A plain
/// `std::sync::Mutex` (never held across an await on either side).
pub type SteerIntake = Arc<Mutex<Vec<String>>>;

/// The future an [`ApprovalFn`] returns: resolves `true` when the live user
/// APPROVED the action, `false` on deny / timeout / cancel (fail-open deny).
pub type ApprovalFuture = Pin<Box<dyn Future<Output = bool> + Send>>;

/// The interactive approval callback: `(action, target) -> approved?`. The TUI
/// implements it over its existing `await_user_approval` pause (surface the item,
/// block on y/n, bounded budget, fail-open deny).
pub type ApprovalFn = Arc<dyn Fn(String, String) -> ApprovalFuture + Send + Sync>;

/// The hooks a hosting UI provides for one director-loop run. `Default` (all
/// `None` / `false`) is exactly "headless" — the loop behaves as today.
#[derive(Clone, Default)]
pub struct RunInteraction {
    /// Mid-run steering intake ([`take_steer`]); `None` = no steering surface.
    pub steer: Option<SteerIntake>,
    /// Interactive approval callback ([`request_approval`]); `None` = headless
    /// (the deterministic trust floor auto-decides, exactly as today).
    pub approval: Option<ApprovalFn>,
    /// Whether the host renders + resumes confirmation gates. Only a hosted,
    /// non-auto run pauses at `docs_confirm` / `preview_confirm`.
    pub confirm_gates: bool,
}

impl std::fmt::Debug for RunInteraction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunInteraction")
            .field("steer", &self.steer.as_ref().map(|_| "<intake>"))
            .field("approval", &self.approval.as_ref().map(|_| "<callback>"))
            .field("confirm_gates", &self.confirm_gates)
            .finish()
    }
}

tokio::task_local! {
    /// The hosting UI's interaction hooks for the CURRENT director-loop task.
    /// Unset (headless CLI / CI / tests that don't opt in) → every consult below
    /// fails open to "no interaction" and behaviour is byte-for-byte today's.
    static RUN_INTERACTION: RunInteraction;
}

/// Run `fut` with `interaction` scoped as the current task's interaction hooks.
/// The host (TUI) wraps its whole director-loop drive in this; everything the
/// loop awaits inside inherits the scope (task-locals span the whole task).
pub async fn hosted<F: Future>(interaction: RunInteraction, fut: F) -> F::Output {
    RUN_INTERACTION.scope(interaction, fut).await
}

/// Whether the current task is hosted by a UI that renders + resumes
/// confirmation gates. `false` when unscoped (headless) — fail-open.
#[must_use]
pub(crate) fn gates_hosted() -> bool {
    RUN_INTERACTION
        .try_with(|i| i.confirm_gates)
        .unwrap_or(false)
}

/// Whether the current task carries a steering intake — i.e. a reply the user
/// types mid-run CAN be folded into the next step's directive. Drives the honest
/// `AskUserQuestion` hint variant (A2#6): with an intake, "your answer applies as
/// follow-up steering" is literally true; without one the caller keeps its
/// existing framing. `false` when unscoped (fail-open).
#[must_use]
pub(crate) fn steering_hosted() -> bool {
    RUN_INTERACTION
        .try_with(|i| i.steer.is_some())
        .unwrap_or(false)
}

/// Drain every queued mid-run steering directive (FIFO). Empty when unscoped,
/// no intake was provided, or the queue is empty — all fail-open.
#[must_use]
pub(crate) fn take_steer() -> Vec<String> {
    RUN_INTERACTION
        .try_with(|i| i.steer.clone())
        .ok()
        .flatten()
        .and_then(|q| q.lock().ok().map(|mut v| std::mem::take(&mut *v)))
        .unwrap_or_default()
}

/// Ask the live user to approve `action` on `target` via the host's callback.
/// `None` when the run is headless (no scope / no callback) — the caller then
/// applies today's deterministic floor decision. `Some(approved)` otherwise.
pub(crate) async fn request_approval(action: &str, target: &str) -> Option<bool> {
    let cb = RUN_INTERACTION
        .try_with(|i| i.approval.clone())
        .ok()
        .flatten()?;
    Some(cb(action.to_string(), target.to_string()).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unscoped_task_fails_open_to_headless() {
        // No scope → gates unhosted, no steering, no approval callback — the
        // exact headless posture the CLI / CI relies on.
        assert!(!gates_hosted());
        assert!(take_steer().is_empty());
        assert!(request_approval("bash", "rm -rf /").await.is_none());
    }

    #[tokio::test]
    async fn scoped_task_reads_the_hosted_hooks() {
        let steer: SteerIntake = Arc::new(Mutex::new(vec!["skip step 2".to_string()]));
        let approval: ApprovalFn = Arc::new(|_a, _t| Box::pin(async { true }) as ApprovalFuture);
        let interaction = RunInteraction {
            steer: Some(Arc::clone(&steer)),
            approval: Some(approval),
            confirm_gates: true,
        };
        hosted(interaction, async {
            assert!(gates_hosted());
            // The intake drains FIFO and then reads empty (consumed).
            assert_eq!(take_steer(), vec!["skip step 2".to_string()]);
            assert!(take_steer().is_empty());
            // The approval callback is consulted and its verdict returned.
            assert_eq!(request_approval("write", "src/x.rs").await, Some(true));
        })
        .await;
        // Outside the scope the task-local is gone again (fail-open headless).
        assert!(!gates_hosted());
    }

    #[test]
    fn debug_impl_never_dumps_the_callback() {
        let i = RunInteraction {
            steer: Some(Arc::new(Mutex::new(Vec::new()))),
            approval: Some(Arc::new(|_a, _t| {
                Box::pin(async { false }) as ApprovalFuture
            })),
            confirm_gates: true,
        };
        let s = format!("{i:?}");
        assert!(s.contains("confirm_gates: true"));
    }
}
