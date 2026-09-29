//! Event-loop glue around the chat turn: cancelling local work, live input,
//! session restarts, keys the legacy input path replays, and startup notices.

use super::*;

/// A chat-screen app: without a configured base a new app opens on the
/// first-run picker, which would take these tests' keys.
fn glue_app(root: &std::path::Path) -> App {
    let mut app = App::new(
        "runtime-glue",
        crate::config::UserConfig::default(),
        root.join("config.toml"),
        root.to_path_buf(),
    );
    app.mode = crate::app::AppMode::Chat;
    app
}

#[cfg(unix)]
#[tokio::test]
async fn cancelling_a_bang_command_keeps_the_resident_session() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut app = glue_app(tmp.path());
    app.chat_session_id = Some("native-thread-1".to_string());
    app.host_chat_session_active = true;
    let chat_session_holder = ChatSessionHolder::new(None);
    let generation = chat_session_holder.generation();
    let approval_holder: ApprovalHolder = Arc::new(std::sync::Mutex::new(None));
    let host_input_holder: HostInputHolder = Arc::new(std::sync::Mutex::new(None));
    let pending_ask_holder: PendingAskHolder = Arc::new(tokio::sync::Mutex::new(None));
    let steer_holder: umadev_agent::SteerIntake = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (route_tx, mut route_rx) = tokio::sync::mpsc::unbounded_channel();
    let memory = app.conversation.len();
    let request = LocalCommandRequest::shell(tmp.path(), "sleep 30");
    let task = local_command::spawn(&mut app, request, &route_tx);

    // Esc Esc while the command runs.
    assert_eq!(app.apply_key(KeyCode::Esc), Action::None);
    assert_eq!(app.apply_key(KeyCode::Esc), Action::Cancel);
    assert!(
        !prepare_cancel_request(
            &mut app,
            false,
            &approval_holder,
            &host_input_holder,
            &pending_ask_holder,
            &steer_holder,
            &chat_session_holder,
        ),
        "a local task never enters the chat-turn cancel"
    );
    let decision = tokio::time::timeout(TURN_HANG_GUARD, route_rx.recv())
        .await
        .expect("a stopped command settles through its own result");
    let Some(RouteDecision::LocalCommandDone(result)) = decision else {
        panic!("expected the local command's own result");
    };
    assert!(!result.ok);
    app.record_local_command_done(result);
    task.await.unwrap();

    assert_eq!(
        chat_session_holder.generation(),
        generation,
        "the resident session was not invalidated"
    );
    assert_eq!(app.chat_session_id.as_deref(), Some("native-thread-1"));
    assert!(app.host_chat_session_active);
    assert_eq!(
        app.conversation.len(),
        memory,
        "no cancelled-request turn was recorded against the previous request"
    );
    assert!(!app.thinking && !app.cancelling && app.local_task.is_none());
    assert!(
        !app.transcript_plaintext()
            .contains(umadev_i18n::t(app.lang, "run.cancelled")),
        "a stopped shell command is not reported as a cancelled run"
    );
}

/// A Director parked at a gate while `!npm test` runs: a gate decision would
/// resume or tear down the parked run under the running command, and stopping
/// the command must not close the gate behind it.
#[test]
fn a_gate_decision_waits_for_a_running_local_command() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut app = glue_app(tmp.path());
    let gate = umadev_agent::Gate::DocsConfirm;
    app.active_gate = Some(gate);
    app.director_gate_paused = true;
    app.gate_choice = umadev_agent::GateChoice::standard(gate);
    let request = LocalCommandRequest::shell(tmp.path(), "npm test");
    app.begin_local_command(&request);
    let _stopped = app.register_local_task();

    // Enter on the picker's "approve" waits for the command.
    assert_eq!(app.apply_key(KeyCode::Enter), Action::None);
    assert_eq!(app.active_gate, Some(gate));
    assert!(app
        .transcript_plaintext()
        .contains(umadev_i18n::t(app.lang, "tui.local.busy")));

    // A typed 「取消」 stops the command; the gate stays open behind it.
    app.input = "取消".to_string();
    app.input_cursor = app.input.chars().count();
    assert_eq!(app.apply_key(KeyCode::Enter), Action::Cancel);
    assert_eq!(app.active_gate, Some(gate));
    assert!(app.gate_choice.is_some());

    // Once the command has settled, the same Enter approves the gate.
    app.record_local_command_done(crate::local_command::LocalCommandResult {
        request,
        ok: false,
        output: umadev_i18n::t(app.lang, "tui.local.cancelled").to_string(),
    });
    assert_eq!(app.apply_key(KeyCode::Enter), Action::Continue(gate));
}

#[test]
fn accepted_live_steer_is_restored_when_the_drain_exits() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut app = glue_app(tmp.path());
    let hub = LiveInputHub::default();
    let lane = hub.register("codex", same_turn_capabilities());
    let turn = SubmittedTurn::text("不要改数据库,只改前端".to_string());
    assert!(matches!(
        hub.dispatch(turn.clone()),
        LiveInputDispatch::EnqueuedSameTurn
    ));

    // The turn ended (it finished, failed or was cancelled) before its drain
    // read the lane.
    drop(lane);

    assert!(sync_live_input_readiness(&mut app, &hub));
    assert_eq!(app.input, turn.text, "the text is back in the input box");
    assert!(app
        .transcript_plaintext()
        .contains(umadev_i18n::t(app.lang, "input.steer.returned")));
}

#[tokio::test]
async fn only_an_unconfirmed_live_steer_is_handed_back() {
    let tmp = tempfile::TempDir::new().unwrap();
    let hub = LiveInputHub::default();

    // Aborted while writing the steer to the base: its delivery is unknown.
    let mut lane = hub.register("codex", same_turn_capabilities());
    let pending = SubmittedTurn::text("换成 SQLite".to_string());
    hub.dispatch(pending.clone());
    assert!(lane.recv().await.is_some());
    drop(lane);
    let mut app = glue_app(tmp.path());
    assert!(sync_live_input_readiness(&mut app, &hub));
    assert_eq!(app.input, pending.text);

    // A steer whose delivery was confirmed is not handed back.
    let mut lane = hub.register("codex", same_turn_capabilities());
    hub.dispatch(SubmittedTurn::text("already delivered".to_string()));
    assert!(lane.recv().await.is_some());
    lane.settle();
    drop(lane);
    let mut app = glue_app(tmp.path());
    assert!(!sync_live_input_readiness(&mut app, &hub));
    assert!(app.input.is_empty());
}

#[tokio::test]
async fn restart_does_not_block_on_a_turn_holding_the_holder() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut app = glue_app(tmp.path());
    let holder = ChatSessionHolder::new(None);
    let pending_ask: PendingAskHolder = Arc::new(tokio::sync::Mutex::new(None));
    let generation = holder.generation();
    // A turn lazily opening its session holds the slot for seconds.
    let busy = holder.clone();
    let (locked_tx, locked_rx) = tokio::sync::oneshot::channel();
    let turn = tokio::spawn(async move {
        let _slot = busy.lock().await;
        let _ = locked_tx.send(());
        tokio::time::sleep(Duration::from_secs(30)).await;
    });
    locked_rx.await.unwrap();

    tokio::time::timeout(
        Duration::from_secs(2),
        restart_resident_chat_session(&mut app, &holder, &pending_ask),
    )
    .await
    .expect("the loop must not wait for the turn's session open");

    assert_ne!(
        holder.generation(),
        generation,
        "the new generation still fences the turn's session"
    );
    turn.abort();
}
