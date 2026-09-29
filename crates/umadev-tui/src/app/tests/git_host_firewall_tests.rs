use super::*;

#[test]
fn live_vendor_input_surfaces_defer_git_commit_to_the_typed_host_fifo() {
    enum LiveLane {
        PromptQueue,
        LiveInput,
        Director,
    }
    for lane in [
        LiveLane::PromptQueue,
        LiveLane::LiveInput,
        LiveLane::Director,
    ] {
        let mut app = fresh_app(Some("grok-build"));
        app.set_trust_mode(umadev_agent::TrustMode::Auto);
        app.thinking = true;
        app.agentic_in_flight = true;
        match lane {
            LiveLane::PromptQueue => app.prompt_queue.set_ready(true),
            LiveLane::LiveInput => app.live_input_ready = true,
            LiveLane::Director => app.director_run_in_flight = true,
        }
        assert_eq!(app.submit_text("提交git记录".into()), Action::None);
        assert!(app.queued_steer.is_empty());
        assert!(matches!(
            app.take_next_queued_dispatch(),
            Some(ResidentDispatch::HostGitCommit(text)) if text == "提交git记录"
        ));
        assert!(app.queued_chat.is_empty());
    }
}

#[test]
fn paused_fifo_drains_only_a_front_host_commit_without_overtaking_chat() {
    let mut app = fresh_app(Some("grok-build"));
    app.set_trust_mode(umadev_agent::TrustMode::Auto);
    app.thinking = true;
    app.agentic_in_flight = true;
    assert_eq!(app.submit_text("先回答这个问题".into()), Action::None);
    assert_eq!(app.submit_text("提交git记录".into()), Action::None);
    assert!(app.take_front_host_git_commit().is_none());
    assert!(matches!(
        app.take_next_queued_dispatch(),
        Some(ResidentDispatch::RoutedChat(text)) if text == "先回答这个问题"
    ));
    assert_eq!(
        app.take_front_host_git_commit().as_deref(),
        Some("提交git记录")
    );
    assert!(app.take_front_host_git_commit().is_none());
}

#[test]
fn live_compound_and_plan_git_requests_never_enter_any_queue() {
    for (mode, request) in [
        (umadev_agent::TrustMode::Auto, "提交git记录，然后推送"),
        (umadev_agent::TrustMode::Plan, "提交git记录"),
    ] {
        let mut app = fresh_app(Some("grok-build"));
        app.set_trust_mode(mode);
        app.thinking = true;
        app.agentic_in_flight = true;
        app.director_run_in_flight = true;
        app.live_input_ready = true;
        app.prompt_queue.set_ready(true);
        assert_eq!(app.submit_text(request.into()), Action::None);
        assert!(app.queued_chat.is_empty(), "{request}");
        assert!(app.queued_steer.is_empty(), "{request}");
        assert!(app.pending_route_input.is_none(), "{request}");
        assert!(app.tasks.is_empty(), "{request}");
    }
}

#[test]
fn submit_wording_in_product_requests_reaches_normal_routing() {
    // 提交 is also the everyday "submit". A button, a form flow or a Git hook is
    // product work for the base; the host commit boundary must not drop it.
    for request in [
        "加一个确认提交按钮",
        "用户提交后推送通知给管理员",
        "创建一个提交按钮",
        "给项目加一个 git commit 前自动跑 eslint 的钩子",
    ] {
        let mut app = fresh_app(Some("offline"));
        assert_eq!(
            app.submit_text(request.into()),
            Action::Route(request.into()),
            "{request}"
        );
        assert!(
            !app.history
                .iter()
                .any(|message| message.body().contains("[blocked]")),
            "{request}"
        );
    }

    // At an open gate the same wording is revision feedback, not a commit.
    let mut app = fresh_app(Some("offline"));
    app.set_trust_mode(umadev_agent::TrustMode::Auto);
    app.apply_engine(EngineEvent::GateOpened {
        gate: Gate::PreviewConfirm,
        choice: None,
    });
    for ch in "/revise 把按钮文字改成确认提交".chars() {
        let _ = app.apply_key(KeyCode::Char(ch));
    }
    assert_eq!(
        app.apply_key(KeyCode::Enter),
        Action::Revise("把按钮文字改成确认提交".into())
    );
}

#[test]
fn host_commit_does_not_replace_the_requirement_that_redo_uses() {
    let mut app = fresh_app(Some("offline"));
    app.set_trust_mode(umadev_agent::TrustMode::Auto);
    app.requirement = "修复登录页".into();
    assert_eq!(
        app.submit_text("提交git记录".into()),
        Action::Route("提交git记录".into())
    );
    assert_eq!(app.requirement, "修复登录页");
    assert_eq!(app.slash_redo(""), Action::StartRun("修复登录页".into()));
}

