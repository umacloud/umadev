use super::*;

// ── BOUNDED RE-PLAN of a blocked subtree (attempt_replan_blocked_subtree) ──

/// scaffold(Done) → api(Blocked) → ui(Pending): a blocked step that STRANDS a
/// dependent (`ui`). `mk` mirrors the inline PlanStep builder the other tests use.
fn blocked_subtree_plan() -> Plan {
    use crate::plan_state::{AcceptanceSpec, PlanStep, StepKind};
    let mk = |id: &str, deps: &[&str], status: StepStatus| PlanStep {
        files: plan_state::StepFiles::default(),
        id: id.into(),
        title: format!("Build the {id}"),
        seat: crate::critics::Seat::BackendEngineer,
        kind: StepKind::Build,
        depends_on: deps.iter().map(|d| (*d).to_string()).collect(),
        acceptance: AcceptanceSpec::SourcePresent,
        evidence: Vec::new(),
        status,
    };
    Plan {
        steps: vec![
            mk("scaffold", &[], StepStatus::Done),
            mk("api", &["scaffold"], StepStatus::Blocked),
            mk("ui", &["api"], StepStatus::Pending),
        ],
        risks: vec![],
        open_questions: vec![],
    }
}

/// A brain re-plan reply: a fresh route {api2 → ui2} around the blocked `api`.
const REPLAN_SUBDAG: &str = r#"{"steps":[
    {"id":"api2","title":"alt api","seat":"backend-engineer","kind":"build",
     "depends_on":["scaffold"],"acceptance":"source-present"},
    {"id":"ui2","title":"alt ui","seat":"frontend-engineer","kind":"build",
     "depends_on":["api2"],"acceptance":"source-present"}]}"#;

#[tokio::test]
async fn replan_triggers_once_merges_a_validated_subdag_and_is_bounded() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (events, _rec) = sink();
    // The fork's judge turn emits the replacement sub-DAG JSON.
    let mut sess = FakeSession::new(vec![], true, REPLAN_SUBDAG);
    let o = opts(tmp.path());
    let mut plan = blocked_subtree_plan();
    let mut replanned = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(3_600);

    // First block WITH stranded dependents → ONE re-plan; the sub-DAG is merged
    // (routed through normalized(): dedup / dangling strip / cycle-break / floors).
    let merged = attempt_replan_blocked_subtree(
        &mut sess,
        &o,
        &events,
        &mut plan,
        "api",
        "Build the api",
        &["source-present: no source files on disk".to_string()],
        &mut replanned,
        deadline,
    )
    .await;
    assert!(
        merged,
        "a blocked step with stranded dependents re-plans once"
    );
    assert!(replanned, "the single-attempt budget is consumed");
    let ids: Vec<&str> = plan.steps.iter().map(|s| s.id.as_str()).collect();
    assert!(
        ids.contains(&"api2") && ids.contains(&"ui2"),
        "the fresh route is spliced in: {ids:?}"
    );
    assert!(
        !ids.contains(&"api") && !ids.contains(&"ui"),
        "the blocked subtree is replaced: {ids:?}"
    );
    // The surviving Done step KEEPS its status (normalized's reset was undone).
    assert_eq!(
        plan.steps
            .iter()
            .find(|s| s.id == "scaffold")
            .unwrap()
            .status,
        StepStatus::Done
    );

    // BOUND: a SECOND block does NOT re-plan again (the flag is already consumed).
    let before = plan.clone();
    let again = attempt_replan_blocked_subtree(
        &mut sess,
        &o,
        &events,
        &mut plan,
        "api2",
        "alt api",
        &[],
        &mut replanned,
        deadline,
    )
    .await;
    assert!(!again, "at most ONE re-plan per run");
    assert_eq!(plan, before, "a second attempt leaves the plan unchanged");
}

#[tokio::test]
async fn replan_falls_back_when_the_consult_fails() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (events, _rec) = sink();
    // can_fork == false → the consult can't open → judge_json None → fallback.
    let mut sess = FakeSession::new(vec![], false, REPLAN_SUBDAG);
    let o = opts(tmp.path());
    let mut plan = blocked_subtree_plan();
    let before = plan.clone();
    let mut replanned = false;
    let merged = attempt_replan_blocked_subtree(
        &mut sess,
        &o,
        &events,
        &mut plan,
        "api",
        "Build the api",
        &[],
        &mut replanned,
        std::time::Instant::now() + Duration::from_secs(3_600),
    )
    .await;
    assert!(!merged, "a failed consult falls back to the honest strand");
    assert_eq!(plan, before, "the plan is unchanged when the consult fails");
    assert!(
        replanned,
        "the attempt is still consumed so a failed consult can never retry (no loop)"
    );
}

#[tokio::test]
async fn replan_falls_back_on_an_unparseable_or_noop_subdag() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (events, _rec) = sink();
    // Unparseable fork reply → parse_brain_steps empty → fallback.
    let mut sess = FakeSession::new(vec![], true, "not json at all");
    let o = opts(tmp.path());
    let mut plan = blocked_subtree_plan();
    let before = plan.clone();
    let mut replanned = false;
    let merged = attempt_replan_blocked_subtree(
        &mut sess,
        &o,
        &events,
        &mut plan,
        "api",
        "Build the api",
        &[],
        &mut replanned,
        std::time::Instant::now() + Duration::from_secs(3_600),
    )
    .await;
    assert!(!merged, "an unparseable sub-DAG falls back");
    assert_eq!(plan, before);

    // A no-op sub-DAG that re-emits ONLY existing ids (no new route) also falls back.
    let mut sess2 = FakeSession::new(
        vec![],
        true,
        r#"{"steps":[{"id":"ui","title":"x","seat":"frontend-engineer","kind":"build","acceptance":"source-present"}]}"#,
    );
    let mut plan2 = blocked_subtree_plan();
    let before2 = plan2.clone();
    let mut replanned2 = false;
    let merged2 = attempt_replan_blocked_subtree(
        &mut sess2,
        &o,
        &events,
        &mut plan2,
        "api",
        "Build the api",
        &[],
        &mut replanned2,
        std::time::Instant::now() + Duration::from_secs(3_600),
    )
    .await;
    assert!(
        !merged2,
        "a sub-DAG adding no NEW id changes nothing → fallback"
    );
    assert_eq!(plan2, before2);
}

