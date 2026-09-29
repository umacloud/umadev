//! Event-loop settlement after each engine event: the auto-preview, continuous
//! run teardown, auto-continued gates, queued steering, and the deferred chat
//! turn a settled legacy block leaves behind.

use std::sync::Arc;

use umadev_agent::{ChannelSink, EngineEvent};

use crate::app::App;
use crate::interaction_bridge::{ApprovalHolder, HostInputHolder, PendingAskHolder};
use crate::route_decision::RouteDecision;
use crate::session_slot::SessionHolder;
use crate::{ChatSessionHolder, LaunchOptions, LiveInputHub};

/// Apply one engine event plus the event-loop side effects that depend on the
/// resulting app state. Both the normal engine branch and the route-terminal
/// pre-drain use this same path, so terminal route decisions cannot overtake
/// already-emitted stream/plan events.
#[allow(clippy::too_many_arguments)]
pub(super) fn apply_engine_event(
    app: &mut App,
    event: EngineEvent,
    opts: &LaunchOptions,
    sink: &Arc<ChannelSink>,
    route_tx: &tokio::sync::mpsc::UnboundedSender<RouteDecision>,
    session_holder: &SessionHolder,
    chat_session_holder: &ChatSessionHolder,
    pending_ask_holder: &PendingAskHolder,
    steer_holder: &umadev_agent::SteerIntake,
    approval_holder: &ApprovalHolder,
    host_input_holder: &HostInputHolder,
    live_input_hub: &LiveInputHub,
    continuous_run_active: &mut bool,
    run_task: &mut Option<tokio::task::JoinHandle<()>>,
) {
    let was_finished = app.finished;
    let block_was_live = app.is_pipeline_active();
    app.apply_engine(event);
    super::maybe_start_auto_preview(app, sink, was_finished);
    finish_terminal_continuous_run(app, continuous_run_active, session_holder);
    super::apply_pending_auto_continue(
        app,
        opts,
        sink,
        session_holder,
        *continuous_run_active,
        run_task,
    );
    super::apply_pending_steer(
        app,
        opts,
        sink,
        route_tx,
        session_holder,
        steer_holder,
        approval_holder,
        host_input_holder,
        *continuous_run_active,
        run_task,
    );
    // A legacy / Light block reports its end only through engine events, never
    // a route decision, so the chat turns deferred while it ran would wait for
    // the NEXT routed turn and then run after it, out of order.
    if legacy_block_settled(app, block_was_live) {
        if let Some(task) = super::drain_next_queued_chat(
            app,
            chat_session_holder,
            session_holder,
            pending_ask_holder,
            approval_holder,
            host_input_holder,
            steer_holder,
            live_input_hub,
            sink,
            route_tx,
        ) {
            *run_task = Some(task);
        }
    }
}

/// Whether this event took a live legacy block to a terminal state (delivered,
/// degraded or aborted) while no other turn, gate or cancel owns the loop.
fn legacy_block_settled(app: &App, block_was_live: bool) -> bool {
    block_was_live
        && !app.is_pipeline_active()
        && app.active_gate.is_none()
        && !app.thinking
        && !app.cancelling
}

/// Release a continuous run whose block reached a terminal state. A degraded
/// block is as final as a delivered or aborted one.
pub(super) fn finish_terminal_continuous_run(
    app: &App,
    active: &mut bool,
    session_holder: &SessionHolder,
) {
    if *active && (app.finished || app.aborted || app.degraded) {
        super::finish_continuous_cancel(active, session_holder);
    }
}

#[cfg(test)]
mod tests {
    use super::legacy_block_settled;
    use crate::app::App;
    use umadev_agent::EngineEvent;
    use umadev_spec::Phase;

    fn app() -> (tempfile::TempDir, App) {
        let tmp = tempfile::TempDir::new().unwrap();
        let app = App::new(
            "engine-settle-test",
            crate::config::UserConfig::default(),
            tmp.path().join("config.toml"),
            tmp.path().to_path_buf(),
        );
        (tmp, app)
    }

    fn started(app: &mut App) {
        app.apply_engine(EngineEvent::PipelineStarted {
            slug: "quick".into(),
            requirement: "改一下标题".into(),
        });
    }

    #[test]
    fn deferred_chat_is_released_exactly_when_a_legacy_block_settles() {
        // Delivered, degraded and aborted blocks all release the queue.
        let (_tmp, mut delivered) = app();
        started(&mut delivered);
        let live = delivered.is_pipeline_active();
        delivered.apply_engine(EngineEvent::BlockCompleted {
            final_phase: Phase::Delivery,
            paused_at: None,
        });
        assert!(legacy_block_settled(&delivered, live));

        let (_tmp, mut degraded) = app();
        started(&mut degraded);
        let live = degraded.is_pipeline_active();
        degraded.apply_engine(EngineEvent::Note(
            "[WARN][降级] 本次有 1 个阶段因底座离线只产出了占位模板".into(),
        ));
        degraded.apply_engine(EngineEvent::BlockCompleted {
            final_phase: Phase::Backend,
            paused_at: None,
        });
        assert!(legacy_block_settled(&degraded, live));

        // A block parked at a gate still owns its queue, and a Director run
        // (no legacy block) never settles here.
        let (_tmp, mut gate) = app();
        started(&mut gate);
        let live = gate.is_pipeline_active();
        gate.apply_engine(EngineEvent::gate_opened(umadev_agent::Gate::DocsConfirm));
        gate.apply_engine(EngineEvent::BlockCompleted {
            final_phase: Phase::DocsConfirm,
            paused_at: Some(umadev_agent::Gate::DocsConfirm),
        });
        assert!(!legacy_block_settled(&gate, live));
        let (_tmp, director) = app();
        assert!(!legacy_block_settled(&director, false));

        // A cancel that is draining owns the queue itself.
        let (_tmp, mut cancelling) = app();
        started(&mut cancelling);
        cancelling.begin_cancelling();
        cancelling.apply_engine(EngineEvent::BlockCompleted {
            final_phase: Phase::Delivery,
            paused_at: None,
        });
        assert!(!legacy_block_settled(&cancelling, true));
    }
}
