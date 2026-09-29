use super::*;

fn resume_options(root: &std::path::Path, requirement: &str) -> RunOptions {
    RunOptions {
        project_root: root.to_path_buf(),
        requirement: requirement.into(),
        slug: "demo".into(),
        model: String::new(),
        backend: "claude-code".into(),
        design_system: String::new(),
        seed_template: String::new(),
        mode: umadev_agent::TrustMode::Guarded,
        strict_coverage: false,
    }
}

/// Drive the TUI's director entry as a resume (`/continue`, or a gate approval)
/// over a scripted session; return the engine events and the route decision.
async fn resume_director(
    options: RunOptions,
    session: FakeChatSession,
) -> (Vec<EngineEvent>, Option<RouteDecision>) {
    let (sink, mut engine_rx) = ChannelSink::new();
    let (route_tx, mut route_rx) = tokio::sync::mpsc::unbounded_channel();
    Box::pin(run_director_loop(
        options,
        Arc::new(sink),
        route_tx,
        Arc::new(tokio::sync::Mutex::new(None)),
        umadev_runtime::BasePermissionProfile::Guarded,
        Vec::new(),
        None,
        false,
        true,
        Arc::new(std::sync::Mutex::new(Vec::new())),
        Arc::new(std::sync::Mutex::new(None)),
        Arc::new(std::sync::Mutex::new(None)),
        Some(Box::new(session)),
    ))
    .await;
    let events = std::iter::from_fn(|| engine_rx.try_recv().ok()).collect();
    (events, route_rx.try_recv().ok())
}

fn save_plan_step(
    root: &std::path::Path,
    id: &str,
    seat: umadev_agent::critics::Seat,
    file: &str,
    status: umadev_agent::plan_state::StepStatus,
) {
    use umadev_agent::plan_state::{AcceptanceSpec, Plan, PlanStep, StepFiles, StepKind};
    let plan = Plan {
        steps: vec![PlanStep {
            id: id.to_string(),
            title: format!("{id} title"),
            seat,
            kind: StepKind::Build,
            depends_on: Vec::new(),
            acceptance: AcceptanceSpec::SourcePresent,
            evidence: Vec::new(),
            files: StepFiles {
                create: vec![file.to_string()],
                modify: Vec::new(),
            },
            status,
        }],
        risks: Vec::new(),
        open_questions: Vec::new(),
    };
    umadev_agent::plan_state::save(&plan, root).expect("persist test plan");
}

#[tokio::test]
async fn tui_gate_approval_resumes_a_plan_whose_last_step_opened_docs_confirm() {
    // `/run 写一份产品需求文档`: the plan's last step (the PRD) went Done and opened
    // docs_confirm with nothing left to build, so the gate is the plan's only resume
    // cursor. Approving it resumes through this entry, whose fresh workflow baseline
    // must not erase that gate first, or the run fails "the saved Director cursor
    // disappeared" and stays stranded.
    use umadev_agent::critics::Seat;
    use umadev_agent::plan_state::StepStatus;
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    let requirement = "写一份产品需求文档";
    std::fs::create_dir_all(root.join("output")).unwrap();
    std::fs::write(
        root.join("output/demo-prd.md"),
        "# PRD\n\n| FR-001 | 登录 |\n",
    )
    .unwrap();
    save_plan_step(
        root,
        "prd",
        Seat::ProductManager,
        "output/demo-prd.md",
        StepStatus::Done,
    );
    let mut state = umadev_agent::WorkflowState::new(umadev_spec::Phase::DocsConfirm);
    state.slug = "demo".into();
    state.requirement = requirement.into();
    state.active_gate = "docs_confirm".into();
    umadev_agent::write_workflow_state(root, &state).unwrap();

    let (session, _sent, _ended) = FakeChatSession::new(Vec::new());
    let (events, decision) = resume_director(resume_options(root, requirement), session).await;

    assert!(
        !matches!(
            &decision,
            Some(RouteDecision::Failed(reason)) if reason.contains("cursor disappeared")
        ),
        "an approved gate resumes its parked plan: {decision:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, EngineEvent::Note(note) if note.contains("1/1"))),
        "the resume re-attached to the saved plan: {events:?}"
    );
}