#[tokio::test]
async fn replan_does_not_trigger_for_a_blocked_leaf_with_no_dependents() {
    use crate::plan_state::{AcceptanceSpec, PlanStep, StepKind};
    let tmp = tempfile::TempDir::new().unwrap();
    let (events, _rec) = sink();
    // A valid sub-DAG is available on the fork, but the blocked step is a LEAF
    // (nothing depends on it) → nothing to recover → no consult, no change.
    let mut sess = FakeSession::new(vec![], true, REPLAN_SUBDAG);
    let o = opts(tmp.path());
    let mut plan = Plan {
        steps: vec![
            PlanStep {
                files: plan_state::StepFiles::default(),
                id: "scaffold".into(),
                title: "scaffold".into(),
                seat: crate::critics::Seat::FrontendEngineer,
                kind: StepKind::Build,
                depends_on: vec![],
                acceptance: AcceptanceSpec::SourcePresent,
                evidence: Vec::new(),
                status: StepStatus::Done,
            },
            PlanStep {
                files: plan_state::StepFiles::default(),
                id: "leaf".into(),
                title: "a leaf".into(),
                seat: crate::critics::Seat::FrontendEngineer,
                kind: StepKind::Build,
                depends_on: vec!["scaffold".into()],
                acceptance: AcceptanceSpec::SourcePresent,
                evidence: Vec::new(),
                status: StepStatus::Blocked,
            },
        ],
        risks: vec![],
        open_questions: vec![],
    };
    let before = plan.clone();
    let mut replanned = false;
    let merged = attempt_replan_blocked_subtree(
        &mut sess,
        &o,
        &events,
        &mut plan,
        "leaf",
        "a leaf",
        &[],
        &mut replanned,
        std::time::Instant::now() + Duration::from_secs(3_600),
    )
    .await;
    assert!(
        !merged,
        "a blocked leaf has nothing to recover → no re-plan"
    );
    assert!(
        !replanned,
        "no stranded dependents → the single-attempt budget is NOT spent"
    );
    assert_eq!(plan, before, "the plan is unchanged for a leaf block");
}

// ── Spec-MUST confirmation gates on the DEFAULT path (A1-GAP1 / UD-FLOW-002 /
//    UD-FLOW-003): a HOSTED, guarded run pauses at docs_confirm once the core-doc
//    steps settle Done, and at preview_confirm once the frontend family settles;
//    auto / unhosted library runs drive through; a resume never
//    re-fires the gate it paused at. ──

/// A doc-first 4-step plan (PM → architect → frontend/backend) whose steps all
/// pass a seeded source-present acceptance, so gate positions are deterministic.
fn gated_plan() -> crate::plan_state::Plan {
    use crate::critics::Seat;
    use crate::plan_state::{AcceptanceSpec, Plan, PlanStep, StepKind, StepStatus};
    let mk = |id: &str, seat: Seat, deps: &[&str]| PlanStep {
        files: plan_state::StepFiles {
            create: vec![format!("work/{id}.txt")],
            modify: Vec::new(),
        },
        id: id.into(),
        title: format!("do the {id} work"),
        seat,
        kind: StepKind::Build,
        depends_on: deps.iter().map(|d| (*d).to_string()).collect(),
        acceptance: AcceptanceSpec::SourcePresent,
        evidence: Vec::new(),
        status: StepStatus::Pending,
    };
    Plan {
        steps: vec![
            mk("pm", Seat::ProductManager, &[]),
            mk("arch", Seat::Architect, &["pm"]),
            mk("fe", Seat::FrontendEngineer, &["arch"]),
            mk("be", Seat::BackendEngineer, &["arch"]),
        ],
        risks: vec![],
        open_questions: vec![],
    }
}

/// The hosted interaction a TUI provides (gates on; no steer/approval needed).
fn gates_hosted_interaction() -> crate::interaction::RunInteraction {
    crate::interaction::RunInteraction {
        steer: None,
        approval: None,
        host_request: None,
        confirm_gates: true,
    }
}

fn boxed<F: Future>(i: RunInteraction, f: F) -> impl Future<Output = F::Output> {
    crate::interaction::hosted(i, Box::pin(f))
}

#[tokio::test]
async fn hosted_guarded_build_pauses_at_docs_confirm_with_persisted_door() {
    // The revived spec-MUST docs gate: once the PM + architect doc steps settle
    // Done on a HOSTED guarded run, the schedule PAUSES — GateOpened emitted,
    // plan persisted (remaining steps Pending), the open door written to
    // workflow-state — and returns PausedAtGate instead of driving the code steps.
    let tmp = tempfile::TempDir::new().unwrap();
    seed_source(tmp.path());
    let (events, rec) = sink();
    let mut sess = FakeSession::new(vec![], false, "");
    let mut o = opts(tmp.path());
    o.mode = TrustMode::Guarded;
    o.requirement = "做一个完整的产品".to_string();
    let route = build_route();
    let mut plan = gated_plan();

    let outcome = boxed(gates_hosted_interaction(), async {
        drive_plan_steps(
            &mut sess,
            &o,
            &events,
            &route,
            &mut plan,
            IdleBudget::new(Duration::from_millis(200), Duration::from_millis(200)),
            std::time::Instant::now() + Duration::from_secs(3_600),
        )
        .await
    })
    .await;
    assert!(
        matches!(
            outcome,
            Some(DirectorLoopOutcome::PausedAtGate {
                gate: crate::gates::Gate::DocsConfirm
            })
        ),
        "a hosted guarded run pauses at docs_confirm: {outcome:?}"
    );
    // GateOpened rendered the gate card (the TUI's active_gate driver).
    assert_eq!(
        rec.count(|e| matches!(
            e,
            EngineEvent::GateOpened {
                gate: crate::gates::Gate::DocsConfirm,
                ..
            }
        )),
        1,
        "exactly one docs_confirm GateOpened was emitted"
    );
    // The persisted plan holds the pause honestly: docs Done, code Pending.
    let loaded = plan_state::load(tmp.path()).expect("plan persisted at the pause");
    let status_of = |id: &str| loaded.steps.iter().find(|s| s.id == id).unwrap().status;
    assert_eq!(status_of("pm"), StepStatus::Done);
    assert_eq!(status_of("arch"), StepStatus::Done);
    assert_eq!(status_of("fe"), StepStatus::Pending);
    assert_eq!(status_of("be"), StepStatus::Pending);
    // The open door is on disk for /status + a fresh session's /continue.
    let state = crate::state::read_workflow_state(tmp.path()).expect("state written");
    assert_eq!(state.active_gate, "docs_confirm");
    assert!(has_resumable_run(tmp.path()));
    assert!(has_resumable_director_plan(tmp.path()));
}