#[test]
fn restored_git_requirement_is_refused_by_redo_continue_and_tasks_resume() {
    let mut app = fresh_app(Some("claude-code"));
    app.set_trust_mode(umadev_agent::TrustMode::Auto);
    let plan = umadev_agent::Plan {
        steps: vec![umadev_agent::PlanStep {
            files: umadev_agent::StepFiles::default(),
            id: "a".into(),
            title: "stale commit".into(),
            seat: umadev_agent::Seat::QaEngineer,
            kind: umadev_agent::StepKind::Build,
            depends_on: vec![],
            acceptance: umadev_agent::AcceptanceSpec::SourcePresent,
            evidence: Vec::new(),
            status: umadev_agent::StepStatus::Pending,
        }],
        risks: vec![],
        open_questions: vec![],
    };
    umadev_agent::save_plan(&plan, &app.project_root).unwrap();
    let mut state = umadev_agent::WorkflowState::new(umadev_spec::Phase::Delivery);
    state.slug = "stale".into();
    state.requirement = "提交git记录".into();
    state.backend = "claude-code".into();
    umadev_agent::write_workflow_state(&app.project_root, &state).unwrap();
    app.requirement = state.requirement.clone();
    for action in [
        app.slash_redo(""),
        app.slash_redo("frontend"),
        app.try_slash_command("/continue").unwrap(),
        app.slash_tasks("resume"),
    ] {
        assert_eq!(action, Action::None);
    }
    assert!(app.tasks.is_empty());
    assert!(!app.run_started && !app.director_run_in_flight);
}

#[derive(Clone, Copy, Debug)]
enum ParkedRun {
    Gate,
    Budget,
    Operational,
}

fn parked_host_git_app(kind: ParkedRun) -> App {
    let mut app = fresh_app(Some("claude-code"));
    app.set_trust_mode(umadev_agent::TrustMode::Auto);
    app.requirement = "修复登录页".into();
    app.register_run_task("修复登录页");
    app.plan_steps.push(PlanStepRow {
        id: "build".into(),
        title: "修复登录页".into(),
        status: "pending".into(),
        seat: "backend-engineer".into(),
    });
    match kind {
        ParkedRun::Gate => {
            app.active_gate = Some(Gate::DocsConfirm);
            app.director_gate_paused = true;
            app.pending_director_gate = Some((Gate::DocsConfirm, None));
        }
        ParkedRun::Budget => app.budget_paused = true,
        ParkedRun::Operational => {
            app.budget_paused = true;
            app.operational_pause_reason = Some("reviewer unavailable".into());
        }
    }
    app.chat_session_id = Some("native-session".into());
    app.chat_resume_identity = crate::session_slot::requested_resume_identity(
        "claude-code",
        &app.project_root,
        app.effective_trust_mode().base_permissions(),
    );
    app.host_chat_session_active = true;
    app.run_session_handed_to_chat = true;
    app.host_git_in_flight = true;
    app.thinking = true;
    app.agentic_in_flight = true;
    app
}

fn assert_parked_run_unchanged(
    app: &App,
    kind: ParkedRun,
    tasks: &[BackgroundTask],
    plan: &[PlanStepRow],
    resume_identity: Option<&umadev_runtime::BaseResumeIdentity>,
) {
    assert_eq!(app.requirement, "修复登录页", "{kind:?}");
    assert_eq!(app.tasks, tasks, "{kind:?}");
    assert_eq!(app.plan_steps, plan, "{kind:?}");
    assert_eq!(app.chat_session_id.as_deref(), Some("native-session"));
    assert_eq!(
        app.chat_resume_identity.as_ref(),
        resume_identity,
        "{kind:?}"
    );
    assert!(app.host_chat_session_active, "{kind:?}");
    assert!(app.run_session_handed_to_chat, "{kind:?}");
    match kind {
        ParkedRun::Gate => {
            assert_eq!(app.active_gate, Some(Gate::DocsConfirm));
            assert!(app.director_gate_paused);
            assert_eq!(app.pending_director_gate, Some((Gate::DocsConfirm, None)));
        }
        ParkedRun::Budget => {
            assert!(app.budget_paused);
            assert!(app.operational_pause_reason.is_none());
        }
        ParkedRun::Operational => {
            assert!(app.budget_paused);
            assert_eq!(
                app.operational_pause_reason.as_deref(),
                Some("reviewer unavailable")
            );
        }
    }
}

#[test]
fn host_git_success_denial_and_error_preserve_all_three_parked_run_kinds() {
    for kind in [ParkedRun::Gate, ParkedRun::Budget, ParkedRun::Operational] {
        for result in [
            Ok("committed".to_string()),
            Err("approval denied".to_string()),
            Err("transaction failed".to_string()),
        ] {
            let mut app = parked_host_git_app(kind);
            let tasks = app.tasks.clone();
            let plan = app.plan_steps.clone();
            let identity = app.chat_resume_identity.clone();
            app.record_host_git_done(result);
            assert_parked_run_unchanged(&app, kind, &tasks, &plan, identity.as_ref());
            assert!(!app.host_git_in_flight && !app.agentic_in_flight);
        }
    }
}

