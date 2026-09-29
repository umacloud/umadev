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

/// A `Term` over stdout with a fixed viewport: building it neither queries nor
/// writes the terminal, and these tests never draw.
fn silent_term() -> Term {
    ratatui::Terminal::with_options(
        AnchoredBackend::new(CrosstermBackend::new(std::io::stdout())),
        ratatui::TerminalOptions {
            viewport: ratatui::Viewport::Fixed(ratatui::layout::Rect::new(0, 0, 80, 24)),
        },
    )
    .expect("a fixed viewport needs no terminal")
}

/// The event-loop state a tick-flushed key can touch (the loop's own locals).
#[allow(clippy::struct_excessive_bools)]
struct FlushLoop {
    run_task: Option<tokio::task::JoinHandle<()>>,
    cancel_drain: Option<tokio::task::JoinHandle<()>>,
    cancel_drain_timed_out: bool,
    cancel_deadline: Option<tokio::time::Instant>,
    continuous_run_active: bool,
    session_holder: SessionHolder,
    chat_session_holder: ChatSessionHolder,
    pending_ask_holder: PendingAskHolder,
    approval_holder: ApprovalHolder,
    host_input_holder: HostInputHolder,
    steer_holder: umadev_agent::SteerIntake,
    live_input_hub: LiveInputHub,
    sink: Arc<ChannelSink>,
    engine_rx: umadev_agent::ChannelReceiver,
    route_tx: tokio::sync::mpsc::UnboundedSender<RouteDecision>,
    route_rx: tokio::sync::mpsc::UnboundedReceiver<RouteDecision>,
    needs_redraw: bool,
    draw_now: bool,
}

impl FlushLoop {
    fn new() -> Self {
        let (sink, engine_rx) = ChannelSink::new();
        let (route_tx, route_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            run_task: None,
            cancel_drain: None,
            cancel_drain_timed_out: false,
            cancel_deadline: None,
            continuous_run_active: false,
            session_holder: Arc::new(tokio::sync::Mutex::new(None)),
            chat_session_holder: ChatSessionHolder::new(None),
            pending_ask_holder: Arc::new(tokio::sync::Mutex::new(None)),
            approval_holder: Arc::new(std::sync::Mutex::new(None)),
            host_input_holder: Arc::new(std::sync::Mutex::new(None)),
            steer_holder: Arc::new(std::sync::Mutex::new(Vec::new())),
            live_input_hub: LiveInputHub::default(),
            sink: Arc::new(sink),
            engine_rx,
            route_tx,
            route_rx,
            needs_redraw: false,
            draw_now: false,
        }
    }

    /// Press a lone Esc on the legacy (Windows) reader: the leaked-mouse filter
    /// holds it, and the next tick flushes it into the loop.
    fn tick_flushed_esc(&mut self, app: &mut App) {
        let mut filter = MouseSeqFilter::default();
        assert!(
            filter.feed(k(KeyCode::Esc)).is_empty(),
            "the legacy filter holds a lone Esc until the tick"
        );
        let mut terminal = silent_term();
        for key in filter.flush() {
            handle_tick_flush_key(
                app,
                &mut terminal,
                key,
                &mut self.needs_redraw,
                &mut self.draw_now,
                &mut self.run_task,
                &mut self.cancel_drain,
                &mut self.cancel_drain_timed_out,
                &mut self.cancel_deadline,
                &mut self.continuous_run_active,
                &self.session_holder,
                &self.chat_session_holder,
                &self.pending_ask_holder,
                &self.approval_holder,
                &self.host_input_holder,
                &self.steer_holder,
                &self.live_input_hub,
                &self.sink,
                &self.route_tx,
                &mut self.engine_rx,
                &mut self.route_rx,
            );
        }
    }
}