#[tokio::test]
async fn resume_after_docs_gate_pauses_at_preview_then_completes() {
    // The full round-trip: docs pause → approve (resume) → the docs gate never
    // re-fires (transition-triggered: the Done doc steps are never re-driven),
    // the frontend step completes → preview_confirm pause → approve (resume) →
    // the backend step completes → a clean Done outcome.
    let tmp = tempfile::TempDir::new().unwrap();
    seed_source(tmp.path());
    let (events, rec) = sink();
    let mut o = opts(tmp.path());
    o.mode = TrustMode::Guarded;
    o.requirement = "做一个完整的产品".to_string();
    let route = build_route();

    // 1. Fresh drive → docs pause (as covered above).
    let mut plan = gated_plan();
    let mut sess = FakeSession::new(vec![], false, "");
    let outcome = boxed(gates_hosted_interaction(), async {
        drive_plan_steps(
            &mut sess,
            &o,
            &events,
            &route,
            &mut plan,
            IdleBudget::new(Duration::from_millis(200), Duration::from_millis(200)),
            std::time::Instant::now() + Duration::from_secs(3_600),
        )
        .await
    })
    .await;
    assert!(matches!(
        outcome,
        Some(DirectorLoopOutcome::PausedAtGate {
            gate: crate::gates::Gate::DocsConfirm
        })
    ));

    // 2. User approves → resume on a FRESH session. The docs gate must NOT
    //    re-fire; the frontend step settles → preview_confirm pause.
    let mut sess2 = FakeSession::new(vec![], false, "");
    let outcome2 = boxed(gates_hosted_interaction(), async {
        drive_director_loop_resume(&mut sess2, &o, &events, &route).await
    })
    .await;
    assert!(
        matches!(
            outcome2,
            Some(DirectorLoopOutcome::PausedAtGate {
                gate: crate::gates::Gate::PreviewConfirm
            })
        ),
        "the resumed run pauses next at preview_confirm (docs never re-fires): {outcome2:?}"
    );
    assert_eq!(
        rec.count(|e| matches!(
            e,
            EngineEvent::GateOpened {
                gate: crate::gates::Gate::DocsConfirm,
                ..
            }
        )),
        1,
        "docs_confirm opened exactly ONCE across the whole run"
    );
    let state = crate::state::read_workflow_state(tmp.path()).expect("state");
    assert_eq!(state.active_gate, "preview_confirm");

    // 3. User approves the preview → resume completes the backend step; the
    //    schedule finishes with a Done outcome and no further gate.
    let mut sess3 = FakeSession::new(vec![], true, r#"{"accepts":true}"#);
    let outcome3 = boxed(gates_hosted_interaction(), async {
        drive_director_loop_resume(&mut sess3, &o, &events, &route).await
    })
    .await;
    assert!(
        matches!(outcome3, Some(DirectorLoopOutcome::Done { .. })),
        "the final resume completes the plan: {outcome3:?}"
    );
    let loaded = plan_state::load(tmp.path()).expect("plan persisted");
    assert!(
        loaded.steps.iter().all(|s| s.status == StepStatus::Done),
        "every step settled Done across the gated round-trip"
    );
    // Phase-monotonicity core assertion (same invariant the phase tests lock):
    // the finished build's persisted phase is at least `backend`.
    let rank = |id: &str| {
        umadev_spec::PHASE_CHAIN
            .iter()
            .position(|p| p.id() == id)
            .unwrap()
    };
    let final_state = crate::state::read_workflow_state(tmp.path()).expect("state");
    assert!(
        rank(&final_state.phase) >= rank("backend"),
        "the completed gated run's phase never regressed: {}",
        final_state.phase
    );
    assert!(
        final_state.active_gate.is_empty(),
        "no stale open door after the run completes"
    );
}

#[tokio::test]
async fn auto_mode_and_unhosted_runs_never_pause_at_a_gate() {
    // Hosted Auto and unhosted Guarded both drive through without gates and finish every step.
    for (mode, hosted) in [(TrustMode::Auto, true), (TrustMode::Guarded, false)] {
        let tmp = tempfile::TempDir::new().unwrap();
        seed_source(tmp.path());
        let (events, rec) = sink();
        let mut sess = FakeSession::new(vec![], true, r#"{"accepts": true, "blocking": []}"#);
        let mut o = opts(tmp.path());
        o.mode = mode;
        o.requirement = "做一个完整的产品".to_string();
        let route = build_route();
        let mut plan = gated_plan();

        let drive = drive_plan_steps(
            &mut sess,
            &o,
            &events,
            &route,
            &mut plan,
            IdleBudget::new(Duration::from_millis(200), Duration::from_millis(200)),
            std::time::Instant::now() + Duration::from_secs(3_600),
        );
        let outcome = if hosted {
            boxed(gates_hosted_interaction(), drive).await
        } else {
            drive.await
        };
        assert!(
            matches!(outcome, Some(DirectorLoopOutcome::Done { .. })),
            "mode={mode:?} hosted={hosted}: drives through with no pause: {outcome:?}"
        );
        assert_eq!(
            rec.count(|e| matches!(e, EngineEvent::GateOpened { .. })),
            0,
            "mode={mode:?} hosted={hosted}: no gate event on the drive-through path"
        );
        assert!(
            plan.steps.iter().all(|s| s.status == StepStatus::Done),
            "every step completed in one schedule"
        );
    }
}

#[tokio::test]
async fn terminal_docs_only_plan_opens_a_resumable_confirmation_gate() {
    // A docs-only plan still owes the user the documented confirmation door.
    // The fully-Done plan is retained as the resume cursor, so `/continue` can
    // close the door and run final document-aware QC without replaying the docs.
    use crate::critics::Seat;
    use crate::plan_state::{AcceptanceSpec, Plan, PlanStep, StepKind, StepStatus};
    let tmp = tempfile::TempDir::new().unwrap();
    seed_source(tmp.path());
    let (events, rec) = sink();
    let mut sess = FakeSession::new(vec![], true, r#"{"accepts": true, "blocking": []}"#);
    let mut o = opts(tmp.path());
    o.mode = TrustMode::Guarded;
    o.requirement = "写两份文档".to_string();
    let route = build_route();
    let mk = |id: &str, seat: Seat, deps: &[&str]| PlanStep {
        files: test_step_files(id),
        id: id.into(),
        title: format!("write the {id} doc"),
        seat,
        kind: StepKind::Build,
        depends_on: deps.iter().map(|d| (*d).to_string()).collect(),
        acceptance: AcceptanceSpec::SourcePresent,
        evidence: Vec::new(),
        status: StepStatus::Pending,
    };
    let mut plan = Plan {
        steps: vec![
            mk("pm", Seat::ProductManager, &[]),
            mk("arch", Seat::Architect, &["pm"]),
        ],
        risks: vec![],
        open_questions: vec![],
    };

    let outcome = boxed(gates_hosted_interaction(), async {
        drive_plan_steps(
            &mut sess,
            &o,
            &events,
            &route,
            &mut plan,
            IdleBudget::new(Duration::from_millis(200), Duration::from_millis(200)),
            std::time::Instant::now() + Duration::from_secs(3_600),
        )
        .await
    })
    .await;
    assert_eq!(
        outcome,
        Some(DirectorLoopOutcome::PausedAtGate {
            gate: crate::gates::Gate::DocsConfirm,
        })
    );
    assert_eq!(
        rec.count(|e| matches!(e, EngineEvent::GateOpened { .. })),
        1,
        "the docs confirmation is visible even when docs are the final deliverable"
    );
    let loaded = load_resumable_plan(tmp.path()).expect("the terminal docs gate is resumable");
    assert!(loaded
        .steps
        .iter()
        .all(|step| step.status == StepStatus::Done));
    let state = crate::state::read_workflow_state(tmp.path()).expect("workflow state");
    assert_eq!(state.active_gate, "docs_confirm");
}

#[tokio::test]
async fn design_tokens_step_never_triggers_or_holds_the_docs_gate() {
    // The designer's design-tokens deliverable step is UIUX-SEATED but is
    // code-phase prep, not doc authoring: (1) a still-pending tokens step must
    // NOT hold the docs gate closed once the actual docs are Done, and (2) the
    // tokens step settling Done AFTER the resume must NOT re-fire docs_confirm
    // (gate-opens-once).
    use crate::critics::Seat;
    use crate::plan_state::{AcceptanceSpec, Plan, PlanStep, StepKind, StepStatus};
    let tmp = tempfile::TempDir::new().unwrap();
    let mut o = opts(tmp.path());
    o.mode = TrustMode::Guarded;
    let mk = |id: &str, seat: Seat, acceptance: AcceptanceSpec, status: StepStatus| PlanStep {
        files: plan_state::StepFiles::default(),
        id: id.into(),
        title: format!("do {id}"),
        seat,
        kind: StepKind::Build,
        depends_on: vec![],
        acceptance,
        evidence: Vec::new(),
        status,
    };
    crate::interaction::hosted(gates_hosted_interaction(), async {
        // Docs all Done, tokens still Pending, settled step = the UIUX doc: the
        // gate opens NOW — the pending tokens step (excluded from the family)
        // does not hold it.
        let uiux = mk(
            "uiux",
            Seat::UiuxDesigner,
            AcceptanceSpec::SourcePresent,
            StepStatus::Done,
        );
        let plan = Plan {
            steps: vec![
                mk(
                    "pm",
                    Seat::ProductManager,
                    AcceptanceSpec::SourcePresent,
                    StepStatus::Done,
                ),
                mk(
                    "arch",
                    Seat::Architect,
                    AcceptanceSpec::SourcePresent,
                    StepStatus::Done,
                ),
                uiux.clone(),
                mk(
                    "tokens",
                    Seat::UiuxDesigner,
                    AcceptanceSpec::DesignTokensPresent,
                    StepStatus::Pending,
                ),
                mk(
                    "fe",
                    Seat::FrontendEngineer,
                    AcceptanceSpec::SourcePresent,
                    StepStatus::Pending,
                ),
            ],
            risks: vec![],
            open_questions: vec![],
        };
        assert_eq!(
            confirm_gate_after_step(&uiux, StepStatus::Done, &plan, &o),
            Some(crate::gates::Gate::DocsConfirm),
            "a pending tokens step does not hold the docs gate closed"
        );
        // Post-resume: the tokens step settles Done — it must NOT re-fire the
        // docs gate (it is not a doc-family member OR trigger).
        let tokens_done = mk(
            "tokens",
            Seat::UiuxDesigner,
            AcceptanceSpec::DesignTokensPresent,
            StepStatus::Done,
        );
        let mut plan2 = plan.clone();
        plan2.mark("tokens", StepStatus::Done);
        assert_eq!(
            confirm_gate_after_step(&tokens_done, StepStatus::Done, &plan2, &o),
            None,
            "a design-tokens step settling post-resume never re-fires docs_confirm"
        );
    })
    .await;
}

#[tokio::test]
async fn docs_gate_fires_before_code_phase_prep_and_opens_exactly_once() {
    // End-to-end over drive_plan_steps + resume with the NEW skeleton shape: a
    // plan carrying the QA test-authoring + designer tokens prep steps between
    // the docs and the code. The docs gate must open when the DOCS complete
    // (prep still pending — it is code-phase work that runs AFTER the user
    // confirms the docs), the prep steps complete after the resume WITHOUT
    // re-firing docs_confirm, and the frontend then pauses at preview_confirm.
    use crate::critics::Seat;
    use crate::plan_state::{AcceptanceSpec, Plan, PlanStep, StepKind, StepStatus};
    let tmp = tempfile::TempDir::new().unwrap();
    seed_source(tmp.path());
    // The prep steps' real evidence: authored tests on disk + a real tokens file.
    std::fs::create_dir_all(tmp.path().join("tests")).unwrap();
    std::fs::write(tmp.path().join("tests").join("probe.test.ts"), "test();").unwrap();
    std::fs::write(
        tmp.path().join("design-tokens.json"),
        "{ \"color\": { \"primary\": \"#0f62fe\" } }",
    )
    .unwrap();
    let (events, rec) = sink();
    let mut o = opts(tmp.path());
    o.mode = TrustMode::Guarded;
    o.requirement = "做一个完整的产品".to_string();
    let route = build_route();
    let mk = |id: &str, seat: Seat, acceptance: AcceptanceSpec, deps: &[&str]| PlanStep {
        files: test_step_files(id),
        id: id.into(),
        title: format!("do the {id} work"),
        seat,
        kind: StepKind::Build,
        depends_on: deps.iter().map(|d| (*d).to_string()).collect(),
        acceptance,
        evidence: Vec::new(),
        status: StepStatus::Pending,
    };
    let mut qa = mk(
        "qa-tests",
        Seat::QaEngineer,
        AcceptanceSpec::SourcePresent,
        &["arch"],
    );
    qa.evidence = vec![crate::plan_state::EvidenceContract::FileExists {
        path: "tests".to_string(),
    }];
    let mut plan = Plan {
        steps: vec![
            mk(
                "pm",
                Seat::ProductManager,
                AcceptanceSpec::SourcePresent,
                &[],
            ),
            mk(
                "arch",
                Seat::Architect,
                AcceptanceSpec::SourcePresent,
                &["pm"],
            ),
            qa,
            mk(
                "tokens",
                Seat::UiuxDesigner,
                AcceptanceSpec::DesignTokensPresent,
                &["arch"],
            ),
            mk(
                "fe",
                Seat::FrontendEngineer,
                AcceptanceSpec::SourcePresent,
                &["qa-tests", "tokens"],
            ),
            mk(
                "be",
                Seat::BackendEngineer,
                AcceptanceSpec::SourcePresent,
                &["qa-tests"],
            ),
        ],
        risks: vec![],
        open_questions: vec![],
    };

    // 1. Fresh drive → pauses at docs_confirm as soon as pm + arch settle (the
    //    prep steps are still Pending — they are NOT doc-family members).
    let mut sess = FakeSession::new(vec![], false, "");
    let outcome = boxed(gates_hosted_interaction(), async {
        drive_plan_steps(
            &mut sess,
            &o,
            &events,
            &route,
            &mut plan,
            IdleBudget::new(Duration::from_millis(200), Duration::from_millis(200)),
            std::time::Instant::now() + Duration::from_secs(3_600),
        )
        .await
    })
    .await;
    assert!(
        matches!(
            outcome,
            Some(DirectorLoopOutcome::PausedAtGate {
                gate: crate::gates::Gate::DocsConfirm
            })
        ),
        "docs gate opens when the DOCS complete, before the prep steps: {outcome:?}"
    );
    let loaded = plan_state::load(tmp.path()).expect("plan persisted at the pause");
    let status_of = |p: &Plan, id: &str| p.steps.iter().find(|s| s.id == id).unwrap().status;
    assert_eq!(
        status_of(&loaded, "qa-tests"),
        StepStatus::Pending,
        "test-authoring is code-phase prep — it runs AFTER the docs gate resumes"
    );
    assert_eq!(status_of(&loaded, "tokens"), StepStatus::Pending);

    // 2. Approve → resume: the prep steps complete (no docs re-fire), the
    //    frontend settles → preview_confirm.
    let mut sess2 = FakeSession::new(vec![], false, "");
    let outcome2 = boxed(gates_hosted_interaction(), async {
        drive_director_loop_resume(&mut sess2, &o, &events, &route).await
    })
    .await;
    assert!(
        matches!(
            outcome2,
            Some(DirectorLoopOutcome::PausedAtGate {
                gate: crate::gates::Gate::PreviewConfirm
            })
        ),
        "after the prep steps the frontend pauses at preview_confirm: {outcome2:?}"
    );
    assert_eq!(
        rec.count(|e| matches!(
            e,
            EngineEvent::GateOpened {
                gate: crate::gates::Gate::DocsConfirm,
                ..
            }
        )),
        1,
        "docs_confirm opened exactly ONCE — the prep steps completing never re-fired it"
    );

    // 3. Approve the preview → every plan step completes, but the scripted
    // fixture's intentionally-minimal design tokens still fail the final whole-
    // build QC. That residual gate evidence must win over all-Done step statuses.
    let mut sess3 = FakeSession::new(vec![], false, "");
    let outcome3 = boxed(gates_hosted_interaction(), async {
        drive_director_loop_resume(&mut sess3, &o, &events, &route).await
    })
    .await;
    let Some(DirectorLoopOutcome::Failed(reason)) = &outcome3 else {
        panic!("dirty final gate must not become Done: {outcome3:?}");
    };
    assert!(reason.contains("design-system"), "{reason}");
    let done = plan_state::load(tmp.path()).expect("plan persisted");
    assert!(
        done.steps.iter().all(|s| s.status == StepStatus::Done),
        "every step (docs, prep, code) settled Done: {:?}",
        done.steps
            .iter()
            .map(|s| (s.id.clone(), s.status))
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn queued_steer_folds_into_the_next_build_step_directive() {
    // A2#4/#5: directives queued in the hosted steering intake are drained at
    // the next STEP BOUNDARY and folded into the doer's instruction — steering
    // applies mid-run instead of evaporating. Auto tier so no gate pause
    // interleaves; the intake is consumed exactly once.
    let tmp = tempfile::TempDir::new().unwrap();
    seed_source(tmp.path());
    let (events, rec) = sink();
    let mut sess = FakeSession::new(vec![], true, r#"{"accepts": true, "blocking": []}"#);
    let sent = sess.sent_handle();
    let mut o = opts(tmp.path());
    o.requirement = "做一个完整的产品".to_string();
    let route = build_route();
    let mut plan = gated_plan();

    let steer: crate::interaction::SteerIntake = Arc::new(std::sync::Mutex::new(vec![
        "Plan steering: SKIP step `be` — do not perform it.".to_string(),
    ]));
    let interaction = crate::interaction::RunInteraction {
        steer: Some(Arc::clone(&steer)),
        approval: None,
        host_request: None,
        confirm_gates: false,
    };
    let outcome = boxed(interaction, async {
        drive_plan_steps(
            &mut sess,
            &o,
            &events,
            &route,
            &mut plan,
            IdleBudget::new(Duration::from_millis(200), Duration::from_millis(200)),
            std::time::Instant::now() + Duration::from_secs(3_600),
        )
        .await
    })
    .await;
    assert!(matches!(outcome, Some(DirectorLoopOutcome::Done { .. })));
    // The FIRST doer directive carries the folded steering block…
    let sent = sent.lock().unwrap();
    let steered: Vec<&String> = sent
        .iter()
        .filter(|d| d.contains("## User steering"))
        .collect();
    assert!(
        steered
            .first()
            .is_some_and(|d| d.contains("SKIP step `be`")),
        "the queued directive folded into a step directive: {sent:?}"
    );
    // …exactly once (the intake drains on consumption, never re-applied).
    assert_eq!(steered.len(), 1, "steering applied at ONE step boundary");
    assert!(
        steer.lock().unwrap().is_empty(),
        "the intake drained on consumption"
    );
    // The fold was surfaced to the user (the i18n note).
    assert!(
        rec.count(|e| matches!(e, EngineEvent::Note(n) if n.contains("1"))) >= 1,
        "a fold note was emitted"
    );
}

#[tokio::test]
async fn resolve_approval_headless_keeps_the_floor_and_hosted_asks_the_user() {
    // A2#3: the interactive approval bridge. Headless (no scope): the floor's
    // escalation degrades to DENY exactly as today. Hosted: the callback's
    // verdict decides — an Allow also records the reversible class in the
    // project trust ledger (like the chat drain), a Deny denies.
    let tmp = tempfile::TempDir::new().unwrap();
    let mut o = opts(tmp.path());
    o.mode = TrustMode::Guarded;
    let (events, _rec) = sink();

    // An out-of-tree write escalates under Guarded (root-aware policy).
    let action = "write";
    let target = "/etc/hosts-not-ours";

    // Headless: escalate → DENY, flagged headless (the caller emits its note).
    let headless = resolve_approval(&o, &events, action, target).await;
    assert!(matches!(headless.decision, ApprovalDecision::Deny));
    assert!(headless.headless);

    // Hosted + user APPROVES → Allow, interactive.
    let approve: crate::interaction::ApprovalFn =
        Arc::new(|_a, _t| Box::pin(async { true }) as crate::interaction::ApprovalFuture);
    let hosted_allow = crate::interaction::hosted(
        crate::interaction::RunInteraction {
            steer: None,
            approval: Some(approve),
            host_request: None,
            confirm_gates: false,
        },
        resolve_approval(&o, &events, action, target),
    )
    .await;
    assert!(matches!(hosted_allow.decision, ApprovalDecision::Allow));
    assert!(!hosted_allow.headless);
    // The approved reversible class was remembered (not re-asked next time):
    // the SAME action now passes the floor without any callback.
    let after = resolve_approval(&o, &events, action, target).await;
    assert!(
        matches!(after.decision, ApprovalDecision::Allow),
        "the remembered class skips the re-ask"
    );

    // Hosted + user DENIES an (unremembered) escalation → Deny, interactive.
    let deny: crate::interaction::ApprovalFn =
        Arc::new(|_a, _t| Box::pin(async { false }) as crate::interaction::ApprovalFuture);
    let hosted_deny = crate::interaction::hosted(
        crate::interaction::RunInteraction {
            steer: None,
            approval: Some(deny),
            host_request: None,
            confirm_gates: false,
        },
        resolve_approval(&o, &events, "bash", "git push --force origin main"),
    )
    .await;
    assert!(matches!(hosted_deny.decision, ApprovalDecision::Deny));
    assert!(!hosted_deny.headless);
}

#[tokio::test]
async fn resolve_approval_auto_frees_installs_but_still_asks_on_disasters() {
    // The narrowed AUTO floor on the director path: a dependency install is
    // ordinary dev work — allowed WITHOUT consulting the user (the callback
    // must never fire; the reported "npm install 待批准 in auto" nag). A true
    // disaster (`rm -rf`) still escalates and — hosted — asks the live user.
    let tmp = tempfile::TempDir::new().unwrap();
    let mut o = opts(tmp.path());
    o.mode = TrustMode::Auto;
    let (events, _rec) = sink();

    let consulted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let consulted_probe = Arc::clone(&consulted);
    let never: crate::interaction::ApprovalFn = Arc::new(move |_a, _t| {
        consulted_probe.store(true, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async { false }) as crate::interaction::ApprovalFuture
    });
    let freed = crate::interaction::hosted(
        crate::interaction::RunInteraction {
            steer: None,
            approval: Some(never),
            host_request: None,
            confirm_gates: false,
        },
        resolve_approval(&o, &events, "bash", "npm install"),
    )
    .await;
    assert!(
        matches!(freed.decision, ApprovalDecision::Allow),
        "auto allows the ordinary inbound network install without a prompt"
    );
    assert!(
        !consulted.load(std::sync::atomic::Ordering::SeqCst),
        "auto must not consult the user for npm install"
    );

    // A destructive disaster still surfaces the interactive prompt in Auto,
    // and the user's verdict decides.
    let approve: crate::interaction::ApprovalFn =
        Arc::new(|_a, _t| Box::pin(async { true }) as crate::interaction::ApprovalFuture);
    let asked = crate::interaction::hosted(
        crate::interaction::RunInteraction {
            steer: None,
            approval: Some(approve),
            host_request: None,
            confirm_gates: false,
        },
        resolve_approval(&o, &events, "bash", "rm -rf node_modules && rm -rf dist"),
    )
    .await;
    assert!(matches!(asked.decision, ApprovalDecision::Allow));
    assert!(
        !asked.headless,
        "the residual auto escalation must ask the live user, not headless-decide"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// RED→GREEN evidence contract — "a test that never failed proves nothing"
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn red_green_verdict_passes_a_test_that_was_red_before_the_step() {
    use crate::verify::NamedTestOutcome as T;
    // The whole contract in one line: the named test FAILED at the pre-state and
    // (the caller has already established) PASSES at head. That is a real test.
    assert!(matches!(
        red_half_verdict("login_rejects_bad_password", T::Failed),
        Some(EvidenceOutcome::Pass)
    ));
}

#[test]
fn red_green_verdict_fails_the_step_when_the_test_already_passed_at_baseline() {
    // THE POINT OF THE WHOLE MECHANISM. A test that was ALREADY GREEN before the
    // step's work existed cannot be asserting that work — it was written after (or
    // around) the code, to match what the code already did. It has never once
    // demonstrated that it can detect the behaviour's absence. `TestPasses` is
    // satisfied by exactly this test; the red→green contract rejects it, by name,
    // with the diagnosis stated plainly.
    use crate::verify::NamedTestOutcome as T;
    let Some(EvidenceOutcome::Gap(msg)) = red_half_verdict("adds_two_numbers", T::Passed) else {
        panic!("a test that was already green at the pre-state MUST reject the step");
    };
    assert!(msg.contains("ALREADY PASSED"), "{msg}");
    assert!(msg.contains("adds_two_numbers"), "{msg}");
}

#[test]
fn red_green_verdict_is_inconclusive_when_the_pre_state_test_cannot_be_run() {
    // FAIL-OPEN: a rewound tree we could not run the test in (a toolchain that
    // needs dependencies the pre-state lacked, a timeout) yields NO verdict — the
    // caller degrades to the ordinary named-test bar. We never block a step on our
    // own inability to verify.
    use crate::verify::NamedTestOutcome as T;
    assert!(
        red_half_verdict("t", T::Unavailable).is_none(),
        "an unrunnable pre-state is inconclusive, never a finding"
    );
}

#[test]
fn the_red_half_memo_never_caches_a_non_verdict() {
    // N3. The memo may only hold IMMUTABLE FACTS ABOUT THE PAST: at commit `pre`,
    // that test did (Passed) or did not (Failed) succeed. `Unavailable` is not such a
    // fact — it is a fact about our TOOLING at one moment (a timeout, a transient
    // runner failure). Caching it froze one flake into the answer for
    // `(root, pre, test)` for the entire process: every later fix round of that step
    // skipped the rewind, read the cached non-verdict, and fell open to the plain
    // `TestPasses` bar — permanently downgrading the red→green contract because of a
    // single transient miss.
    use crate::verify::NamedTestOutcome as T;
    let root = std::path::Path::new("/tmp/umadev-red-half-memo-test");
    let pre = "cafe1234";
    let test = "n3_regression_probe";

    // A transient non-verdict is NOT remembered — the next round asks again.
    red_half_remember(root, pre, test, T::Unavailable);
    assert!(
        red_half_cached(root, pre, test).is_none(),
        "a non-verdict must never be memoized: it would downgrade the contract for the \
         whole process on the strength of one flake"
    );

    // …and the round that DOES get an answer records it, so the (immutable) past is
    // not re-derived by a second rewind.
    red_half_remember(root, pre, test, T::Failed);
    assert_eq!(
        red_half_cached(root, pre, test),
        Some(T::Failed),
        "a real observation about the past IS sound to memoize"
    );

    // A later `Unavailable` must not clobber the fact we already established.
    red_half_remember(root, pre, test, T::Unavailable);
    assert_eq!(
        red_half_cached(root, pre, test),
        Some(T::Failed),
        "a transient miss cannot erase an established fact"
    );
}

#[tokio::test]
async fn red_green_falls_open_to_test_passes_when_no_runner_or_rewind_exists() {
    // END-TO-END FAIL-OPEN, through the real contract path. The workspace has the
    // named test in its source but NO recognised project manifest (so no test
    // runner) and NO pre-state checkpoint. Both halves are unaskable → the step is
    // held to exactly the bar we CAN check (the test exists), never blocked on a
    // question we could not ask.
    use crate::plan_state::EvidenceContract as E;
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("src/app.test.js"),
        "test('login_rejects_bad_password', () => { expect(1).toBe(1); });",
    )
    .unwrap();
    let o = opts(tmp.path());
    let (events, _rec) = sink();
    let mut sess = FakeSession::new(vec![], false, "");
    let route = build_route();
    let step = evidence_step(vec![E::TestFailsThenPasses {
        test: "login_rejects_bad_password".into(),
    }]);

    let v = verify_step_acceptance(
        &mut sess, &o, &events, &route, &step,
        None, // no pre-state checkpoint → the red half is unaskable
    )
    .await;
    assert!(
        v.accepted,
        "an unverifiable red→green degrades to the TestPasses bar: {:?}",
        v.evidence
    );
}

#[tokio::test]
async fn red_green_still_rejects_a_test_that_is_not_in_the_codebase_at_all() {
    // The green half is toolchain-INDEPENDENT: a test that appears nowhere in the
    // source cannot pass, no matter what a runner would say (and this is what stops
    // a "filter matched nothing, exit 0" runner from faking a green).
    use crate::plan_state::EvidenceContract as E;
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(tmp.path().join("src/app.js"), "export const a = 1;\n").unwrap();
    let o = opts(tmp.path());
    let (events, _rec) = sink();
    let mut sess = FakeSession::new(vec![], false, "");
    let route = build_route();
    let step = evidence_step(vec![E::TestFailsThenPasses {
        test: "a_test_nobody_ever_wrote".into(),
    }]);

    let v = verify_step_acceptance(&mut sess, &o, &events, &route, &step, None).await;
    assert!(
        !v.accepted,
        "an absent test cannot satisfy a red→green claim"
    );
    assert!(
        v.evidence_line().contains("a_test_nobody_ever_wrote"),
        "the gap names the missing test: {:?}",
        v.evidence
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// SCOPE CREEP — "which change belongs to no step?" (the dual of coverage)
// ═══════════════════════════════════════════════════════════════════════════

/// A workspace with a run baseline + a persisted plan whose one code step claims
/// `claim` as its file surface. `None` when `git` is unavailable (fail-open env).
fn scoped_workspace(claim: &[&str]) -> Option<tempfile::TempDir> {
    if !std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        return None;
    }
    let tmp = tempfile::TempDir::new().unwrap();
    seed_source(tmp.path());
    crate::checkpoint::create_run_baseline(tmp.path(), "demo")?;
    let plan = plan_state::Plan {
        steps: vec![plan_state::PlanStep {
            files: crate::plan_state::StepFiles {
                create: claim.iter().map(|c| (*c).to_string()).collect(),
                modify: vec![],
            },
            id: "impl".into(),
            title: "build the planned thing".into(),
            seat: crate::critics::Seat::BackendEngineer,
            kind: plan_state::StepKind::Build,
            depends_on: vec![],
            acceptance: plan_state::AcceptanceSpec::SourcePresent,
            evidence: vec![],
            status: plan_state::StepStatus::Pending,
        }],
        risks: vec![],
        open_questions: vec![],
    };
    plan_state::save(&plan, tmp.path()).unwrap();
    Some(tmp)
}

#[test]
fn acceptance_floor_blocks_an_unclaimed_new_source_file() {
    // OVER-BUILDING is the dual of the coverage gap above. The plan said it would
    // write `planned.ts`; the run ALSO wrote `rogue.ts` — a new place for logic to
    // live that nobody sized, nobody asked for, and no reviewer looked at. It
    // blocks, on the same deterministic floor as an uncovered requirement.
    let Some(tmp) = scoped_workspace(&["planned.ts"]) else {
        return;
    };
    std::fs::write(tmp.path().join("planned.ts"), "export const p = 1;\n").unwrap();
    std::fs::write(tmp.path().join("rogue.ts"), "export const r = 1;\n").unwrap();

    let o = opts(tmp.path());
    let route = build_route();
    let blocking = acceptance_floor_blocking(&o, Some(&route));
    assert!(
        blocking
            .iter()
            .any(|b| b.starts_with("scope:") && b.contains("rogue.ts")),
        "an unclaimed new source file blocks: {blocking:?}"
    );
    assert!(
        !blocking.iter().any(|b| b.contains("planned.ts")),
        "the CLAIMED file is in scope and never a finding: {blocking:?}"
    );
}

#[test]
fn acceptance_floor_rejects_a_mutating_plan_with_no_declared_surface() {
    // A writer plan without a path denominator cannot enforce scope. Absence is
    // therefore an incomplete execution contract, not permission to mutate any
    // file in the workspace.
    let Some(tmp) = scoped_workspace(&[]) else {
        return;
    };
    std::fs::write(tmp.path().join("rogue.ts"), "export const r = 1;\n").unwrap();
    let o = opts(tmp.path());
    let route = build_route();
    let blocking = acceptance_floor_blocking(&o, Some(&route));
    assert!(
        blocking.iter().any(|b| {
            b.starts_with("scope:")
                && b.contains("execution contract incomplete")
                && b.contains("files.create/files.modify")
        }),
        "an absent mutating surface is an explicit contract failure: {blocking:?}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// EVIDENCE FRESHNESS — "a proof taken before the last change is not a proof"
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn a_stale_runtime_proof_is_not_read_by_the_floor_in_either_direction() {
    // A runtime proof describes ONE state of the source. Once the code moves, the
    // proof is about a tree that no longer exists — so the floor must not read it,
    // in EITHER direction: a stale FAILURE must not block code that has since been
    // fixed, and (the dangerous one) a stale PASS must never be mistaken for
    // evidence about the code we are shipping.
    let tmp = tempfile::TempDir::new().unwrap();
    seed_source(tmp.path());
    let audit = tmp.path().join(".umadev").join("audit");
    std::fs::create_dir_all(&audit).unwrap();

    let write_failed_proof = |fingerprint: Option<String>| {
        let proof = crate::runtime_proof::RuntimeProof {
            timestamp: "2026-07-01T00:00:00Z".to_string(),
            status: crate::runtime_proof::RuntimeStatus::NotVerified(
                "server did not become ready within 60s".to_string(),
            ),
            dev_server: Some("Vite dev server".to_string()),
            command: Some("npm run dev".to_string()),
            base_url: Some("http://localhost:5173".to_string()),
            ready_ms: None,
            routes: Vec::new(),
            e2e: None,
            source_fingerprint: fingerprint,
        };
        std::fs::write(
            audit.join("runtime-proof.json"),
            serde_json::to_string(&proof).unwrap(),
        )
        .unwrap();
    };

    // FRESH failure (stamped with the tree as it stands) → a real, blocking fact.
    let now = crate::freshness::workspace_fingerprint(tmp.path())
        .expect("a test tree is fingerprintable");
    write_failed_proof(Some(now));
    assert!(
        runtime_proof_blocking(tmp.path()).is_some(),
        "a proof that describes THIS code is read"
    );

    // The code CHANGES. The same recorded failure now describes a tree we are not
    // shipping — it is no longer evidence about anything, so the floor drops it and
    // the check has to be re-run for real.
    std::fs::write(tmp.path().join("fixed.ts"), "export const fixed = true;\n").unwrap();
    assert!(
        runtime_proof_blocking(tmp.path()).is_none(),
        "evidence produced before the last change to the code it describes is not evidence"
    );

    // FAIL-OPEN: an UNSTAMPED proof has no fingerprint to contradict — an unknown is
    // never a finding, so an older artifact behaves exactly as it always did.
    write_failed_proof(None);
    assert!(
        runtime_proof_blocking(tmp.path()).is_some(),
        "an unstamped proof is read as before — we never block on our own blindness"
    );
}