#[test]
fn host_git_escape_cancel_preserves_gate_budget_and_operational_pauses() {
    for kind in [ParkedRun::Gate, ParkedRun::Budget, ParkedRun::Operational] {
        let mut app = parked_host_git_app(kind);
        let tasks = app.tasks.clone();
        let plan = app.plan_steps.clone();
        let identity = app.chat_resume_identity.clone();
        assert_eq!(app.apply_key(KeyCode::Esc), Action::None);
        assert_eq!(app.apply_key(KeyCode::Esc), Action::Cancel);
        app.begin_host_git_cancelling();
        app.record_host_git_cancelled();
        assert_parked_run_unchanged(&app, kind, &tasks, &plan, identity.as_ref());
        assert!(!app.host_git_in_flight && !app.cancelling);
        assert!(app.history.iter().any(|message| {
            let body = message.body();
            body.contains("Git 提交已取消") || body.contains("Git commit cancelled")
        }));
    }
}

#[test]
fn host_git_cancel_linearization_has_one_authoritative_terminal_per_pause_kind() {
    for kind in [ParkedRun::Gate, ParkedRun::Budget, ParkedRun::Operational] {
        for (outcome, published, expected_settled) in [
            (crate::CancelDrainOutcome::Cancelled, None, true),
            (
                crate::CancelDrainOutcome::Cancelled,
                Some(Ok("already committed".to_string())),
                true,
            ),
            (crate::CancelDrainOutcome::Finished, None, false),
            (crate::CancelDrainOutcome::Panicked, None, true),
        ] {
            let mut app = parked_host_git_app(kind);
            app.cancelling = true;
            let tasks = app.tasks.clone();
            let plan = app.plan_steps.clone();
            let identity = app.chat_resume_identity.clone();
            let before = app.full_transcript.len();
            let settled =
                crate::resident_host_git::settle_cancel(&mut app, outcome, published.clone());
            assert_eq!(settled, expected_settled, "{kind:?} {outcome:?}");
            if expected_settled {
                assert_eq!(app.full_transcript.len(), before + 1);
                assert!(!app.host_git_in_flight);
                assert!(!app.cancelling);
                if published.is_some() {
                    assert!(
                        !app.history
                            .iter()
                            .any(|message| message.body().contains("Git 提交已取消")),
                        "a published result wins the cancel race"
                    );
                }
            } else {
                assert_eq!(app.full_transcript.len(), before);
                assert!(app.host_git_in_flight);
                assert!(!app.cancelling);
                app.record_host_git_done(Ok("late terminal".into()));
                assert_eq!(app.full_transcript.len(), before + 1);
            }
            assert_parked_run_unchanged(&app, kind, &tasks, &plan, identity.as_ref());
        }
    }
}

#[tokio::test]
async fn gate_query_terminal_releases_only_a_front_host_git_for_success_and_failure() {
    for succeeds in [true, false] {
        let mut app = fresh_app(Some("claude-code"));
        app.set_trust_mode(umadev_agent::TrustMode::Auto);
        app.active_gate = Some(Gate::DocsConfirm);
        app.gate_query_in_flight = true;
        app.active_gate_query_epoch = Some(7);
        app.thinking = true;
        app.agentic_in_flight = true;
        assert_eq!(app.submit_text("提交git记录".into()), Action::None);
        let accepted = if succeeds {
            app.record_gate_query_done(7, "查询完成".into())
        } else {
            app.record_gate_query_failed(7, "查询失败".into())
        };
        assert!(accepted);
        let approval = std::sync::Arc::new(std::sync::Mutex::new(None));
        let (sink, _engine_rx) = umadev_agent::ChannelSink::new();
        let (route_tx, _route_rx) = tokio::sync::mpsc::unbounded_channel();
        let task = crate::resident_host_git::drain_after_gate_query(
            &mut app,
            accepted,
            &approval,
            &std::sync::Arc::new(sink),
            &route_tx,
        )
        .expect("valid query terminal starts the front host transaction");
        task.abort();
        let _ = task.await;
        assert_eq!(app.active_gate, Some(Gate::DocsConfirm));
    }

    let mut app = fresh_app(Some("claude-code"));
    app.set_trust_mode(umadev_agent::TrustMode::Auto);
    app.thinking = true;
    app.agentic_in_flight = true;
    assert_eq!(app.submit_text("先回答这个问题".into()), Action::None);
    app.active_gate = Some(Gate::DocsConfirm);
    app.gate_query_in_flight = true;
    app.active_gate_query_epoch = Some(9);
    assert_eq!(app.submit_text("提交git记录".into()), Action::None);
    let accepted = app.record_gate_query_done(9, "查询完成".into());
    let approval = std::sync::Arc::new(std::sync::Mutex::new(None));
    let (sink, _engine_rx) = umadev_agent::ChannelSink::new();
    let (route_tx, _route_rx) = tokio::sync::mpsc::unbounded_channel();
    assert!(crate::resident_host_git::drain_after_gate_query(
        &mut app,
        accepted,
        &approval,
        &std::sync::Arc::new(sink),
        &route_tx,
    )
    .is_none());
    assert!(matches!(
        app.take_next_queued_dispatch(),
        Some(ResidentDispatch::RoutedChat(text)) if text == "先回答这个问题"
    ));
    assert_eq!(
        app.take_front_host_git_commit().as_deref(),
        Some("提交git记录")
    );
}
