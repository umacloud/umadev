use super::*;

fn outcome_bodies(project_root: &std::path::Path) -> Vec<String> {
    let dir = project_root
        .join(crate::lessons::RAW_DIR)
        .join(crate::knowledge_feedback::RECEIPTS_DIR);
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            let path = entry.path();
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            name.ends_with(".outcome.json") || name.ends_with(".settled-outcome.json")
        })
        .map(|entry| std::fs::read_to_string(entry.path()).unwrap())
        .collect()
}

#[test]
fn sent_memory_outcome_requires_concrete_mechanical_evidence() {
    let positive = StepVerdict {
        accepted: true,
        has_positive_evidence: true,
        mechanical_build_test_passed_steps: vec!["test".into()],
        mechanical_build_test_failed_steps: Vec::new(),
        evidence: Vec::new(),
        operational_unavailable: Vec::new(),
        raw_log: None,
    };
    assert_eq!(
        memory_outcome_for_step_verdict(&positive),
        TurnOutcome::Pass
    );
    assert_eq!(
        memory_outcome_for_step_verdict(&StepVerdict {
            has_positive_evidence: false,
            ..positive
        }),
        TurnOutcome::Unknown
    );

    let failure = StepVerdict {
        accepted: false,
        has_positive_evidence: false,
        mechanical_build_test_passed_steps: Vec::new(),
        mechanical_build_test_failed_steps: vec!["test".into()],
        evidence: vec!["test: FAILED".into()],
        operational_unavailable: Vec::new(),
        raw_log: None,
    };
    assert_eq!(memory_outcome_for_step_verdict(&failure), TurnOutcome::Fail);
    assert_eq!(
        memory_outcome_for_step_verdict(&StepVerdict {
            evidence: Vec::new(),
            mechanical_build_test_failed_steps: Vec::new(),
            ..failure
        }),
        TurnOutcome::Unknown
    );
}

#[tokio::test]
async fn serial_doer_sent_memory_is_settled_by_that_rounds_mechanical_pass() {
    use crate::plan_state::EvidenceContract as E;

    let isolated_home = crate::test_support::NoBundledCorpus::new();
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join("knowledge/frontend")).unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("knowledge/frontend/delivery.md"),
        "# Deliver the frontend thing\nShip a real source file and verify it exists.",
    )
    .unwrap();
    let mut options = opts(tmp.path());
    options.requirement = "deliver the frontend thing as a real source file".to_string();
    let step = evidence_step(vec![E::FileExists {
        path: "src/App.tsx".into(),
    }]);
    let (events, _rec) = sink();
    let mut session = FakeSession::new(vec![text_turn("Created src/App.tsx. Done.")], false, "")
        .with_main_send_write(
            tmp.path().join("src/App.tsx"),
            "export const App = () => null;",
        );
    let outcome = drive_build_step(
        &mut session,
        &options,
        &events,
        &build_route(),
        &step,
        "",
        0,
        std::time::Instant::now() + Duration::from_secs(3_600),
        &mut std::collections::HashSet::new(),
    )
    .await;
    assert!(outcome.accepted);
    let outcomes = outcome_bodies(tmp.path());
    assert_eq!(outcomes.len(), 1);
    assert!(outcomes[0].contains(r#""outcome":"pass""#));
    drop(isolated_home);
}

#[tokio::test]
async fn post_build_qc_commits_exact_memory_and_next_blocking_qc_fails_it() {
    let isolated_home = crate::test_support::NoBundledCorpus::new();
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join("knowledge/frontend")).unwrap();
    std::fs::write(
        tmp.path().join("knowledge/frontend/accessibility.md"),
        "# Accessible icons\nUse a semantic, labelled icon component instead of emoji controls.",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("App.tsx"),
        "export const Btn = () => <button>🚀 Launch</button>;",
    )
    .unwrap();
    let (events, _rec) = sink();
    let mut session = FakeSession::new(
        vec![text_turn("Attempted the accessible icon fix. Done.")],
        true,
        r#"{"accepts": true, "blocking": []}"#,
    );
    let sent = session.sent_handle();
    let mut options = opts(tmp.path());
    options.requirement = "fix the inaccessible emoji icon in the landing page".to_string();
    let _ = run_post_build_qc(
        &mut session,
        &options,
        &events,
        &chat_build_route(),
        "Built it. Done.",
    )
    .await;

    let sent = sent.lock().unwrap();
    assert_eq!(sent.len(), MAX_QC_ROUNDS - 1);
    assert!(sent.iter().all(|directive| directive
        .lines()
        .any(|line| line.trim().starts_with("<!-- umadev-memory:km1-"))));
    drop(sent);
    let outcomes = outcome_bodies(tmp.path());
    assert_eq!(outcomes.len(), MAX_QC_ROUNDS - 1);
    assert!(outcomes
        .iter()
        .all(|outcome| outcome.contains(r#""outcome":"fail""#)));
    drop(isolated_home);
}

#[tokio::test]
async fn post_build_qc_next_clean_qc_passes_the_sent_memory() {
    let isolated_home = crate::test_support::NoBundledCorpus::new();
    let tmp = tempfile::TempDir::new().unwrap();
    let app = tmp.path().join("App.tsx");
    std::fs::write(&app, "export const Btn = () => <button>🚀 Launch</button>;").unwrap();
    let memory = umadev_knowledge::MemoryRef::from_parts(
        "frontend/accessibility.md",
        "Accessible icons",
        "Use labelled semantic icons.",
    );
    let context = KnowledgeDigest {
        text: format!(
            "{}\n- `frontend/accessibility.md` — Accessible icons: Use labelled semantic icons.",
            crate::knowledge_feedback::sent_memory_marker(&memory.id)
        ),
        memories: vec![memory],
    };
    let (events, _rec) = sink();
    let mut session = FakeSession::new(
        vec![text_turn("Replaced the emoji control. Done.")],
        true,
        r#"{"accepts": true, "blocking": []}"#,
    )
    .with_main_send_write(&app, "export const value = 1;");
    let outcome = run_final_gate(
        &mut session,
        &opts(tmp.path()),
        &events,
        &chat_build_route(),
        "Built it. Done.",
        std::time::Instant::now() + Duration::from_secs(3_600),
        &context,
        false,
        false,
        false,
    )
    .await;
    assert!(outcome.clean);
    let outcomes = outcome_bodies(tmp.path());
    assert_eq!(outcomes.len(), 1);
    assert!(outcomes[0].contains(r#""outcome":"pass""#));
    drop(isolated_home);
}

#[tokio::test]
async fn sent_memory_turn_that_dies_before_next_qc_settles_unknown() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (events, _rec) = sink();
    let memory = umadev_knowledge::MemoryRef::from_parts(
        "frontend/accessibility.md",
        "Icons",
        "Use labelled semantic icons.",
    );
    let directive = format!(
        "{}\nApply the recalled icon guidance.",
        crate::knowledge_feedback::sent_memory_marker(&memory.id)
    );
    let mut session = FakeSession::new(
        vec![vec![SessionEvent::TextDelta("partial fix".into())]],
        false,
        "",
    );
    let result = drive_one_turn_with_memories(
        &mut session,
        &opts(tmp.path()),
        &events,
        directive,
        vec![memory],
        IdleBudget::new(Duration::from_millis(200), Duration::from_millis(200)),
        std::time::Instant::now() + Duration::from_secs(3_600),
    )
    .await;
    assert!(result.is_err());
    let outcomes = outcome_bodies(tmp.path());
    assert_eq!(outcomes.len(), 1);
    assert!(outcomes[0].contains(r#""outcome":"unknown""#));
}