#[tokio::test]
async fn tick_flushed_lone_esc_denies_pending_approval() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut app = glue_app(tmp.path());
    let mut flush_loop = FlushLoop::new();
    // A Guarded turn is paused on the approval bar ("Esc=deny").
    app.thinking = true;
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    *flush_loop.approval_holder.lock().unwrap() = Some(test_pending_approval(reply_tx));
    app.set_pending_approval(pending_approval_item(&flush_loop.approval_holder));
    let generation = flush_loop.chat_session_holder.generation();

    flush_loop.tick_flushed_esc(&mut app);

    assert_eq!(reply_rx.await.ok(), Some(ApprovalReply::Deny));
    assert!(
        !app.interrupt_armed(),
        "the Esc answered the approval instead of arming the interrupt"
    );
    assert!(!app.cancelling);
    assert_eq!(
        flush_loop.chat_session_holder.generation(),
        generation,
        "no cancel reached the resident session"
    );
}

#[tokio::test]
async fn tick_flushed_lone_esc_cancels_a_pending_base_question() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut app = glue_app(tmp.path());
    let mut flush_loop = FlushLoop::new();
    app.thinking = true;
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    *flush_loop.host_input_holder.lock().unwrap() = Some(PendingHostInput {
        token: 7,
        reply_tx,
        req_id: String::new(),
        request: umadev_runtime::HostRequest::UserInput {
            questions: vec![host_choice_question(
                "database",
                umadev_runtime::HostQuestionKind::SingleChoice,
                true,
            )],
            metadata: serde_json::Value::Null,
        },
    });
    app.set_pending_host_input(pending_host_input_item(&flush_loop.host_input_holder));

    flush_loop.tick_flushed_esc(&mut app);

    assert!(matches!(
        reply_rx.await,
        Ok(umadev_runtime::HostResponse::Cancelled { .. })
    ));
    assert!(!app.interrupt_armed());
    assert!(!app.cancelling);
}

#[tokio::test]
async fn tick_flushed_double_esc_on_an_idle_paused_run_resets_the_parked_run() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut app = glue_app(tmp.path());
    let mut flush_loop = FlushLoop::new();
    // A legacy continuous run parked at a gate: nothing is running.
    app.director_gate_paused = true;
    flush_loop.continuous_run_active = true;

    flush_loop.tick_flushed_esc(&mut app);
    assert!(app.interrupt_armed(), "the first Esc arms the interrupt");
    flush_loop.tick_flushed_esc(&mut app);

    assert!(
        !flush_loop.continuous_run_active,
        "the idle cancel resets the parked run like a live Esc does"
    );
    assert!(!app.director_gate_paused);
    assert!(flush_loop.run_task.is_none() && flush_loop.cancel_drain.is_none());
}

#[test]
fn an_unreadable_config_is_announced_and_kept_when_the_picker_saves() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("config.toml");
    let original = "# mine\nbackend = \"codex\"\nshow_process_logs = \"yes\"\n";
    std::fs::write(&path, original).unwrap();
    let (cfg, notice) = crate::config::load_and_migrate_for_startup(&path);
    let mut app = App::new("runtime-glue", cfg, path.clone(), tmp.path().to_path_buf());
    crate::config_notice::show(&mut app, notice);

    let notice = app.picker_notice.clone().expect("the picker shows it");
    assert!(notice.contains("config.toml"), "{notice}");
    assert!(notice.contains("line 3"), "{notice}");
    assert!(notice.contains("expected a boolean"), "{notice}");
    assert!(
        !notice.contains('\n'),
        "the picker footer is one line: {notice}"
    );
    assert!(app.transcript_plaintext().contains("expected a boolean"));

    // The first-run picker reopened on defaults; its first Enter saves the
    // chosen language, and the unreadable file is kept beside the new one.
    assert_eq!(app.apply_key(KeyCode::Enter), Action::None);
    let backups = std::fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| name.starts_with("config.toml.bak-"))
        .collect::<Vec<_>>();
    assert_eq!(backups.len(), 1, "{backups:?}");
    assert_eq!(
        std::fs::read_to_string(tmp.path().join(&backups[0])).unwrap(),
        original
    );
    assert!(crate::config::load_strict(&path).is_ok());

    let mut quiet = glue_app(tmp.path());
    crate::config_notice::show(&mut quiet, None);
    assert_eq!(quiet.picker_notice, None);
}
