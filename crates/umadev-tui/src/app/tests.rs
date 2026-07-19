use super::*;
use crate::config::UserConfig;
use umadev_agent::config::CodexSandbox;
use umadev_runtime::Usage;

fn sample_curated_lesson(
    title: &str,
    status: umadev_agent::CuratedLessonStatus,
) -> umadev_agent::CuratedLessonEntry {
    umadev_agent::CuratedLessonEntry {
            title: title.to_string(),
            rule: "Read the first actionable diagnostic, reproduce it minimally, then verify the documented API before changing code.".to_string(),
            root_cause: "The implementation guessed an API from a generic failure instead of grounding the fix in a reproducible diagnostic.".to_string(),
            evidence_count: 2,
            status,
            source_kind: "pitfall".to_string(),
            source_signatures: vec!["rust/error/e0425".to_string()],
            first_observed_at: "2026-07-14T01:02:03Z".to_string(),
            last_observed_at: Some("2026-07-15T02:03:04Z".to_string()),
            last_verified_at: Some("2026-07-15T03:04:05Z".to_string()),
            timeline_complete: true,
        }
}

fn sample_pitfalls_report() -> umadev_agent::LessonsReport {
    let observation = umadev_agent::PitfallObservation {
        observed_at: "2026-07-15T04:05:06Z".to_string(),
        episode_id: "episode-abcdefghijklmnopqrstuvwxyz-123456".to_string(),
        evidence_hash: "abcdef0123456789abcdef0123456789".to_string(),
        ..umadev_agent::PitfallObservation::default()
    };
    let mut report = umadev_agent::LessonsReport::default();
    report.efficacy.total = 1;
    report.efficacy.recurring = 1;
    report.efficacy.quarantined_records = 1;
    report.efficacy.quarantined_hits = 226;
    report.efficacy.unclassified_candidates = 1;
    report.efficacy.unclassified_candidate_hits = 2;
    report.incidents.push(umadev_agent::PitfallEntry {
        title: "MODULE_NOT_FOUND".to_string(),
        signature: "dependency/module-not-found/lodash".to_string(),
        hits: 2,
        status: umadev_agent::PitfallStatus::Recurring,
        fix: "Install the declared dependency and rerun the original verifier.".to_string(),
        root_cause: "The imported package was absent from the resolved dependency graph."
            .to_string(),
        context: vec!["node".to_string(), "typescript".to_string()],
        failed_fixes: vec!["Clearing an unrelated cache".to_string()],
        first_observed_at: "2026-07-14T01:02:03Z".to_string(),
        last_observed_at: Some("2026-07-15T04:05:06Z".to_string()),
        last_recurred_at: Some("2026-07-15T04:05:06Z".to_string()),
        last_verified_at: Some("2026-07-15T05:06:07Z".to_string()),
        recent_evidence_count: 1,
        timeline_complete: true,
        recent_observations: vec![observation.clone()],
    });
    report
        .unclassified_candidates
        .push(umadev_agent::UnclassifiedCandidateEntry {
            fingerprint: "general/candidate/0123456789abcdef".to_string(),
            hits: 2,
            first_observed_at: "2026-07-13T00:00:00Z".to_string(),
            last_observed_at: Some("2026-07-15T04:05:06Z".to_string()),
            recent_evidence_count: 1,
            timeline_complete: false,
            recent_observations: vec![observation],
        });
    report.curated_lessons.push(sample_curated_lesson(
        "CURATED_RULE_MUST_NOT_RENDER",
        umadev_agent::CuratedLessonStatus::Validated,
    ));
    report
}

#[test]
fn pitfalls_tui_formatter_localizes_all_chrome_and_lists_bounded_timestamps() {
    let report = sample_pitfalls_report();
    let cases = [
        (
            umadev_i18n::Lang::ZhCn,
            [
                "踩坑事故台账",
                "首次观察",
                "近期逐次观察",
                "待分类候选",
                "隔离区",
            ],
        ),
        (
            umadev_i18n::Lang::ZhTw,
            [
                "踩坑事故臺帳",
                "首次觀察",
                "近期逐次觀察",
                "待分類候選",
                "隔離區",
            ],
        ),
        (
            umadev_i18n::Lang::En,
            [
                "Concrete pitfall incident ledger",
                "first observed",
                "recent observations",
                "Unclassified candidates",
                "Quarantine",
            ],
        ),
    ];
    for (lang, expected_chrome) in cases {
        let body = format_pitfalls_report(lang, &report);
        for expected in expected_chrome {
            assert!(body.contains(expected), "missing {expected:?}:\n{body}");
        }
        for timestamp in [
            "2026-07-14T01:02:03Z",
            "2026-07-15T04:05:06Z",
            "2026-07-15T05:06:07Z",
        ] {
            assert!(body.contains(timestamp), "missing {timestamp}:\n{body}");
        }
        assert!(body.contains("episode-…123456"), "{body}");
        assert!(body.contains("abcdef012345…"), "{body}");
        assert!(!body.contains("episode-abcdefghijklmnopqrstuvwxyz-123456"));
        assert!(!body.contains("abcdef0123456789abcdef0123456789"));
        assert!(!body.contains("CURATED_RULE_MUST_NOT_RENDER"));
        assert!(
            body.lines()
                .all(|line| lesson_display_width(line) <= LESSONS_LINE_WIDTH),
            "formatter emitted a row wider than 80 cells:\n{body}"
        );
    }
}

#[test]
fn pitfalls_tui_formatter_has_a_localized_true_empty_state() {
    let report = umadev_agent::LessonsReport::default();
    for (lang, expected) in [
        (umadev_i18n::Lang::ZhCn, "还没有记录具体踩坑事故"),
        (umadev_i18n::Lang::ZhTw, "還沒有記錄具體踩坑事故"),
        (
            umadev_i18n::Lang::En,
            "No concrete pitfall incident has been recorded yet",
        ),
    ] {
        let body = format_pitfalls_report(lang, &report);
        assert!(body.contains(expected), "missing {expected:?}:\n{body}");
    }
}

fn legacy_top_pitfall_marker() -> umadev_agent::PitfallEntry {
    umadev_agent::PitfallEntry {
        title: "TOP_PITFALL_MUST_NOT_RENDER".to_string(),
        signature: "test/top-pitfall".to_string(),
        hits: 99,
        status: umadev_agent::PitfallStatus::Recurring,
        fix: "legacy duplicate".to_string(),
        root_cause: "legacy duplicate".to_string(),
        context: Vec::new(),
        failed_fixes: Vec::new(),
        first_observed_at: "2026-07-14T01:02:03Z".to_string(),
        last_observed_at: None,
        last_recurred_at: None,
        last_verified_at: None,
        recent_evidence_count: 0,
        timeline_complete: false,
        recent_observations: Vec::new(),
    }
}

#[test]
fn lessons_tui_formatter_has_a_true_empty_state() {
    let body = format_lessons_report(
        umadev_i18n::Lang::ZhCn,
        &umadev_agent::LessonsReport::default(),
    );
    assert!(body.contains("还没有提炼出可复用的经验规则"));
    assert!(body.contains("/pitfalls"));
    assert!(!body.contains("已有 0 个具体事故"));
}

#[test]
fn lessons_tui_formatter_explains_incidents_not_yet_distilled() {
    let mut report = umadev_agent::LessonsReport::default();
    report.efficacy.total = 2;
    report.efficacy.active = 2;
    let body = format_lessons_report(umadev_i18n::Lang::ZhCn, &report);
    assert!(body.contains("已有 2 个具体事故"));
    assert!(body.contains("尚未形成可复用规则"));
    assert!(body.contains("/pitfalls"));
}

#[test]
fn lessons_tui_formatter_explains_repeated_unclassified_candidates() {
    let mut report = umadev_agent::LessonsReport::default();
    report.efficacy.unclassified_candidates = 1;
    report.efficacy.unclassified_candidate_hits = 2;
    let body = format_lessons_report(umadev_i18n::Lang::ZhCn, &report);
    assert!(body.contains("1 个待分类候选"));
    assert!(body.contains("2 次独立失败回合"));
    assert!(body.contains("不会伪造修法"));
    assert!(body.contains("/pitfalls"));
    assert!(!body.contains("已有 0 个具体事故"));
}

#[test]
fn lessons_tui_shows_two_precise_episodes_as_a_corroborated_rule() {
    let tmp = tempfile::tempdir().unwrap();
    let error = "Error: Cannot find module 'lodash'".to_string();
    for _ in 0..2 {
        let _ = umadev_agent::capture_dev_errors_detailed(
            tmp.path(),
            std::slice::from_ref(&error),
            "demo",
            "requirement",
        );
    }
    let report = umadev_agent::lessons_report(tmp.path());
    assert_eq!(report.curated_lessons.len(), 1);
    assert_eq!(
        report.curated_lessons[0].status,
        umadev_agent::CuratedLessonStatus::Corroborated
    );
    let body = format_lessons_report(umadev_i18n::Lang::ZhCn, &report);
    assert!(body.contains("已印证"));
    assert!(body.contains("规避复发踩坑:"));
    assert!(!body.contains("Avoid recurring pitfall"));
    assert!(body.contains("lodash"));
    assert!(body.contains('2'));
}

#[test]
fn lessons_tui_formatter_renders_only_curated_rules_with_auditable_fields() {
    let mut report = umadev_agent::LessonsReport {
        curated_lessons: vec![
            sample_curated_lesson("PENDING_RULE", umadev_agent::CuratedLessonStatus::Pending),
            sample_curated_lesson(
                "VALIDATED_RULE",
                umadev_agent::CuratedLessonStatus::Validated,
            ),
            sample_curated_lesson(
                "REVISION_RULE",
                umadev_agent::CuratedLessonStatus::NeedsRevision,
            ),
        ],
        ..Default::default()
    };
    report.curated_lessons[2].timeline_complete = false;
    report.curated_lessons[2].last_observed_at = None;
    report.top_pitfalls.push(legacy_top_pitfall_marker());
    report
        .validated_patterns
        .push(umadev_agent::ValidatedEntry {
            title: "LEGACY_PATTERN_MUST_NOT_RENDER".to_string(),
            summary: "duplicate".to_string(),
        });

    let body = format_lessons_report(umadev_i18n::Lang::ZhCn, &report);
    for expected in [
        "PENDING_RULE",
        "VALIDATED_RULE",
        "REVISION_RULE",
        "假设",
        "已验证",
        "已失效",
        "规则:",
        "根因:",
        "2 条",
        "踩坑事故",
        "rust/error/e0425",
        "首次观察(UTC):",
        "最近观察(UTC):",
        "最近验证(UTC):",
        "旧数据记录时间",
        "旧数据无逐次时间",
    ] {
        assert!(body.contains(expected), "missing {expected:?}:\n{body}");
    }
    assert!(!body.contains("TOP_PITFALL_MUST_NOT_RENDER"));
    assert!(!body.contains("LEGACY_PATTERN_MUST_NOT_RENDER"));
    assert!(
        body.lines()
            .all(|line| lesson_display_width(line) <= LESSONS_LINE_WIDTH),
        "formatter emitted a row wider than 80 cells:\n{body}"
    );
}

#[test]
fn lessons_tui_formatter_does_not_hide_rules_after_twelve() {
    let report = umadev_agent::LessonsReport {
        curated_lessons: (0..14)
            .map(|index| {
                sample_curated_lesson(
                    &format!("RULE_{index}"),
                    umadev_agent::CuratedLessonStatus::Pending,
                )
            })
            .collect(),
        ..Default::default()
    };
    let body = format_lessons_report(umadev_i18n::Lang::En, &report);
    assert!(
        body.contains("RULE_13"),
        "the 14th rule was hidden:\n{body}"
    );
}

#[test]
fn session_lost_note_detected_across_locales_and_forms() {
    // The Windows broken pipe (os error 232, either locale), a `--resume` that found no
    // conversation, and a base-exited note are all SESSION LOST: the stored session id is a
    // corpse, so record_route_failed invalidates it (next turn opens fresh + replays the
    // transcript) instead of re-resuming the corpse forever.
    for note in [
        "session send: 管道正在被关闭 (os error 232)",
        "session send: The pipe is being closed. (os error 232)",
        "base session ended mid-turn - base stderr: No conversation found with session ID x",
        "base session ended before send (base exited: exit status: 1)",
        "session send: Broken pipe (os error 32)",
    ] {
        assert!(
            App::note_indicates_session_lost(note),
            "should be session-lost: {note}"
        );
    }
    // A content/tool failure on a still-LIVE session must NOT drop the session.
    for note in [
        "本轮底座执行出错:模型返回了空回复",
        "the build step failed: cargo test exited non-zero",
    ] {
        assert!(
            !App::note_indicates_session_lost(note),
            "should NOT be session-lost: {note}"
        );
    }
}

#[test]
fn codex_sandbox_warning_only_for_danger_full_access_on_codex() {
    // Fires ONLY for the high-risk tier on the codex base.
    assert!(should_warn_codex_sandbox(
        Some("codex"),
        CodexSandbox::DangerFullAccess
    ));
    // Safe tiers stay silent, even on codex.
    assert!(!should_warn_codex_sandbox(
        Some("codex"),
        CodexSandbox::WorkspaceWrite
    ));
    assert!(!should_warn_codex_sandbox(
        Some("codex"),
        CodexSandbox::ReadOnly
    ));
    // Other bases never warn, even at the high-risk tier (the knob is codex's).
    assert!(!should_warn_codex_sandbox(
        Some("claude-code"),
        CodexSandbox::DangerFullAccess
    ));
    assert!(!should_warn_codex_sandbox(
        None,
        CodexSandbox::DangerFullAccess
    ));
}

fn fresh_app(backend: Option<&str>) -> App {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::SeqCst);
    let cfg = UserConfig {
        backend: backend.map(str::to_string),
        // Pin zh-CN so language-sensitive UI assertions (gate cards etc.)
        // are deterministic regardless of the test host's locale.
        lang: Some("zh-CN".to_string()),
        ..Default::default()
    };
    // Each test gets a unique workspace dir to avoid file races between
    // parallel tests. The .umadevrc disables auto_approve_gates so
    // gate-card tests see the manual-approval path. Remove any leftover dir
    // from a PRIOR run first so a persisted `.umadev/chat/` (Wave 5) can't
    // bleed into a test that expects a clean conversation buffer.
    let workspace = std::env::temp_dir().join(format!("sd-test-ws-{id}"));
    let _ = std::fs::remove_dir_all(&workspace);
    let _ = std::fs::create_dir_all(&workspace);
    let _ = std::fs::write(
        workspace.join(".umadevrc"),
        "[pipeline]\nauto_approve_gates = false\n",
    );
    let mut app = App::new(
        "demo",
        cfg,
        std::env::temp_dir().join(format!("sd-test-cfg-{id}.toml")),
        workspace,
    );
    // P5d: force animations ON in tests so spinner-cadence assertions are
    // deterministic regardless of whether the test host's stdout is a TTY
    // (where `animations_enabled_default` would otherwise pick `false`).
    app.animations = true;
    app
}

fn queue_snapshot(texts: &[&str]) -> umadev_runtime::PromptQueueSnapshot {
    umadev_runtime::PromptQueueSnapshot {
        session_id: "queue-ui".to_string(),
        entries: texts
            .iter()
            .enumerate()
            .map(|(position, text)| umadev_runtime::PromptQueueEntry {
                id: format!("p{position}"),
                version: u64::try_from(position).unwrap_or(0) + 5,
                owner: Some("umadev".to_string()),
                last_editor: None,
                kind: "prompt".to_string(),
                text: (*text).to_string(),
                position,
            })
            .collect(),
        running_prompt_id: Some("running".to_string()),
    }
}

#[test]
fn grok_queue_enter_tail_ctrl_enter_send_now_and_empty_enter_promotes_top() {
    let mut app = fresh_app(Some("grok-build"));
    app.thinking = true;
    app.agentic_in_flight = true;
    app.prompt_queue.set_ready(true);
    app.prompt_queue
        .apply_snapshot(queue_snapshot(&["older queued"]));

    app.input = "normal follow-up".to_string();
    app.input_cursor = app.input_len();
    assert!(matches!(
        app.apply_key(KeyCode::Enter),
        Action::PromptQueueEnqueue {
            placement: PromptQueuePlacement::Tail,
            ..
        }
    ));

    app.input = "urgent follow-up".to_string();
    app.input_cursor = app.input_len();
    assert!(matches!(
        app.apply_key_with_mods(KeyCode::Enter, crossterm::event::KeyModifiers::CONTROL,),
        Action::PromptQueueEnqueue {
            placement: PromptQueuePlacement::SendNow,
            ..
        }
    ));

    app.input = "urgent follow-up via ctrl-i".to_string();
    app.input_cursor = app.input_len();
    assert!(matches!(
        app.apply_key_with_mods(KeyCode::Char('i'), crossterm::event::KeyModifiers::CONTROL,),
        Action::PromptQueueEnqueue {
            placement: PromptQueuePlacement::SendNow,
            ..
        }
    ));

    assert!(matches!(
        app.apply_key(KeyCode::Enter),
        Action::PromptQueueMutate(PromptQueueMutation::Interject {
            ref id,
            expected_version: 5,
            new_text: None,
        }) if id == "p0"
    ));
}

#[test]
fn queue_pane_keys_select_and_request_without_optimistic_changes() {
    let mut app = fresh_app(Some("grok-build"));
    app.prompt_queue.set_ready(true);
    app.prompt_queue
        .apply_snapshot(queue_snapshot(&["one", "two", "three", "four"]));
    let ctrl = crossterm::event::KeyModifiers::CONTROL;
    assert_eq!(
        app.apply_key_with_mods(KeyCode::Char(';'), ctrl),
        Action::None
    );
    assert!(app.prompt_queue.is_open());
    let _ = app.apply_key(KeyCode::Char('j'));
    assert_eq!(app.prompt_queue.selected_id(), Some("p1"));
    let before: Vec<_> = app
        .prompt_queue
        .entries()
        .iter()
        .map(|entry| entry.id.clone())
        .collect();
    assert!(matches!(
        app.apply_key(KeyCode::Char('x')),
        Action::PromptQueueMutate(PromptQueueMutation::Remove {
            ref id,
            expected_version: 6,
        }) if id == "p1"
    ));
    assert_eq!(
        app.prompt_queue
            .entries()
            .iter()
            .map(|entry| entry.id.clone())
            .collect::<Vec<_>>(),
        before,
        "delete is not visible until the next base snapshot"
    );
    assert_eq!(app.prompt_queue.visible_entries().len(), 3);
}

fn session_model(model_id: &str, total_context_tokens: Option<u64>) -> SessionModelInfo {
    SessionModelInfo {
        model_id: model_id.to_string(),
        name: model_id.to_string(),
        description: None,
        total_context_tokens,
        agent_type: None,
        supports_reasoning_effort: false,
        reasoning_effort: None,
        reasoning_efforts: Vec::new(),
    }
}

fn session_command(name: &str) -> SessionCommandInfo {
    SessionCommandInfo {
        name: name.to_string(),
        description: format!("{name} description"),
        input_hint: None,
        scope: None,
        source_path: None,
    }
}

// ---- A2#5: typed approval replies + sticky pending-approval state --------

#[test]
fn classify_approval_reply_exact_tokens_only() {
    for t in [
        "y", "Y", "yes", "批准", " 同意 ", "允许", "允許", "approve", "APPROVE", "ok", "通过",
        "确认", "可以",
    ] {
        assert_eq!(classify_approval_reply(t), Some(true), "{t}");
    }
    for t in [
        "n",
        "no",
        "拒绝",
        "拒絕",
        "不批准",
        "不同意",
        "deny",
        "Reject",
        "取消",
        "cancel",
        "skip",
    ] {
        assert_eq!(classify_approval_reply(t), Some(false), "{t}");
    }
    // NOT decisions: empty, decision words inside longer messages, plain
    // steering text — these fall through to the normal queued lanes.
    for t in [
        "",
        "批准这个改动吧",
        "please approve the plan",
        "先跑一下测试",
        "not ok",
    ] {
        assert_eq!(classify_approval_reply(t), None, "{t}");
    }
}

#[test]
fn typed_approval_reply_resolves_pause_instead_of_queueing() {
    let mut app = fresh_app(Some("claude-code"));
    // A chat turn is in flight and the drain paused on an approval.
    app.thinking = true;
    assert!(app.set_pending_approval(Some(("Bash".into(), "npm install".into()))));
    // Same snapshot again → unchanged (no redraw churn).
    assert!(!app.set_pending_approval(Some(("Bash".into(), "npm install".into()))));
    // The reported trap: 「批准」 used to queue as a normal message with no
    // effect (every key silently consumed, no approval entry point). Now it
    // resolves the pause as ALLOW — and must NOT also park on a queue.
    assert_eq!(
        app.submit_text("批准".to_string()),
        Action::ApprovalReply(true)
    );
    assert!(app.pending_approval.is_none());
    assert!(
        app.queued_chat.is_empty(),
        "the decision must not ALSO queue as a chat turn"
    );
    // A deny word denies.
    let _ = app.set_pending_approval(Some(("Write".into(), ".claude/skills/x.md".into())));
    assert_eq!(
        app.submit_text("拒绝".to_string()),
        Action::ApprovalReply(false)
    );
    // A NON-decision message mid-pause keeps the pause and parks on the
    // normal queued-chat lane, exactly as before.
    let _ = app.set_pending_approval(Some(("Bash".into(), "npm install".into())));
    assert_eq!(
        app.submit_text("先解释一下为什么要装这个依赖".to_string()),
        Action::None
    );
    assert_eq!(app.queued_chat.len(), 1);
    assert!(
        app.pending_approval.is_some(),
        "a steering message must keep the pause registered"
    );
    // The pause resolving (holder emptied) clears the mirrored state.
    assert!(app.set_pending_approval(None));
    assert!(app.pending_approval.is_none());
}

// ---- Context-usage gauge + proactive compaction nudge --------------------

#[test]
fn base_model_engine_event_updates_display_not_context_window() {
    // The base reports its resolved model via `EngineEvent::BaseModel`, so
    // the UI records the real model for DISPLAY only (see `model_meta_text`).
    // It must NOT infer a context window from that id: a hardcoded model
    // table drifts and a base may route to a third-party/local model, so
    // only an EXACT base-config window is ever a denominator.
    let mut app = fresh_app(Some("claude-code"));
    app.session_usage.apply(Some(Usage::exact(100_000, 0)));
    // Pin "user pinned nothing" deterministically — `fresh_app` would otherwise
    // inherit the dev host's ambient `~/.claude/settings.json` model.
    app.base_model = None;
    assert_eq!(app.context_window_tokens(), None);
    // A dashed real Sonnet id updates the display model but proves no window.
    app.apply_engine(EngineEvent::BaseModel {
        id: "claude-sonnet-4-5-20250929[1m]".to_string(),
    });
    assert_eq!(
        app.base_model.as_deref(),
        Some("claude-sonnet-4-5-20250929[1m]")
    );
    assert_eq!(app.context_window_tokens(), None);
    // Fail-open: an empty id is ignored, keeping the last good display model.
    app.apply_engine(EngineEvent::BaseModel { id: String::new() });
    assert_eq!(
        app.base_model.as_deref(),
        Some("claude-sonnet-4-5-20250929[1m]")
    );
    // ONLY an exact base-config window unlocks the denominator.
    app.base_context_window = Some(200_000);
    assert_eq!(app.context_window_tokens(), Some(200_000));
}

#[test]
fn base_session_state_replaces_catalogs_without_transcript_rows() {
    let mut app = fresh_app(Some("grok-build"));
    let history_len = app.history.len();

    app.apply_engine(EngineEvent::BaseSessionState {
        backend_id: "grok-build".to_string(),
        update: SessionStateUpdate::ModelCatalogReplaced {
            current_model_id: "grok-code-fast-1".to_string(),
            available_models: vec![
                session_model("grok-code-fast-1", Some(131_072)),
                session_model("grok-code-next", Some(262_144)),
            ],
        },
    });
    assert_eq!(app.base_session_models.len(), 2);
    assert_eq!(app.base_session_model.as_deref(), Some("grok-code-fast-1"));
    assert_eq!(app.base_model.as_deref(), Some("grok-code-fast-1"));
    assert!(app.base_model_live);
    assert_eq!(app.base_context_window, Some(131_072));

    app.apply_engine(EngineEvent::BaseSessionState {
        backend_id: "grok-build".to_string(),
        update: SessionStateUpdate::ModelChanged {
            model_id: "grok-code-next".to_string(),
            reasoning_effort: None,
        },
    });
    assert_eq!(app.base_session_model.as_deref(), Some("grok-code-next"));
    assert_eq!(app.base_context_window, Some(262_144));

    app.apply_engine(EngineEvent::BaseSessionState {
        backend_id: "grok-build".to_string(),
        update: SessionStateUpdate::ModeChanged {
            mode: SessionMode::Plan,
        },
    });
    app.apply_engine(EngineEvent::BaseSessionState {
        backend_id: "grok-build".to_string(),
        update: SessionStateUpdate::ThinkingChanged {
            enabled: Some(true),
            can_enable: true,
            can_disable: true,
        },
    });
    app.apply_engine(EngineEvent::BaseSessionState {
        backend_id: "grok-build".to_string(),
        update: SessionStateUpdate::CommandCatalogReplaced {
            commands: vec![session_command("review")],
            tools: vec!["Bash".to_string(), "Read".to_string()],
        },
    });
    app.apply_engine(EngineEvent::BaseSessionState {
        backend_id: "grok-build".to_string(),
        update: SessionStateUpdate::PlanReplaced {
            entries: vec![umadev_runtime::SessionPlanEntry {
                content: "Implement source-audited bridge".to_string(),
                priority: umadev_runtime::SessionPlanEntryPriority::Medium,
                status: umadev_runtime::SessionPlanEntryStatus::InProgress,
            }],
        },
    });
    assert_eq!(app.base_session_mode, Some(SessionMode::Plan));
    assert_eq!(app.base_session_thinking, Some(true));
    assert!(app.base_session_thinking_can_enable);
    assert!(app.base_session_thinking_can_disable);
    assert_eq!(app.base_session_commands, vec![session_command("review")]);
    assert_eq!(app.base_session_tools, ["Bash", "Read"]);
    assert_eq!(app.base_session_plan.len(), 1);

    app.apply_engine(EngineEvent::BaseSessionState {
        backend_id: "grok-build".to_string(),
        update: SessionStateUpdate::ModelCatalogReplaced {
            current_model_id: String::new(),
            available_models: Vec::new(),
        },
    });
    app.apply_engine(EngineEvent::BaseSessionState {
        backend_id: "grok-build".to_string(),
        update: SessionStateUpdate::CommandCatalogReplaced {
            commands: Vec::new(),
            tools: Vec::new(),
        },
    });
    app.apply_engine(EngineEvent::BaseSessionState {
        backend_id: "grok-build".to_string(),
        update: SessionStateUpdate::PlanReplaced {
            entries: Vec::new(),
        },
    });
    assert!(app.base_session_models.is_empty());
    assert!(app.base_session_model.is_none());
    assert!(app.base_session_commands.is_empty());
    assert!(app.base_session_tools.is_empty());
    assert!(app.base_session_plan.is_empty());
    assert_eq!(
        app.history.len(),
        history_len,
        "dynamic base state belongs in App state, not the transcript"
    );
}

#[test]
fn base_session_state_ignores_stale_backend_and_clears_on_boundaries() {
    let mut app = fresh_app(Some("grok-build"));
    app.apply_engine(EngineEvent::BaseSessionState {
        backend_id: "codex".to_string(),
        update: SessionStateUpdate::ModelCatalogReplaced {
            current_model_id: "stale".to_string(),
            available_models: vec![session_model("stale", Some(1))],
        },
    });
    assert!(app.base_session_models.is_empty());
    assert!(app.base_session_model.is_none());

    let populate = |app: &mut App| {
        app.apply_engine(EngineEvent::BaseSessionState {
            backend_id: app.backend_label.clone(),
            update: SessionStateUpdate::ModelCatalogReplaced {
                current_model_id: "live".to_string(),
                available_models: vec![session_model("live", Some(65_536))],
            },
        });
        app.apply_engine(EngineEvent::BaseSessionState {
            backend_id: app.backend_label.clone(),
            update: SessionStateUpdate::ModeChanged {
                mode: SessionMode::Ask,
            },
        });
        app.apply_engine(EngineEvent::BaseSessionState {
            backend_id: app.backend_label.clone(),
            update: SessionStateUpdate::ThinkingChanged {
                enabled: Some(false),
                can_enable: true,
                can_disable: true,
            },
        });
        app.apply_engine(EngineEvent::BaseSessionState {
            backend_id: app.backend_label.clone(),
            update: SessionStateUpdate::CommandCatalogReplaced {
                commands: vec![session_command("live")],
                tools: vec!["Read".to_string()],
            },
        });
        app.apply_engine(EngineEvent::BaseSessionState {
            backend_id: app.backend_label.clone(),
            update: SessionStateUpdate::PlanReplaced {
                entries: vec![umadev_runtime::SessionPlanEntry {
                    content: "live".to_string(),
                    priority: umadev_runtime::SessionPlanEntryPriority::Medium,
                    status: umadev_runtime::SessionPlanEntryStatus::Pending,
                }],
            },
        });
    };
    let assert_cleared = |app: &App| {
        assert!(app.base_session_models.is_empty());
        assert!(app.base_session_model.is_none());
        assert!(app.base_session_mode.is_none());
        assert!(app.base_session_thinking.is_none());
        assert!(!app.base_session_thinking_can_enable);
        assert!(!app.base_session_thinking_can_disable);
        assert!(app.base_session_commands.is_empty());
        assert!(app.base_session_tools.is_empty());
        assert!(app.base_session_plan.is_empty());
        assert!(!app.base_model_live);
    };

    populate(&mut app);
    app.reset_base_session_state();
    assert_cleared(&app);

    populate(&mut app);
    assert_eq!(app.try_slash_command("/clear"), Some(Action::None));
    assert_cleared(&app);

    populate(&mut app);
    app.commit_backend(Some("codex".to_string()));
    assert_eq!(app.backend.as_deref(), Some("codex"));
    assert_cleared(&app);
}

#[test]
fn native_command_precedence_and_explicit_base_escape_are_unambiguous() {
    let mut product = fresh_app(Some("grok-build"));
    product.base_session_commands = vec![session_command("compact")];
    for index in 0..12 {
        product.record_user_turn(&format!("user message {index}"));
        product.record_agentic_done(format!("assistant reply {index}"), false, None, None);
    }
    assert!(matches!(
        product.try_slash_command("/compact"),
        Some(Action::Compact)
    ));

    let mut escaped = fresh_app(Some("grok-build"));
    assert_eq!(
        escaped.try_slash_command("/base   /compact --focus src  "),
        Some(Action::NativeCommand("/compact --focus src  ".to_string())),
        "the wrapper strips only its separator and preserves the native payload"
    );
}

#[test]
fn advertised_native_command_preserves_editor_bytes_and_palette_metadata() {
    let mut app = fresh_app(Some("grok-build"));
    let mut review = session_command("review");
    review.description = "Review the current patch".to_string();
    review.input_hint = Some("[focus]".to_string());
    app.base_session_commands = vec![review, session_command("compact")];

    app.input = "/".to_string();
    app.input_cursor = app.input_len();
    let all_matches = app.palette_matches();
    assert_eq!(
        all_matches
            .iter()
            .filter(|entry| entry.verb == "compact")
            .count(),
        1,
        "the product's /compact row wins the catalog collision"
    );
    drop(all_matches);

    app.input = "/review".to_string();
    app.input_cursor = app.input_len();
    let matches = app.palette_matches();
    let row = matches
        .iter()
        .find(|entry| entry.verb == "review")
        .expect("the advertised non-conflicting command is in the palette");
    assert_eq!(row.desc, "Review the current patch");
    assert_eq!(row.arg_hint, Some("[focus]"));
    drop(matches);

    app.input = "/review --focus src  ".to_string();
    app.input_cursor = app.input_len();
    assert_eq!(
        app.apply_key(KeyCode::Enter),
        Action::NativeCommand("/review --focus src  ".to_string()),
        "submit must not trim trailing or internal command whitespace"
    );
}

#[test]
fn busy_native_commands_remain_typed_in_the_resident_fifo() {
    let mut app = fresh_app(Some("grok-build"));
    app.base_session_commands = vec![session_command("review")];
    app.thinking = true;

    assert_eq!(
        app.try_slash_command("/review --focus src  "),
        Some(Action::None)
    );
    assert_eq!(
        app.submit_text("do this after the review".to_string()),
        Action::None
    );
    assert_eq!(
        app.take_next_queued_dispatch(),
        Some(ResidentDispatch::NativeCommand(
            "/review --focus src  ".to_string()
        ))
    );
    assert_eq!(
        app.take_next_queued_dispatch(),
        Some(ResidentDispatch::RoutedChat(
            "do this after the review".to_string()
        ))
    );
}

#[test]
fn unadvertised_slash_command_stays_local_and_visible() {
    let mut app = fresh_app(Some("grok-build"));
    app.base_session_commands = vec![session_command("review")];

    assert_eq!(
        app.try_slash_command("/not-a-base-command"),
        Some(Action::None)
    );
    assert!(app.history.back().is_some_and(|message| {
        let body = message.body();
        body.contains("not-a-base-command")
            && (body.contains("未知") || body.to_ascii_lowercase().contains("unknown"))
    }));
    assert!(app.queued_chat.is_empty());
}

#[test]
fn context_usage_pct_is_bounded_and_saturating() {
    assert_eq!(context_usage_pct(0, 200_000), 0);
    assert_eq!(context_usage_pct(100_000, 200_000), 50);
    assert_eq!(context_usage_pct(160_000, 200_000), 80);
    // A conservative denominator can under-count → clamp at 100, never >100.
    assert_eq!(context_usage_pct(500_000, 200_000), 100);
    // total == 0 → 0, never a divide-by-zero.
    assert_eq!(context_usage_pct(1234, 0), 0);
}

#[test]
fn context_gauge_computes_pct_from_last_turn_input_tokens() {
    let mut app = fresh_app(Some("claude-code"));
    // Pin "no detected model/window" deterministically — `fresh_app` would
    // otherwise inherit the dev host's ambient `~/.claude/settings.json` model
    // (test isolation).
    app.base_model = None;
    // No usage and an empty transcript → nothing to show (fail-open).
    assert!(app.context_used_tokens().is_none());
    assert!(app.context_usage_pct().is_none());
    // A known last-turn input count alone is not enough: without an exact
    // configured context window, the UI must hide the context gauge.
    app.session_usage.apply(Some(Usage::exact(50_000, 0)));
    assert_eq!(app.context_used_tokens(), Some(50_000));
    assert_eq!(app.context_window_tokens(), None);
    assert_eq!(app.context_usage_pct(), None);
    // Exact provider/config metadata unlocks the denominator.
    app.base_context_window = Some(200_000);
    assert_eq!(app.context_window_tokens(), Some(200_000));
    assert_eq!(app.context_usage_pct(), Some(25));
}

#[test]
fn context_gauge_does_not_infer_window_from_a_config_pinned_model() {
    // A codex config pinning a gpt-5 model sets the DISPLAY model, but the gauge
    // must NOT fabricate a window from it — only an exact base-config window is a
    // denominator, so codex (which exposes none) shows the model name and no bar.
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join(".codex")).unwrap();
    std::fs::write(
        tmp.path().join(".codex/config.toml"),
        "model = \"gpt-5.5\"\n",
    )
    .unwrap();
    let mut app = App::new(
        "demo".to_string(),
        UserConfig {
            backend: Some("codex".into()),
            ..Default::default()
        },
        tmp.path().join("config.toml"),
        tmp.path().to_path_buf(),
    );
    app.session_usage.apply(Some(Usage::exact(100_000, 0)));
    assert_eq!(app.base_model.as_deref(), Some("gpt-5.5"));
    assert_eq!(app.context_window_tokens(), None);
    assert_eq!(app.context_usage_pct(), None);
}

#[test]
fn context_gauge_prefers_exact_opencode_provider_window() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(
        tmp.path().join("opencode.json"),
        r#"{
              "model": "provider-auth-big/glm-5",
              "provider": {
                "provider-auth-big": {
                  "models": {
                    "glm-5": { "limit": { "context": 200000 } }
                  }
                }
              }
            }"#,
    )
    .unwrap();
    let mut app = App::new(
        "demo".to_string(),
        UserConfig {
            backend: Some("opencode".into()),
            ..Default::default()
        },
        tmp.path().join("config.toml"),
        tmp.path().to_path_buf(),
    );
    app.session_usage.apply(Some(Usage::exact(100_000, 0)));
    assert_eq!(app.base_model.as_deref(), Some("provider-auth-big/glm-5"));
    assert_eq!(app.base_context_window, Some(200_000));
    assert_eq!(app.context_window_tokens(), Some(200_000));
    assert_eq!(app.context_usage_pct(), Some(50));
}

#[test]
fn live_base_model_recomputes_exact_opencode_context_or_clears_stale_window() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(
        tmp.path().join("opencode.json"),
        r#"{
              "model": "provider-auth-big/glm-5",
              "provider": {
                "provider-auth-big": {
                  "models": {
                    "glm-5": { "limit": { "context": 200000 } },
                    "glm-5-next": {
                      "limit": { "context": 300000 },
                      "variants": { "high": {} }
                    }
                  }
                }
              }
            }"#,
    )
    .unwrap();
    let mut app = App::new(
        "demo".to_string(),
        UserConfig {
            backend: Some("opencode".into()),
            ..Default::default()
        },
        tmp.path().join("config.toml"),
        tmp.path().to_path_buf(),
    );
    assert_eq!(app.base_context_window, Some(200_000));

    app.apply_engine(EngineEvent::BaseModel {
        id: "provider-auth-big/glm-5-next/high".to_string(),
    });
    assert_eq!(
        app.base_context_window,
        Some(300_000),
        "live model can keep the gauge only when provider metadata proves it"
    );

    app.apply_engine(EngineEvent::BaseModel {
        id: "provider-auth-big/unknown".to_string(),
    });
    assert_eq!(
        app.base_context_window, None,
        "a live model mismatch must clear the stale static denominator"
    );
}

#[test]
fn context_gauge_falls_open_when_model_unknown() {
    let mut app = fresh_app(None); // offline / no backend
    app.session_usage.apply(Some(Usage::exact(50_000, 0)));
    // Numerator exists but there's no denominator → gauge shows nothing.
    assert_eq!(app.context_used_tokens(), Some(50_000));
    assert!(app.context_window_tokens().is_none());
    assert!(app.context_usage_pct().is_none());
}

#[test]
fn compaction_nudge_fires_once_on_crossing_and_not_below() {
    let mut app = fresh_app(Some("claude-code"));
    // Pin an exact configured 200K window — the nudge must not use backend/model
    // defaults, but it should still work when the base exposes a real limit.
    app.base_model = None;
    app.base_context_window = Some(200_000);
    let before = app.history.len();
    // Below the 80% threshold (100k/200k = 50%) → no nudge.
    app.session_usage.apply(Some(Usage::exact(100_000, 0)));
    app.maybe_nudge_compaction();
    assert_eq!(app.history.len(), before);
    assert!(!app.context_nudge_shown);
    // Cross the threshold (170k/200k = 85%) → nudge exactly once.
    app.session_usage.apply(Some(Usage::exact(170_000, 0)));
    app.maybe_nudge_compaction();
    assert_eq!(app.history.len(), before + 1);
    assert!(app.context_nudge_shown);
    // Still above threshold next turn → no second nudge (no spam).
    app.session_usage.apply(Some(Usage::exact(180_000, 0)));
    app.maybe_nudge_compaction();
    assert_eq!(app.history.len(), before + 1);
    // Drop back below (e.g. after a /compact) → re-arm; a later crossing nudges again.
    app.session_usage.apply(Some(Usage::exact(90_000, 0)));
    app.maybe_nudge_compaction();
    assert!(!app.context_nudge_shown);
    app.session_usage.apply(Some(Usage::exact(175_000, 0)));
    app.maybe_nudge_compaction();
    assert_eq!(app.history.len(), before + 2);
}

// ---- I1: kill-ring + yank / yank-pop -------------------------------------

use crossterm::event::KeyModifiers;

/// Build the modifier set for a Ctrl-key.
fn ctrl() -> KeyModifiers {
    KeyModifiers::CONTROL
}
/// Build the modifier set for an Alt-key.
fn alt() -> KeyModifiers {
    KeyModifiers::ALT
}

#[test]
fn ctrl_u_pushes_to_kill_ring_and_ctrl_y_yanks_it_back() {
    let mut app = fresh_app(Some("offline"));
    app.input = "hello world".to_string();
    app.input_cursor = app.input_len();
    // Ctrl+U kills the line back to the start — but PUSHES it, not destroys.
    let _ = app.apply_key_with_mods(KeyCode::Char('u'), ctrl());
    assert_eq!(app.input, "", "Ctrl+U cleared the line");
    assert_eq!(
        app.kill_ring.front().map(String::as_str),
        Some("hello world"),
        "the killed text is on the ring, not lost"
    );
    // Ctrl+Y yanks the front entry back in.
    let _ = app.apply_key_with_mods(KeyCode::Char('y'), ctrl());
    assert_eq!(app.input, "hello world", "Ctrl+Y restored the killed text");
    assert_eq!(app.input_cursor, app.input_len());
}

#[test]
fn ctrl_k_and_ctrl_w_both_feed_the_ring() {
    // Ctrl+K (kill to end).
    let mut app = fresh_app(Some("offline"));
    app.input = "abcdef".to_string();
    app.input_cursor = 0;
    let _ = app.apply_key_with_mods(KeyCode::Char('k'), ctrl());
    assert_eq!(app.kill_ring.front().map(String::as_str), Some("abcdef"));
    // Ctrl+W (delete word back).
    let mut app = fresh_app(Some("offline"));
    app.input = "one two".to_string();
    app.input_cursor = app.input_len();
    let _ = app.apply_key_with_mods(KeyCode::Char('w'), ctrl());
    assert_eq!(app.kill_ring.front().map(String::as_str), Some("two"));
}

#[test]
fn consecutive_same_direction_kills_coalesce_into_one_entry() {
    // Two consecutive Ctrl+W (both BACKWARD) build ONE ring entry, the
    // newer-killed text PREPENDED so the chunk reads in document order.
    let mut app = fresh_app(Some("offline"));
    app.input = "one two three".to_string();
    app.input_cursor = app.input_len();
    let _ = app.apply_key_with_mods(KeyCode::Char('w'), ctrl());
    let _ = app.apply_key_with_mods(KeyCode::Char('w'), ctrl());
    assert_eq!(
        app.kill_ring.len(),
        1,
        "two same-direction kills are one ring entry"
    );
    assert_eq!(
        app.kill_ring.front().map(String::as_str),
        Some("two three"),
        "backward kills prepend so the chunk reads in order"
    );
}

#[test]
fn push_kill_coalesces_per_direction_and_forks_on_change() {
    let mut app = fresh_app(Some("offline"));
    // Forward kills APPEND into the front entry.
    app.push_kill("aa", KillDir::Forward);
    app.push_kill("bb", KillDir::Forward);
    assert_eq!(app.kill_ring.len(), 1);
    assert_eq!(app.kill_ring.front().map(String::as_str), Some("aabb"));
    // A direction change FORKS a new entry; backward kills PREPEND.
    app.push_kill("cc", KillDir::Backward);
    app.push_kill("dd", KillDir::Backward);
    assert_eq!(app.kill_ring.len(), 2);
    assert_eq!(app.kill_ring[0], "ddcc");
    assert_eq!(app.kill_ring[1], "aabb");
    // A non-kill key resets coalescing, so the next kill never folds in.
    app.reset_kill_yank();
    app.push_kill("ee", KillDir::Backward);
    assert_eq!(app.kill_ring.len(), 3);
    assert_eq!(app.kill_ring[0], "ee");
}

#[test]
fn alt_y_yank_pops_to_cycle_the_ring_after_a_yank() {
    let mut app = fresh_app(Some("offline"));
    app.input = String::new();
    app.input_cursor = 0;
    // Seed two distinct ring entries (front = most recent).
    app.kill_ring = VecDeque::from(["AAA".to_string(), "BBB".to_string()]);
    // Ctrl+Y yanks the front entry.
    let _ = app.apply_key_with_mods(KeyCode::Char('y'), ctrl());
    assert_eq!(app.input, "AAA");
    // Alt+Y replaces the just-yanked span with the next ring entry.
    let _ = app.apply_key_with_mods(KeyCode::Char('y'), alt());
    assert_eq!(app.input, "BBB", "Alt+Y cycled to the next ring entry");
    // Alt+Y wraps back around the 2-entry ring.
    let _ = app.apply_key_with_mods(KeyCode::Char('y'), alt());
    assert_eq!(app.input, "AAA");
}

#[test]
fn alt_y_is_inert_without_a_preceding_yank() {
    let mut app = fresh_app(Some("offline"));
    app.input = "draft".to_string();
    app.input_cursor = app.input_len();
    app.kill_ring = VecDeque::from(["AAA".to_string(), "BBB".to_string()]);
    // No yank happened first → yank-pop must be a no-op (no span recorded).
    let _ = app.apply_key_with_mods(KeyCode::Char('y'), alt());
    assert_eq!(app.input, "draft");
}

// ---- I2: undo / redo ------------------------------------------------------

#[test]
fn edit_then_undo_restores_text_and_cursor() {
    let mut app = fresh_app(Some("offline"));
    // A pre-existing draft (set directly → not itself a snapshot).
    app.input = "hello".to_string();
    app.input_cursor = app.input_len();
    // Type a char — the FIRST edit always opens a fresh undo step.
    let _ = app.apply_key(KeyCode::Char('!'));
    assert_eq!(app.input, "hello!");
    assert_eq!(app.input_cursor, 6);
    // Ctrl+Z restores both the text AND the caret.
    let _ = app.apply_key_with_mods(KeyCode::Char('z'), ctrl());
    assert_eq!(app.input, "hello");
    assert_eq!(app.input_cursor, 5);
}

#[test]
fn rapid_edits_coalesce_into_one_undo_step() {
    let mut app = fresh_app(Some("offline"));
    // Three keystrokes with no pause between them (the test runs in
    // microseconds, well inside the coalesce window).
    for c in ['a', 'b', 'c'] {
        let _ = app.apply_key(KeyCode::Char(c));
    }
    assert_eq!(app.input, "abc");
    assert_eq!(
        app.undo_stack.len(),
        1,
        "a rapid burst collapses to one undo step"
    );
    // One Ctrl+Z reverts the entire burst.
    let _ = app.apply_key_with_mods(KeyCode::Char('z'), ctrl());
    assert_eq!(app.input, "");
}

#[test]
fn redo_reapplies_after_undo() {
    let mut app = fresh_app(Some("offline"));
    let _ = app.apply_key(KeyCode::Char('a'));
    assert_eq!(app.input, "a");
    let _ = app.apply_key_with_mods(KeyCode::Char('z'), ctrl());
    assert_eq!(app.input, "");
    // Alt+Z replays the undone edit.
    let _ = app.apply_key_with_mods(KeyCode::Char('z'), alt());
    assert_eq!(app.input, "a");
}

#[test]
fn a_fresh_edit_truncates_the_redo_branch() {
    let mut app = fresh_app(Some("offline"));
    let _ = app.apply_key(KeyCode::Char('a'));
    let _ = app.apply_key_with_mods(KeyCode::Char('z'), ctrl());
    assert_eq!(app.input, "");
    // A new edit forks a clean future — the redo branch is gone.
    let _ = app.apply_key(KeyCode::Char('b'));
    assert_eq!(app.input, "b");
    assert!(app.redo_stack.is_empty(), "the redo branch was truncated");
    // Alt+Z now has nothing to replay.
    let _ = app.apply_key_with_mods(KeyCode::Char('z'), alt());
    assert_eq!(app.input, "b");
}

#[test]
fn ring_and_undo_do_not_fire_while_search_owns_the_keys() {
    let mut app = fresh_app(Some("offline"));
    app.input = "hello world".to_string();
    app.input_cursor = app.input_len();
    // Search mode owns EVERY keystroke.
    app.open_search();
    let _ = app.apply_key_with_mods(KeyCode::Char('u'), ctrl());
    let _ = app.apply_key_with_mods(KeyCode::Char('y'), ctrl());
    let _ = app.apply_key_with_mods(KeyCode::Char('z'), ctrl());
    assert_eq!(app.input, "hello world", "the input buffer is untouched");
    assert!(app.kill_ring.is_empty(), "no kill fired");
    assert!(app.undo_stack.is_empty(), "no undo snapshot fired");
}

#[test]
fn bang_prefix_runs_a_local_shell_and_shows_output() {
    let mut app = fresh_app(Some("offline"));
    let before = app.history.len();
    // `!echo <marker>` runs once in the project root and renders as a
    // finished Bash tool row whose result holds the command's output — it is
    // NOT routed to the base, so this works with no live session.
    let action = app.try_bang_command("!echo umadev_bang_marker").unwrap();
    assert!(matches!(action, Action::None));
    assert_eq!(
        app.history.len(),
        before + 1,
        "exactly one tool row is appended"
    );
    let last = app.history.back().unwrap();
    assert_eq!(last.role, ChatRole::Host);
    let MessageBody::Tool(t) = &last.kind else {
        panic!(
            "a bang command must render as a tool row, got {:?}",
            last.kind
        );
    };
    assert_eq!(t.name, "Bash");
    assert_eq!(t.status, ToolStatus::Ok);
    assert!(
        t.result
            .as_deref()
            .unwrap_or_default()
            .contains("umadev_bang_marker"),
        "the shell output must be shown in the row: {:?}",
        t.result
    );
}

#[test]
fn bare_bang_is_a_consumed_no_op() {
    let mut app = fresh_app(Some("offline"));
    let before = app.history.len();
    // A bare `!` (and `!` followed by only whitespace) CONSUMES the input so
    // the literal `!` never reaches the base, but runs nothing + appends no row.
    assert!(matches!(app.try_bang_command("!"), Some(Action::None)));
    assert!(matches!(app.try_bang_command("!   "), Some(Action::None)));
    assert_eq!(
        app.history.len(),
        before,
        "an empty bang must not append any row"
    );
    // Non-`!` input is not a bang command at all (falls through to routing).
    assert!(app.try_bang_command("echo hi").is_none());
    assert!(app.try_bang_command("/help").is_none());
}

#[test]
fn bang_nonzero_exit_surfaces_the_code_and_stays_expanded() {
    let mut app = fresh_app(Some("offline"));
    // A failing command marks the row Fail (kept expanded so the error is
    // never hidden) and surfaces its nonzero exit code in the result.
    let _ = app.try_bang_command("!exit 3").unwrap();
    let MessageBody::Tool(t) = &app.history.back().unwrap().kind else {
        panic!("expected a tool row");
    };
    assert_eq!(t.status, ToolStatus::Fail);
    assert!(!t.collapsed, "a failed shell row stays expanded");
    assert!(
        t.result.as_deref().unwrap_or_default().contains('3'),
        "the nonzero exit code must be shown: {:?}",
        t.result
    );
}

/// M3 regression — a runaway-output command (`yes` emits "y\n" forever) must
/// NOT buffer unbounded into memory the way `Command::output()` did (read to
/// EOF) and must NOT run on / hang. The per-stream reader caps in-memory bytes
/// and drops the pipe at the cap; `yes` then dies on SIGPIPE — so the call
/// returns PROMPTLY with BOUNDED output, with the kill-on-deadline path as the
/// backstop. Unix-only (`yes` / SIGPIPE semantics).
#[cfg(unix)]
#[test]
fn bang_runaway_output_is_bounded_and_does_not_hang() {
    let root = std::env::temp_dir();
    let start = std::time::Instant::now();
    let (ok, out) = run_bang_command(&root, "yes", umadev_i18n::Lang::En);
    let elapsed = start.elapsed();
    // Killed by SIGPIPE (or the deadline) → not a clean success.
    assert!(!ok, "a killed runaway command is not a success");
    // Output is bounded (`bound_shell_output` caps at 300 lines / 16k chars),
    // proving we never buffered the infinite stream into memory; add headroom
    // for the appended failure note.
    assert!(
        out.chars().count() < 17_000,
        "runaway output must be bounded, got {} chars",
        out.chars().count()
    );
    // And it returned well under the 10s kill budget (SIGPIPE death, not a
    // hang) — the old code would never even return from this for `yes`.
    assert!(
        elapsed < std::time::Duration::from_secs(9),
        "runaway command must not hang: {elapsed:?}"
    );
}

#[test]
fn transient_status_updates_field_without_growing_transcript() {
    // The long-phase heartbeat's periodic beats arrive as TransientStatus
    // and must update the in-place status field WITHOUT pushing a transcript
    // row — this is the flood-bug fix. A repeated beat overwrites, never
    // appends; a `None` clears the line.
    let mut app = fresh_app(Some("offline"));
    let before = app.history.len();

    app.apply_engine(EngineEvent::TransientStatus(Some(
        "做事 仍在进行(已 0:03)".to_string(),
    )));
    assert_eq!(
        app.history.len(),
        before,
        "a transient beat must NOT add a transcript row"
    );
    assert_eq!(
        app.transient_status.as_deref(),
        Some("做事 仍在进行(已 0:03)"),
        "the in-place status field must be set"
    );

    // A second beat OVERWRITES the field (still no new row).
    app.apply_engine(EngineEvent::TransientStatus(Some(
        "做事 仍在进行(已 0:10)".to_string(),
    )));
    assert_eq!(app.history.len(), before, "second beat must not add a row");
    assert_eq!(
        app.transient_status.as_deref(),
        Some("做事 仍在进行(已 0:10)"),
        "the field must be overwritten by the newer beat"
    );

    // Completion clears the line.
    app.apply_engine(EngineEvent::TransientStatus(None));
    assert_eq!(app.history.len(), before, "clearing must not add a row");
    assert!(
        app.transient_status.is_none(),
        "TransientStatus(None) must clear the in-place line"
    );
}

#[test]
fn real_output_and_phase_boundary_clear_a_stale_heartbeat_line() {
    // A real sign of life (host output) or a new phase supersedes the
    // heartbeat reassurance — the in-place line must not linger next to
    // fresh content.
    let mut app = fresh_app(Some("offline"));
    app.transient_status = Some("阶段 仍在进行(已 1:51)".to_string());
    app.apply_engine(EngineEvent::HostOutput {
        phase: Phase::Frontend,
        line: "real worker output".to_string(),
    });
    assert!(
        app.transient_status.is_none(),
        "real host output must clear a stale heartbeat line"
    );

    app.transient_status = Some("阶段 仍在进行(已 2:30)".to_string());
    app.apply_engine(EngineEvent::PhaseStarted {
        phase: Phase::Backend,
    });
    assert!(
        app.transient_status.is_none(),
        "a fresh phase must clear the prior phase's heartbeat line"
    );
}

#[test]
fn shift_up_scrolls_transcript_and_stops_auto_stick() {
    let mut app = fresh_app(Some("offline"));
    // Simulate a render having published a scroll bound + viewport.
    app.transcript_max_scroll.set(20);
    app.transcript_viewport_rows.set(10);
    // Shift+↑ nudges the transcript up one row (un-pins from the bottom).
    let _ = app.apply_key_with_mods(
        crossterm::event::KeyCode::Up,
        crossterm::event::KeyModifiers::SHIFT,
    );
    assert_eq!(app.transcript_scroll(), 1);
    // Shift+↓ brings it back.
    let _ = app.apply_key_with_mods(
        crossterm::event::KeyCode::Down,
        crossterm::event::KeyModifiers::SHIFT,
    );
    assert_eq!(app.transcript_scroll(), 0);
}

#[test]
fn page_and_home_end_scroll_against_published_viewport() {
    let mut app = fresh_app(Some("offline"));
    app.transcript_max_scroll.set(100);
    app.transcript_viewport_rows.set(20);
    // PageUp = viewport - 1 rows.
    let _ = app.apply_key(crossterm::event::KeyCode::PageUp);
    assert_eq!(app.transcript_scroll(), 19);
    // Home jumps to the very top (= max scroll).
    let _ = app.apply_key(crossterm::event::KeyCode::Home);
    assert_eq!(app.transcript_scroll(), 100);
    // End re-pins to the bottom.
    let _ = app.apply_key(crossterm::event::KeyCode::End);
    assert_eq!(app.transcript_scroll(), 0);
    // Ctrl+Alt+U = half a page up (the half-page scroll moved off bare
    // Ctrl-U so the shell "clear line" key keeps its job).
    let _ = app.apply_key_with_mods(
        crossterm::event::KeyCode::Char('u'),
        crossterm::event::KeyModifiers::CONTROL | crossterm::event::KeyModifiers::ALT,
    );
    assert_eq!(app.transcript_scroll(), 10);
}

#[test]
fn ctrl_alt_u_and_d_half_page_scroll_transcript() {
    let mut app = fresh_app(Some("offline"));
    app.transcript_max_scroll.set(100);
    app.transcript_viewport_rows.set(20);
    let cmd_alt = crossterm::event::KeyModifiers::CONTROL | crossterm::event::KeyModifiers::ALT;
    // Ctrl+Alt+U → half a viewport up (20 / 2 = 10).
    let _ = app.apply_key_with_mods(crossterm::event::KeyCode::Char('u'), cmd_alt);
    assert_eq!(
        app.transcript_scroll(),
        10,
        "Ctrl+Alt+U scrolls half a page up"
    );
    // Ctrl+Alt+D → half a viewport back down.
    let _ = app.apply_key_with_mods(crossterm::event::KeyCode::Char('d'), cmd_alt);
    assert_eq!(
        app.transcript_scroll(),
        0,
        "Ctrl+Alt+D scrolls half a page down"
    );
    // Ctrl+Alt+B / Ctrl+Alt+F are paging aliases.
    let _ = app.apply_key_with_mods(crossterm::event::KeyCode::Char('b'), cmd_alt);
    assert_eq!(app.transcript_scroll(), 10, "Ctrl+Alt+B aliases scroll-up");
    let _ = app.apply_key_with_mods(crossterm::event::KeyCode::Char('f'), cmd_alt);
    assert_eq!(app.transcript_scroll(), 0, "Ctrl+Alt+F aliases scroll-down");
}

#[test]
fn bare_ctrl_u_and_ctrl_d_no_longer_scroll_transcript() {
    let mut app = fresh_app(Some("offline"));
    app.transcript_max_scroll.set(100);
    app.transcript_viewport_rows.set(20);
    // Empty input + bare Ctrl-U: must NOT scroll (it's the line-clear key,
    // and the input is empty so there is nothing to delete either).
    let _ = app.apply_key_with_mods(
        crossterm::event::KeyCode::Char('u'),
        crossterm::event::KeyModifiers::CONTROL,
    );
    assert_eq!(
        app.transcript_scroll(),
        0,
        "bare Ctrl-U must not move the transcript"
    );
    // Scroll up first, then bare Ctrl-D: it must NOT scroll back (Ctrl-D is
    // the terminal EOF/quit convention, not a scroll key). On empty input
    // it routes to quit, so assert via should_quit and a still-scrolled view.
    app.transcript_scroll.set(30);
    let _ = app.apply_key_with_mods(
        crossterm::event::KeyCode::Char('d'),
        crossterm::event::KeyModifiers::CONTROL,
    );
    assert_eq!(
        app.transcript_scroll(),
        30,
        "bare Ctrl-D must not move the transcript"
    );
    assert!(app.should_quit, "bare Ctrl-D on empty input quits (EOF)");
}

#[test]
fn slash_mouse_emits_set_capture_action_and_uses_i18n() {
    let mut app = fresh_app(Some("offline"));
    assert!(
        app.mouse_scroll,
        "mouse capture defaults ON (wheel-scroll + in-app drag-to-copy both work)"
    );
    // Toggling OFF must emit SetMouseCapture(false) so the event loop issues the
    // real DisableMouseCapture (handing selection back to the terminal), not just
    // flip a bool.
    let action = app.slash_toggle_mouse();
    assert_eq!(action, Action::SetMouseCapture(false));
    assert!(!app.mouse_scroll);
    // The pushed status line must be the i18n string, not a raw literal.
    let last = app.history.back().expect("a status line was pushed");
    assert_eq!(
        last.body(),
        umadev_i18n::t(app.lang, "slash.mouse_off"),
        "/mouse status text must come from the i18n catalog"
    );
    // Toggling back ON emits SetMouseCapture(true).
    let action = app.slash_toggle_mouse();
    assert_eq!(action, Action::SetMouseCapture(true));
    assert!(app.mouse_scroll);
}

/// Serializes the tests that mutate the process-global theme atoms
/// ([`crate::ui`]'s IS_LIGHT / USER_LOCKED) and restores the prior state on drop
/// so a manual `/theme` choice never leaks into a sibling test running in
/// parallel. Poison-robust so a panic in one test never cascades.
static THEME_STATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct ThemeStateRestore {
    light: bool,
    locked: bool,
}

impl ThemeStateRestore {
    fn capture() -> Self {
        Self {
            light: crate::ui::is_light_theme(),
            locked: crate::ui::theme_locked(),
        }
    }
}

impl Drop for ThemeStateRestore {
    fn drop(&mut self) {
        crate::ui::set_light_theme(self.light);
        crate::ui::set_theme_locked(self.locked);
    }
}

#[test]
fn slash_theme_light_and_dark_force_and_pin_the_palette() {
    let _guard = THEME_STATE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _restore = ThemeStateRestore::capture();
    let mut app = fresh_app(Some("offline"));

    let action = app.slash_theme("light");
    assert_eq!(action, Action::ForceRedraw);
    assert!(
        crate::ui::is_light_theme(),
        "/theme light selects the light palette"
    );
    assert!(
        crate::ui::theme_locked(),
        "a manual choice pins the palette against a late probe reply"
    );
    assert_eq!(
        app.history.back().expect("a status line").body(),
        umadev_i18n::t(app.lang, "slash.theme_light"),
    );

    let action = app.slash_theme("dark");
    assert_eq!(action, Action::ForceRedraw);
    assert!(
        !crate::ui::is_light_theme(),
        "/theme dark selects the dark palette"
    );
    assert!(crate::ui::theme_locked());
    assert_eq!(
        app.history.back().expect("a status line").body(),
        umadev_i18n::t(app.lang, "slash.theme_dark"),
    );
}

#[test]
fn slash_theme_auto_releases_the_pin_and_reprobes() {
    let _guard = THEME_STATE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _restore = ThemeStateRestore::capture();
    let mut app = fresh_app(Some("offline"));

    // Pin first, then release with auto.
    let _ = app.slash_theme("dark");
    assert!(crate::ui::theme_locked());

    let action = app.slash_theme("auto");
    assert_eq!(
        action,
        Action::ReprobeTheme,
        "/theme auto asks the event loop to re-fire the OSC 11 probe"
    );
    assert!(
        !crate::ui::theme_locked(),
        "auto releases the manual pin so detection can win again"
    );
    assert_eq!(
        app.history.back().expect("a status line").body(),
        umadev_i18n::t(app.lang, "slash.theme_auto"),
    );

    // A bare `/theme` (no arg) is the same reset as `/theme auto`.
    let _ = app.slash_theme("dark");
    let action = app.slash_theme("");
    assert_eq!(action, Action::ReprobeTheme);
    assert!(!crate::ui::theme_locked());
}

#[test]
fn slash_theme_unknown_arg_is_a_friendly_noop() {
    let _guard = THEME_STATE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _restore = ThemeStateRestore::capture();
    let mut app = fresh_app(Some("offline"));
    let locked_before = crate::ui::theme_locked();
    let light_before = crate::ui::is_light_theme();

    let action = app.slash_theme("sepia");
    assert_eq!(
        action,
        Action::None,
        "an unknown arg never crashes or forces a repaint"
    );
    assert_eq!(
        crate::ui::theme_locked(),
        locked_before,
        "an unknown arg leaves the pin untouched"
    );
    assert_eq!(crate::ui::is_light_theme(), light_before);
    let last = app
        .history
        .back()
        .expect("a status line")
        .body()
        .to_string();
    assert!(
        last.contains("sepia"),
        "the friendly error echoes the bad arg: {last}"
    );
}

#[test]
fn submitting_a_turn_repins_transcript_to_bottom() {
    let mut app = fresh_app(Some("offline"));
    app.transcript_max_scroll.set(50);
    app.transcript_scroll.set(30); // user is reviewing history
    for c in "hello".chars() {
        let _ = app.apply_key(crossterm::event::KeyCode::Char(c));
    }
    let _ = app.apply_key(crossterm::event::KeyCode::Enter);
    assert_eq!(
        app.transcript_scroll(),
        0,
        "submitting must snap back to the newest content"
    );
}

#[test]
fn slash_mouse_toggles_wheel_scroll_flag() {
    let mut app = fresh_app(Some("offline"));
    assert!(app.mouse_scroll, "mouse capture defaults on");
    for c in "/mouse".chars() {
        let _ = app.apply_key(crossterm::event::KeyCode::Char(c));
    }
    let _ = app.apply_key(crossterm::event::KeyCode::Enter);
    assert!(!app.mouse_scroll, "/mouse turns the capture binding off");
}

#[test]
fn conversation_memory_threads_turns_and_bounds_length() {
    let mut app = fresh_app(Some("claude-code"));

    app.record_user_turn("你好");
    app.record_chat_reply("你好,我是底座".to_string());
    app.record_user_turn("我刚才说了什么?");

    // The snapshot handed to the base is the running dialogue, in order,
    // with correct roles — this is what makes chat a real conversation.
    let snap = app.conversation_snapshot();
    assert_eq!(snap.len(), 3);
    assert_eq!(snap[0].role, "user");
    assert_eq!(snap[0].content, "你好");
    assert_eq!(snap[1].role, "assistant");
    assert_eq!(snap[2].content, "我刚才说了什么?");

    // The base's reply is also rendered in the visible chat as a Host line.
    assert!(app
        .history
        .iter()
        .any(|m| m.role == ChatRole::Host && m.body() == "你好,我是底座"));

    // The in-memory working view stays bounded by the safety net, while the
    // durable full transcript keeps EVERY recorded turn (compaction / the FIFO
    // fallback only ever touch the working view, never the on-disk history).
    let full_before = app.full_transcript.len();
    for i in 0..CONVERSATION_CAP * 2 {
        app.record_user_turn(&format!("msg {i}"));
    }
    assert!(app.conversation.len() <= CONVERSATION_HARD_CAP);
    assert_eq!(
        app.full_transcript.len(),
        full_before + CONVERSATION_CAP * 2,
        "the full transcript keeps every recorded turn"
    );
    assert_eq!(
        app.conversation.last().unwrap().content,
        format!("msg {}", CONVERSATION_CAP * 2 - 1)
    );
    assert_eq!(
        app.full_transcript.last().unwrap().content,
        format!("msg {}", CONVERSATION_CAP * 2 - 1)
    );
}

/// Build an app rooted at a UNIQUE temp dir so the `.umadev/chat/` persistence
/// tests don't collide with each other or the shared `fresh_app` temp dirs.
fn temp_app() -> (App, tempfile::TempDir) {
    let tmp = tempfile::TempDir::new().unwrap();
    let cfg = UserConfig {
        backend: Some("claude-code".to_string()),
        lang: Some("zh-CN".to_string()),
        ..Default::default()
    };
    let app = App::new(
        "demo",
        cfg,
        tmp.path().join("config.toml"),
        tmp.path().to_path_buf(),
    );
    (app, tmp)
}

fn record_agentic_done_with_session(app: &mut App, reply: &str, session_id: &str) {
    let backend = app.backend.as_deref().unwrap_or("offline");
    let identity = crate::session_slot::requested_resume_identity(
        backend,
        &app.project_root,
        app.effective_trust_mode().base_permissions(),
    )
    .expect("test workspace has a canonical launch identity");
    app.record_agentic_done(
        reply.to_string(),
        false,
        Some(session_id.to_string()),
        Some(identity),
    );
}

fn seed_pitfall_memory(root: &std::path::Path) -> std::path::PathBuf {
    let path = root.join(".umadev/learned/_raw/dev-errors.jsonl");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"test-pitfall\n").unwrap();
    path
}

#[test]
fn memory_parser_requires_explicit_mutation_scope_and_exact_retention_store() {
    assert_eq!(
        parse_memory_command("capture off"),
        Err(MemoryParseError::MissingScope)
    );
    assert_eq!(
        parse_memory_command("forget --store pitfalls --yes"),
        Err(MemoryParseError::MissingScope)
    );
    assert_eq!(
        parse_memory_command("retention --scope all --store chat-sessions --days 30"),
        Err(MemoryParseError::OneScopeRequired)
    );
    assert_eq!(
        parse_memory_command("retention --scope project --store conversation --days 30"),
        Err(MemoryParseError::ExactStoreRequired)
    );
    assert_eq!(
        parse_memory_command(
            "export --scope project --store pitfalls --output relative-memory.zip --yes"
        ),
        Err(MemoryParseError::AbsoluteOutputRequired)
    );
}

#[test]
fn memory_parser_preserves_quoted_absolute_paths_and_confirmation_state() {
    let dir = tempfile::TempDir::new().unwrap();
    let destination = dir.path().join("memory export.zip");
    let preview = parse_memory_command(&format!(
        "export --scope project --store pitfalls --output \"{}\"",
        destination.display()
    ))
    .unwrap();
    assert!(!preview.mutates());
    assert!(matches!(
        preview,
        MemoryTuiCommand::Export {
            destination: parsed,
            confirmed: false,
            ..
        } if parsed == destination
    ));

    let confirmed =
        parse_memory_command("retention --scope project --store chat-sessions --run --yes")
            .unwrap();
    assert!(confirmed.mutates());
    assert!(matches!(
        confirmed,
        MemoryTuiCommand::RetentionRun {
            confirmed: true,
            ..
        }
    ));
}

#[test]
fn memory_unconfirmed_forget_is_a_read_only_recoverability_preview() {
    let (mut app, tmp) = temp_app();
    let pitfall = seed_pitfall_memory(tmp.path());
    let action = app
        .try_slash_command("/memory forget --scope project --store pitfalls")
        .unwrap();
    assert_eq!(action, Action::None);
    assert!(pitfall.is_file(), "preview must not move active memory");
    let overlay = app.overlay.as_ref().expect("confirmation preview");
    let body = overlay.lines.join("\n");
    assert!(body.contains("可恢复软删除"));
    assert!(body.contains("不是物理擦除"));
    assert!(body.contains("--yes"));
}

#[test]
fn memory_confirmed_forget_soft_deletes_and_reports_recovery_boundary() {
    let (mut app, tmp) = temp_app();
    let pitfall = seed_pitfall_memory(tmp.path());
    let action = app
        .try_slash_command("/memory forget --scope project --store pitfalls --yes")
        .unwrap();
    assert_eq!(action, Action::None);
    assert!(!pitfall.exists(), "confirmed forget moves the active file");
    let result = app.history.back().unwrap().body();
    assert!(result.contains("[ok]"), "{result}");
    assert!(result.contains("可恢复软删除"), "{result}");
    assert!(result.contains("不是物理擦除"), "{result}");
    assert!(tmp.path().join(".umadev/memory/tombstones").is_dir());
}

#[test]
fn memory_mutation_is_blocked_while_work_runs_but_preview_remains_available() {
    let (mut app, tmp) = temp_app();
    let pitfall = seed_pitfall_memory(tmp.path());
    app.thinking = true;

    let _ = app.try_slash_command("/memory forget --scope project --store pitfalls --yes");
    assert!(pitfall.is_file());
    assert_eq!(
        app.history.back().unwrap().body(),
        umadev_i18n::t(app.lang, "memory.busy")
    );

    let _ = app.try_slash_command("/memory forget --scope project --store pitfalls");
    let preview = app
        .overlay
        .as_ref()
        .expect("read-only preview remains open");
    assert!(preview.lines.join("\n").contains("尚未执行"));
    assert!(pitfall.is_file());
}

#[test]
fn memory_export_handles_space_in_absolute_path_and_keeps_source() {
    let (mut app, tmp) = temp_app();
    let pitfall = seed_pitfall_memory(tmp.path());
    let destination = tmp.path().join("private memory export.zip");
    let preview = format!(
        "/memory export --scope project --store pitfalls --output \"{}\"",
        destination.display()
    );
    let _ = app.try_slash_command(&preview);
    assert!(!destination.exists());
    assert!(app
        .overlay
        .as_ref()
        .unwrap()
        .lines
        .join("\n")
        .contains("尚未导出"));

    app.overlay = None;
    let _ = app.try_slash_command(&format!("{preview} --yes"));
    assert!(destination.is_file());
    assert!(pitfall.is_file(), "export never removes source memory");
    let result = app.history.back().unwrap().body();
    assert!(result.contains("源记忆没有被删除"), "{result}");
}

#[test]
fn memory_tui_capture_recall_and_retention_round_trip_through_policy() {
    use umadev_agent::memory_control::{self, MemoryScope, MemoryStore};

    let (mut app, tmp) = temp_app();
    let _ = app.try_slash_command("/memory capture off --scope project --store chat-sessions");
    assert!(!memory_control::capture_enabled(
        tmp.path(),
        MemoryScope::Project,
        MemoryStore::ChatSessions
    ));
    let _ = app.try_slash_command("/memory recall off --scope project --store input-history");
    assert!(!memory_control::recall_enabled(
        tmp.path(),
        MemoryScope::Project,
        MemoryStore::InputHistory
    ));

    let _ =
        app.try_slash_command("/memory retention --scope project --store chat-sessions --days 30");
    let inventory = memory_control::inventory(tmp.path(), MemoryScope::Project);
    assert_eq!(
        inventory
            .entries
            .iter()
            .find(|entry| entry.store == MemoryStore::ChatSessions)
            .unwrap()
            .retention_days,
        Some(30)
    );

    let _ = app.try_slash_command("/memory retention --scope project --store chat-sessions --run");
    assert!(app
        .overlay
        .as_ref()
        .unwrap()
        .lines
        .join("\n")
        .contains("尚未执行"));
    app.overlay = None;
    let _ = app
        .try_slash_command("/memory retention --scope project --store chat-sessions --run --yes");
    let result = app.history.back().unwrap().body();
    assert!(result.contains("可恢复软删除"), "{result}");
    assert!(result.contains("不是物理擦除"), "{result}");

    let _ =
        app.try_slash_command("/memory retention --scope project --store chat-sessions --clear");
    let inventory = memory_control::inventory(tmp.path(), MemoryScope::Project);
    assert_eq!(
        inventory
            .entries
            .iter()
            .find(|entry| entry.store == MemoryStore::ChatSessions)
            .unwrap()
            .retention_days,
        None
    );
}

#[test]
fn memory_inventory_and_retention_views_are_available_during_work() {
    let (mut app, _tmp) = temp_app();
    app.thinking = true;
    let _ = app.try_slash_command("/memory inventory --scope project");
    let inventory = app.overlay.take().expect("inventory overlay");
    assert!(inventory.lines.join("\n").contains("记忆清单"));

    let _ = app.try_slash_command("/memory retention --scope project --store chat-sessions");
    let retention = app.overlay.take().expect("retention overlay");
    let body = retention.lines.join("\n");
    assert!(body.contains("chat-sessions"));
    assert!(!body.contains("test-pitfall"));
}

#[test]
fn memory_lifecycle_copy_is_complete_and_safe_in_all_languages() {
    let cases = [
        (umadev_i18n::Lang::ZhCn, "不是物理擦除"),
        (umadev_i18n::Lang::ZhTw, "不是實體抹除"),
        (umadev_i18n::Lang::En, "not physical erasure"),
    ];
    for (lang, recovery_phrase) in cases {
        let usage = umadev_i18n::t(lang, "memory.usage");
        assert!(usage.contains("/memory retention"));
        assert!(usage.contains("/memory export"));
        assert!(usage.contains("/memory forget"));
        assert!(usage.contains("--scope project|global"));
        assert!(usage.contains("--yes"));
        assert!(usage.contains(recovery_phrase));
        assert!(umadev_i18n::t(lang, "memory.forget_ok").contains(recovery_phrase));
        assert!(umadev_i18n::t(lang, "memory.retention_run_ok").contains(recovery_phrase));
    }
}

#[test]
fn chat_capture_off_stops_new_persistence_without_deleting_old_data() {
    use umadev_agent::memory_control::{self, MemoryScope, MemoryStore};

    let (mut app, tmp) = temp_app();
    app.record_user_turn("persisted-before-opt-out");
    let path = app.chat_path(&app.chat_id);
    let before = std::fs::read_to_string(&path).unwrap();
    memory_control::update_capture(
        tmp.path(),
        MemoryScope::Project,
        Some(MemoryStore::ChatSessions),
        false,
    )
    .unwrap();
    app.record_user_turn("must-not-reach-disk");
    assert_eq!(std::fs::read_to_string(path).unwrap(), before);
}

#[test]
fn saved_chat_recall_is_explicit_even_when_automatic_recall_is_disabled() {
    use umadev_agent::memory_control::{self, MemoryScope, MemoryStore};

    let (mut app, tmp) = temp_app();
    app.record_user_turn("private-auto-reopen-marker");
    let id = app.chat_id.clone();
    drop(app);
    memory_control::update_recall(
        tmp.path(),
        MemoryScope::Project,
        Some(MemoryStore::ChatSessions),
        false,
    )
    .unwrap();
    let cfg = UserConfig {
        backend: Some("claude-code".to_string()),
        lang: Some("zh-CN".to_string()),
        ..Default::default()
    };
    let mut reopened = App::new(
        "demo",
        cfg,
        tmp.path().join("config-reopened.toml"),
        tmp.path().to_path_buf(),
    );
    assert!(!reopened
        .conversation
        .iter()
        .any(|message| message.content.contains("private-auto-reopen-marker")));
    assert!(reopened.list_chats().iter().any(|chat| chat.0 == id));
    assert!(
        reopened.load_chat(&id),
        "explicit /resume remains available"
    );
}

#[test]
fn input_history_policy_controls_disk_capture_and_startup_recall() {
    use umadev_agent::memory_control::{self, MemoryScope, MemoryStore};

    let (mut app, tmp) = temp_app();
    app.remember_submission("history-before-opt-out");
    assert!(app.history_path().is_file());
    memory_control::update_recall(
        tmp.path(),
        MemoryScope::Project,
        Some(MemoryStore::InputHistory),
        false,
    )
    .unwrap();
    memory_control::update_capture(
        tmp.path(),
        MemoryScope::Project,
        Some(MemoryStore::InputHistory),
        false,
    )
    .unwrap();
    app.remember_submission("history-must-not-reach-disk");
    let persisted = std::fs::read_to_string(app.history_path()).unwrap();
    assert!(!persisted.contains("history-must-not-reach-disk"));

    let cfg = UserConfig {
        backend: Some("claude-code".to_string()),
        lang: Some("zh-CN".to_string()),
        ..Default::default()
    };
    let reopened = App::new(
        "demo",
        cfg,
        tmp.path().join("config-history.toml"),
        tmp.path().to_path_buf(),
    );
    assert!(reopened.input_history.is_empty());
}

#[test]
fn chat_resume_rejects_path_traversal_ids() {
    let (mut app, tmp) = temp_app();
    let outside = tmp.path().join("outside.json");
    std::fs::write(&outside, "outside").unwrap();
    assert!(!app.load_chat("../outside"));
    assert_eq!(std::fs::read_to_string(outside).unwrap(), "outside");
}

#[test]
fn pasted_image_path_becomes_a_typed_block_without_path_text() {
    let mut app = fresh_app(Some("offline"));
    let dir = tempfile::TempDir::new().unwrap();
    let img = dir.path().join("shot.png");
    std::fs::write(&img, b"\x89PNG\r\n\x1a\n").unwrap();
    app.handle_paste(img.to_str().unwrap());
    // A chip is shown in the input (not the raw path); one attachment tracked.
    assert!(
        app.input.contains("图片") || app.input.contains("Image"),
        "chip inserted, got: {}",
        app.input
    );
    assert_eq!(app.attachments.len(), 1);
    // Submission preserves a typed image block and never leaks the path into text.
    let abs = std::fs::canonicalize(&img).unwrap();
    let turn = app.compose_submitted_turn(app.input.trim());
    assert!(!turn.text.contains(&abs.to_string_lossy().to_string()));
    assert!(matches!(
        turn.input.blocks.as_slice(),
        [TurnInputBlock::Image { path }] if path == &abs
    ));
}

#[test]
fn enter_snapshots_typed_attachments_before_clearing_the_editor() {
    let mut app = fresh_app(Some("claude-code"));
    let dir = tempfile::TempDir::new().unwrap();
    let image = dir.path().join("send me.png");
    std::fs::write(&image, b"\x89PNG\r\n\x1a\n").unwrap();
    app.handle_paste(image.to_str().unwrap());
    let safe_chip_text = app.input.trim().to_string();

    let action = app.apply_key(KeyCode::Enter);
    assert_eq!(action, Action::Route(safe_chip_text.clone()));
    assert!(app.input.is_empty());
    assert!(app.attachments.is_empty());
    let submitted = app.take_route_input(&safe_chip_text);
    assert!(matches!(
        submitted.input.blocks.as_slice(),
        [TurnInputBlock::Image { path }]
            if path == &std::fs::canonicalize(image).unwrap()
    ));
}

#[test]
fn pasted_plain_text_with_a_png_word_is_verbatim_not_attached() {
    let mut app = fresh_app(Some("offline"));
    app.handle_paste("see the png export in the docs");
    assert_eq!(app.input, "see the png export in the docs");
    assert!(app.attachments.is_empty());
}

#[test]
fn a_nonexistent_image_path_pastes_as_text_not_discarded() {
    let mut app = fresh_app(Some("offline"));
    // A path that merely LOOKS like an image but is NOT a real file is prose (a
    // filename the user is writing about, a URL, a broken path), not an
    // attachment. It must paste as TEXT. The old image branch attach-failed on the
    // non-file then `return`ed without inserting — silently DISCARDING the text.
    app.handle_paste("/no/such/dir/ghost.png");
    assert!(app.attachments.is_empty());
    assert_eq!(app.input, "/no/such/dir/ghost.png");
}

#[test]
fn a_real_non_image_file_with_image_suffix_is_rejected_without_echoing_its_path() {
    let mut app = fresh_app(Some("offline"));
    let dir = tempfile::TempDir::new().unwrap();
    // A REAL file that ends `.png` but is not actually an image fails validation.
    // It IS a real local file, so its (possibly private) path must NOT be echoed
    // as model text — it stays rejected, distinct from a non-existent path above.
    let fake = dir.path().join("ghost.png");
    std::fs::write(&fake, b"not really a png").unwrap();
    app.handle_paste(fake.to_str().unwrap());
    assert!(app.attachments.is_empty());
    assert!(
        app.input.is_empty(),
        "a real file's path is not downgraded into model text"
    );
    assert!(app.history.iter().any(|message| {
        let body = message.body();
        (body.contains("未添加") || body.contains("not added")) && !body.contains("ghost.png")
    }));
}

#[test]
fn structured_turn_preserves_interleaved_order_for_unicode_space_paths() {
    let mut app = fresh_app(Some("claude-code"));
    let dir = tempfile::TempDir::new().unwrap();
    let image = dir.path().join("图 像 🚀.png");
    let file = dir.path().join("设 计 说明.md");
    std::fs::write(&image, b"\x89PNG\r\n\x1a\nbody").unwrap();
    std::fs::write(&file, "# 设计\n").unwrap();

    let image_number = app.attach_image(image.to_str().unwrap()).unwrap();
    let file_number = app.attach_file(&file).unwrap();
    let image_chip = app.image_chip(image_number);
    let file_chip = app.file_chip(file_number);
    let raw = format!("前文 {image_chip} 中段 {file_chip} 后文");
    let turn = app.compose_submitted_turn(&raw);
    let image = std::fs::canonicalize(image).unwrap();
    let file = std::fs::canonicalize(file).unwrap();

    assert_eq!(
        turn.input,
        TurnInput::new(vec![
            TurnInputBlock::Text {
                text: "前文 ".to_string(),
            },
            TurnInputBlock::Image {
                path: image.clone(),
            },
            TurnInputBlock::Text {
                text: " 中段 ".to_string(),
            },
            TurnInputBlock::File {
                path: file.clone(),
                mode: FileInputMode::MaterializeText,
            },
            TurnInputBlock::Text {
                text: " 后文".to_string(),
            },
        ])
    );
    assert_eq!(turn.text, raw);
    assert!(!turn.text.contains(&image.to_string_lossy().to_string()));
    assert!(!turn.text.contains(&file.to_string_lossy().to_string()));
}

#[test]
fn restored_windows_style_attachment_paths_stay_behind_chips() {
    let mut app = fresh_app(Some("codex"));
    let image = std::path::PathBuf::from(r"C:\Users\weiyou\图 像.png");
    let file = std::path::PathBuf::from(r"D:\项目 空间\需求 文档.md");
    app.restore_submitted_turn(SubmittedTurn {
        text: "ignored safe view".to_string(),
        input: TurnInput::new(vec![
            TurnInputBlock::Text {
                text: "看 ".to_string(),
            },
            TurnInputBlock::Image {
                path: image.clone(),
            },
            TurnInputBlock::Text {
                text: " 和 ".to_string(),
            },
            TurnInputBlock::File {
                path: file.clone(),
                mode: FileInputMode::MaterializeText,
            },
        ]),
    });

    assert!(!app.input.contains("C:\\"));
    assert!(!app.input.contains("D:\\"));
    let turn = app.compose_submitted_turn(&app.input);
    assert!(matches!(
        &turn.input.blocks[1],
        TurnInputBlock::Image { path } if path == &image
    ));
    assert!(matches!(
        &turn.input.blocks[3],
        TurnInputBlock::File { path, .. } if path == &file
    ));
}

#[test]
fn rejected_typed_turn_and_newer_draft_are_both_restored_without_paths() {
    let mut app = fresh_app(Some("codex"));
    let secret = std::path::PathBuf::from("/Users/private/方案 图.png");
    app.input = "我同时输入的新草稿".to_string();
    app.input_cursor = app.input_len();
    app.reject_live_input(
        SubmittedTurn {
            text: "[图片 1]".to_string(),
            input: TurnInput::new(vec![TurnInputBlock::Image {
                path: secret.clone(),
            }]),
        },
        "rejected".to_string(),
    );

    assert!(app.input.contains(&app.image_chip(1)));
    assert!(app.input.contains("我同时输入的新草稿"));
    assert!(!app.input.contains(&secret.to_string_lossy().to_string()));
    let restored = app.compose_submitted_turn(&app.input);
    assert!(matches!(
        restored.input.blocks.first(),
        Some(TurnInputBlock::Image { path }) if path == &secret
    ));
}

#[test]
fn attachment_limits_and_mime_failures_are_actionable_and_path_free() {
    let dir = tempfile::TempDir::new().unwrap();

    let spoof = dir.path().join("private spoof.png");
    std::fs::write(&spoof, b"not an image").unwrap();
    let mut mime = fresh_app(Some("claude-code"));
    mime.handle_paste(spoof.to_str().unwrap());
    assert!(mime.attachments.is_empty());
    let note = mime.history.back().unwrap().body();
    assert!(note.contains("图片") || note.contains("image"));
    assert!(!note.contains("private spoof.png"));

    let oversized = dir.path().join("private oversized.md");
    let oversized_file = std::fs::File::create(&oversized).unwrap();
    oversized_file.set_len(MAX_ATTACHMENT_BYTES + 1).unwrap();
    let mut size = fresh_app(Some("claude-code"));
    assert!(size.attach_file(&oversized).is_none());
    let note = size.history.back().unwrap().body();
    assert!(note.contains("8 MiB"));
    assert!(!note.contains("private oversized.md"));

    let ordinary = dir.path().join("ordinary.md");
    std::fs::write(&ordinary, "ok").unwrap();
    let mut count = fresh_app(Some("claude-code"));
    for _ in 0..MAX_TURN_ATTACHMENTS {
        assert!(count.attach_file(&ordinary).is_some());
    }
    assert!(count.attach_file(&ordinary).is_none());
    assert!(count.history.back().unwrap().body().contains("16"));

    let two_mib = dir.path().join("two MiB.bin");
    let two_mib_file = std::fs::File::create(&two_mib).unwrap();
    two_mib_file.set_len(2 * 1024 * 1024).unwrap();
    let mut total = fresh_app(Some("claude-code"));
    for _ in 0..10 {
        assert!(total.attach_file(&two_mib).is_some());
    }
    assert!(total.attach_file(&two_mib).is_none());
    assert!(total.history.back().unwrap().body().contains("20 MiB"));

    let mut regular = fresh_app(Some("claude-code"));
    assert!(regular.attach_file(dir.path()).is_none());
    let note = regular.history.back().unwrap().body();
    assert!(note.contains("常规") || note.contains("regular"));
    assert!(!note.contains(&dir.path().to_string_lossy().to_string()));
}

#[cfg(unix)]
#[test]
fn symlinked_image_is_rejected_without_disclosing_either_path() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::TempDir::new().unwrap();
    let target = dir.path().join("secret target.png");
    let link = dir.path().join("secret link.png");
    std::fs::write(&target, b"\x89PNG\r\n\x1a\n").unwrap();
    symlink(&target, &link).unwrap();
    let mut app = fresh_app(Some("claude-code"));
    app.handle_paste(link.to_str().unwrap());

    assert!(app.attachments.is_empty());
    let note = app.history.back().unwrap().body();
    assert!(note.contains("符号") || note.contains("symbolic"));
    assert!(!note.contains("secret link.png"));
    assert!(!note.contains("secret target.png"));
}

// ---- I4: large-paste collapse to a chip ----

/// Build `n` distinct `"<prefix> <i>\n"` lines — test fixtures for the
/// large-paste chip (distinct markers let the assertions confirm the FULL
/// text round-trips through stash→expand).
fn numbered_lines(prefix: &str, n: usize) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    for i in 0..n {
        let _ = writeln!(s, "{prefix} {i}");
    }
    s
}

#[test]
fn large_paste_collapses_to_a_chip_and_expands_on_submit() {
    let mut a = fresh_app(Some("offline"));
    // 20 lines → over the line threshold → one chip, not a 20-line flood.
    let big = numbered_lines("line", 20);
    a.handle_paste(&big);
    assert!(
        a.input.contains("粘贴") || a.input.contains("pasted") || a.input.contains("貼上"),
        "a chip is shown, got: {}",
        a.input
    );
    assert!(
        !a.input.contains("line 15"),
        "the bulk text is stashed, NOT flooding the box: {}",
        a.input
    );
    assert_eq!(a.text_stash.len(), 1, "one paste stashed");
    // On submit the chip expands back to the full text inline.
    let expanded = a.expand_attachments(a.input.trim());
    assert!(
        expanded.contains("line 0") && expanded.contains("line 19"),
        "chip expands to the full pasted text, got: {expanded}"
    );
}

#[test]
fn huge_single_line_paste_also_collapses_to_a_chip() {
    let mut a = fresh_app(Some("offline"));
    // One line, but past the CHAR threshold → still chipped (1 line of noise
    // is as unscrollable as 40 short ones).
    let big = "x".repeat(PASTE_CHIP_MIN_CHARS + 50);
    a.handle_paste(&big);
    assert_eq!(a.text_stash.len(), 1, "one-line but huge → chipped");
    assert!(
        a.input.chars().count() < 30,
        "box holds a compact chip, not the full {} chars",
        big.len()
    );
    let expanded = a.expand_attachments(a.input.trim());
    assert_eq!(expanded, big, "expands back to the exact pasted text");
}

#[test]
fn small_paste_inserts_inline_without_a_chip() {
    let mut a = fresh_app(Some("offline"));
    a.handle_paste("just a short note\nwith two lines");
    assert_eq!(a.input, "just a short note\nwith two lines");
    assert!(a.text_stash.is_empty(), "a small paste is never stashed");
}

#[test]
fn small_paste_normalizes_windows_cr_newlines() {
    let mut a = fresh_app(Some("offline"));
    a.handle_paste("first\rsecond\r\nthird");
    assert_eq!(a.input, "first\nsecond\nthird");
    assert!(a.text_stash.is_empty(), "a small paste stays inline");
}

#[test]
fn small_paste_strips_ansi_sequences() {
    let mut a = fresh_app(Some("offline"));
    a.handle_paste("\x1b[31mred\x1b[0m \x1b]0;title\x07plain \x1bPignored\x1b\\done");
    assert_eq!(a.input, "red plain done");
}

#[test]
fn large_paste_stashes_normalized_windows_newlines() {
    let mut a = fresh_app(Some("offline"));
    let big = (0..20)
        .map(|i| format!("row {i}"))
        .collect::<Vec<_>>()
        .join("\r");
    a.handle_paste(&big);
    assert_eq!(a.text_stash.len(), 1, "CR-separated paste is still bulky");
    assert!(a.input.contains("粘贴") || a.input.contains("pasted"));
    assert_eq!(a.text_stash[0], big.replace('\r', "\n"));
}

#[test]
fn dragging_a_file_into_the_input_pastes_its_path_verbatim() {
    // Dropping a file onto the terminal is a PASTE of its path — and on Windows
    // that path is backslashed, quoted when it contains spaces, and very often
    // full of CJK. Every one of those is a way to mangle it: a backslash read as
    // an escape, the quotes eaten, or a byte-sliced CJK segment. It arrives as one
    // bracketed-paste burst (see EnableBracketedPaste), so it lands here whole.
    let mut a = fresh_app(Some("offline"));
    let dropped = "\"D:\\我的 项目\\需求文档\\说明.md\"";
    a.handle_paste(dropped);
    assert_eq!(
        a.input, dropped,
        "a dropped path must reach the input exactly as the terminal sent it"
    );
    // And it is ONE line, so it must stay in the input box rather than being
    // stashed away behind a [pasted N lines] chip the user then cannot edit.
    assert!(
        a.text_stash.is_empty(),
        "a single dropped path is not a bulk paste"
    );

    // The same path unquoted (no spaces to quote), dropped at the cursor inside
    // text the user had already typed — the splice must stay on char boundaries.
    let mut b = fresh_app(Some("offline"));
    for c in "看看 ".chars() {
        let _ = b.apply_key(KeyCode::Char(c));
    }
    b.handle_paste("D:\\项目\\readme.md");
    assert_eq!(b.input, "看看 D:\\项目\\readme.md");
}

#[test]
fn paste_preserves_tab_indentation() {
    // Low finding — the insert filter keeps `\n` but used to drop ALL other
    // control chars, silently stripping every `\t` out of pasted tab-indented
    // code. Tabs must survive (other control chars still dropped).
    let mut a = fresh_app(Some("offline"));
    a.insert_str_at_cursor("\tfn main() {\n\t\tprintln!();\n\t}");
    assert_eq!(
        a.input, "\tfn main() {\n\t\tprintln!();\n\t}",
        "pasted tab indentation must be preserved verbatim"
    );
    // A stray control char (e.g. a bell) is still filtered out.
    let mut b = fresh_app(Some("offline"));
    b.insert_str_at_cursor("a\u{7}b");
    assert_eq!(
        b.input, "ab",
        "non-tab/newline control chars are still dropped"
    );
}

#[test]
fn two_large_pastes_each_stash_and_expand_independently() {
    let mut a = fresh_app(Some("offline"));
    let a_text = numbered_lines("alpha", 15);
    let b_text = numbered_lines("beta", 18);
    a.handle_paste(&a_text);
    a.handle_paste(&b_text);
    assert_eq!(a.text_stash.len(), 2, "two pastes → two stash entries");
    let expanded = a.expand_attachments(a.input.trim());
    assert!(
        expanded.contains("alpha 14") && expanded.contains("beta 17"),
        "each chip expands to its OWN stashed text, got: {expanded}"
    );
}

#[test]
fn paste_chip_is_fail_open_clear_resets_stash_and_expand_noops() {
    let mut a = fresh_app(Some("offline"));
    // expand with nothing stashed/attached returns the text unchanged.
    assert_eq!(a.expand_attachments("hello"), "hello");
    let big = numbered_lines("row", 30);
    a.handle_paste(&big);
    assert_eq!(a.text_stash.len(), 1);
    a.clear_input();
    assert!(a.text_stash.is_empty(), "clear_input drops the stash");
    assert!(a.input.is_empty());
}

#[test]
fn large_paste_near_input_cap_never_orphans_the_stash() {
    // Mirrors the image-chip cap guard: if the visible `[pasted N lines]`
    // token cannot fit WHOLE, do not stash the backing text. A partial chip
    // would not expand on submit, silently dropping the pasted content.
    let big = numbered_lines("row", 30);

    let mut a = fresh_app(Some("offline"));
    let chip = a.text_chip(&big);
    let room = chip.chars().count().saturating_sub(1);
    a.input = "x".repeat(INPUT_CAP - room);
    a.input_cursor = a.input_len();
    let before = a.input.clone();
    a.handle_paste(&big);
    assert_eq!(a.input, before, "no partial paste chip should be inserted");
    assert!(
        a.text_stash.is_empty(),
        "no backing stash should exist without a complete chip"
    );

    let mut b = fresh_app(Some("offline"));
    let chip = b.text_chip(&big);
    b.input = "y".repeat(INPUT_CAP - chip.chars().count());
    b.input_cursor = b.input_len();
    b.handle_paste(&big);
    assert_eq!(b.text_stash.len(), 1, "the complete chip fits");
    assert!(b.input.ends_with(&chip), "chip landed whole: {}", b.input);
    assert_eq!(
        b.expand_attachments(&b.input),
        format!("{}{}", "y".repeat(INPUT_CAP - chip.chars().count()), big),
        "the fitted chip expands to the exact pasted text"
    );
}

// ---- chip-aware deletion (user-reported: backspace "does nothing" on a chip) -

/// Attach a throwaway PNG so `handle_paste(path)` produces an `[图片 N]` chip
/// backed by a real `attachments` entry. Returns the temp dir (keep it alive).
fn attach_one_image(app: &mut App) -> tempfile::TempDir {
    let dir = tempfile::TempDir::new().unwrap();
    let img = dir.path().join("shot.png");
    std::fs::write(&img, b"\x89PNG\r\n\x1a\n").unwrap();
    app.handle_paste(img.to_str().unwrap());
    dir
}

#[test]
fn image_paste_near_input_cap_never_orphans_the_attachment() {
    // Fix 5 — dragging an image when the input box is within a chip-width of
    // INPUT_CAP must not push an attachment whose `[图片 N]` chip can't fit
    // whole: that orphaned ref is silently dropped by `expand_attachments` on
    // submit. Either the chip lands whole (attached) or the image is skipped
    // (not attached) — never a pushed attachment with no chip in the buffer.
    let dir = tempfile::TempDir::new().unwrap();
    let img = dir.path().join("shot.png");
    std::fs::write(&img, b"\x89PNG\r\n\x1a\n").unwrap();
    let path = img.to_str().unwrap().to_string();

    // Invariant: every backing image attachment has an intact chip in `input`.
    let no_orphan =
        |a: &App| (0..a.attachments.len()).all(|i| a.input.contains(&a.image_chip(i + 1)));

    // Case 1: NO room for the whole chip → the image is skipped, not orphaned.
    let mut a = fresh_app(Some("offline"));
    a.input = "x".repeat(INPUT_CAP - 2); // room = 2 < chip width
    a.input_cursor = a.input_len();
    a.handle_paste(&path);
    assert!(
        a.attachments.is_empty(),
        "no room for the chip → the image is skipped, not orphaned"
    );
    assert!(no_orphan(&a), "no attachment left without its chip");

    // Case 2: room for the chip (+ its space) → the image attaches with a real
    // chip and resolves to a typed image block on submit.
    let mut b = fresh_app(Some("offline"));
    let chip_width = b.image_chip(1).chars().count();
    b.input = "y".repeat(INPUT_CAP - chip_width - 1); // room = chip + space
    b.input_cursor = b.input_len();
    b.handle_paste(&path);
    assert_eq!(b.attachments.len(), 1, "the chip fits → the image attaches");
    assert!(
        no_orphan(&b),
        "the attached image has its chip in the buffer"
    );
    let turn = b.compose_submitted_turn(&b.input);
    assert!(matches!(
        turn.input.blocks.iter().find(|block| matches!(block, TurnInputBlock::Image { .. })),
        Some(TurnInputBlock::Image { path }) if path == &b.attachments[0]
    ));
}

#[test]
fn backspace_after_a_chip_removes_the_whole_chip_and_drops_its_ref() {
    let mut app = fresh_app(Some("offline"));
    let _dir = attach_one_image(&mut app);
    // Buffer is now "[图片 1] " — caret right after the trailing space.
    let chip = app.image_chip(1);
    assert!(app.input.contains(&chip));
    assert_eq!(app.attachments.len(), 1);
    // First Backspace eats the space the paste appended (normal char delete).
    app.backspace();
    assert!(
        app.input.ends_with(']'),
        "space gone, chip intact: {:?}",
        app.input
    );
    assert_eq!(app.attachments.len(), 1, "the space is not a chip");
    // Caret is now flush against `]` → ONE Backspace removes the entire chip
    // (not just the bracket) and drops the backing attachment.
    app.backspace();
    assert!(
        !app.input.contains('图') && !app.input.contains('['),
        "the whole chip is gone in one stroke, got: {:?}",
        app.input
    );
    assert!(app.attachments.is_empty(), "backing image ref dropped");
    assert_eq!(app.input_cursor, app.input_len());
}

#[test]
fn chip_delete_works_with_cjk_around_it_screenshot_shape() {
    // The exact reported buffer: "shiyong的[图片 1] 出".
    let mut app = fresh_app(Some("offline"));
    app.insert_str_at_cursor("shiyong的");
    let _dir = attach_one_image(&mut app); // appends "[图片 1] "
    app.insert_str_at_cursor("出");
    assert_eq!(app.input, format!("shiyong的{} 出", app.image_chip(1)));
    // Peel the trailing CJK + the space (plain deletes, no panic).
    app.backspace(); // 出
    app.backspace(); // space
    assert_eq!(app.input, format!("shiyong的{}", app.image_chip(1)));
    assert_eq!(app.attachments.len(), 1, "chip still present, ref kept");
    // Caret flush against the chip → one stroke clears it as a unit.
    app.backspace();
    assert_eq!(app.input, "shiyong的", "chip removed atomically");
    assert!(app.attachments.is_empty(), "ref dropped");
    // The CJK before the chip still deletes normally afterward.
    app.backspace();
    assert_eq!(app.input, "shiyong");
}

#[test]
fn char_immediately_before_a_chip_deletes_normally() {
    let mut app = fresh_app(Some("offline"));
    app.insert_str_at_cursor("ab");
    let _dir = attach_one_image(&mut app); // "ab[图片 1] "
                                           // Move the caret to just before the `[` of the chip (after "ab").
    app.input_cursor = 2;
    app.backspace(); // deletes 'b', NOT the chip
    assert_eq!(app.input, format!("a{} ", app.image_chip(1)));
    assert_eq!(app.attachments.len(), 1, "chip untouched, ref kept");
}

#[test]
fn forward_delete_on_a_chip_removes_it_as_a_unit() {
    let mut app = fresh_app(Some("offline"));
    let _dir = attach_one_image(&mut app); // "[图片 1] "
    app.input_cursor = 0; // caret at the chip's left edge
    app.forward_delete();
    assert_eq!(app.input, " ", "chip gone, trailing space remains");
    assert!(app.attachments.is_empty(), "backing ref dropped");
}

#[test]
fn typing_inside_a_chip_drops_the_broken_attachment_instead_of_mis_submitting() {
    // Low/Med: overtyping INTERIOR to a `[图片 1]` chip splits its token so
    // `expand_attachments` can no longer match it. Before the fix the corrupted
    // literal was submitted verbatim and the image silently dropped. The insert
    // paths are now chip-aware: an interior insert reconciles, dropping the
    // now-broken chip's backing ref so submit can't mis-send a corrupted token.
    let mut app = fresh_app(Some("offline"));
    let _dir = attach_one_image(&mut app); // "[图片 1] "
    assert_eq!(app.attachments.len(), 1);
    // Caret between `图` (1) and `片` (2) — strictly interior to span (0,6).
    app.input_cursor = 2;
    app.insert_at_cursor('X');
    // The backing image ref is dropped (no orphaned attachment left behind).
    assert!(
        app.attachments.is_empty(),
        "interior insert into a chip must drop its broken ref, got: {:?}",
        app.attachments
    );
    // Submit no longer mis-expands a corrupted token to a real `@path`.
    let expanded = app.expand_attachments(app.input.trim());
    assert!(
        !expanded.contains('@'),
        "the corrupted chip must not mis-submit a path, got: {expanded}"
    );
}

#[test]
fn pasting_inside_a_chip_drops_the_broken_attachment() {
    // Same hazard via the bulk `insert_str_at_cursor` (bracketed paste / IME).
    let mut app = fresh_app(Some("offline"));
    let _dir = attach_one_image(&mut app); // "[图片 1] "
    app.input_cursor = 3; // interior (between `片` and the space inside the token)
    app.insert_str_at_cursor("zzz");
    assert!(
        app.attachments.is_empty(),
        "interior paste into a chip must drop its broken ref"
    );
    assert!(!app.expand_attachments(app.input.trim()).contains('@'));
}

#[test]
fn typing_at_a_chip_edge_keeps_the_attachment_intact() {
    // Guard the boundary: inserting AT an edge (cursor == start or == end) is
    // adjacent, not interior — the `[图片 N]` token stays whole and the image
    // must survive (the fix must not over-reconcile a still-valid chip).
    let mut app = fresh_app(Some("offline"));
    let _dir = attach_one_image(&mut app); // "[图片 1] "
    let chip_end = app.image_chip(1).chars().count(); // == 6, the `]` boundary
    app.input_cursor = chip_end; // flush against the right edge, not interior
    app.insert_at_cursor('Z');
    assert_eq!(app.attachments.len(), 1, "an edge insert keeps the chip");
    let turn = app.compose_submitted_turn(app.input.trim());
    assert!(matches!(
        turn.input.blocks.first(),
        Some(TurnInputBlock::Image { .. })
    ));
}

#[test]
fn middle_chip_delete_renumbers_remaining_chips_in_lockstep() {
    // Two images: deleting the FIRST must renumber the second to `[图片 1]`
    // and keep it bound to its OWN path (a naive Vec::remove would submit the
    // wrong file or drop one).
    let mut app = fresh_app(Some("offline"));
    let dir = tempfile::TempDir::new().unwrap();
    let img1 = dir.path().join("one.png");
    let img2 = dir.path().join("two.png");
    std::fs::write(&img1, b"\x89PNG\r\n\x1a\n1").unwrap();
    std::fs::write(&img2, b"\x89PNG\r\n\x1a\n2").unwrap();
    app.handle_paste(img1.to_str().unwrap()); // "[图片 1] "
    app.handle_paste(img2.to_str().unwrap()); // "[图片 1] [图片 2] "
    assert_eq!(app.attachments.len(), 2);
    let abs2 = std::fs::canonicalize(&img2).unwrap();
    // Delete the FIRST chip: caret right after its `]` (char index = chip len).
    let first_end = app.image_chip(1).chars().count();
    app.input_cursor = first_end;
    app.backspace();
    assert_eq!(app.attachments.len(), 1, "one image left");
    // The survivor renumbered to `[图片 1]` and still expands to img2's path.
    assert!(
        app.input.contains(&app.image_chip(1)) && !app.input.contains(&app.image_chip(2)),
        "survivor renumbered to chip 1, got: {:?}",
        app.input
    );
    let turn = app.compose_submitted_turn(app.input.trim());
    assert!(matches!(
        turn.input.blocks.first(),
        Some(TurnInputBlock::Image { path }) if path == &abs2
    ));
}

#[test]
fn ctrl_w_swallows_a_chip_flush_against_the_caret() {
    let mut app = fresh_app(Some("offline"));
    app.insert_str_at_cursor("hi ");
    let _dir = attach_one_image(&mut app); // "hi [图片 1] "
                                           // Trim the trailing space so the caret is flush against `]`.
    app.backspace();
    assert!(app.input.ends_with(']'));
    app.delete_word_back(); // Ctrl+W
    assert_eq!(app.input, "hi ", "the whole chip is one word-kill unit");
    assert!(app.attachments.is_empty(), "ref dropped by Ctrl+W");
}

#[test]
fn ctrl_u_clears_chips_and_drops_all_refs_no_orphan() {
    let mut app = fresh_app(Some("offline"));
    let _dir = attach_one_image(&mut app); // "[图片 1] "
    app.insert_str_at_cursor("tail");
    app.delete_to_line_start(); // Ctrl+U from end → wipes the line
    assert!(app.input.is_empty(), "line cleared");
    assert!(
        app.attachments.is_empty(),
        "no orphaned image ref after Ctrl+U"
    );
}

#[test]
fn chip_delete_is_fail_open_on_a_cursor_past_the_buffer() {
    // A desynced caret must never panic the editing helpers.
    let mut app = fresh_app(Some("offline"));
    let _dir = attach_one_image(&mut app);
    app.input_cursor = app.input_len() + 5; // bogus, out of range
    app.backspace(); // must not panic
    app.forward_delete(); // must not panic
    assert!(app.input_cursor <= app.input_len());
}

#[test]
fn restart_is_fresh_and_explicit_resume_restores_the_exact_conversation() {
    // Persistence remains durable, but a process launch is a new task boundary.
    let (mut app, tmp) = temp_app();
    app.record_user_turn("我在做一个看板应用");
    // A host chat turn captured the base's OWN resumable session id (claude's
    // pinned `--session-id`) — it must survive the restart so the base resumes its
    // DEEP context, not just the replayed transcript.
    record_agentic_done_with_session(&mut app, "好的,已经开始搭建。", "base-sess-kanban");
    let saved_id = app.chat_id.clone();
    assert_eq!(app.conversation.len(), 2);

    // Simulate a restart: a brand-new App over the SAME project root.
    let cfg = UserConfig {
        backend: Some("claude-code".to_string()),
        lang: Some("zh-CN".to_string()),
        ..Default::default()
    };
    let mut app2 = App::new(
        "demo",
        cfg,
        tmp.path().join("config.toml"),
        tmp.path().to_path_buf(),
    );
    // Fresh launch: no prior transcript and, critically, no native resume id.
    assert_ne!(app2.chat_id, saved_id, "restart mints a new logical chat");
    assert!(app2.conversation.is_empty());
    assert!(app2.chat_session_id.is_none());
    assert!(!app2.host_chat_session_active);

    // Only an explicit restore imports the transcript and native pointer.
    assert!(app2.load_chat(&saved_id));
    assert_eq!(app2.chat_id, saved_id);
    assert_eq!(app2.conversation.len(), 2);
    assert_eq!(app2.conversation[0].content, "我在做一个看板应用");
    assert_eq!(app2.conversation[1].role, "assistant");
    // The base session id is restored so the NEXT host chat turn RESUMES the base's
    // deep context (the resident pre-load opens it via `--resume <id>`), and the
    // session is flagged active. This is the cross-session base-memory fix.
    assert_eq!(
        app2.chat_session_id.as_deref(),
        Some("base-sess-kanban"),
        "explicit resume restores the base's resumable session id, not the chat file id"
    );
    assert!(
        app2.host_chat_session_active,
        "a restored base session id flags the chat session active"
    );
    assert!(
        app2.chat_session_dirty,
        "explicit restore invalidates any resident before its preload"
    );
    // The explicit restore boundary is visible.
    assert!(app2.history.iter().any(|m| m.role == ChatRole::System
        && m.body() == umadev_i18n::t(app2.lang, "chat.restored_divider")));
}

#[test]
fn restoring_a_chat_never_reuses_a_session_id_on_another_backend() {
    let (mut app, tmp) = temp_app();
    app.record_user_turn("在 Claude 中开始的会话");
    record_agentic_done_with_session(&mut app, "已记录上下文", "claude-native-session");
    let saved_id = app.chat_id.clone();
    drop(app);

    let cfg = UserConfig {
        backend: Some("codex".to_string()),
        lang: Some("zh-CN".to_string()),
        ..Default::default()
    };
    let mut restored = App::new(
        "demo",
        cfg,
        tmp.path().join("codex-config.toml"),
        tmp.path().to_path_buf(),
    );
    assert!(
        restored.conversation.is_empty(),
        "a fresh launch never imports another base's transcript"
    );
    assert!(restored.load_chat(&saved_id));
    assert_eq!(restored.backend.as_deref(), Some("codex"));
    assert!(restored.chat_session_id.is_none());
    assert!(!restored.host_chat_session_active);
    assert!(restored.conversation.iter().any(|message| {
        message.role == "system"
            && message.content.contains("claude-code")
            && message.content.contains("codex")
    }));

    let saved = std::fs::read_to_string(restored.chat_path(&saved_id)).unwrap();
    let saved: ChatSession = serde_json::from_str(&saved).unwrap();
    assert_eq!(saved.backend, "codex");
    assert!(saved.base_session_id.is_none());
}

#[test]
fn slash_sessions_lists_saved_chats_and_resume_reopens_one() {
    let (mut app, _tmp) = temp_app();
    // Chat A — capture its OWN base session id (claude's pinned `--session-id`).
    app.record_user_turn("第一个对话");
    record_agentic_done_with_session(&mut app, "reply A", "base-A");
    let id_a = app.chat_id.clone();
    // `/clear` starts a FRESH persistent chat (A stays on disk).
    let _ = app.try_slash_command("/clear");
    assert_ne!(app.chat_id, id_a, "/clear mints a new chat id");
    app.record_user_turn("第二个对话");
    record_agentic_done_with_session(&mut app, "reply B", "base-B");

    // `/sessions` lists BOTH saved chats.
    let _ = app.try_slash_command("/sessions");
    assert!(app
        .history
        .iter()
        .any(|m| m.body().contains(&id_a) && m.body().contains("已保存")));

    // `/resume <id_a>` reopens chat A's transcript. Clear the dirty flag first so
    // the assertion below proves `/resume` (not the earlier `/clear`) set it.
    app.chat_session_dirty = false;
    let _ = app.try_slash_command(&format!("/resume {id_a}"));
    assert_eq!(app.chat_id, id_a);
    assert_eq!(app.conversation[0].content, "第一个对话");
    // The base session is pinned to chat A's OWN persisted base session id
    // (`base-A`), NOT the chat FILE id (`id_a`) — the bug fix: a host CLI resumes
    // the conversation IT created, not an id it never saw. The resident session
    // is flagged dirty so the loop re-opens against the resumed base id.
    assert_eq!(app.chat_session_id.as_deref(), Some("base-A"));
    assert!(app.host_chat_session_active);
    assert!(
        app.chat_session_dirty,
        "/resume flags the resident session for re-open against the resumed base id"
    );
}

#[test]
fn backend_switch_keeps_conversation_clears_resume_id_and_records_one_handoff() {
    // The reported context-loss chain, link by link: a `/backend` switch must
    // (a) KEEP `conversation` (the bounded transcript the new base's first
    // directive front-loads), (b) CLEAR the old base's resumable session id (a
    // claude id means nothing to codex — the post-switch pre-load must open
    // FRESH, `resume_session_id = None`), and (c) record exactly ONE context
    // handoff block into the conversation so the new base + the user both see
    // what carried over.
    let (mut app, _tmp) = temp_app();
    app.record_user_turn("MARKER-EARLIER-TURN 我要一个登录页");
    record_agentic_done_with_session(&mut app, "已完成登录页初版", "claude-sess-1");
    assert_eq!(app.chat_session_id.as_deref(), Some("claude-sess-1"));
    let chat_id_before = app.chat_id.clone();

    let action = app.slash_backend(Some("codex"));
    assert_eq!(action, Action::BackendChanged);
    // (a) the conversation SURVIVES the switch.
    assert!(
        app.conversation
            .iter()
            .any(|m| m.content.contains("MARKER-EARLIER-TURN")),
        "the bounded transcript must survive a backend switch"
    );
    assert_eq!(
        app.chat_id, chat_id_before,
        "a backend switch never re-mints the chat id"
    );
    // (b) the old base's resume pointer is invalidated → the `BackendChanged`
    // pre-load passes `None` (`app.chat_session_id.clone()`), a fresh open.
    assert!(
        app.chat_session_id.is_none(),
        "a claude session id must not be resumed on codex"
    );
    assert!(!app.host_chat_session_active);
    assert!(
        app.chat_session_dirty,
        "the old base's resident session is flagged for close"
    );
    // (c) exactly ONE handoff block, in the conversation (the new base sees it)
    // AND on screen (the user sees it).
    let handoff = umadev_i18n::tf(app.lang, "backend.handoff", &["claude-code", "codex"]);
    assert_eq!(
        app.conversation
            .iter()
            .filter(|m| m.role == "system" && m.content == handoff)
            .count(),
        1,
        "exactly one context-handoff block is recorded"
    );
    assert!(
        app.history
            .iter()
            .any(|m| m.role == ChatRole::System && m.body() == handoff),
        "the handoff note is also shown to the user"
    );
    // Switching back records a SECOND, direction-reversed handoff (once per switch).
    let _ = app.slash_backend(Some("claude-code"));
    let back = umadev_i18n::tf(app.lang, "backend.handoff", &["codex", "claude-code"]);
    assert_eq!(
        app.conversation
            .iter()
            .filter(|m| m.content == back)
            .count(),
        1
    );
}

#[test]
fn backend_switch_records_no_handoff_for_same_base_or_empty_chat() {
    // Same target → not a real switch → no handoff block.
    let (mut app, _tmp) = temp_app();
    app.record_user_turn("你好");
    let _ = app.slash_backend(Some("claude-code"));
    assert!(
        !app.conversation.iter().any(|m| m.role == "system"),
        "re-selecting the current base records no handoff"
    );
    // Empty conversation → nothing to hand over → no block (fail-open).
    let (mut app, _tmp) = temp_app();
    let _ = app.slash_backend(Some("codex"));
    assert!(
        app.conversation.is_empty(),
        "an empty chat records no handoff block"
    );
}

#[test]
fn resumed_chat_survives_a_backend_switch_into_the_new_base_first_directive() {
    // The exact reported repro, end to end: work on claude in one process →
    // relaunch in ANOTHER process → resume the saved chat by id → switch to
    // codex → the NEXT turn's first directive (what codex actually receives)
    // must carry the pre-resume dialogue AND the handoff marker. This is the
    // full chain: persist → load_chat → slash_backend → conversation_snapshot →
    // first_chat_directive.
    let (mut app, tmp) = temp_app();
    app.record_user_turn("MARKER-RESUME-A17 我们决定数据层用 SQLite");
    record_agentic_done_with_session(
        &mut app,
        "好的,就用 SQLite,表结构已经定了",
        "claude-deep-sess-9",
    );
    let saved_chat_id = app.chat_id.clone();
    drop(app);

    // A brand-new process over the SAME project root (the native-terminal case).
    let cfg = UserConfig {
        backend: Some("claude-code".to_string()),
        lang: Some("zh-CN".to_string()),
        ..Default::default()
    };
    let mut app2 = App::new(
        "demo",
        cfg,
        tmp.path().join("config2.toml"),
        tmp.path().to_path_buf(),
    );
    // Resume the saved chat explicitly (the `/resume <id>` path).
    let _ = app2.try_slash_command(&format!("/resume {saved_chat_id}"));
    assert_eq!(app2.chat_id, saved_chat_id);
    assert_eq!(
        app2.chat_session_id.as_deref(),
        Some("claude-deep-sess-9"),
        "the resumed chat restores the OLD base's resumable id"
    );

    // Switch to codex mid-conversation.
    let action = app2.slash_backend(Some("codex"));
    assert_eq!(action, Action::BackendChanged);
    assert!(
        app2.chat_session_id.is_none(),
        "the claude session id is dropped — the codex pre-load opens fresh"
    );

    // The NEXT turn's first directive on the new base: front-load the snapshot
    // exactly as `drive_chat_session_turn` does for a Warm codex session.
    let convo = app2.conversation_snapshot();
    let route = umadev_agent::deterministic_route("接着把接口做完");
    let directive = crate::first_chat_directive(
        Some("FW-CODEX"),
        "codex",
        &convo,
        "接着把接口做完",
        "接着把接口做完",
        &route,
    );
    assert!(
        directive.contains("MARKER-RESUME-A17"),
        "the first directive on the new base must carry the resumed dialogue: {directive:?}"
    );
    assert!(
        directive.contains("SQLite"),
        "prior decisions ride the front-load: {directive:?}"
    );
    let handoff = umadev_i18n::tf(app2.lang, "backend.handoff", &["claude-code", "codex"]);
    assert!(
        directive.contains(&handoff),
        "the handoff block reaches the new base inside the front-load: {directive:?}"
    );
    assert!(
        directive.starts_with("FW-CODEX"),
        "codex has no native system slot — firmware is front-loaded too"
    );
    assert!(
        directive.contains("接着把接口做完"),
        "the current ask closes the directive"
    );
}

#[test]
fn resume_unknown_id_is_fail_open() {
    let (mut app, _tmp) = temp_app();
    app.record_user_turn("hi");
    let before = app.conversation.clone();
    let _ = app.try_slash_command("/resume does-not-exist");
    // The live conversation is untouched; a clear note explains why.
    assert_eq!(app.conversation, before);
    assert!(app
        .history
        .iter()
        .any(|m| m.role == ChatRole::System && m.body().contains("没找到")));
}

/// (a) `ChatSession` round-trips `base_session_id`, and an OLD chat file written
/// before the field existed deserializes to `None` (back-compat / fail-open).
#[test]
fn chat_session_round_trips_base_session_id_and_is_back_compat() {
    // New schema → the base session id survives a serialize/deserialize cycle.
    let s = ChatSession {
        id: "chat-1".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        backend: "claude-code".to_string(),
        base_session_id: Some("base-xyz".to_string()),
        base_resume_identity: None,
        messages: vec![umadev_runtime::Message {
            role: "user".to_string(),
            content: "hi".to_string(),
        }],
        display: Some(vec![ChatMessage {
            role: ChatRole::You,
            kind: MessageBody::Text("hi".to_string()),
            collapsed: false,
        }]),
    };
    let json = serde_json::to_string(&s).unwrap();
    let back: ChatSession = serde_json::from_str(&json).unwrap();
    assert_eq!(back.base_session_id.as_deref(), Some("base-xyz"));
    // Wave 3 — the display transcript round-trips through the same file.
    let display = back.display.expect("the display rows round-trip");
    assert_eq!(display.len(), 1);
    assert_eq!(display[0].role, ChatRole::You);
    assert_eq!(display[0].body(), "hi");

    // OLD file (no `base_session_id` / `display` key) → `#[serde(default)]`
    // yields `None` for both (back-compat / fail-open).
    let legacy = r#"{"id":"old","updated_at":"x","backend":"codex",
            "messages":[{"role":"user","content":"hi"}]}"#;
    let parsed: ChatSession = serde_json::from_str(legacy).unwrap();
    assert_eq!(
        parsed.base_session_id, None,
        "an old chat file without the field loads as None (back-compat)"
    );
    assert_eq!(parsed.base_resume_identity, None);
    assert_eq!(
        parsed.display, None,
        "an old chat file without display rows loads as None (Wave 3 back-compat)"
    );
    assert_eq!(parsed.messages.len(), 1, "the transcript still loads");
}

/// (c) `persist_chat` writes the LIVE `chat_session_id` into the saved
/// `base_session_id`; (b) `load_chat` restores it into `chat_session_id` and flags
/// the host chat session active.
#[test]
fn persist_writes_and_load_restores_the_base_session_id() {
    let (mut app, _tmp) = temp_app();
    app.record_user_turn("第一句");
    // The live base session id (captured off a host turn) is persisted.
    app.chat_session_id = Some("base-live".to_string());
    app.chat_resume_identity = crate::session_slot::requested_resume_identity(
        "claude-code",
        &app.project_root,
        app.effective_trust_mode().base_permissions(),
    );
    app.persist_chat();
    let saved_id = app.chat_id.clone();

    // (c) The on-disk record carries the base session id.
    let path = app.chat_path(&saved_id);
    let text = std::fs::read_to_string(&path).unwrap();
    let on_disk: ChatSession = serde_json::from_str(&text).unwrap();
    assert_eq!(on_disk.base_session_id.as_deref(), Some("base-live"));
    assert!(on_disk.base_resume_identity.is_some());

    // (b) A fresh App with the id cleared, then `load_chat`, restores it + flags.
    app.chat_session_id = None;
    app.chat_resume_identity = None;
    app.host_chat_session_active = false;
    app.chat_session_dirty = false;
    assert!(app.load_chat(&saved_id), "the saved chat loads");
    assert_eq!(
        app.chat_session_id.as_deref(),
        Some("base-live"),
        "load_chat restores the base session id"
    );
    assert!(
        app.host_chat_session_active,
        "a restored base session id flags the host chat session active"
    );
    assert!(
        app.chat_session_dirty,
        "even a same-backend restore replaces the context and must close the old resident"
    );
}

/// (b, fail-open) Loading a chat whose `base_session_id` is `None` (an old file /
/// opencode / offline) leaves `chat_session_id` `None` and does NOT force the
/// session active — degrading cleanly to today's fresh-session behavior.
#[test]
fn load_chat_with_no_base_session_id_is_fail_open() {
    let (mut app, _tmp) = temp_app();
    app.record_user_turn("仅文本");
    app.chat_session_id = None; // no base id captured (e.g. opencode)
    app.persist_chat();
    let saved_id = app.chat_id.clone();

    app.host_chat_session_active = false;
    assert!(app.load_chat(&saved_id));
    assert_eq!(
        app.chat_session_id, None,
        "no base session id → stays None (fresh session next turn)"
    );
    assert!(
        !app.host_chat_session_active,
        "a None base session id never force-flags the session active"
    );
}

#[test]
fn grok_chat_with_requested_only_sandbox_identity_reopens_fresh() {
    let tmp = tempfile::TempDir::new().unwrap();
    let cfg = UserConfig {
        backend: Some("grok-build".to_string()),
        lang: Some("zh-CN".to_string()),
        ..Default::default()
    };
    let mut app = App::new(
        "demo",
        cfg,
        tmp.path().join("config.toml"),
        tmp.path().to_path_buf(),
    );
    app.record_user_turn("继续这个 Grok 任务");
    app.chat_session_id = Some("grok-native-session".to_string());
    app.chat_resume_identity = crate::session_slot::requested_resume_identity(
        "grok-build",
        &app.project_root,
        app.effective_trust_mode().base_permissions(),
    );
    app.persist_chat();
    let saved_id = app.chat_id.clone();

    app.chat_session_id = None;
    app.chat_resume_identity = None;
    assert!(app.load_chat(&saved_id));
    assert_eq!(app.chat_session_id, None);
    assert_eq!(app.chat_resume_identity, None);
    assert!(
        app.conversation
            .iter()
            .any(|message| message.content.contains("继续这个 Grok 任务")),
        "durable transcript remains the fresh-session handoff"
    );
}

#[test]
fn grok_legacy_and_identity_mismatch_never_authorize_acp_load() {
    let workspace = tempfile::TempDir::new().unwrap();
    let other = tempfile::TempDir::new().unwrap();
    let requested = crate::session_slot::requested_resume_identity(
        "grok-build",
        workspace.path(),
        umadev_runtime::BasePermissionProfile::Guarded,
    )
    .unwrap();
    assert!(!chat_resume_identity_allows_load(
        "grok-build",
        "grok-build",
        None,
        Some(&requested),
    ));

    let wrong_workspace = crate::session_slot::requested_resume_identity(
        "grok-build",
        other.path(),
        umadev_runtime::BasePermissionProfile::Guarded,
    )
    .unwrap();
    let wrong_profile = crate::session_slot::requested_resume_identity(
        "grok-build",
        workspace.path(),
        umadev_runtime::BasePermissionProfile::Auto,
    )
    .unwrap();
    for saved in [&wrong_workspace, &wrong_profile] {
        assert!(!chat_resume_identity_allows_load(
            "grok-build",
            "grok-build",
            Some(saved),
            Some(&requested),
        ));
    }

    let codex_requested = crate::session_slot::requested_resume_identity(
        "codex",
        workspace.path(),
        umadev_runtime::BasePermissionProfile::Plan,
    )
    .unwrap();
    assert!(
        !chat_resume_identity_allows_load("codex", "codex", None, Some(&codex_requested),),
        "legacy chat ids have no proof that the saved launch was Plan rather than Auto"
    );
}

/// Wave 3 P0 — the VISIBLE display transcript round-trips: rich rows (a
/// structured tool row with its result, a system note) survive persist →
/// relaunch verbatim, a tool row saved mid-flight settles to Aborted, the
/// restored transcript ends with the divider, and the view re-pins to the
/// bottom.
#[test]
fn display_transcript_round_trips_rich_rows_and_ends_with_divider() {
    let (mut app, tmp) = temp_app();
    app.push(ChatRole::You, "做一个看板");
    app.push(ChatRole::Host, "好的，开始搭建。");
    // A finished tool row (result attached) + one still in flight.
    app.history.push_back(ChatMessage {
        role: ChatRole::Host,
        kind: MessageBody::Tool(ToolCall {
            call_id: None,
            name: "Bash".to_string(),
            arg: "npm test".to_string(),
            status: ToolStatus::Ok,
            result: Some("42 tests passed".to_string()),
            progress: None,
            merged: false,
            count: 1,
            collapsed: true,
        }),
        collapsed: false,
    });
    app.history.push_back(ChatMessage {
        role: ChatRole::Host,
        kind: MessageBody::Tool(ToolCall {
            call_id: None,
            name: "Write".to_string(),
            arg: "src/app.tsx".to_string(),
            status: ToolStatus::Running,
            result: None,
            progress: None,
            merged: false,
            count: 1,
            collapsed: false,
        }),
        collapsed: false,
    });
    app.push(ChatRole::System, "note: checkpoint saved");
    // A recorded exchange makes the chat persistable; `record_agentic_done`
    // persists (transcript + the display snapshot above).
    app.record_user_turn("做一个看板");
    app.record_agentic_done("好的，开始搭建。".to_string(), false, None, None);
    let saved_id = app.chat_id.clone();

    // Simulate a restart: a brand-new App over the SAME project root.
    let cfg = UserConfig {
        backend: Some("claude-code".to_string()),
        lang: Some("zh-CN".to_string()),
        ..Default::default()
    };
    let mut app2 = App::new(
        "demo",
        cfg,
        tmp.path().join("config.toml"),
        tmp.path().to_path_buf(),
    );
    assert!(app2.history.iter().all(|message| {
            !matches!(&message.kind, MessageBody::Tool(tool) if tool.name == "Bash" || tool.name == "Write")
        }));
    assert!(app2.load_chat(&saved_id));
    // The rich rows were REBUILT, not flattened: the finished tool row keeps
    // its structure, result, and fold state...
    let ok_tool = app2
        .history
        .iter()
        .find_map(|m| match &m.kind {
            MessageBody::Tool(t) if t.name == "Bash" => Some(t.clone()),
            _ => None,
        })
        .expect("the finished tool row survives the relaunch");
    assert_eq!(ok_tool.status, ToolStatus::Ok);
    assert_eq!(ok_tool.result.as_deref(), Some("42 tests passed"));
    assert!(ok_tool.collapsed, "the fold state survives");
    // ...the mid-flight tool row settled to Aborted (nothing spins forever)...
    let write_tool = app2
        .history
        .iter()
        .find_map(|m| match &m.kind {
            MessageBody::Tool(t) if t.name == "Write" => Some(t.clone()),
            _ => None,
        })
        .expect("the in-flight tool row survives the relaunch");
    assert_eq!(
        write_tool.status,
        ToolStatus::Aborted,
        "a restored mid-flight tool row settles to Aborted"
    );
    // ...and the prose + note rows are back too.
    assert!(app2
        .history
        .iter()
        .any(|m| m.role == ChatRole::You && m.body() == "做一个看板"));
    assert!(app2
        .history
        .iter()
        .any(|m| m.role == ChatRole::System && m.body() == "note: checkpoint saved"));
    // The restored transcript ends at the explicit boundary divider, and
    // the view is pinned to the bottom.
    let divider = umadev_i18n::t(app2.lang, "chat.restored_divider");
    let rows: Vec<String> = app2.history.iter().map(|m| m.body().to_string()).collect();
    let divider_idx = rows
        .iter()
        .position(|b| b == divider)
        .expect("the restore divider is present");
    assert_eq!(
        divider_idx,
        rows.len() - 1,
        "the restored rows end with the divider"
    );
    assert_eq!(
        app2.transcript_scroll.get(),
        0,
        "the restored transcript lands pinned to the bottom"
    );
}

/// Wave 3 P0 back-compat — an OLD session file (persisted before the
/// `display` field existed) still reopens VISIBLE: the prose conversation is
/// seeded from the durable transcript (user → You, assistant → Host), ends
/// with the divider, and never crashes.
#[test]
fn old_session_file_without_display_seeds_prose_history() {
    let (mut app, _tmp) = temp_app();
    let dir = app.project_root.join(".umadev").join("chat");
    std::fs::create_dir_all(&dir).unwrap();
    let legacy = r#"{"id":"legacy-1","updated_at":"2026-01-01T00:00:00Z","backend":"claude-code",
            "messages":[{"role":"user","content":"老问题"},{"role":"assistant","content":"老回答"}]}"#;
    std::fs::write(dir.join("legacy-1.json"), legacy).unwrap();

    assert!(
        app.load_chat("legacy-1"),
        "an old file without display rows still loads"
    );
    assert!(
        app.history
            .iter()
            .any(|m| m.role == ChatRole::You && m.body() == "老问题"),
        "the user turn is seeded as a visible You row"
    );
    assert!(
        app.history
            .iter()
            .any(|m| m.role == ChatRole::Host && m.body() == "老回答"),
        "the assistant turn is seeded as a visible Host row"
    );
    assert_eq!(
        app.history.back().unwrap().body(),
        umadev_i18n::t(app.lang, "chat.restored_divider"),
        "the seeded transcript ends with the restore divider"
    );
    assert_eq!(app.transcript_scroll.get(), 0);
}

/// Wave 3 P0 fail-open — a corrupt `display` field can never take the chat
/// down: a wrong-typed field falls back to prose seeding, and a corrupt ROW
/// inside an otherwise-good array is skipped element-wise while the valid
/// rows survive.
#[test]
fn corrupt_display_field_falls_back_cleanly() {
    let (mut app, _tmp) = temp_app();
    let dir = app.project_root.join(".umadev").join("chat");
    std::fs::create_dir_all(&dir).unwrap();

    // (a) `display` is the wrong TYPE entirely → lenient parse yields None →
    // the prose conversation is seeded from the durable transcript.
    let wrong_type = r#"{"id":"corrupt-a","updated_at":"x","backend":"codex",
            "messages":[{"role":"user","content":"你好"}],"display":"not-an-array"}"#;
    std::fs::write(dir.join("corrupt-a.json"), wrong_type).unwrap();
    assert!(
        app.load_chat("corrupt-a"),
        "a corrupt display field never fails the load"
    );
    assert!(
        app.history
            .iter()
            .any(|m| m.role == ChatRole::You && m.body() == "你好"),
        "fell back to prose seeding from the durable transcript"
    );

    // (b) ONE corrupt row inside the array is skipped; the valid row survives
    // (element-wise leniency — never all-or-nothing).
    let mixed = r#"{"id":"corrupt-b","updated_at":"x","backend":"codex",
            "messages":[{"role":"user","content":"第二"}],
            "display":[{"bogus":true},{"role":"System","kind":{"Text":"手写行"},"collapsed":false}]}"#;
    std::fs::write(dir.join("corrupt-b.json"), mixed).unwrap();
    assert!(app.load_chat("corrupt-b"));
    assert!(
        app.history
            .iter()
            .any(|m| m.role == ChatRole::System && m.body() == "手写行"),
        "the valid display row survives element-wise parsing"
    );
    assert!(
        !app.history.iter().any(|m| m.body() == "第二"),
        "the display path was used — no prose-seeding duplicate"
    );
}

#[test]
fn slash_compact_runs_the_structured_summary_path() {
    // `/compact` now folds via the SAME structured-summary path as
    // auto-compaction (a forked base `complete()`), NOT the old lossy 160-char
    // digest. The slash handler validates + signals `Action::Compact`; the
    // event loop drives the fork; `apply_compaction` splices the result.
    let (mut app, _tmp) = temp_app();
    for i in 0..12 {
        app.record_user_turn(&format!("user message {i}"));
        app.record_agentic_done(format!("assistant reply {i}"), false, None, None);
    }
    // The slash handler signals intent (and pushes the "compacting…" note).
    let action = app.try_slash_command("/compact").expect("a slash command");
    assert!(matches!(action, Action::Compact));
    // A manual job folds everything except the recent verbatim tail.
    let job = app.begin_manual_compaction().expect("enough to fold");
    assert!(job.fold_count >= umadev_agent::compaction::MIN_FOLD);
    let before = app.conversation.len();
    // Apply a stand-in structured summary — the same call the event loop makes
    // when the fork returns its summary.
    app.apply_compaction(
        "## Intent / Goal\nBuild a kanban board.",
        job.fold_count,
        job.generation,
    );
    let after = app.conversation.len();
    assert!(
        after < before,
        "compact must shrink the working view: {before}->{after}"
    );
    // The leading block is the structured summary (a user-role grounding note)
    // and carries both the localized header and the model's section text.
    assert_eq!(app.conversation[0].role, "user");
    assert!(
        app.conversation[0].content.contains("摘要"),
        "the summary block carries the localized header"
    );
    assert!(
        app.conversation[0].content.contains("Intent / Goal"),
        "the structured summary body is preserved"
    );
    // The most-recent turn is preserved verbatim.
    assert_eq!(
        app.conversation.last().unwrap().content,
        "assistant reply 11"
    );
    // The on-disk FULL transcript is untouched — every turn still present.
    assert_eq!(app.full_transcript.len(), 24);
    assert_eq!(
        app.full_transcript.last().unwrap().content,
        "assistant reply 11"
    );
}

/// Build a conversation whose estimated token cost is comfortably over
/// [`COMPACTION_TOKEN_BUDGET`], so the auto-compaction trigger fires.
fn fill_over_budget(app: &mut App, exchanges: usize) {
    for i in 0..exchanges {
        app.record_user_turn(&format!("u{i} {}", "alpha ".repeat(80)));
        app.record_agentic_done(format!("a{i} {}", "beta ".repeat(80)), false, None, None);
    }
}

#[test]
fn auto_compaction_triggers_near_budget_and_keeps_tail_verbatim() {
    // The token-budgeted trigger fires once the working transcript crosses the
    // budget; applying the summary replaces the older prefix with ONE block and
    // keeps the recent tail word-for-word.
    let (mut app, _tmp) = temp_app();
    fill_over_budget(&mut app, 16); // 32 messages of long content
    assert!(
        app.should_auto_compact(),
        "a transcript over the token budget triggers compaction"
    );
    let total = app.conversation.len();
    let last_user = app.conversation[total - 2].content.clone();
    let last_asst = app.conversation[total - 1].content.clone();
    let full_before = app.full_transcript.len();

    let job = app.begin_auto_compaction().expect("a job near budget");
    assert!(app.compaction_in_flight, "a job is now in flight");
    assert!(job.fold_count >= umadev_agent::compaction::MIN_FOLD);
    assert!(
        job.fold_count < total,
        "the recent tail must survive the fold"
    );

    app.apply_compaction(
        "## Current work\nWiring the API.",
        job.fold_count,
        job.generation,
    );
    assert!(!app.compaction_in_flight, "the job settled");
    // [structured summary] + [recent verbatim tail].
    assert_eq!(app.conversation[0].role, "user");
    assert!(app.conversation[0].content.contains("Current work"));
    assert!(app.conversation[0].content.contains("摘要"));
    assert_eq!(
        app.conversation.last().unwrap().content,
        last_asst,
        "the most-recent reply is kept verbatim"
    );
    assert_eq!(
        app.conversation[app.conversation.len() - 2].content,
        last_user,
        "the most-recent user turn is kept verbatim"
    );
    // The compacted working view is strictly smaller than the full history.
    assert!(app.conversation.len() < full_before);
    // The on-disk FULL transcript is untouched by compaction.
    assert_eq!(app.full_transcript.len(), full_before);
    assert_eq!(app.full_transcript.last().unwrap().content, last_asst);
}

#[test]
fn apply_compaction_marks_resident_session_dirty() {
    // After a SUCCESSFUL fold the resident base session still holds the FULL
    // pre-compaction history in its own process memory, and that is what drives
    // each turn — so `apply_compaction` must flag it for close. The event loop
    // then reopens a FRESH session that front-loads the COMPACTED transcript,
    // which is what stops the base re-emitting folded turns (history bleed) and
    // driving in stale build context (a plain question misrouted as a build).
    let (mut app, _tmp) = temp_app();
    fill_over_budget(&mut app, 16);
    let job = app.begin_auto_compaction().expect("a job near budget");
    // Seed an ACTIVE base-session pin so we can prove the fold breaks it: a fresh
    // open that RESUMED this id would reload the base's full uncompacted native
    // history and defeat the fold.
    app.chat_session_id = Some("sid".into());
    app.host_chat_session_active = true;
    assert!(
        !app.chat_session_dirty,
        "no pending resident-close before the fold settles"
    );
    app.apply_compaction(
        "## Current work\nWiring the API.",
        job.fold_count,
        job.generation,
    );
    assert!(
        app.chat_session_dirty,
        "a successful fold flags the resident base session for close so the next \
             turn reopens fresh against the compacted transcript"
    );
    assert!(
        app.chat_session_id.is_none(),
        "the fold clears the base-session pin so the next open is truly fresh, not \
             a resume of the full uncompacted native history"
    );
    assert!(
        !app.host_chat_session_active,
        "the fold breaks the active base session so the next turn does not continue \
             the pre-compaction session"
    );
}

#[test]
fn failed_summary_falls_back_to_fifo_without_losing_history() {
    // Fail-open: a failed / empty / offline summary must NOT lose or corrupt the
    // conversation — it falls back to the original FIFO drop on the working view,
    // and the full transcript on disk is untouched.
    let (mut app, _tmp) = temp_app();
    for i in 0..40 {
        app.record_user_turn(&format!("m{i}"));
    }
    let full_before = app.full_transcript.len();
    // Pretend a summary was in flight and then failed.
    app.compaction_in_flight = true;
    let generation = app.conversation_generation;
    app.fail_compaction(generation);
    assert!(!app.compaction_in_flight, "the failed job is cleared");
    // Working view FIFO-bounded; the most-recent turn is still there (no corruption).
    assert!(app.conversation.len() <= CONVERSATION_CAP);
    assert_eq!(app.conversation.last().unwrap().content, "m39");
    // The full transcript on disk kept EVERY message — nothing lost.
    assert_eq!(app.full_transcript.len(), full_before);
    assert_eq!(app.full_transcript.last().unwrap().content, "m39");
}

#[test]
fn circuit_breaker_suppresses_auto_compaction_while_tripped() {
    // The breaker bounds retries: after N consecutive summary failures the
    // trigger is suppressed (no more wasted base calls) until a success resets it.
    let (mut app, _tmp) = temp_app();
    fill_over_budget(&mut app, 16);
    assert!(app.should_auto_compact(), "over budget → would compact");
    for _ in 0..umadev_agent::compaction::Breaker::LIMIT {
        app.compaction_breaker.record_failure();
    }
    assert!(
        !app.should_auto_compact(),
        "a tripped breaker suppresses the trigger even while over budget"
    );
    assert!(app.begin_auto_compaction().is_none());
    // A later success un-trips it → compaction resumes.
    app.compaction_breaker.record_success();
    assert!(app.should_auto_compact());
}

#[test]
fn stale_compaction_result_is_dropped_after_clear() {
    // A summary that returns AFTER a `/clear` carries a stale generation and must
    // be dropped — it can never splice into the fresh conversation.
    let (mut app, _tmp) = temp_app();
    fill_over_budget(&mut app, 16);
    let job = app.begin_auto_compaction().expect("a job");
    for _ in 0..umadev_agent::compaction::Breaker::LIMIT {
        app.compaction_breaker.record_failure();
    }
    // `/clear` happens while the summary is in flight → generation bumps.
    let _ = app.try_slash_command("/clear");
    let convo_after_clear = app.conversation.clone();
    app.apply_compaction("late summary", job.fold_count, job.generation);
    assert_eq!(
        app.conversation, convo_after_clear,
        "a stale summary is dropped, not spliced into the cleared conversation"
    );
    assert!(
        app.compaction_breaker.tripped(),
        "a stale success must not reset the new generation's breaker"
    );
}

#[test]
fn stale_compaction_failure_cannot_trim_or_trip_after_clear() {
    let (mut app, _tmp) = temp_app();
    fill_over_budget(&mut app, 16);
    let job = app.begin_auto_compaction().expect("an old-generation job");
    for _ in 1..umadev_agent::compaction::Breaker::LIMIT {
        app.compaction_breaker.record_failure();
    }
    assert!(!app.compaction_breaker.tripped());

    let _ = app.try_slash_command("/clear");
    for i in 0..24 {
        app.record_user_turn(&format!("fresh-{i}"));
    }
    let fresh = app.conversation.clone();
    app.fail_compaction(job.generation);

    assert_eq!(
        app.conversation, fresh,
        "the old failure must not FIFO-trim the fresh conversation"
    );
    assert!(
        !app.compaction_breaker.tripped(),
        "the old failure must not advance the fresh generation's breaker"
    );
}

#[test]
fn stale_compaction_failure_cannot_trim_or_trip_loaded_chat() {
    let (mut app, _tmp) = temp_app();
    app.record_user_turn("restored target");
    app.persist_chat();
    let target_id = app.chat_id.clone();

    let _ = app.try_slash_command("/clear");
    fill_over_budget(&mut app, 16);
    let job = app.begin_auto_compaction().expect("an old-generation job");
    for _ in 1..umadev_agent::compaction::Breaker::LIMIT {
        app.compaction_breaker.record_failure();
    }
    assert!(!app.compaction_breaker.tripped());

    assert!(app.load_chat(&target_id), "the target chat restores");
    let restored = app.conversation.clone();
    app.fail_compaction(job.generation);

    assert_eq!(
        app.conversation, restored,
        "the old failure must not mutate the restored chat"
    );
    assert!(
        !app.compaction_breaker.tripped(),
        "the old failure must not advance the restored chat's breaker"
    );
}

#[test]
fn director_run_finish_hands_session_back_to_chat() {
    // Wave 5 deliverable 2: a finished director build hands its session to chat
    // so the next chat turn continues the SAME build session. The build-ness now
    // rides the terminal decision (`director_build: true`), NOT the pre-spawn
    // `director_run_in_flight` flag — the chat surface classifies in the task.
    let (mut app, _tmp) = temp_app();
    app.director_run_in_flight = true;
    app.record_agentic_done("built the app".to_string(), true, None, None);
    assert!(
        app.run_session_handed_to_chat,
        "a finished director build hands its session back to chat"
    );
    assert!(
        !app.host_chat_session_active && app.chat_session_id.is_none(),
        "a terminal turn without a native id cannot fabricate resume authority"
    );
    assert!(
        !app.director_run_in_flight,
        "the in-flight marker is cleared"
    );

    // A PLAIN chat turn (`director_build: false`) does NOT trigger the handoff,
    // even if the in-flight marker was left set.
    app.run_session_handed_to_chat = false;
    app.director_run_in_flight = true;
    app.record_agentic_done("just chatting".to_string(), false, None, None);
    assert!(
        !app.run_session_handed_to_chat,
        "a non-build turn never hands a session back"
    );
    assert!(
        !app.director_run_in_flight,
        "the in-flight marker is always cleared on a terminal turn"
    );
    assert!(!app.host_chat_session_active);
}

#[test]
fn new_chat_session_ids_are_unique() {
    let a = new_chat_session_id();
    let b = new_chat_session_id();
    assert_ne!(a, b, "back-to-back ids must differ");
}

#[test]
fn parse_notes_section_extracts_url() {
    let body = "# Frontend notes\n\n## Preview URL\n\nhttp://localhost:5173\n\n## Run command\n\ncd web && npm run dev\n";
    assert_eq!(
        parse_notes_section(body, "Preview URL"),
        Some("http://localhost:5173")
    );
    assert_eq!(
        parse_notes_section(body, "Run command"),
        Some("cd web && npm run dev")
    );
}

#[test]
fn parse_notes_section_skips_placeholder() {
    let body = "## Preview URL\n\n_(worker fills this)_\n\nhttp://localhost:3000\n";
    // Skips the italic placeholder, returns the real URL.
    assert_eq!(
        parse_notes_section(body, "Preview URL"),
        Some("http://localhost:3000")
    );
}

#[test]
fn parse_notes_section_missing_returns_none() {
    assert_eq!(parse_notes_section("no headings here", "Preview URL"), None);
}

#[test]
fn parse_notes_section_stops_at_next_heading() {
    let body = "## Preview URL\n\nhttp://localhost:5173\n\n## Other\n\nhttp://wrong\n";
    assert_eq!(
        parse_notes_section(body, "Preview URL"),
        Some("http://localhost:5173")
    );
}

#[test]
fn preview_url_from_notes_reads_file() {
    let tmp = tempfile::TempDir::new().unwrap();
    let slug = "demo";
    std::fs::create_dir_all(tmp.path().join("output")).unwrap();
    std::fs::write(
        tmp.path()
            .join("output")
            .join(format!("{slug}-frontend-notes.md")),
        "# Notes\n\n## Preview URL\n\nhttp://localhost:4321\n\n## Run command\n\nnpm run dev\n",
    )
    .unwrap();
    let app = App::new(
        slug.to_string(),
        UserConfig {
            backend: Some("offline".into()),
            ..Default::default()
        },
        tmp.path().join("config.toml"),
        tmp.path().to_path_buf(),
    );
    assert_eq!(
        app.preview_url_from_notes().as_deref(),
        Some("http://localhost:4321")
    );
    assert_eq!(app.run_command_from_notes().as_deref(), Some("npm run dev"));
}

#[test]
fn slash_preview_with_no_notes_gives_hint() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut app = App::new(
        "demo".to_string(),
        UserConfig {
            backend: Some("offline".into()),
            lang: Some("zh-CN".into()),
            ..Default::default()
        },
        tmp.path().join("config.toml"),
        tmp.path().to_path_buf(),
    );
    // No output dir / notes file → guidance message, no StartPreview.
    let action = app.slash_preview();
    assert!(matches!(action, Action::None));
    assert!(app
        .history
        .iter()
        .any(|m| m.body().contains("还没有可预览")));
}

#[test]
fn slash_preview_with_url_and_command_emits_start() {
    let tmp = tempfile::TempDir::new().unwrap();
    let slug = "demo";
    std::fs::create_dir_all(tmp.path().join("output")).unwrap();
    std::fs::write(
        tmp.path()
            .join("output")
            .join(format!("{slug}-frontend-notes.md")),
        "## Preview URL\n\nhttp://localhost:5173\n\n## Run command\n\ncd web && npm run dev\n",
    )
    .unwrap();
    let mut app = App::new(
        slug.to_string(),
        UserConfig {
            backend: Some("offline".into()),
            ..Default::default()
        },
        tmp.path().join("config.toml"),
        tmp.path().to_path_buf(),
    );
    let action = app.slash_preview();
    match action {
        Action::StartPreview { url, command } => {
            assert_eq!(url, "http://localhost:5173");
            assert_eq!(command, "cd web && npm run dev");
        }
        other => panic!("expected StartPreview, got {other:?}"),
    }
}

#[test]
fn slash_preview_ignores_harness_notes_when_real_frontend_exists() {
    let tmp = tempfile::TempDir::new().unwrap();
    let slug = "demo";
    std::fs::create_dir_all(tmp.path().join("output")).unwrap();
    std::fs::create_dir_all(tmp.path().join("src/backend")).unwrap();
    std::fs::create_dir_all(tmp.path().join("src/frontend")).unwrap();
    std::fs::write(tmp.path().join("src/backend/server.mjs"), "listen()").unwrap();
    std::fs::write(
            tmp.path()
                .join("output")
                .join(format!("{slug}-frontend-notes.md")),
            "## Preview URL\n\nhttp://127.0.0.1:4173\n\n## Run command\n\nnode src/backend/server.mjs\n",
        )
        .unwrap();
    std::fs::create_dir_all(tmp.path().join("jeecgboot-vue3")).unwrap();
    std::fs::write(
        tmp.path().join("jeecgboot-vue3/package.json"),
        r#"{"scripts":{"dev":"vite"},"devDependencies":{"vite":"^5.0.0"}}"#,
    )
    .unwrap();

    let mut app = App::new(
        slug.to_string(),
        UserConfig {
            backend: Some("offline".into()),
            ..Default::default()
        },
        tmp.path().join("config.toml"),
        tmp.path().to_path_buf(),
    );
    let action = app.slash_preview();
    match action {
        Action::StartPreview { url, command } => {
            assert_eq!(url, "http://localhost:5173");
            assert_eq!(command, "cd jeecgboot-vue3 && npm run dev");
        }
        other => panic!("expected real frontend StartPreview, got {other:?}"),
    }
}

#[test]
fn web_build_completion_card_has_files_entry_run_and_pending_preview() {
    // A finished web build's card shows what changed + the key entry + the
    // run command, and (when a dev server is detected) a "starting preview"
    // line — the "✅ done + here's the demo" finish.
    let (app, _tmp) = temp_app();
    let root = app.project_root.clone();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("package.json"),
        r#"{"dependencies":{"vite":"^5"},"scripts":{"dev":"vite"}}"#,
    )
    .unwrap();
    std::fs::write(root.join("src").join("App.tsx"), "export default 1;").unwrap();

    // A web project resolves a dev-server target (Vite) → preview is pending.
    let target = app.auto_preview_target();
    assert!(
        target.is_some(),
        "vite project must resolve a preview target"
    );
    let card = app.build_completion_card(target.is_some());
    // Headline + the three substantive sections.
    assert!(card.contains("构建完成"), "card carries the done headline");
    assert!(
        card.contains("App.tsx"),
        "card names the key entry / a changed file"
    );
    assert!(
        card.contains("vite") || card.contains("npm run dev"),
        "card carries the run command: {card}"
    );
    // The "starting dev server…" placeholder shows because a server was found.
    assert!(
        card.contains(umadev_i18n::t(app.lang, "build.complete.preview_starting")),
        "web card flags the pending preview"
    );
}

#[test]
fn non_web_build_completion_card_has_no_preview_line_fail_open() {
    // Fail-open: a non-web project detects no dev server → the card is still
    // produced (✅ done + what changed) but carries NO preview line and the
    // auto-preview target is None (so the event loop starts no server).
    let (app, _tmp) = temp_app();
    let root = app.project_root.clone();
    // A pure-Rust project: a main.rs but no package.json / index.html.
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src").join("main.rs"), "fn main() {}").unwrap();

    assert!(
        app.auto_preview_target().is_none(),
        "a non-web project resolves no preview target"
    );
    let card = app.build_completion_card(false);
    assert!(
        card.contains("构建完成"),
        "card still shows the done headline"
    );
    assert!(card.contains("main.rs"), "card names the rust entry");
    assert!(
        !card.contains(umadev_i18n::t(app.lang, "build.complete.preview_starting")),
        "non-web card must NOT show a preview-starting line"
    );
}

#[test]
fn build_completion_card_falls_back_to_dirs_without_git_delta() {
    // No git repo (no porcelain delta) → the card still names a concrete
    // product directory instead of an empty "files changed" section.
    let (app, _tmp) = temp_app();
    std::fs::create_dir_all(app.project_root.join("src")).unwrap();
    let card = app.build_completion_card(false);
    assert!(card.contains("构建完成"));
    assert!(
        card.contains("src"),
        "falls back to naming an output dir: {card}"
    );
}

#[test]
fn build_completion_card_persists_task_statuses_before_panel_clears() {
    // The user's core ask: when a build finishes — worst when it ends
    // INCOMPLETE — the completion card must carry the per-step task
    // breakdown (which are DONE, BLOCKED, INCOMPLETE), because the live
    // `/plan` panel is cleared right after. This proves the ordering holds:
    // `post_build_completion_card` reads the rows for the card BEFORE
    // `finalize_live_panels` clears them, so the statuses survive in the
    // transcript even though the panel goes away.
    let (mut app, _tmp) = temp_app();
    // Real source so a completion card is legitimate; a non-web project so
    // no dev server is started under the unit test.
    std::fs::create_dir_all(app.project_root.join("src")).unwrap();
    std::fs::write(app.project_root.join("src").join("main.rs"), "fn main(){}").unwrap();
    // A plan that ended INCOMPLETE: 2 done, 1 blocked, 1 pending.
    app.apply_engine(EngineEvent::PlanPosted {
        steps: vec![
            "s1 · scaffold (frontend)".into(),
            "s2 · login route (backend)".into(),
            "s3 · login form (frontend)".into(),
            "s4 · e2e review (qa)".into(),
        ],
        statuses: vec![
            "done".into(),
            "done".into(),
            "blocked".into(),
            "pending".into(),
        ],
        done: 2,
        total: 4,
    });
    // Build + push the card through the REAL finalize path (the one that
    // also clears the live panel afterwards).
    let target = app.post_build_completion_card();
    assert!(target.is_none(), "non-web build resolves no preview target");
    let card = app
        .history
        .iter()
        .rev()
        .find(|m| m.role == ChatRole::UmaDev)
        .expect("a completion card was pushed")
        .body()
        .clone();
    // Same `done/total` convention as the live `/plan` panel.
    assert!(card.contains("2/4"), "card carries the done count: {card}");
    // Every step with its final glyph — DONE / BLOCKED / INCOMPLETE visible.
    assert!(card.contains("[x] s1"), "done step checked: {card}");
    assert!(card.contains("[!] s3"), "blocked step flagged: {card}");
    assert!(card.contains("[ ] s4"), "pending step blank: {card}");
    // The incomplete lead names 1 blocked + 1 unfinished (pending).
    assert!(
        card.contains(&umadev_i18n::tf(
            app.lang,
            "build.complete.tasks_incomplete",
            &["1", "1"],
        )),
        "incomplete build leads with blocked/unfinished counts: {card}"
    );
    // The live panel is cleared AFTER the card captured the statuses.
    assert!(
        app.plan_steps.is_empty(),
        "the live plan panel clears after the card is built"
    );
}

#[test]
fn build_completion_card_omits_task_section_without_a_plan() {
    // A plan-less chat/Fast build (no live plan) → NO empty "tasks" block:
    // the section renders only when there are steps to report.
    let (app, _tmp) = temp_app();
    std::fs::create_dir_all(app.project_root.join("src")).unwrap();
    assert!(app.plan_steps.is_empty());
    let card = app.build_completion_card(false);
    assert!(
        !card.contains(&umadev_i18n::tf(app.lang, "build.complete.tasks", &["0/0"])),
        "no task-status header when there is no plan: {card}"
    );
    // And no orphan glyph lines leak in.
    assert!(
        !card.contains("[x]") && !card.contains("[ ]"),
        "no step glyphs: {card}"
    );
}

#[test]
fn no_backend_opens_picker() {
    let app = fresh_app(None);
    assert_eq!(app.mode, AppMode::Picker);
}

#[test]
fn backend_picker_exposes_exactly_the_five_supported_bases() {
    let mut app = fresh_app(None);
    app.goto_picker_step(PickerStep::BaseCli);
    let ids: Vec<&str> = app
        .picker_items
        .iter()
        .filter_map(|item| item.backend_id.as_deref())
        .collect();
    assert_eq!(ids, crate::FIRST_CLASS_BACKEND_IDS);
    assert!(app
        .picker_items
        .iter()
        .all(|item| item.group == PickerGroup::HostCli));
}

#[test]
fn retired_backend_migration_opens_base_picker_with_clear_notice() {
    let mut app = fresh_app(None);
    app.show_retired_backend_migration(Some("qwen-code"));

    assert_eq!(app.mode, AppMode::Picker);
    assert_eq!(app.picker_step, PickerStep::BaseCli);
    assert_eq!(
        app.backend, None,
        "migration must not select offline or another base"
    );
    let notice = app.picker_notice.as_deref().expect("migration notice");
    assert!(notice.contains("qwen-code"), "old base is named: {notice}");
    assert!(notice.contains("Claude Code") && notice.contains("Grok Build"));
}

#[test]
fn retired_backend_commands_and_aliases_are_unavailable() {
    let mut app = fresh_app(Some("codex"));
    for retired in ["cursor", "codebuddy", "cbc", "droid", "qwen", "qwen-code"] {
        assert!(
            App::resolve_command(retired).is_none(),
            "/{retired} is not registered"
        );
        let action = app
            .try_slash_command(&format!("/{retired}"))
            .expect("slash input is handled as an unknown command");
        assert_eq!(action, Action::None);
        assert_eq!(app.backend.as_deref(), Some("codex"));
        assert_eq!(app.config.backend.as_deref(), Some("codex"));
        assert!(app
            .history
            .back()
            .is_some_and(|message| message.role == ChatRole::System));
    }
    assert_eq!(
        App::resolve_command("grok-build").map(|cmd| cmd.name),
        Some("grok")
    );
    assert_eq!(
        App::resolve_command("kimi-code").map(|cmd| cmd.name),
        Some("kimi")
    );
}

#[test]
fn retired_or_unknown_current_config_cannot_enter_chat() {
    for backend in ["cursor", "qwen-code", "transport-only"] {
        let app = fresh_app(Some(backend));
        assert_eq!(app.mode, AppMode::Picker, "{backend} must reopen setup");
        assert_eq!(app.backend, None);
    }
}

#[test]
fn configured_backend_opens_chat_with_greeting() {
    let app = fresh_app(Some("claude-code"));
    assert_eq!(app.mode, AppMode::Chat);
    assert_eq!(app.backend_label, "claude-code");
    // Greeting is the very first message.
    let first = app.history.front().unwrap();
    assert_eq!(first.role, ChatRole::UmaDev);
    assert!(first.body().contains("claude-code"));
}

#[test]
fn picker_arrow_keys_navigate() {
    let mut app = fresh_app(None);
    let last = app.picker_items.len() - 1;
    assert_eq!(app.picker_selected, 0);
    // Walk all the way down — should clamp at `last`.
    for _ in 0..(app.picker_items.len() + 2) {
        let _ = app.apply_key(KeyCode::Down);
    }
    assert_eq!(app.picker_selected, last);
    let _ = app.apply_key(KeyCode::Up);
    assert_eq!(app.picker_selected, last - 1);
}

#[test]
fn picker_enter_on_unavailable_host_stays() {
    let mut app = fresh_app(None);
    // Base CLIs live in step 3; with no probes yet they're all unready.
    app.goto_picker_step(PickerStep::BaseCli);
    let action = app.apply_key(KeyCode::Enter);
    assert_eq!(action, Action::None);
    assert_eq!(app.mode, AppMode::Picker);
    // The refusal is surfaced INLINE on the picker (visible to the user),
    // not pushed to the not-yet-visible chat screen.
    assert!(app.picker_notice.is_some());
    // …and navigating away clears it.
    let _ = app.apply_key(KeyCode::Down);
    assert!(app.picker_notice.is_none());
}

#[test]
fn picker_refreshes_on_backend_probed() {
    let mut app = fresh_app(None);
    app.apply_engine(EngineEvent::BackendProbed {
        backend_id: "claude-code".into(),
        ready: true,
        detail: "claude 1.6.0".into(),
    });
    // Walk to the base-CLI step (language -> mode -> base) where the host
    // rows live; the probe just cached marks claude-code ready.
    app.goto_picker_step(PickerStep::BaseCli);
    let idx = app
        .picker_items
        .iter()
        .position(|i| i.backend_id.as_deref() == Some("claude-code"))
        .unwrap();
    app.picker_selected = idx;
    assert!(app.picker_items[idx].ready);
    let action = app.apply_key(KeyCode::Enter);
    assert_eq!(action, Action::BackendChanged);
    assert_eq!(app.mode, AppMode::Chat);
    assert_eq!(app.backend_label, "claude-code");
}

// --- Wave 1: intent card / live plan / team review event rendering ---

#[test]
fn intent_decided_pushes_intent_card_and_records_class() {
    let mut app = fresh_app(Some("offline"));
    let before = app.history.len();
    app.apply_engine(EngineEvent::IntentDecided {
        class: "build".into(),
        depth: "deep".into(),
        team: vec!["architect".into(), "qa".into()],
        est_tool_calls: 160,
        rationale: "完整构建,进研发流程".into(),
    });
    // A prominent UmaDev card landed in the transcript…
    assert!(app.history.len() > before);
    let card = app
        .history
        .iter()
        .rev()
        .find(|m| m.role == ChatRole::UmaDev)
        .unwrap();
    // …carrying the BUILD headline, the rough budget, the team, and the reason.
    assert!(card.body().contains("160"), "shows the budget");
    assert!(card.body().contains("architect"), "shows the team");
    assert!(card.body().contains("研发流程"), "carries the rationale");
    // …and the class is recorded so the status chip can show it.
    assert_eq!(app.last_intent_class.as_deref(), Some("build"));
}

#[test]
fn intent_decided_unknown_class_falls_open_to_chat_headline() {
    let mut app = fresh_app(Some("offline"));
    // A bogus class id must not panic and must not show a budget/team line.
    app.apply_engine(EngineEvent::IntentDecided {
        class: "totally-unknown".into(),
        depth: "weird".into(),
        team: vec![],
        est_tool_calls: 0,
        rationale: String::new(),
    });
    assert_eq!(app.last_intent_class.as_deref(), Some("totally-unknown"));
    // Still produced a card (the neutral chat headline), no crash.
    assert!(app.history.iter().any(|m| m.role == ChatRole::UmaDev));
}

#[test]
fn plan_posted_then_step_status_drives_the_checklist() {
    let mut app = fresh_app(Some("offline"));
    app.apply_engine(EngineEvent::PlanPosted {
        statuses: vec![],
        steps: vec![
            "s1 · scaffold the app (frontend)".into(),
            "s2 · login route (backend)".into(),
            "s3 · login form (frontend)".into(),
        ],
        done: 0,
        total: 3,
    });
    assert_eq!(app.plan_steps.len(), 3);
    assert_eq!(app.plan_steps[0].id, "s1");
    assert!(app.plan_steps[0].title.contains("scaffold"));
    assert!(app.plan_steps.iter().all(|s| s.status == "pending"));
    // A status transition ticks the matching step in place (not a new row).
    app.apply_engine(EngineEvent::PlanStepStatus {
        id: "s1".into(),
        title: "scaffold the app".into(),
        status: "done".into(),
    });
    assert_eq!(app.plan_steps.len(), 3, "no new row appended");
    assert_eq!(app.plan_steps[0].status, "done");
    // A status for an UNKNOWN id is appended, never dropped (fail-open).
    app.apply_engine(EngineEvent::PlanStepStatus {
        id: "s9".into(),
        title: "extra".into(),
        status: "active".into(),
    });
    assert_eq!(app.plan_steps.len(), 4);
    assert_eq!(app.plan_steps[3].id, "s9");
}

#[test]
fn resumed_plan_post_restores_persisted_step_statuses() {
    // Cross-session resume (user-reported): after /continue the re-posted
    // plan must render the persisted truth — earlier done steps stay
    // checked, the blocked one stays flagged — instead of resetting the
    // checklist to all-pending with a 0/N done count.
    let mut app = fresh_app(Some("offline"));
    app.apply_engine(EngineEvent::PlanPosted {
        steps: vec![
            "s1 · scaffold (frontend)".into(),
            "s2 · login route (backend)".into(),
            "s3 · login form (frontend)".into(),
            "s4 · e2e review (qa)".into(),
        ],
        statuses: vec![
            "done".into(),
            "done".into(),
            "blocked".into(),
            "pending".into(),
        ],
        done: 2,
        total: 4,
    });
    let statuses: Vec<&str> = app.plan_steps.iter().map(|s| s.status.as_str()).collect();
    assert_eq!(statuses, vec!["done", "done", "blocked", "pending"]);
    // The panel header + `/plan` derive the done-count from the rows: 2/4.
    assert_eq!(
        app.plan_steps.iter().filter(|s| s.status == "done").count(),
        2
    );
    // Pre-resume completions are NOT replayed as fresh handoffs.
    assert!(app.handoffs.is_empty(), "no handoff invented on a resume");
    // The transcript `/plan` card matches the restored panel.
    let _ = app.try_slash_command("/plan").unwrap();
    let card = app.history.back().unwrap().body().clone();
    assert!(card.contains("2/4"), "card counts restored steps: {card}");
    assert!(card.contains("[x] s1"), "done step checked: {card}");
    assert!(card.contains("[!] s3"), "blocked step flagged: {card}");
    assert!(card.contains("[ ] s4"), "pending step blank: {card}");
}

#[test]
fn plan_post_with_short_statuses_falls_open_to_pending() {
    // Fail-open: a statuses list shorter than the steps (or absent, as on a
    // fresh post) leaves the uncovered steps `pending` — never a panic or a
    // dropped row.
    let mut app = fresh_app(Some("offline"));
    app.apply_engine(EngineEvent::PlanPosted {
        steps: vec![
            "s1 · scaffold (frontend)".into(),
            "s2 · login route (backend)".into(),
        ],
        statuses: vec!["done".into()],
        done: 1,
        total: 2,
    });
    assert_eq!(app.plan_steps[0].status, "done");
    assert_eq!(app.plan_steps[1].status, "pending");
}

// ---- Wave C: live team roster + handoff timeline ----------------------

#[test]
fn convened_roster_shows_only_seated_steps_with_live_status() {
    let mut app = fresh_app(Some("offline"));
    app.apply_engine(EngineEvent::PlanPosted {
        statuses: vec![],
        steps: vec![
            "s1 · API contract (architect)".into(),
            "s2 · login form (frontend)".into(),
            // No `(seat)` → unattributed; anti-theater drops it from the roster.
            "s3 · housekeeping step".into(),
        ],
        done: 0,
        total: 3,
    });
    // The step seat was captured from the `(seat)` token.
    assert_eq!(app.plan_steps[0].seat, "architect");
    assert_eq!(app.plan_steps[1].seat, "frontend-engineer");
    assert_eq!(
        app.plan_steps[2].seat, "",
        "no seat parsed for the bare step"
    );
    // Only the two seat-attributed steps convene a seat; all pending → idle.
    let roster = app.convened_roster();
    assert_eq!(roster.len(), 2, "only seated steps convene a teammate");
    assert!(roster.iter().all(|r| r.status == SeatStatus::Idle));
    // A reviewing seat (architect) active reads `Reviewing`; a doing seat
    // (frontend) active reads `Working`.
    app.apply_engine(EngineEvent::PlanStepStatus {
        id: "s1".into(),
        title: "API contract".into(),
        status: "active".into(),
    });
    app.apply_engine(EngineEvent::PlanStepStatus {
        id: "s2".into(),
        title: "login form".into(),
        status: "active".into(),
    });
    let roster = app.convened_roster();
    let arch = roster.iter().find(|r| r.role == "architect").unwrap();
    let fe = roster
        .iter()
        .find(|r| r.role == "frontend-engineer")
        .unwrap();
    assert_eq!(arch.status, SeatStatus::Reviewing);
    assert_eq!(fe.status, SeatStatus::Working);
}

#[test]
fn step_done_marks_seat_done_and_records_a_handoff() {
    let mut app = fresh_app(Some("offline"));
    app.apply_engine(EngineEvent::PlanPosted {
        statuses: vec![],
        steps: vec!["s1 · API contract (architect)".into()],
        done: 0,
        total: 1,
    });
    assert!(
        app.handoffs.is_empty(),
        "no handoff before a step completes"
    );
    app.apply_engine(EngineEvent::PlanStepStatus {
        id: "s1".into(),
        title: "API contract".into(),
        status: "done".into(),
    });
    // The (only) step done → the seat reads Done…
    assert_eq!(app.convened_roster()[0].status, SeatStatus::Done);
    // …and a handoff entry was recorded for the architect.
    assert_eq!(app.handoffs.len(), 1);
    assert_eq!(app.handoffs[0].seat, "architect");
    assert!(app.handoffs[0].title.contains("API contract"));
    // A repeated `done` event does NOT double-record the handoff (idempotent).
    app.apply_engine(EngineEvent::PlanStepStatus {
        id: "s1".into(),
        title: "API contract".into(),
        status: "done".into(),
    });
    assert_eq!(
        app.handoffs.len(),
        1,
        "no duplicate handoff on a repeat done"
    );
}

#[test]
fn roster_verdict_chip_reflects_critic_verdict_only_for_convened_seats() {
    let mut app = fresh_app(Some("offline"));
    app.apply_engine(EngineEvent::PlanPosted {
        statuses: vec![],
        steps: vec![
            "s1 · API contract (architect)".into(),
            "s2 · login form (frontend)".into(),
        ],
        done: 0,
        total: 2,
    });
    // Architect (convened) accepts; QA (NO plan step) blocks.
    app.apply_engine(EngineEvent::CriticVerdict {
        seat: "architect".into(),
        accepts: true,
        blocking: vec![],
        remediation: vec![],
        advisory: vec![],
    });
    app.apply_engine(EngineEvent::CriticVerdict {
        seat: "qa".into(),
        accepts: false,
        blocking: vec!["missing tests".into()],
        remediation: vec![],
        advisory: vec![],
    });
    let roster = app.convened_roster();
    // Anti-theater: QA reviewed but has no step → it never joins the roster.
    assert!(
        roster.iter().all(|r| r.role != "qa-engineer"),
        "an unconvened reviewer is not shown in the roster"
    );
    // The architect's chip carries its accept verdict; frontend has no verdict.
    let arch = roster.iter().find(|r| r.role == "architect").unwrap();
    assert_eq!(arch.verdict, Some((true, 0)));
    let fe = roster
        .iter()
        .find(|r| r.role == "frontend-engineer")
        .unwrap();
    assert_eq!(fe.verdict, None);
}

#[test]
fn roster_and_handoffs_are_empty_with_no_active_build() {
    // Fail-open: a fresh app with no plan shows nothing extra and never panics.
    let app = fresh_app(Some("offline"));
    assert!(app.convened_roster().is_empty());
    assert!(app.handoffs.is_empty());
}

#[test]
fn critic_transcript_note_carries_per_blocker_resolution() {
    // The never-lost transcript note lists each must-fix problem AND, right
    // under it, the seat's suggested fix (the per-blocker remediation) so the
    // full resolution is always in the scrollable history.
    let mut app = fresh_app(Some("offline"));
    app.lang = umadev_i18n::Lang::En;
    app.apply_engine(EngineEvent::CriticVerdict {
        seat: "security-engineer".into(),
        accepts: false,
        blocking: vec![
            "Authentication is effectively bypassed".into(),
            "Hardcoded, guessable session identifiers".into(),
        ],
        remediation: vec![
            "add a signed session token + a real identity provider".into(),
            "generate a random per-session id server-side".into(),
        ],
        advisory: vec![],
    });
    let note = app
        .history
        .iter()
        .find(|m| m.role == ChatRole::System && m.body().contains("must-fix"))
        .expect("a blocking critic pushes a transcript note");
    let body = note.body();
    // Each problem is present…
    assert!(
        body.contains("Authentication is effectively bypassed"),
        "{body}"
    );
    assert!(
        body.contains("Hardcoded, guessable session identifiers"),
        "{body}"
    );
    // …with its concrete fix surfaced right under it.
    assert!(
        body.contains("signed session token"),
        "fix 1 surfaced: {body}"
    );
    assert!(
        body.contains("random per-session id"),
        "fix 2 surfaced: {body}"
    );
}

#[test]
fn team_command_surfaces_convened_roster_and_handoff_timeline() {
    let mut app = fresh_app(Some("offline"));
    app.apply_engine(EngineEvent::PlanPosted {
        statuses: vec![],
        steps: vec![
            "s1 · API contract (architect)".into(),
            "s2 · login form (frontend)".into(),
        ],
        done: 0,
        total: 2,
    });
    app.apply_engine(EngineEvent::PlanStepStatus {
        id: "s1".into(),
        title: "API contract".into(),
        status: "done".into(),
    });
    let before = app.history.len();
    app.slash_team("");
    let note = app
        .history
        .iter()
        .skip(before)
        .find(|m| m.role == ChatRole::UmaDev)
        .expect("a team note was pushed");
    let body = note.body();
    // The convened architect appears with its done status word, and the handoff
    // timeline names the architect's completed deliverable.
    let arch_name = seat_display_name(app.lang, "architect");
    assert!(body.contains(&arch_name), "names the convened architect");
    assert!(
        body.contains(umadev_i18n::t(app.lang, "team.handoff.header")),
        "shows the handoff timeline header once a step is done"
    );
    assert!(
        body.contains("API contract"),
        "names the handed-off deliverable"
    );
}

#[test]
fn a_blocked_step_makes_its_seat_read_blocked() {
    let mut app = fresh_app(Some("offline"));
    app.apply_engine(EngineEvent::PlanPosted {
        statuses: vec![],
        steps: vec!["s1 · login form (frontend)".into()],
        done: 0,
        total: 1,
    });
    app.apply_engine(EngineEvent::PlanStepStatus {
        id: "s1".into(),
        title: "login form".into(),
        status: "blocked".into(),
    });
    assert_eq!(app.convened_roster()[0].status, SeatStatus::Blocked);
    // A blocked step is not a completion → no handoff entry.
    assert!(app.handoffs.is_empty());
}

// ---- background-run task registry + /tasks ----------------------------

#[test]
fn run_registers_a_running_task_and_tracks_step_progress() {
    let mut app = fresh_app(Some("offline"));
    // A started run (legacy path emits PipelineStarted) registers a live task.
    app.apply_engine(EngineEvent::PipelineStarted {
        slug: "demo".into(),
        requirement: "build a todo app with login".into(),
    });
    let t = app.active_task().expect("a Running task is registered");
    assert_eq!(t.status, TaskStatus::Running);
    assert!(t.requirement.contains("todo app"));
    assert_eq!((t.done, t.total), (0, 0));
    // A posted plan + a step tick drive the X/Y progress.
    app.apply_engine(EngineEvent::PlanPosted {
        statuses: vec![],
        steps: vec![
            "s1 · scaffold (frontend)".into(),
            "s2 · login route (backend)".into(),
            "s3 · login form (frontend)".into(),
        ],
        done: 0,
        total: 3,
    });
    let t = app.active_task().unwrap();
    assert_eq!((t.done, t.total), (0, 3), "total seeded from the plan");
    app.apply_engine(EngineEvent::PlanStepStatus {
        id: "s1".into(),
        title: "scaffold".into(),
        status: "done".into(),
    });
    let t = app.active_task().unwrap();
    assert_eq!((t.done, t.total), (1, 3), "a done step advances progress");
    // Still exactly ONE task (idempotent: PipelineStarted + PlanPosted reuse it).
    assert_eq!(app.tasks.len(), 1);
}

#[test]
fn director_path_registers_a_task_from_a_posted_plan() {
    // The director build emits NO PipelineStarted — a posted plan is the
    // "a build is live" signal that must still register the task.
    let mut app = fresh_app(Some("offline"));
    app.requirement = "做一个登录页".into();
    app.apply_engine(EngineEvent::PlanPosted {
        statuses: vec![],
        steps: vec!["s1 · 登录页 (frontend)".into()],
        done: 0,
        total: 1,
    });
    let t = app.active_task().expect("plan post registers a task");
    assert_eq!(t.status, TaskStatus::Running);
    assert!(t.requirement.contains("登录页"));
}

#[test]
fn tasks_command_lists_the_active_run() {
    let mut app = fresh_app(Some("offline"));
    app.register_run_task("build a blog engine");
    let before = app.history.len();
    let action = app.slash_tasks("");
    assert!(matches!(action, Action::None));
    assert!(
        app.history
            .iter()
            .skip(before)
            .any(|m| m.body().contains("build a blog engine")),
        "the list names the active run"
    );
}

#[test]
fn persisted_task_summary_redacts_credentials() {
    let mut app = fresh_app(Some("offline"));
    app.register_run_task(concat!(
        "fix api_key=sk-",
        "example-task-secret-value in checkout"
    ));
    let body = std::fs::read_to_string(app.tasks_path()).unwrap();
    assert!(!body.contains(concat!("sk-", "example-task-secret-value")));
    assert!(body.contains("[redacted]"));
}

#[test]
fn legacy_task_registry_secrets_are_redacted_on_load_and_rewrite() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();
    std::fs::create_dir_all(root.join(".umadev")).unwrap();
    std::fs::write(
        root.join(".umadev/tasks.json"),
        concat!(
            r#"{"seq":1,"tasks":[{"id":"t1","requirement":"token=sk-"#,
            r#"legacy-task-secret","status":"running","started_at_unix":0,"done":0,"total":1}]}"#
        ),
    )
    .unwrap();
    let mut app = App::new(
        "demo",
        cfg_offline(),
        root.join("config.toml"),
        root.clone(),
    );
    assert!(!app.tasks[0]
        .requirement
        .contains(concat!("sk-", "legacy-task-secret")));
    app.register_run_task("safe follow-up");
    let body = std::fs::read_to_string(app.tasks_path()).unwrap();
    assert!(!body.contains(concat!("sk-", "legacy-task-secret")));
    assert!(body.contains("[redacted]"));
}

#[test]
fn tasks_command_lists_durable_agent_team_children() {
    let mut app = fresh_app(Some("offline"));
    let run_id = {
        let mut ledger = umadev_agent::task_lifecycle::AgentTaskLedger::open_scoped(
            &app.project_root,
            "durable team test",
        )
        .unwrap();
        ledger
            .queue(
                "director",
                None,
                "director",
                "coordinate checkout",
                umadev_agent::task_lifecycle::AgentTaskMode::ReadOnly,
            )
            .unwrap();
        ledger.start("director").unwrap();
        ledger
            .queue(
                "api",
                Some("director".into()),
                "backend-engineer",
                "implement checkout API",
                umadev_agent::task_lifecycle::AgentTaskMode::Writer,
            )
            .unwrap();
        ledger.start("api").unwrap();
        ledger.run_id().to_string()
    };

    let before = app.history.len();
    assert_eq!(app.slash_tasks(""), Action::None);
    let body = app
        .history
        .iter()
        .skip(before)
        .find(|message| message.role == ChatRole::System)
        .unwrap()
        .body();
    assert!(body.contains(&run_id), "run id is visible: {body}");
    assert!(body.contains("backend-engineer"), "role is visible: {body}");
    assert!(
        body.contains("implement checkout API"),
        "task is visible: {body}"
    );
}

#[test]
fn tasks_stop_cancels_the_active_run_then_marks_it_stopped() {
    let mut app = fresh_app(Some("offline"));
    app.register_run_task("build a wiki");
    // The director run set this; has_active_run sees it.
    app.agentic_in_flight = true;
    let action = app.slash_tasks("stop");
    assert_eq!(action, Action::Cancel, "/tasks stop reuses the cancel path");
    // The event loop's cancel completes via cancel_run, settling the task.
    app.cancel_run();
    let t = app.tasks.last().unwrap();
    assert_eq!(t.status, TaskStatus::Stopped);
    assert!(app.active_task().is_none(), "no live task after a stop");
}

#[test]
fn tasks_resume_with_a_resumable_run_triggers_resume_run() {
    let mut app = fresh_app(Some("claude-code"));
    // Persist a plan + workflow-state exactly as an interrupted /run leaves.
    let plan = umadev_agent::Plan {
        steps: vec![umadev_agent::PlanStep {
            files: umadev_agent::StepFiles::default(),
            id: "a".into(),
            title: "build the login page".into(),
            seat: umadev_agent::Seat::FrontendEngineer,
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
    let mut state = umadev_agent::WorkflowState::new(umadev_spec::Phase::Frontend);
    state.slug = "demo".into();
    state.requirement = "做一个登录页".into();
    state.backend = "claude-code".into();
    umadev_agent::write_workflow_state(&app.project_root, &state).unwrap();

    let action = app.slash_tasks("resume");
    assert_eq!(
        action,
        Action::ResumeRun("做一个登录页".to_string()),
        "/tasks resume re-attaches to the persisted run"
    );
}

#[test]
fn second_run_while_one_is_active_is_guarded() {
    let mut app = fresh_app(Some("offline"));
    // A first run is live (registered + agentic in flight).
    app.register_run_task("build app one");
    app.agentic_in_flight = true;
    assert!(app.has_active_run());
    let before_tasks = app.tasks.len();
    let action = app
        .try_slash_command("/run build app two")
        .expect("/run is a slash command");
    assert!(
        matches!(action, Action::None),
        "a second /run is rejected, not started"
    );
    assert_eq!(app.tasks.len(), before_tasks, "no second task registered");
    // The guard names the /tasks surface.
    assert!(app.history.iter().any(|m| m.body().contains("/tasks")));
    // The original task is untouched (still Running).
    assert_eq!(app.active_task().unwrap().requirement, "build app one");
}

#[test]
fn tasks_is_fail_open_with_no_active_task() {
    let mut app = fresh_app(Some("offline"));
    // Empty registry: list shows the empty hint, no panic.
    let action = app.slash_tasks("");
    assert!(matches!(action, Action::None));
    // stop / resume with nothing to act on are polite no-ops, never a panic.
    assert!(matches!(app.slash_tasks("stop"), Action::None));
    assert!(matches!(app.slash_tasks("resume"), Action::None));
    // Progress + terminal hooks with no task are pure no-ops.
    app.sync_active_task_progress();
    app.mark_active_task(TaskStatus::Done);
    assert!(app.tasks.is_empty());
    assert!(!app.has_active_run());
}

#[test]
fn processes_command_routes_only_bounded_exact_ids() {
    let mut app = fresh_app(Some("grok-build"));
    assert_eq!(app.slash_processes(""), Action::ListBackgroundProcesses);
    assert_eq!(app.slash_processes("list"), Action::ListBackgroundProcesses);
    assert_eq!(
        app.slash_processes("stop task-42"),
        Action::StopBackgroundProcess("task-42".to_string())
    );
    assert_eq!(
        app.try_slash_command("/ps")
            .expect("the alias resolves through the command registry"),
        Action::ListBackgroundProcesses
    );

    let before = app.history.len();
    assert_eq!(app.slash_processes("stop task-42 extra"), Action::None);
    assert_eq!(app.slash_processes("stop bad\u{1b}[31m"), Action::None);
    assert!(app.history.iter().skip(before).all(|message| {
        !message.body().contains("task-42 extra") && !message.body().contains('\u{1b}')
    }));
}

#[test]
fn terminal_transitions_settle_the_task_status() {
    // Done on a delivered build.
    let mut app = fresh_app(Some("offline"));
    app.register_run_task("ship it");
    app.apply_engine(EngineEvent::BlockCompleted {
        final_phase: Phase::Delivery,
        paused_at: None,
    });
    assert_eq!(app.tasks.last().unwrap().status, TaskStatus::Done);

    // Failed on an aborted run.
    let mut app = fresh_app(Some("offline"));
    app.register_run_task("build x");
    app.mark_block_aborted("boom".into());
    assert_eq!(app.tasks.last().unwrap().status, TaskStatus::Failed);

    // Done on a clean director build hand-back.
    let mut app = fresh_app(Some("offline"));
    app.register_run_task("build y");
    app.record_agentic_done("done".into(), true, None, None);
    assert_eq!(app.tasks.last().unwrap().status, TaskStatus::Done);
}

#[test]
fn task_registry_caps_history_without_evicting_the_live_run() {
    let mut app = fresh_app(Some("offline"));
    // Fill past the cap with settled tasks.
    for i in 0..(TASKS_CAP + 4) {
        app.register_run_task(&format!("run {i}"));
        app.mark_active_task(TaskStatus::Done);
    }
    // Now a live one.
    app.register_run_task("the live run");
    assert!(app.tasks.len() <= TASKS_CAP);
    assert_eq!(
        app.active_task().unwrap().requirement,
        "the live run",
        "the live run is never evicted"
    );
}

// ---- Task-registry persistence (relaunch survival) -----------------------

fn cfg_offline() -> UserConfig {
    UserConfig {
        backend: Some("offline".to_string()),
        lang: Some("zh-CN".to_string()),
        ..Default::default()
    }
}

#[test]
fn task_registry_persists_and_reloads_across_a_relaunch() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();
    // First session: one settled run + one still-running run.
    {
        let mut app = App::new(
            "demo",
            cfg_offline(),
            root.join("config.toml"),
            root.clone(),
        );
        app.register_run_task("first run");
        app.mark_active_task(TaskStatus::Done);
        app.register_run_task("second run"); // stays Running at exit
        assert_eq!(app.tasks.len(), 2);
    }
    // Relaunch: a fresh App on the SAME root reloads the registry from disk.
    let app2 = App::new(
        "demo",
        cfg_offline(),
        root.join("config.toml"),
        root.clone(),
    );
    assert_eq!(app2.tasks.len(), 2, "recent tasks survive a relaunch");
    // Order preserved (newest last); the settled one kept its outcome.
    assert_eq!(app2.tasks[0].requirement, "first run");
    assert_eq!(app2.tasks[0].status, TaskStatus::Done);
    // The interrupted run is surfaced as Stopped (no live writer after relaunch)
    // — resumable, but not counted as an active run.
    assert_eq!(app2.tasks[1].requirement, "second run");
    assert_eq!(app2.tasks[1].status, TaskStatus::Stopped);
    assert!(!app2.has_active_run());
    // The id sequence advanced past the reloaded ids (no id reuse).
    assert!(
        app2.task_seq >= 2,
        "task_seq carried forward across relaunch"
    );
}

#[test]
fn task_registry_load_is_fail_open_with_no_file() {
    // temp_app builds on a fresh tempdir with no tasks.json → empty, no panic.
    let (app, _tmp) = temp_app();
    assert!(app.tasks.is_empty());
    assert_eq!(app.task_seq, 0);
}

#[test]
fn task_registry_load_is_fail_open_on_a_corrupt_file() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();
    std::fs::create_dir_all(root.join(".umadev")).unwrap();
    std::fs::write(root.join(".umadev").join("tasks.json"), "not json {{{").unwrap();
    // A corrupt registry is ignored (fail-open), never a crash.
    let app = App::new(
        "demo",
        cfg_offline(),
        root.join("config.toml"),
        root.clone(),
    );
    assert!(app.tasks.is_empty());
}

// ---- Trust record-on-approval --------------------------------------------

#[test]
fn approving_a_reversible_action_records_to_the_trust_ledger() {
    let (mut app, _tmp) = temp_app();
    // A plain shell command is a reversible class → remembered.
    let recorded = app.record_action_approval("npm run build", "");
    assert!(recorded, "a reversible action class is remembered");
    // Consultable in-memory for the rest of this session…
    assert!(app.trust_ledger.remembers("npm run build", ""));
    // …and persisted to disk so a later session / consult sees it too.
    let on_disk = umadev_agent::TrustLedger::load(&app.project_root);
    assert!(on_disk.remembers("npm run build", ""));
}

#[test]
fn approving_an_irreversible_action_is_floor_safe_and_records_nothing() {
    let (mut app, _tmp) = temp_app();
    // A network push is the irreversible floor — never remembered (always re-asked).
    let recorded = app.record_action_approval("git push origin main", "");
    assert!(!recorded);
    assert!(!app.trust_ledger.remembers("git push origin main", ""));
    assert!(
        !umadev_agent::TrustLedger::load(&app.project_root).remembers("git push origin main", "")
    );
}

#[test]
fn critic_verdict_records_and_replaces_per_seat() {
    let mut app = fresh_app(Some("offline"));
    app.apply_engine(EngineEvent::CriticVerdict {
        seat: "architect".into(),
        accepts: true,
        blocking: vec![],
        remediation: vec![],
        advisory: vec!["consider a cache".into()],
    });
    app.apply_engine(EngineEvent::CriticVerdict {
        seat: "qa".into(),
        accepts: false,
        blocking: vec!["no tests".into(), "no error handling".into()],
        remediation: vec![],
        advisory: vec![],
    });
    assert_eq!(app.critic_verdicts.len(), 2);
    // A re-review of the SAME seat replaces its row (does not stack).
    app.apply_engine(EngineEvent::CriticVerdict {
        seat: "qa".into(),
        accepts: true,
        blocking: vec![],
        remediation: vec![],
        advisory: vec![],
    });
    assert_eq!(app.critic_verdicts.len(), 2, "seat replaced, not stacked");
    let qa = app.critic_verdicts.iter().find(|c| c.seat == "qa").unwrap();
    assert!(qa.accepts);
}

#[test]
fn split_plan_summary_fails_open_on_odd_shape() {
    // Normal shape: `id · title (seat)`.
    let (id, title) = split_plan_summary("s2 · build the API (backend)", 1);
    assert_eq!(id, "s2");
    assert_eq!(title, "build the API (backend)");
    // No separator → positional id, whole string as title (never drops it).
    let (id, title) = split_plan_summary("just a bare title", 4);
    assert_eq!(id, "s4");
    assert_eq!(title, "just a bare title");
}

#[test]
fn slash_plan_shows_usage_when_no_plan() {
    let mut app = fresh_app(Some("offline"));
    let action = app.try_slash_command("/plan").unwrap();
    assert_eq!(action, Action::None);
    // A "no active plan" hint + the usage line land (not silent).
    let joined: String = app.history.iter().map(|m| m.body().clone()).collect();
    assert!(joined.contains("/plan skip"), "usage shown: {joined}");
}

#[test]
fn slash_plan_skip_folds_into_queued_steer() {
    let mut app = fresh_app(Some("offline"));
    app.apply_engine(EngineEvent::PlanPosted {
        statuses: vec![],
        steps: vec![
            "s1 · scaffold (frontend)".into(),
            "s2 · login route (backend)".into(),
        ],
        done: 0,
        total: 2,
    });
    let action = app.try_slash_command("/plan skip s2").unwrap();
    assert_eq!(action, Action::None);
    // The skip directive is folded into the queued-steer queue (same-session
    // delivery), and it references the skipped step id.
    assert_eq!(app.queued_steer.len(), 1);
    assert!(app.queued_steer[0].contains("s2"));
    assert!(app.queued_steer[0].to_ascii_uppercase().contains("SKIP"));
}

#[test]
fn slash_plan_unknown_step_does_not_queue() {
    let mut app = fresh_app(Some("offline"));
    app.apply_engine(EngineEvent::PlanPosted {
        statuses: vec![],
        steps: vec!["s1 · only step (frontend)".into()],
        done: 0,
        total: 1,
    });
    let _ = app.try_slash_command("/plan veto s9").unwrap();
    // No such step → nothing queued, an honest "no step" note instead.
    assert!(app.queued_steer.is_empty());
    let joined: String = app.history.iter().map(|m| m.body().clone()).collect();
    assert!(joined.contains("s9"));
}

#[test]
fn slash_plan_add_takes_free_text() {
    let mut app = fresh_app(Some("offline"));
    let _ = app
        .try_slash_command("/plan add write integration tests")
        .unwrap();
    assert_eq!(app.queued_steer.len(), 1);
    assert!(app.queued_steer[0].contains("write integration tests"));
    assert!(app.queued_steer[0].to_ascii_uppercase().contains("ADD"));
}

#[test]
fn slash_plan_collapse_toggles_panel() {
    let mut app = fresh_app(Some("offline"));
    assert!(!app.plan_collapsed);
    let _ = app.try_slash_command("/plan collapse").unwrap();
    assert!(app.plan_collapsed);
    let _ = app.try_slash_command("/plan collapse").unwrap();
    assert!(!app.plan_collapsed);
}

#[test]
fn new_run_clears_the_plan_and_review_panels() {
    let mut app = fresh_app(Some("offline"));
    app.apply_engine(EngineEvent::PlanPosted {
        statuses: vec![],
        steps: vec!["s1 · do a thing (frontend)".into()],
        done: 0,
        total: 1,
    });
    app.apply_engine(EngineEvent::CriticVerdict {
        seat: "qa".into(),
        accepts: false,
        blocking: vec!["x".into()],
        remediation: vec![],
        advisory: vec![],
    });
    assert!(!app.plan_steps.is_empty() && !app.critic_verdicts.is_empty());
    app.reset_for_new_run();
    assert!(app.plan_steps.is_empty(), "plan cleared for a fresh run");
    assert!(app.critic_verdicts.is_empty(), "review cleared too");
}

#[test]
fn critic_verdict_is_mirrored_into_the_transcript_with_full_findings() {
    // Defect 1: the panel collapses extra verdicts to "… +N"; the FULL set
    // (seat + every blocking finding) must always reach the scrollable
    // transcript so nothing is lost behind the clip.
    let mut app = fresh_app(Some("offline"));
    app.apply_engine(EngineEvent::CriticVerdict {
        seat: "frontend-engineer".into(),
        accepts: false,
        blocking: vec![
            "API contract drift: /login missing".into(),
            "no error states on the form".into(),
        ],
        remediation: vec![],
        advisory: vec![],
    });
    let joined: String = app.history.iter().map(|m| m.body().clone()).collect();
    assert!(joined.contains("[frontend-engineer]"), "seat in transcript");
    assert!(
        joined.contains("API contract drift: /login missing"),
        "first must-fix in transcript: {joined}"
    );
    assert!(
        joined.contains("no error states on the form"),
        "second must-fix (beyond the panel's first-line inline) in transcript"
    );
}

#[test]
fn a_new_review_round_replaces_the_previous_rounds_seats() {
    // Defect 2a: round 1 blocks with three seats; a plan-step transition seals
    // the round; round 2 has a single passing seat. The panel must show ONLY
    // round 2's seat, not a stale mix of both rounds.
    let mut app = fresh_app(Some("offline"));
    for seat in ["frontend-engineer", "backend-engineer", "qa"] {
        app.apply_engine(EngineEvent::CriticVerdict {
            seat: seat.into(),
            accepts: false,
            blocking: vec!["fix it".into()],
            remediation: vec![],
            advisory: vec![],
        });
    }
    assert_eq!(app.critic_verdicts.len(), 3, "round 1 has three seats");
    // Work resumes (the director drives the next step) → the round is sealed.
    app.apply_engine(EngineEvent::PlanStepStatus {
        id: "s1".into(),
        title: "rework".into(),
        status: "active".into(),
    });
    // Round 2: a single seat now passes.
    app.apply_engine(EngineEvent::CriticVerdict {
        seat: "qa".into(),
        accepts: true,
        blocking: vec![],
        remediation: vec![],
        advisory: vec![],
    });
    assert_eq!(
        app.critic_verdicts.len(),
        1,
        "the new round replaced the old one, not a stale mix"
    );
    assert_eq!(app.critic_verdicts[0].seat, "qa");
    assert!(app.critic_verdicts[0].accepts, "shows the CURRENT round");
}

#[test]
fn contiguous_verdicts_in_one_round_do_not_clear_each_other() {
    // The seal must NOT fire between two seats of the SAME round (no work
    // event interleaves a review burst), so both seats accumulate.
    let mut app = fresh_app(Some("offline"));
    app.apply_engine(EngineEvent::CriticVerdict {
        seat: "architect".into(),
        accepts: true,
        blocking: vec![],
        remediation: vec![],
        advisory: vec![],
    });
    app.apply_engine(EngineEvent::CriticVerdict {
        seat: "qa".into(),
        accepts: false,
        blocking: vec!["no tests".into()],
        remediation: vec![],
        advisory: vec![],
    });
    assert_eq!(app.critic_verdicts.len(), 2, "one round keeps both seats");
}

#[test]
fn delivery_finish_clears_the_live_plan_and_review_panels() {
    // Defect 2b: a finished run must not leave a stale live plan / verdict
    // list hanging below the transcript — the terminal transition clears them
    // and folds the round into a one-line summary in the transcript.
    let mut app = fresh_app(Some("offline"));
    app.run_started = true;
    app.apply_engine(EngineEvent::PlanPosted {
        statuses: vec![],
        steps: vec!["s1 · ship it (frontend)".into()],
        done: 0,
        total: 1,
    });
    app.apply_engine(EngineEvent::CriticVerdict {
        seat: "qa".into(),
        accepts: true,
        blocking: vec![],
        remediation: vec![],
        advisory: vec![],
    });
    assert!(!app.plan_steps.is_empty() && !app.critic_verdicts.is_empty());
    app.apply_engine(EngineEvent::BlockCompleted {
        final_phase: Phase::Delivery,
        paused_at: None,
    });
    assert!(app.finished, "the run reached its terminal delivery state");
    assert!(
        app.plan_steps.is_empty(),
        "the live plan panel is cleared on finish"
    );
    assert!(
        app.critic_verdicts.is_empty(),
        "the live team-review panel is cleared on finish"
    );
    // The verdict content isn't silently dropped — a settle summary lands.
    let joined: String = app.history.iter().map(|m| m.body().clone()).collect();
    assert!(
        joined.contains(umadev_i18n::t(app.lang, "plan.review.title")),
        "a team-review settle summary is folded into the transcript: {joined}"
    );
}

#[test]
fn an_aborted_block_clears_the_live_plan_and_review_panels() {
    // Defect 2b (abort branch): a bailed round is terminal too — its panels
    // must not linger as stale state.
    let mut app = fresh_app(Some("offline"));
    app.run_started = true;
    app.apply_engine(EngineEvent::PlanPosted {
        statuses: vec![],
        steps: vec!["s1 · do a thing (frontend)".into()],
        done: 0,
        total: 1,
    });
    app.apply_engine(EngineEvent::CriticVerdict {
        seat: "qa".into(),
        accepts: false,
        blocking: vec!["broken".into()],
        remediation: vec![],
        advisory: vec![],
    });
    assert!(!app.plan_steps.is_empty() && !app.critic_verdicts.is_empty());
    app.apply_engine(EngineEvent::Note(format!(
        "{}本轮已中止:磁盘写入失败",
        crate::ABORT_SENTINEL
    )));
    assert!(app.aborted, "the sentinel flips the run into aborted");
    assert!(app.plan_steps.is_empty(), "plan panel cleared on abort");
    assert!(
        app.critic_verdicts.is_empty(),
        "team-review panel cleared on abort"
    );
}

// --- Wave 1: honest picker auth state (gap G10) ---

#[test]
fn parse_probe_detail_unpacks_packed_auth_metadata() {
    // The packed shape spawn_probe emits.
    let s = PROBE_AUTH_SENTINEL;
    let packed =
        format!("{s}auth=not_logged_in|login=claude auth login|install=npm i -g x{s}claude 1.6.0");
    let (auth, login, install, human) = parse_probe_detail(&packed);
    assert_eq!(auth, AuthMark::NotLoggedIn);
    assert_eq!(login, "claude auth login");
    assert_eq!(install, "npm i -g x");
    assert_eq!(human, "claude 1.6.0");
    // Fail-open: a plain (untagged) detail keeps the human text, Unknown auth.
    let (auth, login, _i, human) = parse_probe_detail("claude 1.6.0");
    assert_eq!(auth, AuthMark::Unknown);
    assert!(login.is_empty());
    assert_eq!(human, "claude 1.6.0");
}

// Drive a probe through the engine, then select that base in the picker.
fn probe_and_select(app: &mut App, id: &str, auth: &str, login: &str, install: &str) {
    let s = PROBE_AUTH_SENTINEL;
    let detail = format!("{s}auth={auth}|login={login}|install={install}{s}{id} 1.0.0");
    // `ready` mirrors spawn_probe: true only when logged in.
    app.apply_engine(EngineEvent::BackendProbed {
        backend_id: id.into(),
        ready: auth == "logged_in",
        detail,
    });
    app.goto_picker_step(PickerStep::BaseCli);
    let idx = app
        .picker_items
        .iter()
        .position(|i| i.backend_id.as_deref() == Some(id))
        .unwrap();
    app.picker_selected = idx;
}

#[test]
fn picker_blocks_commit_on_not_logged_in_with_login_cmd() {
    let mut app = fresh_app(None);
    probe_and_select(
        &mut app,
        "claude-code",
        "not_logged_in",
        "claude auth login",
        "npm i -g claude",
    );
    let action = app.apply_key(KeyCode::Enter);
    // FIRST Enter: soft-warned, NOT committed — stays on the picker with the login command
    // surfaced (the probe may be a false negative for a local/third-party-configured base).
    assert_eq!(action, Action::None);
    assert_eq!(app.mode, AppMode::Picker);
    let notice = app.picker_notice.as_deref().unwrap_or("");
    assert!(
        notice.contains("claude auth login"),
        "login cmd shown: {notice}"
    );
    // SECOND Enter on the SAME base: proceeds anyway (drive-whatever-is-configured), so a
    // local/third-party model that needs no login is never permanently blocked.
    let action2 = app.apply_key(KeyCode::Enter);
    assert_ne!(action2, Action::None, "second select commits, not blocked");
    assert_eq!(app.mode, AppMode::Chat, "second select enters chat");
}

#[test]
fn picker_blocks_commit_on_not_installed_with_install_cmd() {
    let mut app = fresh_app(None);
    probe_and_select(
        &mut app,
        "codex",
        "not_installed",
        "codex login",
        "npm install -g @openai/codex",
    );
    let action = app.apply_key(KeyCode::Enter);
    assert_eq!(action, Action::None);
    assert_eq!(app.mode, AppMode::Picker);
    let notice = app.picker_notice.as_deref().unwrap_or("");
    assert!(
        notice.contains("npm install -g @openai/codex"),
        "install cmd shown: {notice}"
    );
}

#[test]
fn picker_commits_when_logged_in() {
    let mut app = fresh_app(None);
    probe_and_select(
        &mut app,
        "claude-code",
        "logged_in",
        "claude auth login",
        "",
    );
    let action = app.apply_key(KeyCode::Enter);
    // A logged-in base commits straight into chat.
    assert_eq!(action, Action::BackendChanged);
    assert_eq!(app.mode, AppMode::Chat);
    assert_eq!(app.backend_label, "claude-code");
}

#[test]
fn chat_plain_text_routes_to_worker() {
    let mut app = fresh_app(Some("offline"));
    for c in "build a login".chars() {
        let _ = app.apply_key(KeyCode::Char(c));
    }
    let action = app.apply_key(KeyCode::Enter);
    assert_eq!(action, Action::Route("build a login".to_string()));
    // Input is cleared after submit.
    assert!(app.input.is_empty());
}

#[test]
fn chat_empty_enter_is_noop() {
    let mut app = fresh_app(Some("offline"));
    let action = app.apply_key(KeyCode::Enter);
    assert_eq!(action, Action::None);
}

#[test]
fn slash_help_toggles_help_overlay() {
    let mut app = fresh_app(Some("offline"));
    for c in "/help".chars() {
        let _ = app.apply_key(KeyCode::Char(c));
    }
    let action = app.apply_key(KeyCode::Enter);
    assert_eq!(action, Action::None);
    assert!(app.show_help);
}

#[test]
fn slash_quit_returns_quit() {
    let mut app = fresh_app(Some("offline"));
    for c in "/quit".chars() {
        let _ = app.apply_key(KeyCode::Char(c));
    }
    let action = app.apply_key(KeyCode::Enter);
    assert_eq!(action, Action::Quit);
    assert!(app.should_quit);
}

#[test]
fn slash_clear_clears_history() {
    let mut app = fresh_app(Some("offline"));
    assert!(!app.history.is_empty()); // greeting present
    for c in "/clear".chars() {
        let _ = app.apply_key(KeyCode::Char(c));
    }
    let _ = app.apply_key(KeyCode::Enter);
    // After /clear: only the localized "history cleared" system note remains.
    assert_eq!(app.history.len(), 1);
    assert_eq!(
        app.history.front().unwrap().body(),
        umadev_i18n::t(app.lang, "slash.history_cleared")
    );
}

#[test]
fn slash_clear_drops_transient_routing_and_queued_input_state() {
    let mut app = fresh_app(Some("offline"));
    app.queued_steer.push_back("change the old run".into());
    app.pending_steer = Some("pending old revision".into());
    app.queued_chat.push_back("old deferred chat".into());
    app.route_backlog_len = 1;
    app.last_dispatched_chat = Some("same first message".into());

    let action = app.try_slash_command("/clear");
    assert_eq!(action, Some(Action::None));
    assert!(app.queued_steer.is_empty());
    assert!(app.pending_steer.is_none());
    assert!(app.queued_chat.is_empty());
    assert_eq!(app.route_backlog_len, 0);
    assert!(app.last_dispatched_chat.is_none());

    assert!(
        matches!(
            app.submit_text("same first message".into()),
            Action::Route(_)
        ),
        "the new chat's first turn must be routed"
    );
}

#[test]
fn explicit_retry_after_recoverable_base_failure_is_never_swallowed() {
    let mut app = fresh_app(Some("codex"));
    let request = "下一步计划是什么";
    app.last_dispatched_chat = Some(request.to_string());
    app.record_route_failed(
        "底座 codex 返回 502 Bad Gateway".to_string(),
        FailedRouteOrigin::Chat,
    );

    assert!(matches!(
        app.submit_text(request.to_string()),
        Action::Route(text) if text == request
    ));
}

#[test]
fn session_tokens_accumulate_across_turns_and_reset_on_clear() {
    let mut a = fresh_app(Some("offline"));
    assert_eq!(
        a.session_usage.tokens(),
        0,
        "a fresh session meters from zero"
    );
    // The base reports per-turn usage; the gauge total sums input+output.
    a.apply_engine(EngineEvent::TurnUsage {
        usage: Some(Usage::exact(1_200, 800)),
        est_tokens: 0,
    });
    assert_eq!(
        a.session_usage.tokens(),
        2_000,
        "the first turn's usage accrues"
    );
    a.apply_engine(EngineEvent::TurnUsage {
        usage: Some(Usage::exact(500, 500)),
        est_tokens: 0,
    });
    assert_eq!(
        a.session_usage.tokens(),
        3_000,
        "usage accumulates across turns"
    );
    // `/clear` starts a fresh session — the meter resets with the transcript.
    let _ = a.try_slash_command("/clear");
    assert_eq!(a.session_usage.tokens(), 0, "/clear resets the token meter");
}

#[test]
fn slash_claude_switches_backend_and_saves() {
    let tmp = tempfile::TempDir::new().unwrap();
    let cfg_path = tmp.path().join("config.toml");
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let cfg = UserConfig {
        backend: Some("offline".to_string()),
        ..Default::default()
    };
    let mut app = App::new("demo", cfg, cfg_path.clone(), workspace);
    for c in "/claude".chars() {
        let _ = app.apply_key(KeyCode::Char(c));
    }
    let action = app.apply_key(KeyCode::Enter);
    assert_eq!(action, Action::BackendChanged);
    assert_eq!(app.backend_label, "claude-code");
    // Config is persisted.
    let loaded = crate::config::load_from(&cfg_path);
    assert_eq!(loaded.backend.as_deref(), Some("claude-code"));
}

#[test]
fn slash_continue_with_open_gate_returns_continue() {
    let mut app = fresh_app(Some("offline"));
    app.apply_engine(EngineEvent::GateOpened {
        gate: Gate::DocsConfirm,
        choice: None,
    });
    for c in "/continue".chars() {
        let _ = app.apply_key(KeyCode::Char(c));
    }
    let action = app.apply_key(KeyCode::Enter);
    assert_eq!(action, Action::Continue(Gate::DocsConfirm));
}

#[test]
fn slash_continue_without_gate_is_noop_with_hint() {
    let mut app = fresh_app(Some("offline"));
    for c in "/continue".chars() {
        let _ = app.apply_key(KeyCode::Char(c));
    }
    let action = app.apply_key(KeyCode::Enter);
    assert_eq!(action, Action::None);
    assert!(app
        .history
        .iter()
        .any(|m| m.body().contains("还没启动流水线") || m.body().contains("没有打开的 gate")));
}

#[test]
fn slash_continue_with_a_resumable_plan_resumes_instead_of_hinting() {
    // /continue on a FRESH session (no in-memory gate) with a persisted, resumable
    // director-loop run on disk must RE-ATTACH (Action::ResumeRun + a resuming
    // note), not show the "no pipeline started" restart hint.
    let mut app = fresh_app(Some("claude-code"));
    // Persist a plan with one Pending step + a workflow-state carrying the
    // requirement — exactly what an interrupted /run leaves behind.
    let plan = umadev_agent::Plan {
        steps: vec![umadev_agent::PlanStep {
            files: umadev_agent::StepFiles::default(),
            id: "a".into(),
            title: "build the login page".into(),
            seat: umadev_agent::Seat::FrontendEngineer,
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
    let mut state = umadev_agent::WorkflowState::new(umadev_spec::Phase::Frontend);
    state.slug = "demo".into();
    state.requirement = "做一个登录页".into();
    state.backend = "claude-code".into();
    umadev_agent::write_workflow_state(&app.project_root, &state).unwrap();

    let before = app.history.len();
    let action = app
        .try_slash_command("/continue")
        .expect("/continue is a slash command");
    assert_eq!(
        action,
        Action::ResumeRun("做一个登录页".to_string()),
        "a resumable run resumes with the persisted requirement"
    );
    // The trilingual resuming note was surfaced (not the restart hint).
    assert!(
        app.history
            .iter()
            .skip(before)
            .any(|m| m.body().contains("续跑")),
        "the resuming note is shown"
    );
    assert!(
        !app.history
            .iter()
            .skip(before)
            .any(|m| m.body().contains("还没启动流水线")),
        "the restart hint is NOT shown"
    );
}

#[test]
fn resuming_a_blocked_run_keeps_earlier_transcript_and_marks_a_continued_divider() {
    // User-reported: after a run BLOCKS and the user `/continue`s, the earlier
    // steps must NOT disappear from the transcript. The block only clears the
    // LIVE PANEL (plan / verdict) state; the durable `history` (plan-posted
    // memo + per-seat critic notes + the block message) is preserved, and the
    // resume APPENDS a "— continued —" divider so the run reads as one
    // continuous history the user can scroll back through.
    let mut app = fresh_app(Some("claude-code"));

    // ---- Earlier steps: a posted plan + two seat verdicts (one a BLOCK). ----
    app.apply_engine(EngineEvent::PlanPosted {
        statuses: vec![],
        steps: vec![
            "s1 · scaffold app (frontend-engineer)".into(),
            "s2 · wire auth API (backend-engineer)".into(),
        ],
        done: 0,
        total: 2,
    });
    app.apply_engine(EngineEvent::CriticVerdict {
        seat: "architect".into(),
        accepts: true,
        blocking: vec![],
        remediation: vec![],
        advisory: vec![],
    });
    app.apply_engine(EngineEvent::CriticVerdict {
        seat: "security".into(),
        accepts: false,
        blocking: vec!["step-2-auth-token-leak".into()],
        remediation: vec!["scope the token to the session".into()],
        advisory: vec![],
    });
    // The run hits a hard block and stops (clears the live panels, keeps the
    // transcript).
    app.mark_block_aborted("step-2-blocked-marker".into());

    // The panels are gone, but the earlier per-step content is still in the
    // scrollable transcript.
    assert!(
        app.plan_steps.is_empty(),
        "block clears the live plan panel"
    );
    assert!(
        app.critic_verdicts.is_empty(),
        "block clears the live review panel"
    );
    // The per-step team-review notes (seat + blocking finding) and the block
    // message are the durable transcript record that must survive the block.
    // (Plan step TITLES live only in the panel, so they are re-posted on
    // resume, not asserted here.)
    let earlier_markers = [
        "security",
        "step-2-auth-token-leak",
        "step-2-blocked-marker",
    ];
    for m in earlier_markers {
        assert!(
            app.history.iter().any(|row| row.body().contains(m)),
            "earlier transcript content `{m}` is present after the block"
        );
    }

    // A resumable run exists on disk (what an interrupted /run leaves behind).
    let plan = umadev_agent::Plan {
        steps: vec![umadev_agent::PlanStep {
            files: umadev_agent::StepFiles::default(),
            id: "s2".into(),
            title: "wire auth API".into(),
            seat: umadev_agent::Seat::BackendEngineer,
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
    let mut state = umadev_agent::WorkflowState::new(umadev_spec::Phase::Backend);
    state.slug = "demo".into();
    state.requirement = "做一个登录页".into();
    state.backend = "claude-code".into();
    umadev_agent::write_workflow_state(&app.project_root, &state).unwrap();

    // Snapshot the earlier transcript row bodies, then resume.
    let earlier_bodies: Vec<String> = app.history.iter().map(|m| m.body().to_string()).collect();
    let len_before = app.history.len();
    let action = app
        .try_slash_command("/continue")
        .expect("/continue is a slash command");
    assert_eq!(action, Action::ResumeRun("做一个登录页".to_string()));

    // Every earlier row is STILL present, in order (nothing was dropped).
    let after_bodies: Vec<String> = app.history.iter().map(|m| m.body().to_string()).collect();
    assert_eq!(
        &after_bodies[..len_before],
        &earlier_bodies[..],
        "the earlier transcript is preserved, unmodified, as a prefix"
    );

    // The resume APPENDED a "— continued —" divider (the localized separator).
    let separator = umadev_i18n::t(app.lang, "continue.separator");
    let sep_idx = app
        .history
        .iter()
        .position(|m| m.body() == separator)
        .expect("a continued divider was appended on resume");
    assert!(
        sep_idx >= len_before,
        "the divider is appended AFTER the preserved earlier transcript"
    );
    // The resuming note follows the divider (earlier steps · divider · resume).
    let resume_idx = app
        .history
        .iter()
        .rposition(|m| m.body().contains("续跑"))
        .expect("the resuming note is shown");
    assert!(
        sep_idx < resume_idx,
        "the divider precedes the resuming note"
    );
}

#[test]
fn slash_revise_at_gate_returns_revise_with_text() {
    let mut app = fresh_app(Some("offline"));
    app.apply_engine(EngineEvent::GateOpened {
        gate: Gate::DocsConfirm,
        choice: None,
    });
    for c in "/revise 把 OAuth 删掉".chars() {
        let _ = app.apply_key(KeyCode::Char(c));
    }
    let action = app.apply_key(KeyCode::Enter);
    assert_eq!(action, Action::Revise("把 OAuth 删掉".to_string()));
}

#[test]
fn slash_revise_without_args_is_noop_with_usage_hint() {
    let mut app = fresh_app(Some("offline"));
    app.apply_engine(EngineEvent::GateOpened {
        gate: Gate::DocsConfirm,
        choice: None,
    });
    for c in "/revise".chars() {
        let _ = app.apply_key(KeyCode::Char(c));
    }
    let action = app.apply_key(KeyCode::Enter);
    assert_eq!(action, Action::None);
    assert!(app.history.iter().any(|m| m.body().contains("/revise")));
}

#[test]
fn slash_goal_with_objective_starts_a_goal_driven_build() {
    // `/goal <objective>` → a goal-driven director build (StartGoal), carrying
    // the whole arg as the objective (no slug parsing — a goal is a sentence).
    let mut app = fresh_app(Some("offline"));
    for c in "/goal build a shippable todo app".chars() {
        let _ = app.apply_key(KeyCode::Char(c));
    }
    let action = app.apply_key(KeyCode::Enter);
    assert_eq!(
        action,
        Action::StartGoal("build a shippable todo app".to_string())
    );
    // A goal acknowledgement was surfaced to the user (the `goal.starting` line).
    assert!(app
        .history
        .iter()
        .any(|m| m.body().contains("build a shippable todo app")));
}

#[test]
fn slash_goal_without_objective_is_noop_with_usage_hint() {
    // Empty `/goal` → a usage hint, no build kicked off.
    let mut app = fresh_app(Some("offline"));
    for c in "/goal".chars() {
        let _ = app.apply_key(KeyCode::Char(c));
    }
    let action = app.apply_key(KeyCode::Enter);
    assert_eq!(action, Action::None);
    assert!(app.history.iter().any(|m| m.body().contains("/goal")));
}

#[test]
fn goal_is_a_registered_slash_verb() {
    // The `/goal` verb is in the palette so completion + help surface it.
    assert!(App::COMMANDS.iter().any(|c| c.name == "goal"));
}

/// Parse the canonical verbs from the dispatch `match` arms by reading THIS
/// source between the `COMMAND-DISPATCH-START/END` sentinels. An arm head is
/// a line that (after trimming) starts with a string literal and contains
/// `=>`; its `|`-separated quoted literals are the verbs it handles. The `_`
/// fallback sits past the END sentinel. This reads the REAL dispatcher, so it
/// can't drift from the registry.
fn dispatch_arm_verbs() -> Vec<String> {
    let src = include_str!("../app.rs");
    let start = src
        .find("// COMMAND-DISPATCH-START")
        .expect("dispatch start sentinel present");
    let end = src
        .find("// COMMAND-DISPATCH-END")
        .expect("dispatch end sentinel present");
    assert!(end > start, "END sentinel follows START");
    let mut verbs = Vec::new();
    for line in src[start..end].lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with('"') {
            continue;
        }
        let Some(arrow) = trimmed.find("=>") else {
            continue;
        };
        for part in trimmed[..arrow].split('|') {
            let part = part.trim();
            if let Some(inner) = part.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
                verbs.push(inner.to_string());
            }
        }
    }
    verbs
}

#[test]
fn commands_and_dispatch_are_in_lockstep() {
    // The ONE-registry invariant (UX maturity Fix A): the palette, the help
    // overlay, and the dispatcher all read `App::COMMANDS`. This test locks
    // the registry against the actual dispatch arms so the three surfaces can
    // never drift again (the historical bugs: `/model` dispatchable yet not
    // in the palette; a dozen verbs missing from help; aliases only in
    // dispatch). Mirrors how a mature TUI locks its built-in command names.
    let dispatch = dispatch_arm_verbs();
    assert!(
        dispatch.len() >= 40,
        "parsed the dispatch arms (got {}): {dispatch:?}",
        dispatch.len()
    );

    // (1) Every non-hidden registry command has a dispatch arm on its
    //     canonical name — the palette/help can't advertise an unwired verb.
    for c in App::COMMANDS {
        if c.hidden {
            continue;
        }
        assert!(
            dispatch.iter().any(|v| v == c.name),
            "/{} is in COMMANDS but has no dispatch arm",
            c.name
        );
        // Each alias resolves CENTRALLY back to its command (aliases live only
        // in the registry now), so a typed alias always reaches the handler.
        for alias in c.aliases {
            let resolved = App::resolve_command(alias);
            assert!(
                resolved.is_some_and(|r| r.name == c.name),
                "alias /{alias} of /{} does not resolve to it",
                c.name
            );
        }
    }

    // (2) Every dispatch arm is a registered command name — a hand-added
    //     `match` arm that forgot the registry fails right here.
    for verb in &dispatch {
        assert!(
            App::COMMANDS.iter().any(|c| c.name == verb),
            "dispatch arm \"{verb}\" is not a registered COMMANDS name"
        );
    }

    // (3) Names + aliases are globally unique, so resolution is unambiguous.
    let mut seen = std::collections::HashSet::new();
    for c in App::COMMANDS {
        assert!(seen.insert(c.name), "duplicate command name /{}", c.name);
        for alias in c.aliases {
            assert!(
                seen.insert(*alias),
                "alias /{alias} collides with another verb"
            );
        }
        // Every description key must be present in the catalog (resolves to a
        // real string, not the key echoed back) so no palette/help row is blank.
        assert_ne!(
            umadev_i18n::t(umadev_i18n::Lang::En, c.desc_key),
            c.desc_key,
            "/{} desc_key {} is missing from the i18n catalog",
            c.name,
            c.desc_key
        );
    }
}

#[test]
fn slash_unknown_command_hints() {
    let mut app = fresh_app(Some("offline"));
    for c in "/foo".chars() {
        let _ = app.apply_key(KeyCode::Char(c));
    }
    let _ = app.apply_key(KeyCode::Enter);
    assert!(app
        .history
        .iter()
        .any(|m| m.body().contains("未知命令") && m.body().contains("/foo")));
}

#[test]
fn plain_text_at_open_gate_routes_to_revise() {
    for text in ["去掉 OAuth", "改成暗色", "不要继续评审"] {
        let mut app = fresh_app(Some("offline"));
        app.apply_engine(EngineEvent::GateOpened {
            gate: Gate::DocsConfirm,
            choice: None,
        });
        for c in text.chars() {
            let _ = app.apply_key(KeyCode::Char(c));
        }
        let action = app.apply_key(KeyCode::Enter);
        assert_eq!(action, Action::Revise(text.to_string()), "{text}");
    }
}

#[test]
fn approval_words_at_open_gate_approve_instead_of_revising() {
    // A2#2: "确认" / "通过" / "approve" / "ok" / "lgtm" at a gate run through
    // `classify_reply` and APPROVE — the reported trap was typing 确认 and
    // watching the whole producing block re-run as a "revision". The literal
    // `c` shortcut keeps working (covered elsewhere).
    for word in ["确认", "通过", "approve", "ok", "LGTM"] {
        let mut app = fresh_app(Some("offline"));
        app.apply_engine(EngineEvent::GateOpened {
            gate: Gate::DocsConfirm,
            choice: None,
        });
        let action = app.submit_text(word.to_string());
        assert_eq!(
            action,
            Action::Continue(Gate::DocsConfirm),
            "`{word}` approves the gate"
        );
        assert!(app.active_gate.is_none(), "`{word}` cleared the gate");
    }
}

#[test]
fn cancel_words_at_open_gate_cancel_the_run() {
    // A2#2: "取消" / "cancel" at a gate cancels (the picker's Cancel path) —
    // never a revision that re-runs the block with "取消" as feedback.
    for word in ["取消", "停止", "重来", "cancel", "stop", "restart"] {
        let mut app = fresh_app(Some("offline"));
        app.apply_engine(EngineEvent::GateOpened {
            gate: Gate::DocsConfirm,
            choice: None,
        });
        let action = app.submit_text(word.to_string());
        assert_eq!(action, Action::Cancel, "`{word}` cancels at the gate");
        assert!(app.active_gate.is_none(), "`{word}` cleared the gate");
    }
}

#[test]
fn cancel_remains_immediate_while_gate_question_is_in_flight() {
    let mut app = fresh_app(Some("offline"));
    app.active_gate = Some(Gate::DocsConfirm);
    assert!(matches!(
        app.submit_text("为什么需要这个依赖？".into()),
        Action::GateQuery { .. }
    ));
    assert!(app.gate_query_in_flight);

    assert_eq!(app.submit_text("停止".into()), Action::Cancel);
    assert!(app.active_gate.is_none());
}

#[test]
fn gate_query_is_exclusive_and_stale_results_are_ignored() {
    let mut app = fresh_app(Some("offline"));
    app.apply_engine(EngineEvent::GateOpened {
        gate: Gate::DocsConfirm,
        choice: GateChoice::standard(Gate::DocsConfirm),
    });
    let Action::GateQuery { epoch, .. } = app.submit_text("为什么要加这个依赖？".into())
    else {
        panic!("gate question should start a query");
    };

    for command in ["/continue", "/revise 改小一点", "/redo frontend", "/clear"] {
        assert_eq!(
            app.try_slash_command(command),
            Some(Action::None),
            "{command}"
        );
        assert_eq!(app.active_gate, Some(Gate::DocsConfirm));
    }
    assert_eq!(app.gate_choice_pick(0), Action::None);
    assert!(app.gate_query_in_flight);

    app.begin_cancelling();
    let history_len = app.history.len();
    assert!(!app.record_gate_query_done(epoch, "late answer".into()));
    assert_eq!(
        app.history.len(),
        history_len,
        "late answer stays invisible"
    );
    app.cancel_run();
}

#[test]
fn current_gate_query_result_is_displayed_and_recorded_atomically() {
    let mut app = fresh_app(Some("offline"));
    app.active_gate = Some(Gate::DocsConfirm);
    let Action::GateQuery { epoch, .. } = app.submit_text("为什么？".into()) else {
        panic!("query expected");
    };

    assert!(app.record_gate_query_done(epoch, "因为有可复核证据。".into()));
    assert!(!app.gate_query_in_flight);
    assert_eq!(app.active_gate, Some(Gate::DocsConfirm));
    assert!(app
        .history
        .iter()
        .any(|row| row.body() == "因为有可复核证据。"));
    assert_eq!(
        app.conversation.last().map(|turn| turn.content.as_str()),
        Some("因为有可复核证据。")
    );
}

#[test]
fn director_run_splits_current_steer_from_question_and_later_turns() {
    // A clear adjustment to the current artifact reaches the step-boundary
    // steer intake.
    let mut app = fresh_app(Some("offline"));
    app.thinking = true;
    app.director_run_in_flight = true;
    for text in ["把配色换成暗色", "改成紧凑布局", "不要继续评审"] {
        let action = app.submit_text(text.into());
        assert_eq!(action, Action::None, "{text}");
    }
    assert_eq!(app.queued_steer.len(), 3, "steer lane took the messages");
    assert!(app.queued_chat.is_empty(), "chat lane untouched");

    // A natural-language stop is a control action, never delayed steering.
    assert_eq!(app.submit_text("停止".into()), Action::Cancel);

    // A turn-scoped observation is answered from local facts immediately;
    // it is neither a writer steer nor a deferred model turn.
    assert_eq!(app.submit_text("这次改了什么".into()), Action::None);
    assert_eq!(app.queued_steer.len(), 3);
    assert!(app.queued_chat.is_empty());

    // Questions, explicit future tasks, and ambiguity all wait for an
    // ordinary model-routed turn after the run; they never contaminate the
    // current writer's directive.
    for text in ["为什么正在跑 Maven？", "完成后再做登录", "另一个问题"] {
        let action = app.submit_text(text.into());
        assert_eq!(action, Action::None);
    }
    assert_eq!(
        app.queued_steer.len(),
        3,
        "only the explicit steer is injected"
    );
    assert_eq!(app.queued_chat.len(), 3, "later model turns are deferred");

    // A non-director thinking turn still parks on the chat lane.
    let mut chat = fresh_app(Some("offline"));
    chat.thinking = true;
    let action = chat.submit_text("另一个问题".into());
    assert_eq!(action, Action::None);
    assert!(chat.queued_steer.is_empty());
    assert_eq!(chat.queued_chat.len(), 1);
}

#[test]
fn non_steering_gate_input_is_deferred_not_reinterpreted_as_revision() {
    for text in [
        "为什么要增加这个依赖？",
        "為什麼要增加這個相依套件？",
        "why are we adding this dependency?",
        "解释这个依赖的作用",
        "說明新增依賴的理由",
        "这个依赖是干嘛的",
        "另一个问题",
    ] {
        let mut app = fresh_app(Some("offline"));
        app.active_gate = Some(Gate::DocsConfirm);

        let action = app.submit_text(text.into());

        assert!(
            matches!(action, Action::GateQuery { question, .. } if question == text),
            "{text}"
        );
        assert_eq!(app.active_gate, Some(Gate::DocsConfirm), "{text}");
        assert!(app.gate_query_in_flight, "{text}");
        assert!(app.queued_chat.is_empty(), "{text}");
        assert!(app.queued_steer.is_empty(), "{text}");
    }

    for text in ["完成后再做登录", "after this, add account export"] {
        let mut app = fresh_app(Some("offline"));
        app.active_gate = Some(Gate::DocsConfirm);

        let action = app.submit_text(text.into());

        assert_eq!(action, Action::None, "{text}");
        assert_eq!(app.active_gate, Some(Gate::DocsConfirm), "{text}");
        assert!(app.queued_steer.is_empty(), "{text}");
        assert_eq!(
            app.queued_chat.front().map(String::as_str),
            Some(text),
            "{text}"
        );
        assert!(
            app.history
                .iter()
                .all(|message| !message.body().contains("收到修订")),
            "a gate question must not trigger Action::Revise: {text}"
        );
    }
}

#[test]
fn live_meta_classifier_is_bounded_and_trilingual() {
    for (text, expected) in [
        ("这次改动都做了啥？", LiveMetaIntent::Changes),
        ("能说下这次都改了哪些内容吗？", LiveMetaIntent::Changes),
        ("你这次都改了些什么？", LiveMetaIntent::Changes),
        ("本輪改動", LiveMetaIntent::Changes),
        ("what did you change?", LiveMetaIntent::Changes),
        (
            "could you tell me what changed this time?",
            LiveMetaIntent::Changes,
        ),
        ("what files did you change?", LiveMetaIntent::Changes),
        ("当前进度", LiveMetaIntent::Progress),
        ("现在进展到哪一步啦？", LiveMetaIntent::Progress),
        ("目前進度？", LiveMetaIntent::Progress),
        ("what are you working on?", LiveMetaIntent::Progress),
        ("how far along are you?", LiveMetaIntent::Progress),
        (
            "could you give me a current progress update?",
            LiveMetaIntent::Progress,
        ),
    ] {
        assert_eq!(classify_live_meta(text), Some(expected), "{text}");
    }
    for mutation in [
        "修改当前进度组件",
        "当前进度组件如何修改",
        "把本次改动写进 CHANGELOG",
        "这次修改有哪些要求",
        "总结本次改动并补测试",
        "show me the changes and then fix the tests",
        "what changes should we make?",
        "build a current status component",
    ] {
        assert_eq!(classify_live_meta(mutation), None, "{mutation}");
    }
}

#[test]
fn live_meta_skips_running_queues_but_preserves_conversation_memory() {
    let mut app = fresh_app(Some("offline"));
    app.push(ChatRole::You, "帮我优化 SEO");
    app.thinking = true;
    app.director_run_in_flight = true;
    app.plan_steps.push(PlanStepRow {
        id: "s1".into(),
        title: "更新页面元数据".into(),
        status: "active".into(),
        seat: "frontend-engineer".into(),
    });
    let conversation_before = app.conversation.len();
    let full_before = app.full_transcript.len();

    assert_eq!(app.submit_text("当前进度？".into()), Action::None);
    assert!(app.queued_steer.is_empty());
    assert!(app.queued_chat.is_empty());
    assert_eq!(app.conversation.len(), conversation_before + 2);
    assert_eq!(app.full_transcript.len(), full_before + 2);
    assert_eq!(
        app.conversation[conversation_before].role, "user",
        "the local question remains available to the next model turn"
    );
    assert_eq!(app.conversation[conversation_before + 1].role, "assistant");
    let answer = app.history.back().unwrap().body();
    assert!(answer.contains("s1") && answer.contains("更新页面元数据"));

    app.active_gate = Some(Gate::DocsConfirm);
    assert_eq!(app.submit_text("本次改动".into()), Action::None);
    assert_eq!(app.active_gate, Some(Gate::DocsConfirm));
    assert!(app.queued_steer.is_empty());
}

#[test]
fn live_change_answer_prefers_this_turns_diff_rows() {
    let mut app = fresh_app(Some("offline"));
    app.push(ChatRole::You, "修复标题");
    app.thinking = true;
    app.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ToolUse {
            name: "Edit".into(),
            detail: "src/App.tsx".into(),
            edit: Some(umadev_runtime::ToolEdit {
                path: "src/App.tsx".into(),
                before: "old\n".into(),
                after: "new\n".into(),
            }),
        },
    });

    assert_eq!(app.submit_text("这次改了什么？".into()), Action::None);
    assert!(app.queued_chat.is_empty());
    assert!(app.queued_steer.is_empty());
    assert!(app.history.back().unwrap().body().contains("src/App.tsx"));

    assert_eq!(app.submit_text("当前进度".into()), Action::None);
    let progress = app.history.back().unwrap().body();
    assert!(progress.contains("修复标题"));
    assert!(!progress.contains("正在处理：这次改了什么"));

    assert_eq!(app.submit_text("本次改动".into()), Action::None);
    assert!(
        app.history.back().unwrap().body().contains("src/App.tsx"),
        "consecutive meta questions keep the prior real-turn diff boundary"
    );
}

#[test]
fn git_status_fallback_discloses_pre_run_change_risk() {
    let (mut app, tmp) = temp_app();
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(tmp.path())
        .args(["init", "-q"])
        .status()
        .unwrap();
    std::fs::write(tmp.path().join("already-dirty.txt"), "dirty").unwrap();

    assert_eq!(app.submit_text("what changed?".into()), Action::None);
    let answer = app.history.back().unwrap().body();
    assert!(answer.contains("already-dirty.txt"));
    assert!(answer.contains("运行开始前") || answer.contains("before the run began"));
}

#[test]
fn record_run_paused_at_gate_clears_thinking_and_arms_the_pause() {
    // The RunPausedAtGate terminal decision: the in-flight state clears, the
    // director-pause marker arms, and only then does the staged gate become
    // interactive (the writer session is already ended at this boundary).
    let mut app = fresh_app(Some("offline"));
    app.thinking = true;
    app.agentic_in_flight = true;
    app.director_run_in_flight = true;
    let choice = GateChoice::standard(Gate::DocsConfirm).expect("standard gate choice");
    app.apply_engine(EngineEvent::GateOpened {
        gate: Gate::DocsConfirm,
        choice: Some(choice.clone()),
    });
    assert!(
        app.active_gate.is_none(),
        "gate is not actionable mid-teardown"
    );
    assert!(
        app.gate_choice.is_none(),
        "picker is not actionable mid-teardown"
    );
    assert!(app.pending_director_gate.is_some(), "gate is staged");
    app.record_run_paused_at_gate(Gate::DocsConfirm);
    assert!(!app.thinking && !app.agentic_in_flight && !app.director_run_in_flight);
    assert!(app.director_gate_paused, "the pause marker is armed");
    assert_eq!(app.active_gate, Some(Gate::DocsConfirm));
    assert_eq!(app.gate_choice, Some(choice));
    assert!(app.pending_director_gate.is_none());
    // A cancel resolves the pause (no stale marker into the next run).
    app.cancel_run();
    assert!(!app.director_gate_paused);
}

#[test]
fn model_promoted_gate_is_staged_even_if_engine_channel_wins_first() {
    let mut app = fresh_app(Some("offline"));
    // The routed writer is live, but the independent DirectorStarted channel
    // has not yet been observed by the UI.
    app.thinking = true;
    app.agentic_in_flight = true;
    app.director_run_in_flight = false;

    app.apply_engine(EngineEvent::GateOpened {
        gate: Gate::DocsConfirm,
        choice: GateChoice::standard(Gate::DocsConfirm),
    });

    assert!(app.active_gate.is_none());
    assert!(app.gate_choice.is_none());
    assert!(app.pending_director_gate.is_some());
}

#[test]
fn director_started_reclassifies_only_current_task_corrections() {
    let mut app = fresh_app(Some("offline"));
    app.queued_chat.extend([
        "把当前页面改成暗色".to_string(),
        "为什么要用这个依赖？".to_string(),
        "完成后再加导出".to_string(),
    ]);

    app.promote_queued_inputs_for_director();

    assert_eq!(
        app.queued_steer.into_iter().collect::<Vec<_>>(),
        vec!["把当前页面改成暗色"]
    );
    assert_eq!(
        app.queued_chat.into_iter().collect::<Vec<_>>(),
        vec!["为什么要用这个依赖？", "完成后再加导出"]
    );
}

#[test]
fn director_started_never_steals_an_older_fifo_backlog_turn() {
    let mut app = fresh_app(Some("offline"));
    app.queued_chat
        .extend(["构建登录页".to_string(), "删除旧支付模块".to_string()]);
    assert_eq!(app.take_next_queued_chat().as_deref(), Some("构建登录页"));
    app.begin_route_dispatch();
    app.queued_chat.extend([
        "把当前登录页改成暗色".to_string(),
        "为什么需要 OAuth？".to_string(),
    ]);

    app.promote_queued_inputs_for_director();

    assert_eq!(
        app.queued_steer.into_iter().collect::<Vec<_>>(),
        vec!["把当前登录页改成暗色"]
    );
    assert_eq!(
        app.queued_chat.into_iter().collect::<Vec<_>>(),
        vec!["删除旧支付模块", "为什么需要 OAuth？"]
    );
}

#[test]
fn surface_unsent_steer_reports_leftovers_once_and_clears_the_chip() {
    // A2#4: steering that never reached a step boundary is surfaced honestly
    // (run.queued_unsent) and the queued chip clears; nothing queued → no note.
    let mut app = fresh_app(Some("offline"));
    app.queued_steer.push_back("skip step 2".into());
    let before = app.history.len();
    app.surface_unsent_steer(vec!["make it dark".into()]);
    assert!(app.queued_steer.is_empty(), "the queued chip cleared");
    assert_eq!(app.history.len(), before + 1, "ONE surfacing note");
    let body = app.history.back().unwrap().body().to_string();
    assert!(body.contains("skip step 2") && body.contains("make it dark"));
    // Empty → silent no-op.
    let before = app.history.len();
    app.surface_unsent_steer(Vec::new());
    assert_eq!(app.history.len(), before);
}

#[test]
fn rewind_is_refused_while_a_run_is_writing_the_workspace() {
    // A2#11: `/rewind <id>` during an active run would be a second writer
    // racing the build — refused with the busy note (same guard as /redo).
    // The read-only list form stays allowed.
    let mut app = fresh_app(Some("offline"));
    app.agentic_in_flight = true; // a director/agentic build is live
    for c in "/rewind c1".chars() {
        let _ = app.apply_key(KeyCode::Char(c));
    }
    let action = app.apply_key(KeyCode::Enter);
    assert_eq!(action, Action::None);
    assert!(
        app.history
            .iter()
            .any(|m| m.body() == umadev_i18n::t(app.lang, "rewind.busy")),
        "the busy note was surfaced"
    );
    // Listing (no id) is read-only and never refused.
    for c in "/rewind".chars() {
        let _ = app.apply_key(KeyCode::Char(c));
    }
    let action = app.apply_key(KeyCode::Enter);
    assert_eq!(action, Action::None);
    let busy_notes = app
        .history
        .iter()
        .filter(|m| m.body() == umadev_i18n::t(app.lang, "rewind.busy"))
        .count();
    assert_eq!(busy_notes, 1, "the list form is not refused");
}

#[test]
fn plain_text_after_delivery_routes_to_worker() {
    let mut app = fresh_app(Some("offline"));
    app.run_started = true;
    app.apply_engine(EngineEvent::BlockCompleted {
        final_phase: Phase::Delivery,
        paused_at: None,
    });
    assert!(app.finished);
    for c in "make another tool".chars() {
        let _ = app.apply_key(KeyCode::Char(c));
    }
    let action = app.apply_key(KeyCode::Enter);
    assert_eq!(action, Action::Route("make another tool".to_string()));
    // Routing alone does not reset the delivered run; reset happens after
    // the worker returns a `run` decision.
    assert!(app.finished);
}

#[test]
fn worker_routed_run_after_delivery_resets_phases() {
    let mut app = fresh_app(Some("offline"));
    app.run_started = true;
    app.apply_engine(EngineEvent::BlockCompleted {
        final_phase: Phase::Delivery,
        paused_at: None,
    });
    assert!(app.finished);
    app.prepare_worker_routed_run("make another tool");

    assert!(app.phases.iter().all(|r| r.status == PhaseStatus::Pending));
    assert!(!app.finished);
}

#[test]
fn abort_sentinel_note_surfaces_explicit_terminal_state_not_idle() {
    // THE VISIBILITY BUG: a block that ended with an error used to emit a
    // bare, easily-missed note and leave the bar reading "ready / 0/9". A
    // terminal-abort note (carrying `ABORT_SENTINEL`) must instead flip the
    // run into an explicit aborted state and stop the live counters.
    let mut app = fresh_app(Some("offline"));
    app.run_started = true;
    app.run_started_at = Some(std::time::Instant::now());
    assert!(app.is_pipeline_active(), "run is active before the abort");

    app.apply_engine(EngineEvent::Note(format!(
        "{}本轮已中止:磁盘写入失败 — 释放空间后重试",
        crate::ABORT_SENTINEL
    )));

    assert!(app.aborted, "the sentinel note flips the run into aborted");
    assert!(
        !app.is_pipeline_active(),
        "an aborted run is NOT active — a retry must not be refused as busy"
    );
    assert!(
        app.run_started_at.is_none() && app.phase_started_at.is_none(),
        "live elapsed counters stop on abort so the bar isn't a fake idle"
    );
    // The user sees the cause, and the sentinel marker is stripped.
    let last = app.history.back().unwrap();
    assert!(last.body().contains("本轮已中止"));
    assert!(
        !last.body().contains(crate::ABORT_SENTINEL),
        "the internal sentinel marker must never be shown to the user"
    );
    // The status bar carries the explicit aborted label, not an idle look.
    app.refresh_status();
    assert!(app.status.contains("aborted"));
}

#[test]
fn a_new_pipeline_start_clears_a_prior_aborted_state() {
    // Retrying after an abort: `PipelineStarted` must clear `aborted` so the
    // fresh run reads as live, not stuck in the previous terminal state.
    let mut app = fresh_app(Some("offline"));
    app.aborted = true;
    app.apply_engine(EngineEvent::PipelineStarted {
        slug: "demo".into(),
        requirement: "retry it".into(),
    });
    assert!(!app.aborted, "a fresh block clears the prior aborted state");
    assert!(app.is_pipeline_active(), "the retried run is active again");
}

#[test]
fn ordinary_progress_note_does_not_abort() {
    // A normal progress note (no sentinel) must keep the run active — only
    // the explicit terminal-abort marker flips it.
    let mut app = fresh_app(Some("offline"));
    app.run_started = true;
    app.apply_engine(EngineEvent::Note("[plan] 动态规划:greenfield".into()));
    assert!(!app.aborted, "a plain progress note never aborts");
    assert!(app.is_pipeline_active());
}

#[test]
fn degraded_block_completion_reads_degraded_and_stops_timer() {
    // SCENARIO A: the base failed and phases DEGRADED to placeholder templates,
    // then the block returned `Ok` with a non-`Delivery` `final_phase` (no gate)
    // — no `ABORT_SENTINEL`. The old handler had no `else`, so the timer kept
    // ticking and the bar stayed `[run] 运行中`. It must now read a terminal
    // `[degraded]` with the elapsed timer STOPPED.
    let mut app = fresh_app(Some("offline"));
    app.run_started = true;
    app.run_started_at = Some(std::time::Instant::now());
    app.phase_started_at = Some(std::time::Instant::now());
    assert!(app.is_pipeline_active(), "live before the degraded exit");

    // The runner's block-end summary carries the stable `[WARN][降级]` banner
    // (the exact one the user saw), latched as it streams past.
    app.apply_engine(EngineEvent::Note(
        "[WARN][降级] 本次有 2 个阶段因底座离线只产出了占位模板(非真实交付) \
         …… 修好底座后用 /redo 重跑以拿到真实产物。"
            .into(),
    ));
    assert!(app.run_degraded_seen, "the degrade banner is latched");

    app.apply_engine(EngineEvent::BlockCompleted {
        final_phase: Phase::Backend,
        paused_at: None,
    });

    assert_eq!(app.run_state(), RunState::Degraded);
    assert!(app.degraded, "flagged degraded");
    assert!(
        !app.finished,
        "a degrade is NOT a clean delivery — never print `[ok] delivered`"
    );
    assert!(
        app.run_started_at.is_none() && app.phase_started_at.is_none(),
        "the elapsed timer STOPS (not still ticking '运行中')"
    );
    assert!(
        !app.is_pipeline_active(),
        "a degraded run is terminal, not active"
    );

    app.refresh_status();
    assert!(
        app.status.contains("[degraded]"),
        "top bar reads degraded: {}",
        app.status
    );
    assert!(
        !app.status.contains("[time]"),
        "no live timer segment: {}",
        app.status
    );
    assert!(
        !app.status.contains("[ok] delivered"),
        "not a false delivery: {}",
        app.status
    );
}

#[test]
fn blocked_quality_gate_completion_reads_degraded() {
    // A hard-blocked quality gate returns `Ok { final_phase: Quality, paused_at:
    // None }`, tagged with the stable `UD-EVID-003` evidence code — a terminal
    // degrade/failure, not a clean short-circuit.
    let mut app = fresh_app(Some("offline"));
    app.run_started = true;
    app.run_started_at = Some(std::time::Instant::now());
    app.apply_engine(EngineEvent::Note(
        "[warn] 质量门未通过(得分 41/100)— 阻断 delivery(UD-EVID-003)。".into(),
    ));
    assert!(app.run_degraded_seen);
    app.apply_engine(EngineEvent::BlockCompleted {
        final_phase: Phase::Quality,
        paused_at: None,
    });
    assert_eq!(app.run_state(), RunState::Degraded);
    assert!(app.degraded && !app.finished);
    assert!(app.run_started_at.is_none(), "timer stopped");
}

#[test]
fn no_real_code_failure_completion_reads_degraded() {
    // The "no real code produced" hard failure exits at `Backend/None` with its
    // stable Chinese marker — degraded, not a clean stop.
    let mut app = fresh_app(Some("offline"));
    app.run_started = true;
    app.run_started_at = Some(std::time::Instant::now());
    app.apply_engine(EngineEvent::Note(
        "[fail] 未产出任何真实代码文件 —— 流水线停止,未交付。".into(),
    ));
    assert!(app.run_degraded_seen);
    app.apply_engine(EngineEvent::BlockCompleted {
        final_phase: Phase::Backend,
        paused_at: None,
    });
    assert_eq!(app.run_state(), RunState::Degraded);
    assert!(app.degraded && !app.finished);
}

#[test]
fn clean_short_circuit_completion_is_finished_not_degraded() {
    // A docs-only task legitimately ends before Delivery (settles at its docs
    // gate) with NO degrade/fail signal — it must settle as a CLEAN finish, never
    // be mislabeled `[degraded] /redo`, and stop the timer (not hang on `运行中`).
    let mut app = fresh_app(Some("offline"));
    app.run_started = true;
    app.run_started_at = Some(std::time::Instant::now());
    app.apply_engine(EngineEvent::Note(
        "[plan] 任务类型 Docs — 仅产出文档/调研,已在文档确认门完成,无需进入实现阶段。".into(),
    ));
    assert!(
        !app.run_degraded_seen,
        "a clean note is NOT a degrade signal"
    );
    app.apply_engine(EngineEvent::BlockCompleted {
        final_phase: Phase::DocsConfirm,
        paused_at: None,
    });
    assert!(!app.degraded, "a clean short-circuit is not degraded");
    assert!(app.finished, "it settled cleanly");
    assert_eq!(app.run_state(), RunState::Delivered);
    assert!(
        app.run_started_at.is_none(),
        "the timer stops on a clean short-circuit too"
    );
    assert!(!app.is_pipeline_active());
}

#[test]
fn director_redrive_after_abort_reads_running_not_aborted() {
    // SCENARIO B: a base-failure recovery / model-switch re-drives on the
    // director path, which sets `thinking` WITHOUT a `PipelineStarted` /
    // `PhaseStarted`. The stale `aborted=true` from the prior block must NOT read
    // as `[aborted]` while the run is demonstrably working.
    let mut app = fresh_app(Some("offline"));
    app.run_started = true;
    app.run_started_at = Some(std::time::Instant::now());
    // A genuine hard abort of the prior block.
    app.apply_engine(EngineEvent::Note(format!(
        "{}本轮已中止:底座连接失败",
        crate::ABORT_SENTINEL
    )));
    assert_eq!(app.run_state(), RunState::Aborted);
    app.refresh_status();
    assert!(app.status.contains("[aborted]"), "aborted before re-drive");

    // The re-drive begins: `thinking` flips on (no PipelineStarted on this path).
    app.thinking = true;
    assert_eq!(
        app.run_state(),
        RunState::Running,
        "a live thinking re-drive reads Running, never the stale Aborted"
    );
    app.refresh_status();
    assert!(
        !app.status.contains("[aborted]"),
        "the stale [aborted] is GONE from the top bar while working: {}",
        app.status
    );

    // A posted plan on the director path ALSO clears the raw stale flag (honest).
    app.apply_engine(EngineEvent::PlanPosted {
        steps: vec!["s1 · scaffold".into()],
        statuses: vec!["running".into()],
        done: 0,
        total: 1,
    });
    assert!(!app.aborted, "a live plan clears the stale aborted flag");
}

#[test]
fn normal_delivery_completion_reads_delivered_unchanged() {
    let mut app = fresh_app(Some("offline"));
    app.run_started = true;
    app.run_started_at = Some(std::time::Instant::now());
    app.apply_engine(EngineEvent::BlockCompleted {
        final_phase: Phase::Delivery,
        paused_at: None,
    });
    assert_eq!(app.run_state(), RunState::Delivered);
    assert!(app.finished && !app.degraded);
    app.refresh_status();
    assert!(app.status.contains("[ok] delivered"), "{}", app.status);
}

#[test]
fn gate_pause_reads_paused_at_gate() {
    let mut app = fresh_app(Some("offline"));
    app.run_started = true;
    app.run_started_at = Some(std::time::Instant::now());
    app.active_gate = Some(umadev_agent::gates::Gate::DocsConfirm);
    app.run_started_at = None; // a real gate pause nulls the live counter
    assert_eq!(app.run_state(), RunState::PausedAtGate);
    app.refresh_status();
    assert!(
        !app.status.contains("[time]"),
        "no live timer at a gate: {}",
        app.status
    );
}

#[test]
fn budget_pause_presents_as_paused_not_aborted() {
    // The RunPausedAtBudget terminal decision: the run reads as a PAUSE (timer
    // stopped, `[paused]` label — NEVER `[aborted]`/`[degraded]`), the in-flight
    // state clears, the budget-pause marker arms, and the `/continue` resume hint
    // (carrying done/total) lands in the transcript. Mirrors the gate-pause path.
    let mut app = fresh_app(Some("offline"));
    app.run_started = true;
    app.run_started_at = Some(std::time::Instant::now());
    app.thinking = true;
    app.agentic_in_flight = true;
    app.director_run_in_flight = true;

    app.record_run_paused_at_budget(2, 6);

    // Reads paused, never a stale abort/degrade; the live counters stopped.
    assert_eq!(app.run_state(), RunState::PausedAtBudget);
    assert!(app.budget_paused, "the budget-pause marker is armed");
    assert!(!app.aborted && !app.degraded && !app.finished);
    assert!(!app.thinking && !app.agentic_in_flight && !app.director_run_in_flight);
    assert!(app.run_started_at.is_none(), "the live timer is stopped");
    // The run is not "active" (a fresh /run or /continue must not be refused).
    assert!(!app.is_pipeline_active());

    app.refresh_status();
    assert!(
        app.status.contains("[paused]") && !app.status.contains("[aborted]"),
        "the status bar reads [paused], never [aborted]: {}",
        app.status
    );
    assert!(
        !app.status.contains("[time]"),
        "no live timer at a budget pause: {}",
        app.status
    );
    // The resume hint (with done/total) landed in the transcript.
    assert!(
        app.history
            .back()
            .is_some_and(|m| m.body().contains("/continue") && m.body().contains("2/6")),
        "the /continue resume hint carries done/total"
    );

    // A fresh live turn clears the pause so its label can't paint over live work.
    app.thinking = true;
    app.clear_stale_terminal_on_live_turn();
    assert!(!app.budget_paused);
    assert_eq!(app.run_state(), RunState::Running);
}

#[test]
fn budget_pause_frees_active_run_so_continue_resumes_and_esc_codex_are_not_busy() {
    // C1b: `record_run_paused_at_budget` must clear `run_started` AND settle the run's
    // registry task, so a PARKED run stops reading as "active". Otherwise the leftover
    // `Running` task keeps `has_active_run()`/`has_interruptible_work()` TRUE, which
    // makes `/continue` answer "a run is still in flight" (never reaching the resume
    // branch), ESC arm a phantom interrupt, and `/codex` refuse as busy — all on a run
    // that has actually stopped.
    let mut app = fresh_app(Some("offline"));

    // A resumable director-loop run on disk (plan with a Pending step + the
    // workflow-state carrying the requirement) — exactly what a budget pause leaves.
    let plan = umadev_agent::Plan {
        steps: vec![umadev_agent::PlanStep {
            files: umadev_agent::StepFiles::default(),
            id: "b".into(),
            title: "build the remaining feature".into(),
            seat: umadev_agent::Seat::FrontendEngineer,
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
    let mut state = umadev_agent::WorkflowState::new(umadev_spec::Phase::Frontend);
    state.slug = "demo".into();
    state.requirement = "做一个登录页".into();
    state.backend = "offline".into();
    umadev_agent::write_workflow_state(&app.project_root, &state).unwrap();

    // A live run: registered task + the in-flight flags a mid-build carries.
    app.register_run_task("做一个登录页");
    app.run_started = true;
    app.run_started_at = Some(std::time::Instant::now());
    app.thinking = true;
    app.agentic_in_flight = true;
    app.director_run_in_flight = true;
    assert!(app.has_active_run(), "the run reads as active while live");

    // The budget pause parks it.
    app.record_run_paused_at_budget(1, 3);

    // The parked run is NO LONGER active/interruptible: the flag cleared and the
    // registry task settled off `Running`.
    assert!(!app.run_started, "the pipeline-live flag is cleared");
    assert!(
        app.active_task().is_none(),
        "the run's registry task is settled off Running"
    );
    assert!(
        !app.has_active_run() && !app.has_interruptible_work(),
        "a parked budget pause is neither active nor interruptible"
    );

    // ESC no longer arms a phantom interrupt (the interrupt branch keys off
    // `has_interruptible_work`, which is now false).
    let esc = app.apply_key(KeyCode::Esc);
    assert_ne!(esc, Action::Cancel, "ESC must not cancel a parked run");
    assert!(
        !app.interrupt_armed(),
        "ESC after a budget pause must not arm an interrupt"
    );

    // `/continue` reaches the RESUME branch (Action::ResumeRun), NOT "a run is still
    // in flight".
    let before = app.history.len();
    let action = app
        .try_slash_command("/continue")
        .expect("/continue is a slash command");
    assert_eq!(
        action,
        Action::ResumeRun("做一个登录页".to_string()),
        "a parked budget pause resumes the persisted run"
    );
    assert!(
        app.history
            .iter()
            .skip(before)
            .any(|m| m.body().contains("续跑")),
        "the resume path is taken (the resuming note is shown, not the busy hint)"
    );

    // `/codex` is no longer refused as busy — the backend switches.
    let switched = app.slash_backend(Some("codex"));
    assert_eq!(
        switched,
        Action::BackendChanged,
        "a base switch is allowed after a budget pause (not 'busy')"
    );
    assert_eq!(app.backend.as_deref(), Some("codex"));
}

#[test]
fn note_signals_degraded_matches_stable_markers_only() {
    assert!(note_signals_degraded("[WARN][降级] 占位模板"));
    assert!(note_signals_degraded("… 阻断 delivery(UD-EVID-003)。"));
    assert!(note_signals_degraded(
        "[fail] 未产出任何真实代码文件 —— 停止"
    ));
    // A clean progress / completion note is NOT a degrade signal.
    assert!(!note_signals_degraded("[plan] 仅产出文档/调研,已完成"));
    assert!(!note_signals_degraded("[redo] backend 阶段已重跑完成"));
}

#[test]
fn host_output_lands_in_history_as_host_role() {
    let mut app = fresh_app(Some("offline"));
    app.apply_engine(EngineEvent::HostOutput {
        phase: Phase::Research,
        line: "## Similar products".into(),
    });
    let last = app.history.back().unwrap();
    assert_eq!(last.role, ChatRole::Host);
    assert!(last.body().contains("Similar products"));
}

#[test]
fn history_is_bounded() {
    let mut app = fresh_app(Some("offline"));
    for i in 0..(HISTORY_CAP + 50) {
        app.apply_engine(EngineEvent::Note(format!("line {i}")));
    }
    assert!(app.history.len() <= HISTORY_CAP);
}

#[test]
fn f1_toggles_help_in_both_modes() {
    let mut a = fresh_app(None);
    assert!(!a.show_help);
    let _ = a.apply_key(KeyCode::F(1));
    assert!(a.show_help);
    let mut b = fresh_app(Some("offline"));
    let _ = b.apply_key(KeyCode::F(1));
    assert!(b.show_help);
}

#[test]
fn help_scroll_clamps_and_accepts_arrow_vim_keys() {
    let mut a = fresh_app(Some("offline"));
    let _ = a.apply_key(KeyCode::F(1));
    a.help_max_scroll.set(3);

    for _ in 0..10 {
        let _ = a.apply_key(KeyCode::Down);
    }
    assert_eq!(a.help_scroll, 3, "Down must clamp at the rendered bottom");

    let _ = a.apply_key(KeyCode::Up);
    assert_eq!(
        a.help_scroll, 2,
        "Up must move immediately after bottom clamp"
    );

    let _ = a.apply_key(KeyCode::Char('J'));
    assert_eq!(a.help_scroll, 3, "uppercase J mirrors j/Down");

    let _ = a.apply_key(KeyCode::Char('K'));
    assert_eq!(a.help_scroll, 2, "uppercase K mirrors k/Up");

    let _ = a.apply_key(KeyCode::Char('G'));
    assert_eq!(a.help_scroll, 3, "G jumps to the bottom");

    let _ = a.apply_key(KeyCode::Home);
    assert_eq!(a.help_scroll, 0, "Home jumps to the top");
}

#[test]
fn slash_spec_opens_overlay() {
    let mut a = fresh_app(Some("offline"));
    for c in "/spec".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let _ = a.apply_key(KeyCode::Enter);
    let ov = a.overlay.as_ref().expect("overlay should open");
    assert!(ov.title.contains("UMADEV_HOST_SPEC_V1"));
    assert!(ov.lines.iter().any(|l| l.contains("UMADEV_HOST_SPEC_V1")));
}

#[test]
fn slash_doctor_opens_overlay_with_binary_line() {
    let mut a = fresh_app(Some("offline"));
    for c in "/doctor".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let _ = a.apply_key(KeyCode::Enter);
    let ov = a.overlay.as_ref().expect("doctor overlay");
    // Locale-independent: the binary line carries the crate version, and the
    // worker-availability section header is always present. (The labels
    // themselves are localized, so we assert on the language-neutral parts.)
    assert!(
        ov.lines
            .iter()
            .any(|l| l.contains(env!("CARGO_PKG_VERSION"))),
        "doctor overlay should show the binary version line"
    );
    let avail = umadev_i18n::t(a.lang, "doctor.worker_availability");
    assert!(
        ov.lines.iter().any(|l| l.contains(avail.trim())),
        "doctor overlay should show the worker-availability section"
    );
}

#[test]
fn slash_verify_opens_overlay_with_sections() {
    let mut a = fresh_app(Some("offline"));
    for c in "/verify".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let _ = a.apply_key(KeyCode::Enter);
    let ov = a.overlay.as_ref().unwrap();
    let joined = ov.lines.join("\n");
    assert!(joined.contains("## Spec manifest"));
    assert!(joined.contains("## Workflow state"));
    assert!(joined.contains("## Artifacts"));
}

#[test]
fn slash_diff_missing_artifact_shows_available_list() {
    let mut a = fresh_app(Some("offline"));
    for c in "/diff".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let _ = a.apply_key(KeyCode::Enter);
    let ov = a.overlay.as_ref().unwrap();
    // Empty workspace → fallback message kicks in.
    assert!(ov
        .lines
        .iter()
        .any(|l| l.contains("找不到") || l.contains("还不存在")));
}

#[test]
fn slash_init_writes_umadev_yaml() {
    let tmp = tempfile::TempDir::new().unwrap();
    let cfg = UserConfig {
        backend: Some("offline".into()),
        lang: Some("zh-CN".into()),
        ..Default::default()
    };
    let mut app = App::new(
        "demo",
        cfg,
        tmp.path().join("config.toml"),
        tmp.path().to_path_buf(),
    );
    for c in "/init".chars() {
        let _ = app.apply_key(KeyCode::Char(c));
    }
    let action = app.apply_key(KeyCode::Enter);
    assert_eq!(action, Action::WorkspaceInitialized);
    for path in [
        "umadev.yaml",
        ".umadevrc",
        "CLAUDE.md",
        "AGENTS.md",
        ".umadev/rules.toml",
    ] {
        assert!(tmp.path().join(path).is_file(), "missing {path}");
    }
    assert!(app.history.iter().any(|m| m.role == ChatRole::UmaDev
        && m.body().contains("空目录")
        && m.body().contains("umadev.yaml")));
}

#[test]
fn slash_init_detects_existing_repo_without_replacing_agents() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        "[package]\nname='demo'\nversion='0.1.0'\n",
    )
    .unwrap();
    std::fs::write(tmp.path().join("src/lib.rs"), "pub fn demo() {}\n").unwrap();
    std::fs::write(
        tmp.path().join("AGENTS.md"),
        "# Team rules\n\nNever replace me.\n",
    )
    .unwrap();
    let cfg = UserConfig {
        backend: Some("offline".into()),
        lang: Some("zh-CN".into()),
        ..Default::default()
    };
    let mut app = App::new(
        "demo",
        cfg,
        tmp.path().join("config.toml"),
        tmp.path().to_path_buf(),
    );

    for c in "/init".chars() {
        let _ = app.apply_key(KeyCode::Char(c));
    }

    assert_eq!(app.apply_key(KeyCode::Enter), Action::WorkspaceInitialized);
    let agents = std::fs::read_to_string(tmp.path().join("AGENTS.md")).unwrap();
    assert!(agents.contains("Never replace me."));
    assert_eq!(agents.matches("<!-- umadev:project:begin -->").count(), 1);
    assert!(app.history.iter().any(|message| {
        message.role == ChatRole::UmaDev
            && message.body().contains("已有仓库")
            && message.body().contains("Rust")
    }));
}

#[test]
fn esc_during_active_pipeline_interrupts_the_run() {
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::PipelineStarted {
        slug: "demo".into(),
        requirement: "build".into(),
    });
    assert!(a.is_pipeline_active());
    // Esc INTERRUPTS the running pipeline (like Claude Code), but a DELIBERATE
    // double-press — the first arms, the second cancels — so a stray keypress
    // can't nuke a long build. Neither press quits the app.
    assert_eq!(a.apply_key(KeyCode::Esc), Action::None);
    assert!(a.interrupt_armed(), "first Esc arms the interrupt");
    assert!(!a.should_quit);
    assert_eq!(a.apply_key(KeyCode::Esc), Action::Cancel);
    assert!(!a.should_quit);
}

// ---- Windows-console render garble: force a full repaint when an operation
// shifts the layout, so ratatui's incremental diff can't leave stale
// overlapping rows on conhost / PowerShell. ------------------------------

#[test]
fn multiline_history_recall_forces_full_repaint() {
    let mut a = fresh_app(Some("offline"));
    // The renderer publishes the available input text width; pin it so the
    // height comparison is deterministic.
    a.input_text_cols.set(40);
    // A multi-line prior submission. Recalling it into the empty one-row box
    // GROWS the prompt, shifting the transcript above it — exactly the case
    // that leaves overlapping garble on the Windows console.
    a.remember_submission("line one\nline two\nline three");
    assert!(
        !a.terminal_contaminated.get(),
        "no repaint pending before the recall"
    );
    a.input_history_back();
    assert_eq!(a.input, "line one\nline two\nline three");
    assert!(
        a.take_terminal_contaminated(),
        "a multi-line recall that grows the input box must force a full repaint"
    );
    // The request drains in ONE shot — exactly one full repaint, then the
    // cheap incremental diff resumes.
    assert!(
        !a.take_terminal_contaminated(),
        "the repaint request drains once"
    );
}

#[test]
fn same_height_history_recall_does_not_force_repaint() {
    let mut a = fresh_app(Some("offline"));
    a.input_text_cols.set(40);
    // A short single-line entry: recalling it into the empty box keeps the
    // box one row tall (nothing above shifts), so no full repaint is needed.
    a.remember_submission("hi");
    a.input_history_back();
    assert_eq!(a.input, "hi");
    assert!(
        !a.take_terminal_contaminated(),
        "a same-height recall must NOT force a needless full repaint"
    );
}

#[test]
fn history_forward_shrink_forces_full_repaint() {
    let mut a = fresh_app(Some("offline"));
    a.input_text_cols.set(40);
    a.remember_submission("a\nb\nc\nd"); // four rows tall
    a.remember_submission("short"); // one row
    a.input_history_back(); // -> "short" (same height as empty draft)
    let _ = a.take_terminal_contaminated(); // clear whatever that step set
    a.input_history_back(); // -> the tall entry (grows)
    assert!(
        a.take_terminal_contaminated(),
        "growing the box on the way back forces a repaint"
    );
    // Stepping FORWARD shrinks the tall entry back to "short": the box loses
    // rows, which must also force a full repaint (the shrink-leaves-stale-rows
    // case — the back-buffer reset is what wipes them).
    a.input_history_forward();
    assert_eq!(a.input, "short");
    assert!(
        a.take_terminal_contaminated(),
        "shrinking the input box on forward-recall must force a full repaint"
    );
}

// ---- long-run transcript garble: a transcript reflow / scroll must force a
// full repaint too (not just the input box), so a diff-only console can't
// leave stale/overlapping rows over a long streaming run. ----------------

#[test]
fn scroll_jump_does_not_force_full_repaint() {
    let mut a = fresh_app(Some("offline"));
    // The renderer publishes the scroll bound; pin one so a scroll actually
    // moves the offset.
    a.transcript_max_scroll.set(100);
    assert!(
        !a.take_transcript_repaint(),
        "no transcript repaint pending before any scroll"
    );
    // A real scroll UP (0 → 10) replaces the visible window, but it must not
    // clear the whole terminal on every wheel/PageUp step; that visibly
    // flickers on Windows. Structural reflow still repaints via the renderer.
    a.transcript_scroll_up(10);
    assert_eq!(a.transcript_scroll(), 10);
    assert!(
        !a.take_transcript_repaint(),
        "scrolling history must not force a full clear/repaint"
    );
    // Scrolling back to the bottom also moves the window, but stays
    // incremental for the same reason.
    a.transcript_scroll_to_bottom();
    assert_eq!(a.transcript_scroll(), 0);
    assert!(
        !a.take_transcript_repaint(),
        "jumping back to bottom must not force a full clear/repaint"
    );
}

#[test]
fn boundary_scroll_that_moves_nothing_does_not_repaint() {
    let mut a = fresh_app(Some("offline"));
    a.transcript_max_scroll.set(0); // everything fits: nothing to scroll
                                    // Already pinned to the bottom (offset 0): a scroll-down / to-bottom is a
                                    // no-op and must NOT force a needless repaint (no thrash on a static view).
    a.transcript_scroll_down(5);
    a.transcript_scroll_to_bottom();
    assert!(
        !a.take_transcript_repaint(),
        "a scroll that changed nothing must not force a repaint"
    );
    // A scroll UP clamped to a zero bound also moves nothing → no repaint.
    a.transcript_scroll_up(5);
    assert_eq!(a.transcript_scroll(), 0);
    assert!(
        !a.take_transcript_repaint(),
        "a scroll clamped to a zero bound moves nothing → no repaint"
    );
}

#[test]
fn slash_clear_forces_full_repaint() {
    let mut a = fresh_app(Some("offline"));
    // Put content in the transcript so `/clear` actually drops rows.
    a.push(ChatRole::You, "hello");
    a.push(ChatRole::UmaDev, "hi there");
    // Dispatch `/clear` exactly as the user types it.
    for c in "/clear".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let action = a.apply_key(KeyCode::Enter);
    assert_eq!(action, Action::None);
    // The prior conversation is dropped (only the "history cleared" system
    // confirmation remains).
    assert!(
        !a.history.iter().any(|m| m.body().contains("hi there")),
        "/clear drops the prior transcript"
    );
    assert!(
        a.take_terminal_contaminated(),
        "/clear drops transcript rows without changing the input height, so it \
             must force a full repaint itself (the generic height guard can't catch it)"
    );
}

// ---- P3: out-of-band terminal writes contaminate the render, forcing one
// healing clear+repaint on the next frame (the primary heal on terminals
// without confirmed synchronized output). --------------------------------

#[test]
fn taking_an_armed_bell_contaminates_the_terminal() {
    let mut a = fresh_app(Some("offline"));
    a.bell_pending = true;
    assert!(a.take_bell(), "the armed bell drains");
    assert!(
        a.take_terminal_contaminated(),
        "the out-of-band BEL write must contaminate the terminal (one heal frame)"
    );
    // Both flags are one-shot: no bell, no second heal.
    assert!(!a.take_bell(), "the bell drains once");
    assert!(
        !a.take_terminal_contaminated(),
        "the contamination flag drains once — exactly one healing repaint"
    );
}

#[test]
fn an_unarmed_bell_does_not_contaminate() {
    let mut a = fresh_app(Some("offline"));
    assert!(!a.take_bell(), "nothing armed → no bell");
    assert!(
        !a.take_terminal_contaminated(),
        "no out-of-band write happened → the steady state stays clean"
    );
}

#[test]
fn input_block_rows_clamps_so_oversized_inputs_report_equal_height() {
    // Two inputs that both exceed the visible cap report the SAME box height,
    // so swapping one for the other never forces a needless repaint.
    let tall = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj";
    let taller = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm";
    assert_eq!(
        crate::ui::input_block_rows(tall, 40),
        crate::ui::input_block_rows(taller, 40),
        "the visible-row clamp makes both oversized inputs report one height"
    );
    // A one-line vs a three-line input DO differ in height.
    assert_ne!(
        crate::ui::input_block_rows("one", 40),
        crate::ui::input_block_rows("one\ntwo\nthree", 40),
    );
}

#[test]
fn interrupt_seals_a_half_streamed_reply_as_incomplete() {
    let mut a = fresh_app(Some("offline"));
    // Simulate a Host reply mid-stream.
    a.push(ChatRole::Host, "the answer so far".to_string());
    a.stream_text_active = true;
    a.cancel_run();
    let marker = umadev_i18n::t(a.lang, "chat.interrupted");
    let last = a
        .history
        .iter()
        .rev()
        .find(|m| m.role == ChatRole::Host)
        .unwrap();
    assert!(
        last.body().contains(marker.trim()),
        "an interrupted reply must be marked incomplete: {:?}",
        last.body()
    );
    assert!(!a.stream_text_active, "the stream flag is cleared on seal");
}

#[test]
fn seal_is_a_noop_when_nothing_was_streaming() {
    let mut a = fresh_app(Some("offline"));
    a.push(ChatRole::Host, "a finished reply".to_string());
    a.stream_text_active = false;
    a.seal_interrupted_stream();
    let last = a
        .history
        .iter()
        .rev()
        .find(|m| m.role == ChatRole::Host)
        .unwrap();
    assert_eq!(
        last.body(),
        "a finished reply",
        "no marker when nothing streamed"
    );
}

#[test]
fn typing_clears_pending_quit_confirm() {
    let mut a = fresh_app(Some("offline"));
    // Idle Esc arms the quit confirmation (no pipeline running).
    let _ = a.apply_key(KeyCode::Esc);
    assert!(a.pending_quit_confirm);
    // Any typing — even one char — clears the pending confirmation.
    let _ = a.apply_key(KeyCode::Char('x'));
    assert!(!a.pending_quit_confirm);
}

#[test]
fn typing_mid_phase_queues_and_fires_at_the_next_gate() {
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::PipelineStarted {
        slug: "demo".into(),
        requirement: "build".into(),
    });
    assert!(a.is_pipeline_active() && a.active_gate.is_none());
    // Typing while a phase runs (no gate open) QUEUES the message instead of
    // dropping it.
    for c in "make it dark mode".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let action = a.apply_key(KeyCode::Enter);
    assert_eq!(action, Action::None);
    assert_eq!(
        a.queued_steer
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["make it dark mode"]
    );
    // At the next gate (the gap), the queued message is promoted to a
    // pending steer — fired as a revision — instead of auto-approving.
    a.apply_engine(EngineEvent::GateOpened {
        gate: Gate::DocsConfirm,
        choice: None,
    });
    assert!(a.queued_steer.is_empty());
    assert_eq!(a.pending_steer.as_deref(), Some("make it dark mode"));
    assert!(a.pending_auto_continue.is_none());
}

#[test]
fn aborted_block_drains_and_surfaces_a_parked_queued_steer() {
    // M2 — a steer parked mid-phase that then hits an ABORT (the run errored,
    // so no further gate/completion fires) must NOT stay stuck forever: the
    // queue drains (the "queued N" chip clears) and the dropped text is
    // surfaced so the user knows to resend.
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::PipelineStarted {
        slug: "demo".into(),
        requirement: "build".into(),
    });
    for c in "make it dark mode".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let _ = a.apply_key(KeyCode::Enter);
    assert_eq!(
        a.queued_steer.len(),
        1,
        "the steer parked while the phase ran"
    );
    let before = a.history.len();
    // The producing block errors out (the ABORT_SENTINEL path).
    a.mark_block_aborted("the base errored".into());
    assert!(
        a.queued_steer.is_empty(),
        "an abort must drain the parked steer so the chip clears"
    );
    let surfaced = a
        .history
        .iter()
        .skip(before)
        .any(|m| m.body().contains("make it dark mode"));
    assert!(
        surfaced,
        "the dropped steer must be surfaced for the user to resend"
    );
}

#[test]
fn cancel_run_clears_a_parked_queued_steer() {
    // M2 — a user cancel ends the run, so a parked steer can never reach a
    // gate; it must be cleared so the "queued N" chip doesn't stay falsely lit.
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::PipelineStarted {
        slug: "demo".into(),
        requirement: "build".into(),
    });
    a.queued_steer.push_back("steer me".into());
    a.cancel_run();
    assert!(
        a.queued_steer.is_empty(),
        "a user cancel must drop the parked steer"
    );
}

#[test]
fn cancel_run_preserves_deferred_model_turns() {
    let mut a = fresh_app(Some("offline"));
    a.run_started = true;
    a.chat_session_id = Some("unfinished-native-session".into());
    a.host_chat_session_active = true;
    a.record_user_turn("构建登录模块");
    a.queued_chat.push_back("完成后再做登录".into());

    a.cancel_run();

    assert!(
        a.chat_session_id.is_none(),
        "cancelled native context is not resumed"
    );
    assert!(!a.host_chat_session_active);
    assert!(a.conversation.iter().any(|message| {
        message.role == "assistant"
            && message
                .content
                .contains("preceding in-flight request was cancelled")
    }));

    assert_eq!(
        a.queued_chat.front().map(String::as_str),
        Some("完成后再做登录"),
        "a future turn belongs to the user, not the cancelled writer"
    );
    let expected = umadev_i18n::tf(a.lang, "chat.queued_preserved", &["1"]);
    assert!(a.history.iter().any(|message| message.body() == expected));

    // `cancel_run` is terminal now. The same FIFO drain used by the event
    // loop can dispatch the preserved turn, and only at that point does it
    // enter model conversation memory.
    let conversation_before = a.conversation.len();
    assert_eq!(a.take_next_queued_chat().as_deref(), Some("完成后再做登录"));
    assert!(a.queued_chat.is_empty());
    assert_eq!(a.conversation.len(), conversation_before + 1);
    assert_eq!(
        a.conversation
            .last()
            .map(|message| message.content.as_str()),
        Some("完成后再做登录")
    );
}

// ── Structured-choice gate picker ──────────────────────────────────────

#[test]
fn structured_choice_gate_arms_picker_and_approve_drives_continue() {
    let mut a = fresh_app(Some("offline"));
    // A confirm gate opened via the standard constructor carries the picker.
    a.apply_engine(EngineEvent::gate_opened(Gate::DocsConfirm));
    assert_eq!(a.active_gate, Some(Gate::DocsConfirm));
    let choice = a.gate_choice.as_ref().expect("picker armed");
    assert_eq!(choice.options.len(), 3, "approve / revise / add-more");
    assert_eq!(a.gate_choice_sel, 0);
    // Arrow keys move the highlight (wrapping both ways).
    let _ = a.apply_key(KeyCode::Down);
    let _ = a.apply_key(KeyCode::Down);
    assert_eq!(a.gate_choice_sel, 2);
    let _ = a.apply_key(KeyCode::Down); // wraps to 0
    assert_eq!(a.gate_choice_sel, 0);
    let _ = a.apply_key(KeyCode::Up); // wraps to 2
    assert_eq!(a.gate_choice_sel, 2);
    let _ = a.apply_key(KeyCode::Down); // back to the Approve row
    assert_eq!(a.gate_choice_sel, 0);
    // Enter on the highlighted Approve option drives the EXISTING confirm path.
    let action = a.apply_key(KeyCode::Enter);
    assert_eq!(action, Action::Continue(Gate::DocsConfirm));
    assert!(a.gate_choice.is_none() && a.active_gate.is_none());
}

#[test]
fn text_question_mode_suppresses_gate_picker_default_still_shows_it() {
    // Default (picker) mode: the numbered picker is armed (existing behavior,
    // so users who never opt in are unaffected).
    let mut picker = fresh_app(Some("offline"));
    picker.apply_engine(EngineEvent::gate_opened(Gate::DocsConfirm));
    assert!(
        picker.gate_choice.is_some(),
        "picker mode (the default) still arms the numbered picker"
    );

    // Text-question mode: the picker is SUPPRESSED and the gate is framed as
    // prose the user answers in natural language (the free-text reply path is
    // unchanged — only the presentation differs).
    let mut text = fresh_app(Some("offline"));
    text.config.question_form = Some("text".into());
    let lang = text.lang;
    let before = text.history.len();
    text.apply_engine(EngineEvent::gate_opened(Gate::DocsConfirm));
    assert_eq!(text.active_gate, Some(Gate::DocsConfirm));
    assert!(
        text.gate_choice.is_none(),
        "text mode suppresses the numbered picker"
    );
    let hint = umadev_i18n::t(lang, "question.text_hint");
    let framed = text
        .history
        .iter()
        .skip(before)
        .any(|m| m.body().contains(hint));
    assert!(
        framed,
        "text mode frames the gate as prose with the answer-in-words hint"
    );
}

#[test]
fn gate_picker_number_key_selects_and_drives_decision() {
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::gate_opened(Gate::DocsConfirm));
    // `1` picks the first option (Approve) directly → Continue.
    let action = a.apply_key(KeyCode::Char('1'));
    assert_eq!(action, Action::Continue(Gate::DocsConfirm));
    assert!(a.gate_choice.is_none());
}

#[test]
fn gate_picker_revise_option_hands_off_to_free_text_then_revises() {
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::gate_opened(Gate::DocsConfirm));
    // `2` picks "Revise": no immediate Action, the picker is consumed, and the
    // gate STAYS open awaiting the free-text revision (reuses the revise path).
    let action = a.apply_key(KeyCode::Char('2'));
    assert_eq!(action, Action::None);
    assert!(a.gate_choice.is_none(), "picker consumed");
    assert_eq!(
        a.active_gate,
        Some(Gate::DocsConfirm),
        "gate open for the revision"
    );
    // The next typed line drives the existing Action::Revise.
    for c in "make the header sticky".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let action = a.apply_key(KeyCode::Enter);
    assert_eq!(action, Action::Revise("make the header sticky".to_string()));
}

#[test]
fn gate_without_options_falls_back_to_free_form() {
    let mut a = fresh_app(Some("offline"));
    // No structured choice on the event → no picker (fail-open).
    a.apply_engine(EngineEvent::GateOpened {
        gate: Gate::DocsConfirm,
        choice: None,
    });
    assert!(a.gate_choice.is_none(), "no picker → free-form");
    assert_eq!(a.active_gate, Some(Gate::DocsConfirm));
    // The free-text approval (`c`) still works exactly as before.
    let _ = a.apply_key(KeyCode::Char('c'));
    let action = a.apply_key(KeyCode::Enter);
    assert_eq!(action, Action::Continue(Gate::DocsConfirm));
}

#[test]
fn gate_picker_coexists_with_free_text_fallback() {
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::gate_opened(Gate::PreviewConfirm));
    assert!(a.gate_choice.is_some(), "picker present");
    // Typing letters is NOT swallowed by the picker — the box only yields its
    // keys to the picker while empty, so a custom response still types in.
    for c in "use lucide".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    assert_eq!(a.input, "use lucide");
    let action = a.apply_key(KeyCode::Enter);
    assert_eq!(action, Action::Revise("use lucide".to_string()));
}

#[test]
fn gate_picker_out_of_range_digit_is_fail_open() {
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::gate_opened(Gate::DocsConfirm));
    // `9` is past the 3 options → not a selection; it falls through to normal
    // insertion (fail-open: never panics, never picks a phantom option).
    let action = a.apply_key(KeyCode::Char('9'));
    assert_eq!(action, Action::None);
    assert_eq!(a.input, "9");
    assert!(a.gate_choice.is_some(), "picker untouched");
}

#[test]
fn gate_picker_pick_is_noop_without_active_gate() {
    // Direct fail-open guard: picking with no active picker/gate is a no-op.
    let mut a = fresh_app(Some("offline"));
    assert_eq!(a.gate_choice_pick(0), Action::None);
}

#[test]
fn multiple_mid_phase_steers_queue_without_loss_and_count_correctly() {
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::PipelineStarted {
        slug: "demo".into(),
        requirement: "build".into(),
    });
    assert!(a.is_pipeline_active() && a.active_gate.is_none());
    // Three separate mid-phase turns. The old `Option<String>` overwrote all
    // but the last; a `VecDeque` keeps every one, in order.
    let turns = ["把标题改成 A", "把按钮换成 B", "把页脚删掉"];
    for turn in turns {
        for c in turn.chars() {
            let _ = a.apply_key(KeyCode::Char(c));
        }
        let action = a.apply_key(KeyCode::Enter);
        assert_eq!(action, Action::None);
    }
    assert_eq!(
        a.queued_steer
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        turns,
        "every steer is retained in FIFO order — none overwritten"
    );
    // The `queued N` chip reflects all three, not a stuck 1.
    assert_eq!(a.queued_count(), 3, "count is the real queue depth");
    // At the next gate, ALL of them fold into one pending revision (in order).
    a.apply_engine(EngineEvent::GateOpened {
        gate: Gate::DocsConfirm,
        choice: None,
    });
    assert!(a.queued_steer.is_empty(), "queue drained at the gate");
    assert_eq!(
        a.pending_steer.as_deref(),
        Some("把标题改成 A\n把按钮换成 B\n把页脚删掉")
    );
    assert_eq!(a.queued_count(), 0);
}

#[test]
fn esc_during_agentic_turn_interrupts_not_quits() {
    let mut a = fresh_app(Some("offline"));
    // An agentic chat turn is streaming in a base subprocess — note this is
    // NOT a pipeline run (`run_started` stays false), so the only thing that
    // can interrupt it is the `agentic_in_flight` branch.
    a.agentic_in_flight = true;
    assert!(!a.is_pipeline_active());
    // Esc INTERRUPTS the agentic subprocess (parity with Ctrl-C) via a
    // deliberate double-press, and does NOT arm quit-confirm or drop the app.
    assert_eq!(a.apply_key(KeyCode::Esc), Action::None);
    assert!(a.interrupt_armed(), "first Esc arms the interrupt");
    assert_eq!(a.apply_key(KeyCode::Esc), Action::Cancel);
    assert!(!a.should_quit);
    assert!(
        !a.pending_quit_confirm,
        "Esc on an agentic turn interrupts, it does not arm quit-confirm"
    );
}

// ---- resume hint on chat init ----

#[test]
fn resume_hint_appears_when_workflow_state_paused_at_gate() {
    let tmp = tempfile::TempDir::new().unwrap();
    // Seed a workflow-state.json that looks like "paused at docs_confirm".
    let state_dir = tmp.path().join(".umadev");
    std::fs::create_dir_all(&state_dir).unwrap();
    let state_json = r#"{
            "phase": "docs_confirm",
            "active_gate": "docs_confirm",
            "slug": "demo",
            "requirement": "做一个登录系统",
            "last_transition_at": "2026-05-23T10:00:00Z",
            "note": "",
            "spec_version": "UMADEV_HOST_SPEC_V1"
        }"#;
    std::fs::write(state_dir.join("workflow-state.json"), state_json).unwrap();

    let cfg = UserConfig {
        backend: Some("offline".into()),
        ..Default::default()
    };
    let app = App::new(
        "demo",
        cfg,
        tmp.path().join("config.toml"),
        tmp.path().to_path_buf(),
    );

    // Greeting + resume hint both land in history.
    let resume_msg = app
        .history
        .iter()
        .find(|m| m.body().contains("docs_confirm"))
        .expect("resume hint should mention the paused gate");
    assert_eq!(resume_msg.role, ChatRole::System);
    assert!(resume_msg.body().contains("做一个登录系统"));
    assert!(resume_msg.body().contains("/continue"));
}

#[test]
fn resume_hint_marks_completed_runs() {
    let tmp = tempfile::TempDir::new().unwrap();
    let state_dir = tmp.path().join(".umadev");
    std::fs::create_dir_all(&state_dir).unwrap();
    let state_json = r#"{
            "phase": "delivery",
            "active_gate": "",
            "slug": "demo",
            "requirement": "做个 todo",
            "last_transition_at": "2026-05-23T10:00:00Z",
            "note": "Pipeline complete.",
            "spec_version": "UMADEV_HOST_SPEC_V1"
        }"#;
    std::fs::write(state_dir.join("workflow-state.json"), state_json).unwrap();

    let cfg = UserConfig {
        backend: Some("offline".into()),
        lang: Some("zh-CN".into()),
        ..Default::default()
    };
    let app = App::new(
        "demo",
        cfg,
        tmp.path().join("config.toml"),
        tmp.path().to_path_buf(),
    );
    let msg = app
        .history
        .iter()
        .find(|m| m.body().contains("上次跑完了") || m.body().contains("上次会话"))
        .expect("delivery-state should produce a chat hint");
    assert!(msg.body().contains("做个 todo"));
}

#[test]
fn no_resume_hint_for_clean_workspace() {
    let tmp = tempfile::TempDir::new().unwrap();
    let cfg = UserConfig {
        backend: Some("offline".into()),
        ..Default::default()
    };
    let app = App::new(
        "demo",
        cfg,
        tmp.path().join("config.toml"),
        tmp.path().to_path_buf(),
    );
    // Greeting still present (always), but no resume hint.
    assert!(!app
        .history
        .iter()
        .any(|m| m.body().contains("docs_confirm") || m.body().contains("上次")));
}

// ---- /model + /version + /changelog + typo did-you-mean ----

#[test]
fn slash_cancel_returns_cancel_action_only_while_running() {
    let mut a = fresh_app(Some("offline"));
    // Not running → /cancel is a no-op with a hint.
    for c in "/cancel".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    assert!(matches!(a.apply_key(KeyCode::Enter), Action::None));
    assert!(a.history.iter().any(|m| m.body().contains("没有正在运行")));
    // Running → /cancel returns Action::Cancel (event loop aborts the task).
    a.run_started = true;
    a.finished = false;
    for c in "/cancel".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    assert!(matches!(a.apply_key(KeyCode::Enter), Action::Cancel));
    // cancel_run resets state back to a clean prompt.
    a.cancel_run();
    assert!(!a.is_pipeline_active());
    assert!(a.history.iter().any(|m| m.body().contains("已取消")));
}

#[test]
fn slash_cancel_aborts_an_in_flight_agentic_round() {
    // P1-H: an agentic round (`agentic_in_flight`, but NOT a full pipeline) must
    // be cancellable via `/cancel`. The old pipeline-only check left it
    // un-cancellable from the prompt (only Ctrl-C worked).
    let mut a = fresh_app(Some("offline"));
    a.agentic_in_flight = true;
    assert!(
        !a.is_pipeline_active(),
        "an agentic round is not a pipeline"
    );
    for c in "/cancel".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    assert!(
        matches!(a.apply_key(KeyCode::Enter), Action::Cancel),
        "/cancel must abort an in-flight agentic round"
    );
}

#[test]
fn aborted_run_free_text_routes_to_chat_not_a_dead_queue() {
    // P1-G: after an abort the run keeps `run_started = true`, `finished =
    // false`, `aborted = true`. Free text in that state must route to the base
    // as a fresh chat turn (Action::Route) — NOT get queued into `queued_steer`,
    // which never drains after an abort (no further phase/gate gaps), silently
    // swallowing the input.
    let mut a = fresh_app(Some("offline"));
    a.run_started = true;
    a.finished = false;
    a.aborted = true;
    assert!(!a.is_pipeline_active(), "an aborted run is not active");
    for c in "hello again".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let action = a.apply_key(KeyCode::Enter);
    assert_eq!(
        action,
        Action::Route("hello again".to_string()),
        "aborted-state free text must route to chat"
    );
    assert!(
        a.queued_steer.is_empty(),
        "aborted-state input must NOT land in the never-draining steer queue"
    );
}

#[test]
fn slash_backend_is_rejected_during_an_active_run() {
    // P1-I: switching the base mid-run would leave the in-flight run on the old
    // base while config/UI claim the new one (a silent backend mismatch on the
    // next resume). `/backend` must refuse while a run is active and leave the
    // backend unchanged.
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::PipelineStarted {
        slug: "demo".into(),
        requirement: "build a dashboard".into(),
    });
    assert!(a.is_pipeline_active());
    let before = a.backend.clone();
    // `/codex` is the backend-switch verb (TUI uses per-base verbs, not
    // `/backend <id>`); it routes through `slash_backend`.
    for c in "/codex".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let action = a.apply_key(KeyCode::Enter);
    assert_eq!(
        action,
        Action::None,
        "mid-run base switch is a rejected no-op"
    );
    assert_eq!(a.backend, before, "the backend must be unchanged mid-run");
    assert!(
        a.history.iter().any(|m| m.body().contains("/cancel")),
        "the rejection tells the user to /cancel first"
    );
}

#[test]
fn slash_backend_is_rejected_during_an_agentic_chat_turn() {
    // A streaming chat turn is `agentic_in_flight` but NOT `is_pipeline_active()`.
    // A `/codex` here must be refused the same as during a pipeline — otherwise it
    // would commit the new backend + preload a new session while the old turn parks
    // its old-base session, racing into a leaked session or a silent base mismatch.
    let mut a = fresh_app(Some("offline"));
    a.agentic_in_flight = true;
    assert!(!a.is_pipeline_active());
    let before = a.backend.clone();
    for c in "/codex".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let action = a.apply_key(KeyCode::Enter);
    assert_eq!(
        action,
        Action::None,
        "a mid-agentic-turn base switch is a rejected no-op"
    );
    assert_eq!(
        a.backend, before,
        "the backend must be unchanged during an agentic chat turn"
    );
    assert!(
        a.history.iter().any(|m| m.body().contains("/cancel")),
        "the rejection tells the user to /cancel first"
    );
}

#[test]
fn slash_backend_switches_when_no_run_is_active() {
    // The guard is scoped to an ACTIVE run only — switching at the idle prompt
    // still works exactly as before.
    let mut a = fresh_app(Some("offline"));
    assert!(!a.is_pipeline_active());
    for c in "/codex".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let action = a.apply_key(KeyCode::Enter);
    assert_eq!(action, Action::BackendChanged);
    assert_eq!(a.backend.as_deref(), Some("codex"));
}

#[test]
fn enter_on_partial_slash_runs_highlighted_palette_command() {
    let mut a = fresh_app(Some("offline"));
    // "/usag" is a partial that uniquely prefixes "usage".
    for c in "/usag".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let _ = a.apply_key(KeyCode::Enter);
    // It RAN /usage (usage summary), not "未知命令 /usag".
    assert!(
        a.history
            .iter()
            .any(|m| m.body().contains("使用统计") || m.body().contains("还没有使用记录")),
        "partial /usag + Enter should run /usage"
    );
    assert!(
        !a.history.iter().any(|m| m.body().contains("未知命令")),
        "should not report unknown command for a resolvable partial"
    );
}

#[test]
fn model_is_not_a_registered_command() {
    // The `/model` selection feature was removed ENTIRELY: UmaDev owns no model
    // endpoint, so it neither picks nor overrides one — the base runs its own.
    // The command must be gone from the registry AND the dispatcher, so the
    // unknown-command did-you-mean can never suggest it again.
    assert!(
        !App::COMMANDS.iter().any(|c| c.name == "model"),
        "/model must not be a registered command"
    );
    assert!(
        !dispatch_arm_verbs().iter().any(|v| v == "model"),
        "/model must not have a dispatch arm"
    );
}

#[test]
fn kimi_thinking_command_is_typed_model_aware_and_never_chat() {
    let submit = |app: &mut App, command: &str| {
        for character in command.chars() {
            let _ = app.apply_key(KeyCode::Char(character));
        }
        app.apply_key(KeyCode::Enter)
    };

    let mut app = fresh_app(Some("kimi-code"));
    app.base_session_thinking = Some(true);
    app.base_session_thinking_can_enable = true;
    app.base_session_thinking_can_disable = true;
    assert_eq!(
        submit(&mut app, "/thinking off"),
        Action::SetThinking(false)
    );

    app.base_session_thinking = Some(true);
    app.base_session_thinking_can_disable = false;
    assert_eq!(submit(&mut app, "/thinking off"), Action::None);
    assert!(app.history.iter().any(|message| {
        message.body().contains("不允许切换") || message.body().contains("does not allow")
    }));

    let mut other = fresh_app(Some("codex"));
    assert_eq!(submit(&mut other, "/thinking on"), Action::None);
    assert!(other.history.iter().any(|message| {
        message.body().contains("只控制 Kimi") || message.body().contains("Kimi Code's native")
    }));
}
// ---- backend / brain-spec selection ----

#[test]
fn brain_spec_host_cli_when_no_provider() {
    let app = fresh_app(Some("codex"));
    assert!(matches!(app.brain_spec(), crate::BrainSpec::HostCli(_)));
}

#[test]
fn clarify_answer_appended_to_file() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut app = App::new(
        "demo".to_string(),
        UserConfig {
            backend: Some("offline".into()),
            ..Default::default()
        },
        tmp.path().join("config.toml"),
        tmp.path().to_path_buf(),
    );
    // Simulate ClarifyGate open.
    app.active_gate = Some(Gate::ClarifyGate);
    // User types an answer.
    let action = app.submit_text("面向个人开发者".into());
    assert!(matches!(action, Action::None), "answer should not continue");
    // File must exist with the answer.
    let answers =
        std::fs::read_to_string(tmp.path().join("output").join("demo-clarify-answers.md")).unwrap();
    assert!(answers.contains("面向个人开发者"));
}

#[test]
fn clarify_answer_multiple_appends() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut app = App::new(
        "demo".to_string(),
        UserConfig {
            backend: Some("offline".into()),
            ..Default::default()
        },
        tmp.path().join("config.toml"),
        tmp.path().to_path_buf(),
    );
    app.active_gate = Some(Gate::ClarifyGate);
    app.submit_text("answer 1".into());
    app.submit_text("answer 2".into());
    let answers =
        std::fs::read_to_string(tmp.path().join("output").join("demo-clarify-answers.md")).unwrap();
    assert!(answers.contains("answer 1"));
    assert!(answers.contains("answer 2"));
}

#[test]
fn clarify_gate_defers_questions_and_future_tasks_without_polluting_answers() {
    let text = "为什么需要 PostgreSQL？";
    let tmp = tempfile::TempDir::new().unwrap();
    let mut app = App::new(
        "demo".to_string(),
        UserConfig {
            backend: Some("offline".into()),
            ..Default::default()
        },
        tmp.path().join("config.toml"),
        tmp.path().to_path_buf(),
    );
    app.active_gate = Some(Gate::ClarifyGate);

    assert!(
        matches!(
            app.submit_text(text.to_string()),
            Action::GateQuery { question, .. } if question == text
        ),
        "{text}"
    );
    assert!(app.queued_chat.is_empty());
    assert!(app.gate_query_in_flight);
    assert!(
        !tmp.path().join("output/demo-clarify-answers.md").exists(),
        "a gate question must not become a clarification answer: {text}"
    );

    for text in [
        "完成后再做登录",
        "afterwards add account export",
        "另一个问题",
    ] {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut app = App::new(
            "demo".to_string(),
            UserConfig {
                backend: Some("offline".into()),
                ..Default::default()
            },
            tmp.path().join("config.toml"),
            tmp.path().to_path_buf(),
        );
        app.active_gate = Some(Gate::ClarifyGate);

        assert_eq!(app.submit_text(text.to_string()), Action::None, "{text}");
        assert_eq!(
            app.queued_chat.front().map(String::as_str),
            Some(text),
            "the separate turn waits for normal model routing"
        );
        assert!(
            !tmp.path().join("output/demo-clarify-answers.md").exists(),
            "deferred text must not become a clarification answer: {text}"
        );
    }
}

#[test]
fn clarify_c_submits_and_continues() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut app = App::new(
        "demo".to_string(),
        UserConfig {
            backend: Some("offline".into()),
            ..Default::default()
        },
        tmp.path().join("config.toml"),
        tmp.path().to_path_buf(),
    );
    app.active_gate = Some(Gate::ClarifyGate);
    app.submit_text("my answer".into());
    let action = app.submit_text("c".into());
    assert!(matches!(action, Action::Continue(Gate::ClarifyGate)));
    assert!(app.active_gate.is_none(), "gate must clear on continue");
}

#[test]
fn brain_spec_offline_when_backend_offline() {
    let app = fresh_app(Some("offline"));
    assert!(matches!(app.brain_spec(), crate::BrainSpec::Offline));
}

#[test]
fn deploy_command_reads_delivery_notes() {
    let tmp = tempfile::TempDir::new().unwrap();
    let slug = "demo";
    std::fs::create_dir_all(tmp.path().join("output")).unwrap();
    std::fs::write(
            tmp.path().join("output").join(format!("{slug}-delivery-notes.md")),
            "# Delivery\n\n## Deploy command\n\nnpx vercel --prod\n\n## Frontend URL\n\n(not yet deployed)\n",
        ).unwrap();
    let app = App::new(
        slug.to_string(),
        UserConfig {
            backend: Some("offline".into()),
            ..Default::default()
        },
        tmp.path().join("config.toml"),
        tmp.path().to_path_buf(),
    );
    assert_eq!(
        app.deploy_command_from_notes().as_deref(),
        Some("npx vercel --prod")
    );
    // "(not yet deployed)" is filtered out (not http).
    assert!(app.deploy_url_from_notes().is_none());
}

#[test]
fn deploy_url_reads_live_url() {
    let tmp = tempfile::TempDir::new().unwrap();
    let slug = "demo";
    std::fs::create_dir_all(tmp.path().join("output")).unwrap();
    std::fs::write(
        tmp.path()
            .join("output")
            .join(format!("{slug}-delivery-notes.md")),
        "## Frontend URL\n\nhttps://my-app.vercel.app\n",
    )
    .unwrap();
    let app = App::new(
        slug.to_string(),
        UserConfig {
            backend: Some("offline".into()),
            ..Default::default()
        },
        tmp.path().join("config.toml"),
        tmp.path().to_path_buf(),
    );
    assert_eq!(
        app.deploy_url_from_notes().as_deref(),
        Some("https://my-app.vercel.app")
    );
}

#[test]
fn slash_deploy_without_notes_gives_hint() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut app = App::new(
        "demo".to_string(),
        UserConfig {
            backend: Some("offline".into()),
            lang: Some("zh-CN".into()),
            ..Default::default()
        },
        tmp.path().join("config.toml"),
        tmp.path().to_path_buf(),
    );
    let action = app.slash_deploy("");
    assert!(matches!(action, Action::None));
    assert!(app
        .history
        .iter()
        .any(|m| m.body().contains("还没有部署指令")));
}

#[test]
fn slash_deploy_with_command_emits_run_deploy() {
    let tmp = tempfile::TempDir::new().unwrap();
    let slug = "demo";
    std::fs::create_dir_all(tmp.path().join("output")).unwrap();
    std::fs::write(
        tmp.path()
            .join("output")
            .join(format!("{slug}-delivery-notes.md")),
        "## Deploy command\n\nnpx vercel --prod\n",
    )
    .unwrap();
    let mut app = App::new(
        slug.to_string(),
        UserConfig {
            backend: Some("offline".into()),
            ..Default::default()
        },
        tmp.path().join("config.toml"),
        tmp.path().to_path_buf(),
    );
    // Bare /deploy only PREVIEWS — it must not deploy without confirmation.
    let preview = app.slash_deploy("");
    assert!(
        matches!(preview, Action::None),
        "bare /deploy is preview-only"
    );
    // Assert on the locale-independent command — the "not yet run" note is
    // i18n'd, so it differs by resolved locale (zh-CN on dev, English on CI).
    assert!(app
        .history
        .iter()
        .any(|m| m.body().contains("npx vercel --prod")));
    // /deploy confirm actually runs it.
    let action = app.slash_deploy("confirm");
    match action {
        Action::RunDeploy { command } => assert_eq!(command, "npx vercel --prod"),
        other => panic!("expected RunDeploy, got {other:?}"),
    }
}

#[test]
fn slash_deploy_floor_requires_confirm_even_in_auto_mode() {
    // Gap 3 reversibility floor: a deploy is an irreversible network action,
    // so even in the AUTO trust tier bare /deploy must NOT fire — it
    // previews and waits for an explicit confirm. `auto` does not get to
    // skip the floor.
    let tmp = tempfile::TempDir::new().unwrap();
    let slug = "demo";
    std::fs::create_dir_all(tmp.path().join("output")).unwrap();
    std::fs::write(
        tmp.path()
            .join("output")
            .join(format!("{slug}-delivery-notes.md")),
        "## Deploy command\n\nnpx vercel --prod\n",
    )
    .unwrap();
    let mut app = App::new(
        slug.to_string(),
        UserConfig {
            backend: Some("offline".into()),
            ..Default::default()
        },
        tmp.path().join("config.toml"),
        tmp.path().to_path_buf(),
    );
    // Force the strictest-skipping tier; the floor must still gate.
    app.trust_mode_override = Some(umadev_agent::TrustMode::Auto);
    assert_eq!(app.effective_trust_mode(), umadev_agent::TrustMode::Auto);
    let preview = app.slash_deploy("");
    assert!(
        matches!(preview, Action::None),
        "auto mode must NOT skip the deploy confirmation floor"
    );
    // Explicit confirm still works.
    match app.slash_deploy("confirm") {
        Action::RunDeploy { command } => assert_eq!(command, "npx vercel --prod"),
        other => panic!("expected RunDeploy after confirm, got {other:?}"),
    }
}

#[test]
fn slash_version_opens_overlay_with_binary_info() {
    let mut a = fresh_app(Some("offline"));
    for c in "/version".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let _ = a.apply_key(KeyCode::Enter);
    let ov = a.overlay.as_ref().expect("version overlay");
    let joined = ov.lines.join("\n");
    assert!(joined.contains("umadev"));
    assert!(joined.contains(env!("CARGO_PKG_VERSION")));
    assert!(joined.contains("UMADEV_HOST_SPEC_V1"));
}

#[test]
fn slash_version_prefers_live_base_model_over_static_config() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join(".codex")).unwrap();
    std::fs::write(
        tmp.path().join(".codex/config.toml"),
        "model = \"gpt-static-config\"\n",
    )
    .unwrap();
    let mut app = App::new(
        "demo".to_string(),
        UserConfig {
            backend: Some("codex".into()),
            ..Default::default()
        },
        tmp.path().join("config.toml"),
        tmp.path().to_path_buf(),
    );

    app.apply_engine(EngineEvent::BaseModel {
        id: "  gpt-live-session  ".to_string(),
    });
    app.open_version_overlay();

    let joined = app.overlay.as_ref().unwrap().lines.join("\n");
    assert!(joined.contains("model        gpt-live-session (reported by the base)"));
    assert!(!joined.contains("gpt-static-config"));
}

#[test]
fn a_workspace_recovery_note_lands_in_the_transcript() {
    // The heal that puts a user's source tree back (after a run was killed inside a
    // temporary evidence rewind) runs before any UI exists — under the TUI its
    // `tracing::warn!` goes to a log FILE and its startup `eprintln!` is wiped by the
    // alternate screen. So it spoke to nobody. The note now travels through the
    // workspace-notice queue onto the transcript, which is the surface the user reads.
    let tmp = tempfile::TempDir::new().unwrap();
    let mut app = App::new(
        "demo".to_string(),
        UserConfig::default(),
        tmp.path().join("config.toml"),
        tmp.path().to_path_buf(),
    );
    let before = app.history.len();

    umadev_agent::checkpoint::record_workspace_notice("workspace restored: …".to_string());
    let notices = umadev_agent::checkpoint::take_workspace_notices();
    assert!(
        notices.iter().any(|n| n.contains("workspace restored")),
        "the queue carries the note across the surface boundary"
    );
    for note in notices {
        app.push_workspace_notice(note);
    }

    assert_eq!(
        app.history.len(),
        before + 1,
        "the note is IN the transcript"
    );
    let row = app.history.back().expect("row");
    assert!(matches!(row.role, ChatRole::System));
    assert!(row.kind.as_text().contains("workspace restored"));
}

#[test]
fn slash_version_labels_static_model_as_configured() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join(".codex")).unwrap();
    std::fs::write(
        tmp.path().join(".codex/config.toml"),
        "model = \"gpt-static-config\"\n",
    )
    .unwrap();
    let mut app = App::new(
        "demo".to_string(),
        UserConfig {
            backend: Some("codex".into()),
            ..Default::default()
        },
        tmp.path().join("config.toml"),
        tmp.path().to_path_buf(),
    );

    app.open_version_overlay();

    let joined = app.overlay.as_ref().unwrap().lines.join("\n");
    assert!(joined.contains("model        gpt-static-config (configured by the base)"));
    assert!(!joined.contains("reported by the base"));
}

#[test]
fn slash_changelog_opens_overlay_with_header() {
    let mut a = fresh_app(Some("offline"));
    for c in "/changelog".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let _ = a.apply_key(KeyCode::Enter);
    let ov = a.overlay.as_ref().expect("changelog overlay");
    assert!(ov.lines.iter().any(|l| l.contains("Changelog")));
}

#[test]
fn did_you_mean_suggests_for_typo() {
    // "/quitz" → suggest /quit
    let suggestion = App::did_you_mean("quitz");
    assert_eq!(suggestion, Some("quit"));
}

#[test]
fn did_you_mean_corrects_lessions_to_lessons() {
    assert_eq!(App::did_you_mean("lessions"), Some("lessons"));
}

#[test]
fn misspelled_lessions_command_surfaces_the_lessons_correction() {
    let mut app = fresh_app(Some("offline"));
    for ch in "/lessions".chars() {
        let _ = app.apply_key(KeyCode::Char(ch));
    }
    let _ = app.apply_key(KeyCode::Enter);
    let message = app.history.back().expect("unknown-command hint").body();
    assert!(message.contains("/lessions"));
    assert!(message.contains("/lessons"));
}

#[test]
fn did_you_mean_suggests_via_prefix() {
    // "/rev" → /revise (prefix wins)
    let suggestion = App::did_you_mean("rev");
    assert_eq!(suggestion, Some("revise"));
}

#[test]
fn did_you_mean_returns_none_for_garbage() {
    assert_eq!(App::did_you_mean("xxxxxxxxxx"), None);
}

#[test]
fn unknown_slash_command_includes_did_you_mean_hint() {
    let mut a = fresh_app(Some("offline"));
    for c in "/quitz".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let _ = a.apply_key(KeyCode::Enter);
    let last = a.history.back().unwrap();
    assert!(last.body().contains("/quitz"));
    assert!(last.body().contains("/quit"));
    assert!(last.body().contains("是想用"));
}

#[test]
fn extract_json_number_pulls_score() {
    let json = r#"{"score": 95, "passed": true, "notes": "ok"}"#;
    assert_eq!(extract_json_number(json, "score"), Some(95));
    assert_eq!(extract_json_number(json, "missing"), None);
}

#[test]
fn extract_json_bool_pulls_passed() {
    let json = r#"{"score": 70, "passed": false}"#;
    assert_eq!(extract_json_bool(json, "passed"), Some(false));
    assert_eq!(extract_json_bool(json, "score"), None);
}

#[test]
fn verify_overlay_surfaces_quality_gate_when_present() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    let out_dir = root.join("output");
    std::fs::create_dir_all(&out_dir).unwrap();
    std::fs::write(
        out_dir.join("demo-quality-gate.json"),
        r#"{"score": 88, "passed": true}"#,
    )
    .unwrap();

    let mut app = App::new(
        "demo",
        UserConfig {
            backend: Some("offline".into()),
            ..Default::default()
        },
        tmp.path().join("config.toml"),
        root.to_path_buf(),
    );
    for c in "/verify".chars() {
        let _ = app.apply_key(KeyCode::Char(c));
    }
    let _ = app.apply_key(KeyCode::Enter);
    let ov = app.overlay.as_ref().expect("verify overlay");
    let joined = ov.lines.join("\n");
    assert!(joined.contains("Quality gate"));
    assert!(joined.contains("88/100"));
    assert!(joined.contains("PASSED"));
}

#[test]
fn gate_card_lists_artifacts_and_next_steps() {
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::PipelineStarted {
        slug: "demo".into(),
        requirement: "x".into(),
    });
    a.apply_engine(EngineEvent::GateOpened {
        gate: Gate::DocsConfirm,
        choice: None,
    });
    let card = a
        .history
        .iter()
        .find(|m| m.role == ChatRole::Gate)
        .expect("gate card must land in chat");
    // Lists the three core docs by slug.
    assert!(card.body().contains("output/demo-prd.md"));
    assert!(card.body().contains("output/demo-architecture.md"));
    assert!(card.body().contains("output/demo-uiux.md"));
    // Lists next-step verbs.
    assert!(card.body().contains("/continue"));
    assert!(card.body().contains("/revise"));
    assert!(card.body().contains("/diff"));
}

#[test]
fn gate_card_for_preview_confirm_lists_frontend_artifacts() {
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::PipelineStarted {
        slug: "shop".into(),
        requirement: "x".into(),
    });
    a.apply_engine(EngineEvent::GateOpened {
        gate: Gate::PreviewConfirm,
        choice: None,
    });
    let card = a
        .history
        .iter()
        .find(|m| m.role == ChatRole::Gate)
        .expect("gate card must land in chat");
    assert!(card.body().contains("output/shop-frontend-notes.md"));
    assert!(card.body().contains("output/shop-execution-plan.md"));
}

#[test]
fn gate_card_includes_approval_checklist() {
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::PipelineStarted {
        slug: "demo".into(),
        requirement: "x".into(),
    });
    a.apply_engine(EngineEvent::GateOpened {
        gate: Gate::DocsConfirm,
        choice: None,
    });
    let card = a
        .history
        .iter()
        .find(|m| m.role == ChatRole::Gate)
        .expect("gate card must land in chat");
    // The checklist tells the user WHAT to verify before approving.
    assert!(card.body().contains("审批清单"));
    assert!(card.body().contains("验收标准") || card.body().contains("验收"));
}

#[test]
fn fmt_elapsed_formats_seconds_and_minutes() {
    assert_eq!(fmt_elapsed(5), "5s");
    assert_eq!(fmt_elapsed(59), "59s");
    assert_eq!(fmt_elapsed(60), "1:00");
    assert_eq!(fmt_elapsed(125), "2:05");
    assert_eq!(fmt_elapsed(3661), "61:01");
}

#[test]
fn pipeline_started_sets_run_timer() {
    let mut a = fresh_app(Some("offline"));
    assert!(a.run_started_at.is_none());
    a.apply_engine(EngineEvent::PipelineStarted {
        slug: "demo".into(),
        requirement: "x".into(),
    });
    assert!(a.run_started_at.is_some(), "run timer must start");
}

#[test]
fn gate_open_stops_run_timer() {
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::PipelineStarted {
        slug: "demo".into(),
        requirement: "x".into(),
    });
    a.apply_engine(EngineEvent::GateOpened {
        gate: Gate::DocsConfirm,
        choice: None,
    });
    // Timer stops while waiting on the user — status bar shouldn't keep
    // ticking during an approval pause.
    assert!(a.run_started_at.is_none());
    assert!(a.phase_started_at.is_none());
}

#[test]
fn verify_failed_appends_actionable_hint() {
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::VerifyFailed {
        phase: Phase::Frontend,
        exit_code: 1,
        stderr: "error: cannot find module 'react'".into(),
    });
    // The verify-failed line is now localized (the word "verify" itself is
    // translated), so find it by its language-neutral [fail] tag instead.
    let msg = a
        .history
        .iter()
        .find(|m| m.body().contains("[fail]"))
        .expect("verify failure message");
    assert!(msg.body().contains("依赖未安装"), "got: {}", msg.body());
}

#[test]
fn bare_c_at_gate_is_treated_as_continue_shortcut() {
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::GateOpened {
        gate: Gate::DocsConfirm,
        choice: None,
    });
    let _ = a.apply_key(KeyCode::Char('c'));
    let action = a.apply_key(KeyCode::Enter);
    assert_eq!(action, Action::Continue(Gate::DocsConfirm));
    assert!(a.active_gate.is_none());
}

#[test]
fn bare_c_without_gate_is_plain_chat() {
    let mut a = fresh_app(Some("offline"));
    let _ = a.apply_key(KeyCode::Char('c'));
    let action = a.apply_key(KeyCode::Enter);
    // Outside a gate, "c" is neither approval nor a real requirement.
    assert_eq!(action, Action::Route("c".to_string()));
    assert!(!a.history.iter().any(|m| m.body().contains("直接描述需求")));
}

#[test]
fn chinese_greeting_is_plain_chat_not_pipeline() {
    let mut a = fresh_app(Some("offline"));
    for c in "你好".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let action = a.apply_key(KeyCode::Enter);
    assert_eq!(action, Action::Route("你好".to_string()));
    assert!(!a.history.iter().any(|m| m.body().contains("收到需求")));
}

#[test]
fn how_are_you_is_plain_chat_not_pipeline() {
    let mut a = fresh_app(Some("offline"));
    for c in "你好吗？我很好啊".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let action = a.apply_key(KeyCode::Enter);
    assert_eq!(action, Action::Route("你好吗？我很好啊".to_string()));
    assert!(!a.history.iter().any(|m| m.body().contains("流水线启动")));
}

#[test]
fn slash_continue_no_run_hint_redirects_to_typing_a_requirement() {
    let mut a = fresh_app(Some("offline"));
    for c in "/continue".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let _ = a.apply_key(KeyCode::Enter);
    let last = a.history.back().unwrap();
    assert!(
        last.body().contains("还没启动流水线"),
        "expected redirect hint, got: {}",
        last.body()
    );
}

#[test]
fn preflight_message_lands_when_starting_run() {
    let mut a = fresh_app(Some("offline"));
    a.prepare_worker_routed_run("build me a thing");
    // The UmaDev preflight message includes the plan with HUMAN checkpoint labels —
    // never the raw snake_case gate ids or the internal pipeline-shape wording.
    assert!(a.history.iter().any(|m| m.role == ChatRole::UmaDev
        && m.body().contains("确认核心文档")
        && m.body().contains("确认前端预览")));
    assert!(
        !a.history
            .iter()
            .any(|m| m.body().contains("docs_confirm") || m.body().contains("preview_confirm")),
        "preflight must not leak raw snake_case gate ids"
    );
}

// ---- cursor + editing ----

#[test]
fn left_arrow_moves_cursor_back_one_char() {
    let mut a = fresh_app(Some("offline"));
    for c in "abc".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    assert_eq!(a.input_cursor, 3);
    let _ = a.apply_key(KeyCode::Left);
    assert_eq!(a.input_cursor, 2);
}

#[test]
fn home_and_end_jump_cursor() {
    let mut a = fresh_app(Some("offline"));
    for c in "abc".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let _ = a.apply_key(KeyCode::Home);
    assert_eq!(a.input_cursor, 0);
    let _ = a.apply_key(KeyCode::End);
    assert_eq!(a.input_cursor, 3);
}

#[test]
fn forward_delete_removes_char_at_cursor() {
    let mut a = fresh_app(Some("offline"));
    for c in "abc".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let _ = a.apply_key(KeyCode::Home);
    let _ = a.apply_key(KeyCode::Delete);
    assert_eq!(a.input, "bc");
    assert_eq!(a.input_cursor, 0);
}

#[test]
fn insertion_in_middle_preserves_surrounding_chars() {
    let mut a = fresh_app(Some("offline"));
    for c in "ac".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let _ = a.apply_key(KeyCode::Left);
    let _ = a.apply_key(KeyCode::Char('b'));
    assert_eq!(a.input, "abc");
    assert_eq!(a.input_cursor, 2);
}

#[test]
fn backspace_respects_cjk_boundary() {
    let mut a = fresh_app(Some("offline"));
    for c in "做个".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    assert_eq!(a.input, "做个");
    // Backspace once → just one CJK char gone, no panic.
    let _ = a.apply_key(KeyCode::Backspace);
    assert_eq!(a.input, "做");
}

#[test]
fn windows_bs_control_char_backspaces() {
    let mut a = fresh_app(Some("offline"));
    for c in "abc".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let _ = a.apply_key(KeyCode::Char('\u{8}'));
    assert_eq!(a.input, "ab");
    assert_eq!(a.input_cursor, 2);
}

#[test]
fn alt_backspace_deletes_word_not_one_char() {
    let mut a = fresh_app(Some("offline"));
    for c in "hello world".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }

    let _ = a.apply_key_with_mods(KeyCode::Backspace, crossterm::event::KeyModifiers::ALT);

    assert_eq!(a.input, "hello ");
    assert_eq!(a.input_cursor, 6);
}

// ---- I5: grapheme-cluster-aware cursor ----

#[test]
fn cursor_steps_over_zwj_emoji_as_one_grapheme() {
    let mut a = fresh_app(Some("offline"));
    // A ZWJ family emoji is several codepoints but ONE user-perceived glyph.
    let family = "👨‍👩‍👧";
    assert!(
        family.chars().count() > 1,
        "precondition: multi-codepoint cluster"
    );
    let n = family.chars().count();
    a.insert_str_at_cursor(family);
    assert_eq!(a.input_cursor, n, "cursor at the end after insert");
    // One ← steps over the WHOLE cluster, not one codepoint.
    a.move_cursor(-1);
    assert_eq!(a.input_cursor, 0, "one ← jumps the whole ZWJ cluster");
    // One → steps forward over the whole cluster.
    a.move_cursor(1);
    assert_eq!(a.input_cursor, n, "one → crosses the whole cluster");
    // Backspace removes the whole glyph — no half-mojibake left behind.
    a.backspace();
    assert_eq!(a.input, "", "backspace deletes the whole cluster");
}

#[test]
fn cursor_steps_over_combining_mark_as_one_grapheme() {
    let mut a = fresh_app(Some("offline"));
    // 'e' + U+0301 COMBINING ACUTE = 2 codepoints, one grapheme "é".
    let e_acute = "e\u{301}";
    a.insert_str_at_cursor(e_acute);
    assert_eq!(a.input.chars().count(), 2, "precondition: base + combining");
    a.move_cursor(-1);
    assert_eq!(a.input_cursor, 0, "← steps over base+combining as one unit");
    // Forward-delete from the start removes the whole cluster, not just 'e'.
    a.forward_delete();
    assert_eq!(a.input, "", "forward-delete removes the whole cluster");
}

#[test]
fn cursor_still_steps_single_ascii_and_cjk_chars() {
    let mut a = fresh_app(Some("offline"));
    a.insert_str_at_cursor("ab做");
    assert_eq!(a.input_cursor, 3);
    a.move_cursor(-1);
    assert_eq!(a.input_cursor, 2, "one ← over the CJK char");
    a.move_cursor(-1);
    assert_eq!(a.input_cursor, 1, "one ← over 'b'");
    a.move_cursor(-1);
    assert_eq!(a.input_cursor, 0, "one ← over 'a'");
    // Forward-delete removes exactly one char (no over-eager cluster merge).
    a.forward_delete();
    assert_eq!(a.input, "b做", "forward-delete removed only 'a'");
}

// ---- Shift+Enter multi-line ----

#[test]
fn shift_enter_inserts_newline_and_does_not_submit() {
    let mut a = fresh_app(Some("offline"));
    for c in "line1".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let action = a.apply_key_with_mods(KeyCode::Enter, crossterm::event::KeyModifiers::SHIFT);
    assert_eq!(action, Action::None);
    assert!(a.input.contains("line1\n"));
    // Cursor advances past the newline.
    assert!(a.input_cursor >= 6);
}

#[test]
fn plain_enter_after_shift_enter_keeps_short_multiline_as_chat() {
    let mut a = fresh_app(Some("offline"));
    for c in "line1".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let _ = a.apply_key_with_mods(KeyCode::Enter, crossterm::event::KeyModifiers::SHIFT);
    for c in "line2".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let action = a.apply_key(KeyCode::Enter);
    assert_eq!(action, Action::Route("line1\nline2".to_string()));
}

#[test]
fn plain_enter_after_shift_enter_submits_multiline_requirement() {
    let mut a = fresh_app(Some("offline"));
    for c in "build a login app".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let _ = a.apply_key_with_mods(KeyCode::Enter, crossterm::event::KeyModifiers::SHIFT);
    for c in "with email authentication".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let action = a.apply_key(KeyCode::Enter);
    assert_eq!(
        action,
        Action::Route("build a login app\nwith email authentication".to_string())
    );
}

// ---- Ctrl+J universal newline (works on EVERY terminal) ----

#[test]
fn ctrl_j_inserts_newline_and_does_not_submit_on_the_owned_path() {
    // The owned byte tokenizer surfaces a Ctrl+J press as the raw LF byte
    // 0x0A; `keymap::normalize_key` (applied by `apply_key_with_mods`) folds
    // that to `Char('j')` + CONTROL, exactly like the decoder. Feeding the
    // raw form exercises the whole owned path end-to-end.
    let mut a = fresh_app(Some("offline"));
    for c in "line1".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let action = a.apply_key_with_mods(
        KeyCode::Char('\u{0a}'),
        crossterm::event::KeyModifiers::NONE,
    );
    assert_eq!(action, Action::None, "Ctrl+J must NOT submit");
    assert!(
        a.input.contains("line1\n"),
        "Ctrl+J inserts a literal newline"
    );
    assert!(a.input_cursor >= 6, "cursor advances past the newline");
}

#[test]
fn ctrl_v_requests_image_capture_without_touching_the_text_paste_path() {
    let mut app = fresh_app(Some("claude-code"));
    app.input = "draft ".into();
    app.input_cursor = app.input_len();
    let before = app.input.clone();

    let action =
        app.apply_key_with_mods(KeyCode::Char('v'), crossterm::event::KeyModifiers::CONTROL);

    assert_eq!(action, Action::PasteImage);
    assert_eq!(app.input, before, "the blocking worker owns image capture");
    assert!(app.attachments.is_empty());

    // Ordinary terminal text paste remains the direct, synchronous old path:
    // no image Action and no platform probe is involved.
    app.handle_paste("plain clipboard text");
    assert_eq!(app.input, "draft plain clipboard text");
    assert!(app.attachments.is_empty());
}

#[test]
fn ctrl_j_post_decode_form_also_inserts_a_newline() {
    // The already-decoded form (`Char('j')` + CONTROL) — what the dispatch
    // actually matches — must reach the same newline-insert arm.
    let mut a = fresh_app(Some("offline"));
    for c in "abc".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let action = a.apply_key_with_mods(KeyCode::Char('j'), crossterm::event::KeyModifiers::CONTROL);
    assert_eq!(action, Action::None);
    assert_eq!(a.input, "abc\n");
}

#[test]
fn plain_enter_still_submits_the_common_case() {
    // The daily-common path: a bare Enter (a plain CR on every terminal) must
    // keep SUBMITTING, never regress to a newline.
    let mut a = fresh_app(Some("offline"));
    for c in "just ship it".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let action = a.apply_key(KeyCode::Enter);
    assert_eq!(action, Action::Route("just ship it".to_string()));
}

#[test]
fn shift_enter_via_kitty_csi_u_inserts_a_newline() {
    // On a kitty-capable terminal (protocol enabled in `setup_terminal`),
    // Shift+Enter arrives as the CSI-u sequence `\x1b[13;2u`. Drive the FULL
    // owned pipeline (tokenizer → decoder) over those bytes and feed the
    // resulting key to the app — it must insert a newline, not submit.
    let mut tk = crate::input::tokenize::Tokenizer::for_stdin();
    let mut dec = crate::input::decode::Decoder::new();
    let mut keyed: Option<(KeyCode, crossterm::event::KeyModifiers)> = None;
    for token in tk.feed(b"\x1b[13;2u") {
        for ev in dec.feed_token(token) {
            if let crate::input::decode::InputEvent::Key(k) = ev {
                keyed = Some((k.code, k.modifiers));
            }
        }
    }
    let (code, mods) = keyed.expect("CSI-u Shift+Enter must decode to one key");
    assert_eq!(code, KeyCode::Enter);
    assert!(mods.contains(crossterm::event::KeyModifiers::SHIFT));

    let mut a = fresh_app(Some("offline"));
    for c in "line1".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let action = a.apply_key_with_mods(code, mods);
    assert_eq!(action, Action::None, "Shift+Enter (CSI-u) must NOT submit");
    assert!(a.input.contains("line1\n"));
}

#[test]
fn no_kitty_terminal_still_submits_enter_and_newlines_on_ctrl_j() {
    // A terminal that does NOT report kitty support gets no protocol push, so
    // Shift+Enter can't be distinguished — but the daily pain is still fixed:
    // plain Enter submits (a bare CR), and Ctrl+J (a literal LF) newlines.
    let mut a = fresh_app(Some("offline"));
    for c in "draft".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    // Ctrl+J newlines mid-draft.
    let nl = a.apply_key_with_mods(KeyCode::Char('j'), crossterm::event::KeyModifiers::CONTROL);
    assert_eq!(nl, Action::None);
    for c in "more".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    // Plain Enter submits the whole multi-line draft.
    let action = a.apply_key(KeyCode::Enter);
    assert_eq!(action, Action::Route("draft\nmore".to_string()));
}

// ---- palette ----

#[test]
fn slash_run_only_treats_a_separatored_ascii_first_word_as_a_slug() {
    // `todo-app` (ASCII + a `-` separator) IS the optional run slug.
    let mut a = fresh_app(Some("offline"));
    let _ = a.slash_run("todo-app 做一个待办应用");
    assert_eq!(a.slug, "todo-app");
    // A multi-word / Chinese requirement's first word is NOT mistaken for a
    // slug (no separator / not ASCII), so the whole thing stays the requirement
    // and no slug-invalid error fires (was: '/run with spaces' wrongly rejected).
    let mut b = fresh_app(Some("offline"));
    let _ = b.slash_run("做一个 带空格 的登录页");
    assert_ne!(
        b.slug, "做一个",
        "the first word must not become a phantom slug"
    );
}

#[test]
fn plan_mode_slash_run_settles_before_any_run_state_or_task_is_created() {
    let mut app = fresh_app(Some("claude-code"));
    app.set_trust_mode(umadev_agent::TrustMode::Plan);
    let root = app.project_root.clone();

    let action = app.slash_run("todo-app build a todo app");

    assert_eq!(action, Action::None);
    assert!(
        app.tasks.is_empty(),
        "a non-executed plan is not a build task"
    );
    assert!(!app.run_started && !app.director_run_in_flight);
    assert!(app.requirement.is_empty());
    assert!(
        !root.join(".umadev/run.lock").exists()
            && !root.join(".umadev/workflow-state.json").exists()
            && !root.join(".umadev/governance-context.json").exists(),
        "Plan /run must settle before all execution persistence"
    );
    assert!(app
        .history
        .iter()
        .any(|m| m.body().contains("计划模式") || m.body().contains("Plan mode")));
}

#[test]
fn palette_fuzzy_finds_deploy_from_dpl() {
    let mut a = fresh_app(Some("offline"));
    for c in "/dpl".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let verbs: Vec<&str> = a.palette_matches().iter().map(|p| p.verb).collect();
    assert!(
        verbs.contains(&"deploy"),
        "fuzzy /dpl → deploy, got: {verbs:?}"
    );
}

#[test]
fn word_motion_jumps_across_words() {
    let mut a = fresh_app(Some("offline"));
    for c in "hello world foo".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    a.move_word_left();
    assert_eq!(a.input_cursor, 12, "→ start of last word 'foo'");
    a.move_word_left();
    assert_eq!(a.input_cursor, 6, "→ start of 'world'");
    a.move_word_right();
    assert_eq!(a.input_cursor, 12, "→ back to start of 'foo'");
}

#[test]
fn palette_matches_filter_by_prefix() {
    let mut a = fresh_app(Some("offline"));
    for c in "/cl".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let matches = a.palette_matches();
    // /claude /clear → 2 matches.
    let verbs: Vec<&str> = matches.iter().map(|p| p.verb).collect();
    assert!(verbs.contains(&"claude"));
    assert!(verbs.contains(&"clear"));
}

#[test]
fn arrow_down_navigates_palette_when_active() {
    let mut a = fresh_app(Some("offline"));
    for c in "/c".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let before = a.palette_selected;
    let _ = a.apply_key(KeyCode::Down);
    assert_ne!(a.palette_selected, before);
}

#[test]
fn tab_autocompletes_selected_palette_match() {
    let mut a = fresh_app(Some("offline"));
    for c in "/cla".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let _ = a.apply_key(KeyCode::Tab);
    assert_eq!(a.input, "/claude ");
}

// ---- @-file-mention typeahead ----

/// Seed a few files into the test workspace so the `@`-typeahead has real
/// candidates to rank: `src/main.rs`, `src/lib.rs`, `README.md`.
fn seed_mention_files(a: &App) {
    let root = &a.project_root;
    let _ = std::fs::create_dir_all(root.join("src"));
    let _ = std::fs::write(root.join("src/main.rs"), "fn main() {}\n");
    let _ = std::fs::write(root.join("src/lib.rs"), "// lib\n");
    let _ = std::fs::write(root.join("README.md"), "# readme\n");
}

#[test]
fn mention_detects_partial_under_cursor() {
    let mut a = fresh_app(Some("offline"));
    for c in "look at @sr".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    assert_eq!(
        a.mention_token(),
        Some((8, "sr".to_string())),
        "the `@sr` token under the cursor is detected with its partial"
    );
}

#[test]
fn mention_inactive_without_at_token() {
    let mut a = fresh_app(Some("offline"));
    for c in "hello world".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    assert_eq!(a.mention_token(), None, "no `@` → no mention context");
    assert!(a.mention_matches().is_empty(), "no `@` → no candidates");
    // An `@` glued to a preceding non-space (an email) must NOT open it.
    let mut b = fresh_app(Some("offline"));
    for c in "ping a@host".chars() {
        let _ = b.apply_key(KeyCode::Char(c));
    }
    assert_eq!(b.mention_token(), None, "`a@host` is not a file mention");
}

#[test]
fn mention_candidates_filter_by_partial() {
    let mut a = fresh_app(Some("offline"));
    seed_mention_files(&a);
    for c in "@main".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let m = a.mention_matches();
    assert!(
        m.iter().any(|p| p == "src/main.rs"),
        "`@main` ranks src/main.rs, got {m:?}"
    );
    assert!(
        !m.iter().any(|p| p == "README.md"),
        "`README.md` is filtered out by the `main` partial, got {m:?}"
    );
}

#[test]
fn mention_accept_inserts_typed_file_chip_and_replaces_partial() {
    let mut a = fresh_app(Some("offline"));
    seed_mention_files(&a);
    for c in "@main".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let _ = a.apply_key(KeyCode::Tab);
    assert_eq!(
        a.input, "[文件 1] ",
        "Tab replaced `@main` with a path-free file chip"
    );
    assert_eq!(a.file_attachments.len(), 1);
    assert_eq!(
        a.input_cursor,
        a.input_len(),
        "caret lands after the insert"
    );
    assert!(
        a.mention_matches().is_empty(),
        "the trailing space closes the popover"
    );
}

#[test]
fn mention_enter_inserts_selected_file_chip() {
    let mut a = fresh_app(Some("offline"));
    seed_mention_files(&a);
    for c in "@README".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let _ = a.apply_key(KeyCode::Enter);
    assert_eq!(a.input, "[文件 1] ", "Enter accepted the mention");
    assert_eq!(a.file_attachments.len(), 1);
}

#[test]
fn mention_popover_suppresses_slash_palette() {
    // A line that is BOTH a slash command and carries an `@`-token: the
    // mention popover wins, so Tab inserts the file path — not the slash
    // completion. Proves the two popovers are mutually exclusive.
    let mut a = fresh_app(Some("offline"));
    seed_mention_files(&a);
    for c in "/run @main".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    assert!(
        !a.mention_matches().is_empty(),
        "the `@main` token is active"
    );
    assert!(
        !a.palette_matches().is_empty(),
        "`/run` still matches the palette registry"
    );
    let _ = a.apply_key(KeyCode::Tab);
    assert_eq!(
        a.input, "/run [文件 1] ",
        "Tab accepted the mention, not the slash autocomplete"
    );
}

#[test]
fn mention_esc_closes_without_inserting() {
    let mut a = fresh_app(Some("offline"));
    seed_mention_files(&a);
    for c in "@main".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    assert!(!a.mention_matches().is_empty(), "popover open before Esc");
    let _ = a.apply_key(KeyCode::Esc);
    assert!(a.mention_dismissed, "Esc dismissed the popover");
    assert!(a.mention_matches().is_empty(), "popover closed after Esc");
    assert_eq!(a.input, "@main", "Esc left the prompt text untouched");
    // A further edit re-opens the popover (dismissal is not sticky).
    let _ = a.apply_key(KeyCode::Char('.'));
    assert!(
        !a.mention_matches().is_empty(),
        "editing re-opened the popover"
    );
}

#[test]
fn delete_reopens_dismissed_mention_after_query_changes() {
    let mut a = fresh_app(Some("offline"));
    seed_mention_files(&a);
    for c in "@mainx".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    a.dismiss_mention();
    assert!(
        a.mention_matches().is_empty(),
        "dismissed mention starts closed"
    );

    let _ = a.apply_key(KeyCode::Left);
    assert!(
        a.mention_matches().is_empty(),
        "pure cursor movement must not re-open a dismissed mention"
    );
    let _ = a.apply_key(KeyCode::Delete);

    assert_eq!(a.input, "@main");
    assert!(
        !a.mention_matches().is_empty(),
        "forward delete changed the query and must re-open matches"
    );
}

#[test]
fn kill_edit_keys_reset_dismissed_mention_state() {
    let mut a = fresh_app(Some("offline"));
    seed_mention_files(&a);
    a.input = "@mainx".to_string();
    a.input_cursor = a.input_len() - 1;
    a.dismiss_mention();

    let _ = a.apply_key_with_mods(KeyCode::Char('k'), crossterm::event::KeyModifiers::CONTROL);
    assert_eq!(a.input, "@main");
    assert!(
        !a.mention_matches().is_empty(),
        "Ctrl+K changed the active mention token and must re-open it"
    );

    a.dismiss_mention();
    let _ = a.apply_key_with_mods(KeyCode::Char('w'), crossterm::event::KeyModifiers::CONTROL);
    assert!(
        !a.mention_dismissed,
        "Ctrl+W is an edit and must not leave a stale dismissed flag"
    );
}

#[test]
fn mention_arrow_down_cycles_selection() {
    let mut a = fresh_app(Some("offline"));
    seed_mention_files(&a);
    // A bare `@` lists every file (≥2), so ↓ can move the highlight.
    let _ = a.apply_key(KeyCode::Char('@'));
    let count = a.mention_matches().len();
    assert!(count >= 2, "expected ≥2 candidates, got {count}");
    assert_eq!(a.mention_selected, 0, "starts on the first candidate");
    let _ = a.apply_key(KeyCode::Down);
    assert_eq!(a.mention_selected, 1, "↓ moved the mention highlight");
}

// ---- I8 — fzf-style positional fuzzy scorer ----

#[test]
fn fuzzy_score_ranks_boundary_path_above_incidental_subsequence() {
    // `main` matched contiguously at a path boundary (`src/main.rs`) must
    // outscore the same chars buried mid-word (`domain_libs.rs` — d-o-MAIN).
    let boundary = fuzzy_score("main", "src/main.rs").expect("boundary match");
    let incidental = fuzzy_score("main", "domain_libs.rs").expect("incidental match");
    assert!(
        boundary > incidental,
        "boundary/path match ({boundary}) should beat incidental subsequence ({incidental})"
    );
}

#[test]
fn fuzzy_score_rejects_non_subsequence_and_no_ops_empty_query() {
    // Not a subsequence → None (the scan is also the existence test).
    assert!(fuzzy_score("xyz", "src/main.rs").is_none());
    assert!(fuzzy_score("nima", "src/main.rs").is_none()); // out of order
                                                           // Empty query is a ranking no-op (callers short-circuit it).
    assert_eq!(fuzzy_score("", "anything"), Some(0));
    // Case-insensitive (ASCII fold).
    assert!(fuzzy_score("MAIN", "src/main.rs").is_some());
}

#[test]
fn palette_ranks_exact_command_first() {
    // An exact verb sorts ahead of looser fuzzy hits (tier wins over score).
    let mut a = fresh_app(Some("offline"));
    for c in "/clear".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let m = a.palette_matches();
    assert_eq!(
        m.first().map(|p| p.verb),
        Some("clear"),
        "exact `/clear` ranks first, got {:?}",
        m.iter().map(|p| p.verb).collect::<Vec<_>>()
    );
}

#[test]
fn palette_prefix_outranks_fuzzy() {
    // `/cla` → `claude` is a prefix (tier 1); `clear` is only a fuzzy hit
    // (c-l-e-A-r, tier 2). The prefix must rank first regardless of score.
    let mut a = fresh_app(Some("offline"));
    for c in "/cla".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let verbs: Vec<&str> = a.palette_matches().iter().map(|p| p.verb).collect();
    let pc = verbs.iter().position(|v| *v == "claude");
    let pl = verbs.iter().position(|v| *v == "clear");
    assert_eq!(pc, Some(0), "prefix `claude` is first, got {verbs:?}");
    if let (Some(pc), Some(pl)) = (pc, pl) {
        assert!(pc < pl, "prefix outranks fuzzy: {verbs:?}");
    }
}

#[test]
fn mention_fuzzy_ranks_path_match_above_incidental_hit() {
    let mut a = fresh_app(Some("offline"));
    let root = a.project_root.clone();
    let _ = std::fs::create_dir_all(root.join("src"));
    let _ = std::fs::write(root.join("src/main.rs"), "");
    let _ = std::fs::write(root.join("domain_libs.rs"), "");
    for c in "@main".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let m = a.mention_matches();
    let pos_main = m.iter().position(|p| p == "src/main.rs");
    let pos_dom = m.iter().position(|p| p == "domain_libs.rs");
    assert!(pos_main.is_some(), "src/main.rs is a candidate: {m:?}");
    if let (Some(pm), Some(pd)) = (pos_main, pos_dom) {
        assert!(
            pm < pd,
            "the path/boundary match ranks above the incidental hit: {m:?}"
        );
    }
}

#[test]
fn arrow_up_with_input_not_in_palette_recalls_history() {
    let mut a = fresh_app(Some("offline"));
    // Submit a prompt to populate history.
    for c in "first request".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let _ = a.apply_key(KeyCode::Enter);
    // After submit, input is empty. ↑ should recall it.
    assert!(a.input.is_empty());
    let _ = a.apply_key(KeyCode::Up);
    assert_eq!(a.input, "first request");
}

#[test]
fn arrow_down_at_newest_history_returns_to_fresh_draft() {
    let mut a = fresh_app(Some("offline"));
    for c in "request".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let _ = a.apply_key(KeyCode::Enter);
    let _ = a.apply_key(KeyCode::Up);
    assert_eq!(a.input, "request");
    let _ = a.apply_key(KeyCode::Down);
    assert!(a.input.is_empty());
    assert!(a.input_history_idx.is_none());
}

#[test]
fn submit_dedups_consecutive_identical_recalls() {
    let tmp = tempfile::TempDir::new().unwrap();
    let cfg = UserConfig {
        backend: Some("offline".to_string()),
        ..Default::default()
    };
    let mut a = App::new(
        "demo",
        cfg,
        tmp.path().join("config.toml"),
        tmp.path().join("workspace"),
    );
    for c in "same".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let _ = a.apply_key(KeyCode::Enter);
    a.finished = true;
    a.run_started = false;
    for c in "same".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let _ = a.apply_key(KeyCode::Enter);
    assert_eq!(
        a.input_history
            .iter()
            .filter(|s| s.as_str() == "same")
            .count(),
        1
    );
}

#[test]
fn esc_when_idle_needs_a_second_press_to_quit() {
    let mut a = fresh_app(Some("offline"));
    // First idle Esc arms the confirmation (guards against an accidental
    // quit — including the Esc that just interrupted a run).
    let action = a.apply_key(KeyCode::Esc);
    assert_eq!(action, Action::None);
    assert!(a.pending_quit_confirm);
    // Second Esc actually quits.
    let action = a.apply_key(KeyCode::Esc);
    assert_eq!(action, Action::Quit);
}

// ── Feature B: idle double-Esc rewinds (edit + resend the last message) ──

#[test]
fn double_esc_on_empty_idle_rewinds_last_user_message() {
    let mut a = fresh_app(Some("offline"));
    // `fresh_app` seeds a greeting; measure from there so the test is robust
    // to the welcome prefix.
    let base = a.history.len();
    // A short conversation: two user turns, each with a reply.
    a.push(ChatRole::You, "first");
    a.push(ChatRole::Host, "reply one");
    a.push(ChatRole::You, "second");
    a.push(ChatRole::Host, "reply two");
    assert!(a.input.is_empty(), "starts on an empty idle input");

    // First Esc ARMS the rewind (a stray single Esc can't rewind) — input
    // and transcript are untouched, and it never quits.
    let r1 = a.apply_key(KeyCode::Esc);
    assert_eq!(r1, Action::None);
    assert!(a.pending_rewind, "first Esc arms the rewind");
    assert!(a.input.is_empty(), "first Esc does not yet reload");
    assert!(!a.should_quit);

    // Second Esc FIRES: the last user message is re-loaded into the box, and
    // the transcript is truncated to everything BEFORE that turn.
    let r2 = a.apply_key(KeyCode::Esc);
    assert_eq!(r2, Action::None);
    assert_eq!(a.input, "second", "last user message reloaded for editing");
    assert_eq!(a.input_cursor, a.input_len(), "cursor parked at the end");
    assert!(!a.pending_rewind, "rewind disarmed after firing");
    assert!(!a.should_quit, "rewind never quits");
    // The last user turn + everything after it is gone; the earlier turn
    // (`first` + its reply) survives.
    assert_eq!(
        a.history.len(),
        base + 2,
        "the last user turn + everything after dropped"
    );
    let users: Vec<_> = a
        .history
        .iter()
        .filter(|m| m.role == ChatRole::You)
        .collect();
    assert_eq!(users.len(), 1, "exactly the earlier user turn remains");
    assert_eq!(users[0].body().as_ref(), "first");
}

#[test]
fn rewind_truncates_conversation_and_transcript_to_match_history() {
    // Low finding — double-Esc rewind dropped the last user turn from the
    // VISIBLE history but not from `conversation` (the base-facing memory) or
    // `full_transcript` (the on-disk record), so a resend re-asked WITH the
    // dropped turn and a relaunch `/resume` restored it. All three must stay
    // in lockstep.
    let mut a = fresh_app(Some("offline"));
    // Two complete turns recorded into BOTH the visible history and the
    // base-facing memory, mirroring a real chat session.
    a.push(ChatRole::You, "first");
    a.record_user_turn("first");
    a.record_chat_reply("reply one".to_string());
    a.push(ChatRole::You, "second");
    a.record_user_turn("second");
    a.record_chat_reply("reply two".to_string());
    assert_eq!(a.conversation.len(), 4, "two user + two assistant turns");
    assert_eq!(a.full_transcript.len(), 4);

    // Double-Esc rewind (idle, empty box): arm, then fire.
    let _ = a.apply_key(KeyCode::Esc);
    let _ = a.apply_key(KeyCode::Esc);
    assert_eq!(a.input, "second", "last user turn reloaded for editing");

    // The dropped turn is gone from the memory + durable transcript too — the
    // base won't see it on resend and a relaunch won't restore it.
    assert_eq!(
        a.conversation
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>(),
        vec!["first", "reply one"],
        "conversation truncated to before the rewound user turn"
    );
    assert_eq!(
        a.full_transcript
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>(),
        vec!["first", "reply one"],
        "durable transcript truncated to match"
    );
}

#[test]
fn rewinding_the_first_user_turn_clears_the_on_disk_chat() {
    // Fix 8 — rewinding the FIRST user turn empties `full_transcript`, and
    // `persist_chat` early-returns on an empty transcript, so without an
    // explicit delete the OLD, un-rewound chat survives on disk and a relaunch
    // `/resume` would restore the conversation the rewind dropped. The rewind
    // must clear the persisted chat instead.
    let mut a = fresh_app(Some("offline"));
    // One complete first turn; `record_user_turn` persists it to disk.
    a.push(ChatRole::You, "only turn");
    a.record_user_turn("only turn");
    a.record_chat_reply("a reply".to_string());
    let path = a.chat_path(&a.chat_id);
    assert!(path.exists(), "the first turn was persisted to disk");

    // Double-Esc rewind of the (only) user turn (idle, empty box).
    let _ = a.apply_key(KeyCode::Esc);
    let _ = a.apply_key(KeyCode::Esc);
    assert_eq!(
        a.input, "only turn",
        "the first user turn reloaded for editing"
    );
    assert!(a.full_transcript.is_empty(), "the transcript is now empty");

    // The on-disk chat is gone → a relaunch cannot restore the un-rewound convo.
    assert!(
        !path.exists(),
        "a first-turn rewind that empties the transcript must delete the stale on-disk chat"
    );
}

#[test]
fn double_esc_rewind_is_a_noop_without_a_prior_user_message() {
    let mut a = fresh_app(Some("offline"));
    // No user turn has been spoken yet → there is nothing to rewind, so the
    // idle double-Esc falls through to the existing quit-confirm path.
    assert!(a.last_user_msg_index().is_none());
    let r1 = a.apply_key(KeyCode::Esc);
    assert_eq!(r1, Action::None);
    assert!(!a.pending_rewind, "no user turn → the rewind never arms");
    assert!(
        a.pending_quit_confirm,
        "falls through to quit-confirm instead"
    );
    assert!(a.input.is_empty(), "input stays empty — nothing reloaded");
}

#[test]
fn esc_rewind_never_fires_mid_run() {
    let mut a = fresh_app(Some("offline"));
    a.push(ChatRole::You, "build me an app");
    let len_before = a.history.len();
    // A brain-driven turn is streaming — Esc must INTERRUPT it (double-press),
    // never rewind the transcript out from under a live run.
    a.agentic_in_flight = true;
    let r1 = a.apply_key(KeyCode::Esc);
    assert_eq!(r1, Action::None);
    assert!(a.interrupt_armed(), "first Esc arms the interrupt mid-run");
    assert!(!a.pending_rewind, "mid-run Esc never arms the rewind");
    let r2 = a.apply_key(KeyCode::Esc);
    assert_eq!(r2, Action::Cancel, "second Esc interrupts the run");
    assert!(a.input.is_empty(), "rewind did not fire — input untouched");
    assert_eq!(a.history.len(), len_before, "transcript untouched mid-run");
    assert!(!a.should_quit);
}

#[test]
fn transcript_plaintext_handoff_renders_history_and_skips_empties() {
    // The scrollback handoff: a clean-exit dump of the conversation. Each turn
    // is its speaker tag + body; whitespace-only turns are dropped.
    let mut a = fresh_app(Some("offline"));
    a.history.clear();
    a.push(ChatRole::You, "build me an app");
    a.push(ChatRole::Host, "sure, here is the plan");
    a.push(ChatRole::System, "   "); // whitespace-only → skipped
    a.push(ChatRole::UmaDev, "done");
    let dump = a.transcript_plaintext();
    assert!(
        dump.contains("build me an app"),
        "user turn present: {dump}"
    );
    assert!(
        dump.contains("sure, here is the plan"),
        "host turn present (untagged): {dump}"
    );
    assert!(
        dump.contains("UmaDev: done"),
        "umadev turn is tagged: {dump}"
    );
    assert!(
        !dump.lines().any(|l| l.trim() == "·"),
        "the whitespace-only system turn is skipped: {dump}"
    );
    // An empty history hands off nothing (the caller prints nothing).
    a.history.clear();
    assert!(a.transcript_plaintext().is_empty());
}

#[test]
fn transcript_plaintext_keeps_multiline_bodies_intact() {
    // A multi-line body keeps its own line breaks; only the first line is
    // tagged so the block reads cleanly in scrollback.
    let mut a = fresh_app(Some("offline"));
    a.history.clear();
    a.push(ChatRole::You, "line one\nline two\nline three");
    let dump = a.transcript_plaintext();
    assert!(dump.contains("line one"));
    assert!(dump.contains("line two"));
    assert!(dump.contains("line three"));
    assert!(dump.ends_with('\n'), "the dump ends on a fresh line");
}

#[test]
fn exit_footer_text_is_compact_for_a_stateful_session() {
    // The DEFAULT clean-exit sign-off: a short footer, NOT the whole transcript.
    let mut app = fresh_app(Some("offline"));
    app.history.clear();
    app.push(ChatRole::You, "build me a landing page");
    app.push(ChatRole::Host, "done — here is the plan");
    let footer = app
        .exit_footer_text()
        .expect("a session with a conversation gets a footer");
    // At most two short lines (one here — no artifacts in a fresh workspace).
    assert!(
        footer.lines().count() <= 2,
        "footer stays compact: {footer:?}"
    );
    // The resume hint names the command to run again.
    assert!(
        footer.to_lowercase().contains("umadev"),
        "footer carries the resume hint: {footer:?}"
    );
    // It is emphatically NOT the transcript dump.
    assert!(
        !footer.contains("build me a landing page"),
        "footer is a sign-off, not the conversation: {footer:?}"
    );
}

#[test]
fn exit_footer_text_points_at_artifacts_when_present() {
    let mut app = fresh_app(Some("offline"));
    app.history.clear();
    app.push(ChatRole::You, "ship it");
    // An `output/` deliverable adds the footer's optional second line.
    let output = app.project_root.join("output");
    std::fs::create_dir_all(&output).expect("create output dir");
    std::fs::write(output.join("demo-prd.md"), "# PRD").expect("write artifact");
    let footer = app
        .exit_footer_text()
        .expect("a stateful session gets a footer");
    assert_eq!(
        footer.lines().count(),
        2,
        "two lines once artifacts exist: {footer:?}"
    );
    assert!(
        footer.contains("output/"),
        "the second line points at the artifacts: {footer:?}"
    );
}

#[test]
fn exit_footer_text_is_none_for_an_empty_session() {
    // An empty conversation has nothing to resume → fully clean exit, no footer.
    let mut app = fresh_app(Some("offline"));
    app.history.clear();
    assert!(
        app.exit_footer_text().is_none(),
        "an empty session prints nothing at all"
    );
}

// ── wheel / edge extends a drag-selection past the viewport ───────────
//
// Shared geometry: 10 content rows "row0".."row9", a 4-row viewport at the
// top-left, `hidden_above` (max_scroll) = 6. Pinned to the bottom
// (`transcript_scroll` = 0) the renderer would publish `first_visible` = 6,
// so rows 6,7,8,9 are on screen and rows 0..5 are hidden ABOVE.
fn seed_transcript_geometry(a: &App) {
    *a.transcript_rows.borrow_mut() = (0..10).map(|i| format!("row{i}")).collect();
    a.transcript_gutters.borrow_mut().clear();
    a.transcript_area.set((0, 0, 10, 4));
    a.transcript_max_scroll.set(6);
    a.set_transcript_scroll(0);
    a.transcript_first_visible.set(6);
}

#[test]
fn transcript_copy_sets_toast_without_growing_history() {
    let mut a = fresh_app(Some("offline"));
    a.lang = umadev_i18n::Lang::En;
    seed_transcript_geometry(&a);
    let before = (
        a.history.len(),
        a.conversation.len(),
        a.full_transcript.len(),
    );

    a.selection_begin(0, 0);
    a.selection_extend(4, 0);
    assert_eq!(a.selection_finish_copy().as_deref(), Some("row6"));

    assert_eq!(
        a.copy_toast_text(),
        Some(umadev_i18n::tf(a.lang, "tui.copied", &["4"]).as_str())
    );
    assert_eq!(
        (
            a.history.len(),
            a.conversation.len(),
            a.full_transcript.len()
        ),
        before,
        "copy feedback must not enter chat or persisted transcript state"
    );
}

#[test]
fn wheel_during_drag_extends_selection_past_viewport() {
    let mut a = fresh_app(Some("offline"));
    seed_transcript_geometry(&a);
    // Press at the end of the bottom-most visible row (screen row 3 → content
    // row 9): the anchor that the wheel must keep fixed.
    a.selection_begin(9, 3);
    assert!(
        a.selection_dragging,
        "a down inside the transcript opens a drag"
    );
    assert_eq!(a.selection.unwrap().anchor, (9, 4));
    // Drag up to the TOP visible row (screen row 0 → content row 6). The
    // selection now spans only what is on screen: rows 6..9.
    a.selection_extend(0, 0);
    assert_eq!(a.selection.unwrap().cursor, (6, 0));
    assert_eq!(
        crate::selection::extract(&a.transcript_rows.borrow(), &a.selection.unwrap()),
        "row6\nrow7\nrow8\nrow9",
        "before the wheel the span is just the visible viewport",
    );
    // Wheel UP three rows WHILE the drag is live: the transcript scrolls AND
    // the selection end re-resolves at the last drag position (screen row 0),
    // which now sits over content row 3 — so the span GROWS to rows 3..9,
    // reaching content that was hidden above the old viewport.
    assert!(a.mouse_wheel_select(true, 3));
    assert_eq!(
        a.transcript_scroll(),
        3,
        "the wheel still scrolls the transcript"
    );
    assert_eq!(
        a.selection.unwrap().cursor,
        (3, 0),
        "end grew to the revealed row"
    );
    assert_eq!(
        a.selection.unwrap().anchor,
        (9, 4),
        "the anchor stays pinned"
    );
    assert_eq!(
        crate::selection::extract(&a.transcript_rows.borrow(), &a.selection.unwrap()),
        "row3\nrow4\nrow5\nrow6\nrow7\nrow8\nrow9",
        "extract returns the now-larger, beyond-the-viewport span",
    );
}

#[test]
fn wheel_without_active_drag_only_scrolls() {
    let mut a = fresh_app(Some("offline"));
    seed_transcript_geometry(&a);
    // Make a real selection then release the button (mouse-up): the span
    // stays highlighted but the drag is over.
    a.selection_begin(9, 3); // anchor (9,4)
    a.selection_extend(0, 0); // cursor (6,0)
    let copied = a.selection_finish_copy();
    assert_eq!(copied.as_deref(), Some("row6\nrow7\nrow8\nrow9"));
    assert!(!a.selection_dragging, "mouse-up ends the drag");
    let before = a.selection.unwrap();
    // A wheel notch now must ONLY scroll — the highlighted span is frozen.
    assert!(a.mouse_wheel_select(true, 3));
    assert_eq!(a.transcript_scroll(), 3, "the wheel scrolls as usual");
    assert_eq!(
        a.selection.unwrap(),
        before,
        "no active drag → the selection is left untouched",
    );
}

#[test]
fn drag_outside_transcript_surfaces_copy_hint_once() {
    let mut a = fresh_app(Some("offline"));
    seed_transcript_geometry(&a);
    let before = a.history.len();
    // A drag whose mouse-down landed OUTSIDE the transcript (the input box)
    // never opened an in-app selection.
    assert!(a.selection.is_none());
    a.hint_native_copy_once();
    assert!(
        a.native_copy_hint_shown,
        "the first outside-drag latches the hint"
    );
    assert_eq!(a.history.len(), before + 1, "the copy hint is posted once");
    // A SECOND outside-drag must NOT nag again.
    a.hint_native_copy_once();
    assert_eq!(a.history.len(), before + 1, "the hint never repeats");
}

#[test]
fn copy_hint_suppressed_during_a_real_transcript_selection() {
    let mut a = fresh_app(Some("offline"));
    seed_transcript_geometry(&a);
    // A drag that began INSIDE the transcript opened a selection — that path
    // copies via the in-app layer, so the native-selection hint stays silent.
    a.selection_begin(9, 3);
    assert!(a.selection.is_some());
    let before = a.history.len();
    a.hint_native_copy_once();
    assert_eq!(
        a.history.len(),
        before,
        "no hint while a real selection is live"
    );
    assert!(!a.native_copy_hint_shown, "and it does not latch");
}

// ── in-app drag-select+copy INSIDE the input composer box ─────────────
//
// The renderer publishes the input geometry every frame; a test seeds it
// directly (the same seam `ui::render_prompt` writes) then drives the same
// begin/extend/finish methods the event loop calls. A uniform 3-cell mode
// prefix (`>_ `) is the gutter, so screen col N maps to logical col N-3.

/// Seed the published input-box geometry for `input` at `text_cols` text
/// columns, wrapped exactly as the renderer would. Prefix gutter = 3, pinned
/// to the top (no scroll).
fn seed_input_geometry(a: &App, input: &str, text_cols: u16) {
    a.input_text_cols.set(text_cols);
    let rows = crate::ui::wrap_input_rows(input, text_cols);
    let visible = u16::try_from(rows.len().min(6)).unwrap_or(1);
    *a.input_rows.borrow_mut() = rows;
    a.input_gutter.set(3);
    a.input_scroll.set(0);
    // width = gutter + text columns; height = the visible (capped) row count.
    a.input_area.set((0, 0, 3 + text_cols, visible));
}

#[test]
fn dragging_over_input_box_selects_and_copies_the_substring() {
    let mut a = fresh_app(Some("offline"));
    a.lang = umadev_i18n::Lang::En;
    a.input = "hello world".to_string();
    seed_input_geometry(&a, "hello world", 40);
    let before = (
        a.history.len(),
        a.conversation.len(),
        a.full_transcript.len(),
    );
    // Down on the first content cell (screen col 3 = gutter) → logical col 0.
    assert!(
        a.input_selection_begin(3, 0),
        "a down inside the input box begins an input selection"
    );
    assert!(a.input_selection_dragging);
    assert_eq!(a.input_selection.unwrap().anchor, (0, 0));
    // Drag to screen col 8 (gutter 3 + 5) → logical col 5, the end of "hello".
    a.input_selection_extend(8, 0);
    assert_eq!(a.input_selection.unwrap().cursor, (0, 5));
    // Mouse-up copies the dragged substring.
    let copied = a.input_selection_finish_copy();
    assert_eq!(copied.as_deref(), Some("hello"));
    assert!(!a.input_selection_dragging, "mouse-up ends the input drag");
    assert_eq!(
        a.copy_toast_text(),
        Some(umadev_i18n::tf(a.lang, "tui.copied", &["5"]).as_str())
    );
    assert_eq!(
        (
            a.history.len(),
            a.conversation.len(),
            a.full_transcript.len()
        ),
        before,
        "input copy feedback must not enter chat or persisted transcript state"
    );
}

#[test]
fn input_drag_preserves_a_hard_newline_but_not_a_soft_wrap() {
    // A hard `Ctrl+J` newline is a real char in the buffer → copied. A
    // soft-wrap boundary is NOT a char → the wrapped line copies unbroken.
    let mut a = fresh_app(Some("offline"));
    // Hard newline: "abc\ndef" wraps into two rows at a wide width.
    a.input = "abc\ndef".to_string();
    seed_input_geometry(&a, "abc\ndef", 40);
    a.input_selection_begin(3, 0); // logical (0,0)
    a.input_selection_extend(6, 1); // row 1, logical col 3 (end of "def")
    assert_eq!(
        a.input_selection_finish_copy().as_deref(),
        Some("abc\ndef"),
        "the hard newline survives the copy"
    );
    // Soft wrap: "abcdefgh" folded at 4 columns → ["abcd","efgh"], no newline.
    let mut b = fresh_app(Some("offline"));
    b.input = "abcdefgh".to_string();
    seed_input_geometry(&b, "abcdefgh", 4);
    b.input_selection_begin(3, 0); // logical (0,0)
    b.input_selection_extend(6, 1); // row 1, logical col 3 → char index 7
    assert_eq!(
        b.input_selection_finish_copy().as_deref(),
        Some("abcdefg"),
        "a soft-wrapped span copies as one line — no spurious newline"
    );
}

#[test]
fn input_copy_normalizes_crlf_and_counts_wide_glyphs_as_characters() {
    let mut a = fresh_app(Some("offline"));
    a.lang = umadev_i18n::Lang::ZhCn;
    a.handle_paste("甲\r\n乙");
    assert_eq!(
        a.input, "甲\n乙",
        "ConPTY/Windows CRLF is normalized at paste"
    );
    let input = a.input.clone();
    seed_input_geometry(&a, &input, 40);
    a.input_selection_begin(3, 0); // first content cell
    a.input_selection_extend(5, 1); // after the double-width `乙`

    assert_eq!(
        a.input_selection_finish_copy().as_deref(),
        Some("甲\n乙"),
        "clipboard text uses one logical LF and never leaks a carriage return"
    );
    assert_eq!(
        a.copy_toast_text(),
        Some(umadev_i18n::tf(a.lang, "tui.copied", &["3"]).as_str()),
        "two wide glyphs plus one newline are three characters, not UTF-8 bytes or cells"
    );
}

#[test]
fn input_selection_and_transcript_selection_do_not_coexist() {
    let mut a = fresh_app(Some("offline"));
    seed_transcript_geometry(&a);
    a.input = "typed text".to_string();
    seed_input_geometry(&a, "typed text", 40);
    // Begin a transcript selection, then a down inside the input box: the
    // transcript highlight is retired so the two layers never collide.
    a.selection_begin(9, 3);
    assert!(a.selection.is_some());
    assert!(a.input_selection_begin(3, 0));
    assert!(
        a.selection.is_none(),
        "the input down cleared the transcript span"
    );
    assert!(a.input_selection.is_some());
    // And the reverse: a transcript down clears the input selection.
    a.selection_begin(0, 0);
    assert!(
        a.input_selection.is_none(),
        "the transcript down cleared the input span"
    );
}

#[test]
fn a_down_outside_the_input_box_falls_through_to_the_transcript() {
    let mut a = fresh_app(Some("offline"));
    // Input box occupies rows 10..=10; the transcript occupies rows 0..4.
    seed_transcript_geometry(&a); // area (0,0,10,4)
    a.input = "x".to_string();
    a.input_text_cols.set(40);
    *a.input_rows.borrow_mut() = vec!["x".to_string()];
    a.input_gutter.set(3);
    a.input_scroll.set(0);
    a.input_area.set((0, 10, 43, 1)); // far below the transcript
                                      // A down at a transcript cell is NOT inside the input box.
    assert!(
        !a.input_selection_begin(2, 1),
        "point is outside the input box"
    );
}

#[test]
fn typing_after_an_input_selection_clears_the_stale_highlight() {
    let mut a = fresh_app(Some("offline"));
    a.input = "hello world".to_string();
    a.input_cursor = a.input_len();
    seed_input_geometry(&a, "hello world", 40);
    a.input_selection_begin(3, 0);
    a.input_selection_extend(8, 0);
    let _ = a.input_selection_finish_copy();
    assert!(
        a.input_selection.is_some(),
        "the copied span stays highlighted"
    );
    // A keystroke that EDITS the buffer retires the now-stale highlight…
    let _ = a.apply_key(KeyCode::Char('!'));
    assert!(
        a.input_selection.is_none(),
        "an edit invalidates the cached selection coords"
    );
}

#[path = "tests/tail_interaction_tests.rs"]
mod tail_interaction_tests;

// ---------------------------------------------------------------------------
// Frozen plan rehydration after a TRANSIENT (rate-limit) abort.
// ---------------------------------------------------------------------------

/// Persist a 3-step director plan under `root` — one `done`, one `active` (the
/// step that was mid-flight), one `pending` — so a transient-abort rehydrate has
/// real `.umadev/plan.json` to read and we can assert each status maps correctly.
fn save_three_step_plan(root: &std::path::Path) {
    use umadev_agent::critics::Seat;
    use umadev_agent::plan_state::{
        AcceptanceSpec, Plan, PlanStep, StepFiles, StepKind, StepStatus,
    };
    let step = |id: &str, seat: Seat, status: StepStatus| PlanStep {
        id: id.to_string(),
        title: format!("{id} title"),
        seat,
        kind: StepKind::Build,
        depends_on: Vec::new(),
        acceptance: AcceptanceSpec::SourcePresent,
        evidence: Vec::new(),
        files: StepFiles::default(),
        status,
    };
    let plan = Plan {
        steps: vec![
            step("scaffold", Seat::Architect, StepStatus::Done),
            step("api", Seat::BackendEngineer, StepStatus::Active),
            step("ui", Seat::FrontendEngineer, StepStatus::Pending),
        ],
        risks: Vec::new(),
        open_questions: Vec::new(),
    };
    umadev_agent::plan_state::save(&plan, root).expect("persist test plan");
}

/// The rendered status of the plan row with `id` (helper for the assertions).
fn row_status(app: &App, id: &str) -> Option<String> {
    app.plan_steps
        .iter()
        .find(|s| s.id == id)
        .map(|s| s.status.clone())
}

#[test]
fn transient_abort_rehydrates_the_plan_panel_frozen_not_live() {
    let (mut app, tmp) = temp_app();
    save_three_step_plan(tmp.path());
    // Seed a stale LIVE panel state (as if a run had been ticking) so we prove the
    // abort clears it and then repaints from disk, rather than leaving old rows.
    app.plan_steps = vec![PlanStepRow {
        id: "stale".to_string(),
        title: "stale live row".to_string(),
        status: "active".to_string(),
        seat: "architect".to_string(),
    }];

    // A rate-limit abort routed through the real event path (ABORT_SENTINEL note).
    let reason = "API Error: Request rejected (429) · exceeded the 5-hour usage quota";
    app.apply_engine(EngineEvent::Note(format!(
        "{}{reason}",
        crate::ABORT_SENTINEL
    )));

    // The panel came back from disk, frozen.
    assert!(app.plan_frozen, "a transient abort marks the panel frozen");
    assert_eq!(
        app.plan_steps.len(),
        3,
        "the saved 3-step plan is rehydrated"
    );
    assert!(
        app.plan_steps.iter().all(|s| s.id != "stale"),
        "the stale live row is gone — the panel was repainted from the saved plan"
    );
    // No live spinner: the step that was ACTIVE at the abort shows PAUSED, and no
    // row is left `active` (the "· running" suffix / working roster key off it).
    assert_eq!(row_status(&app, "api").as_deref(), Some("paused"));
    assert!(
        app.plan_steps.iter().all(|s| s.status != "active"),
        "no rehydrated row is live-active — the panel is frozen"
    );
    // Persisted done/pending statuses are preserved verbatim.
    assert_eq!(row_status(&app, "scaffold").as_deref(), Some("done"));
    assert_eq!(row_status(&app, "ui").as_deref(), Some("pending"));

    // The discoverability hint is decided by the engine-side helper on exactly the
    // same conditions (it is emitted in `run_director_loop`'s Failed arm).
    assert_eq!(
        umadev_agent::transient_resume_hint(reason, tmp.path()).as_deref(),
        Some(umadev_i18n::tl("run.transient_resume_hint")),
        "the transient abort surfaces the /continue discoverability hint"
    );
}

#[test]
fn a_fresh_plan_post_clears_the_frozen_panel_and_shows_the_new_plan() {
    let (mut app, tmp) = temp_app();
    save_three_step_plan(tmp.path());
    let reason = "429 too many requests";
    app.apply_engine(EngineEvent::Note(format!(
        "{}{reason}",
        crate::ABORT_SENTINEL
    )));
    assert!(
        app.plan_frozen,
        "precondition: the panel is frozen after abort"
    );

    // A genuine new build posts a brand-new plan — the frozen rows must be gone,
    // the frozen flag cleared, and only the new plan's steps shown (no stale carry).
    app.apply_engine(EngineEvent::PlanPosted {
        steps: vec!["newstep · fresh work (backend-engineer)".to_string()],
        statuses: vec!["pending".to_string()],
        done: 0,
        total: 1,
    });
    assert!(!app.plan_frozen, "a fresh plan post clears the frozen flag");
    assert_eq!(app.plan_steps.len(), 1, "only the new plan's steps remain");
    assert_eq!(app.plan_steps[0].id, "newstep");
    assert!(
        app.plan_steps
            .iter()
            .all(|s| s.id != "api" && s.id != "scaffold"),
        "no frozen rows from the interrupted run bleed into the fresh plan"
    );
}

#[test]
fn a_hard_or_planless_abort_does_not_repopulate_the_panel() {
    // Hard failure (auth) even WITH a resumable plan on disk → no false panel.
    let (mut app, tmp) = temp_app();
    save_three_step_plan(tmp.path());
    app.apply_engine(EngineEvent::Note(format!(
        "{}Error 401 Unauthorized: invalid api key",
        crate::ABORT_SENTINEL
    )));
    assert!(
        app.plan_steps.is_empty() && !app.plan_frozen,
        "a hard auth abort leaves the panel cleared, never a frozen false panel"
    );

    // Transient reason but NO plan on disk → nothing to rehydrate.
    let (mut app2, _tmp2) = temp_app();
    app2.apply_engine(EngineEvent::Note(format!(
        "{}429 too many requests",
        crate::ABORT_SENTINEL
    )));
    assert!(
        app2.plan_steps.is_empty() && !app2.plan_frozen,
        "a transient abort with no saved plan surfaces no panel"
    );
}

#[test]
fn frozen_step_status_maps_active_to_paused_only() {
    use super::frozen_plan::frozen_step_status;
    use umadev_agent::plan_state::StepStatus;
    assert_eq!(frozen_step_status(StepStatus::Active), "paused");
    assert_eq!(frozen_step_status(StepStatus::Done), "done");
    assert_eq!(frozen_step_status(StepStatus::Pending), "pending");
    assert_eq!(frozen_step_status(StepStatus::Blocked), "blocked");
}

// ── Audit fixes ────────────────────────────────────────────────────────────

#[test]
fn paste_of_a_nonexistent_image_path_inserts_as_text() {
    // A single line that merely ENDS in an image suffix but is NOT a real file —
    // prose, or a URL — must paste as TEXT. The old image branch attach-failed on
    // the non-file then `return`ed without inserting, silently DISCARDING the text.
    let mut app = fresh_app(None);
    app.handle_paste("see the dashboard.png");
    assert_eq!(app.input, "see the dashboard.png");
    assert!(
        app.attachments.is_empty(),
        "no attachment for a non-file path"
    );

    let mut app = fresh_app(None);
    app.handle_paste("https://example.com/pic.png");
    assert_eq!(app.input, "https://example.com/pic.png");
    assert!(app.attachments.is_empty());
}

#[test]
fn ambiguous_bare_slash_prefix_does_not_autorun() {
    let mut app = fresh_app(None);
    // `/co` matches several verbs (codex / config / constitution / …). On the
    // DEFAULT top match, Enter must NOT auto-run one — that could silently switch
    // the backend (`/codex`). No completion is offered until the user disambiguates.
    app.input = "/co".to_string();
    app.palette_selected = 0;
    assert!(
        app.palette_matches().len() > 1,
        "fixture must be ambiguous for this test to mean anything"
    );
    assert_eq!(app.palette_enter_completion(), None);

    // Explicitly highlighting one with ↑/↓ (selection off the default) opts in.
    app.palette_selected = 1;
    assert!(app.palette_enter_completion().is_some());
}

#[test]
fn yank_pop_respects_input_cap() {
    // Alt+Y's raw `replace_range` bypasses the cap that `insert_str_at_cursor`
    // enforces; cycling to a LONGER older ring entry must be bounded so the box
    // can't overflow INPUT_CAP.
    let mut app = fresh_app(None);
    app.input = "x".repeat(INPUT_CAP - 10);
    app.input_cursor = 3;
    app.yank_span = Some((0, 3));
    app.yank_ring_idx = 0;
    app.kill_ring = std::collections::VecDeque::from(vec![
        "abc".to_string(),
        "y".repeat(INPUT_CAP), // a much LONGER older entry
    ]);
    app.yank_pop();
    assert!(
        app.input_len() <= INPUT_CAP,
        "yank_pop overflowed the cap: {}",
        app.input_len()
    );
}

#[test]
fn questions_unknown_arg_shows_usage_instead_of_toggling() {
    let mut app = fresh_app(None);
    let before = app.config.prefers_text_questions();
    let action = app.slash_questions("banana");
    assert!(matches!(action, Action::None));
    assert_eq!(
        app.config.prefers_text_questions(),
        before,
        "an unknown arg must not flip the preference"
    );
    let last = app
        .history
        .back()
        .map(|m| m.body().to_string())
        .unwrap_or_default();
    assert!(last.contains("/questions"), "usage line shown: {last}");

    // An EMPTY arg still TOGGLES (unchanged behaviour).
    let mut app = fresh_app(None);
    let before = app.config.prefers_text_questions();
    app.slash_questions("");
    assert_eq!(app.config.prefers_text_questions(), !before);
}

#[test]
fn config_status_design_overlays_are_localized() {
    let mut app = fresh_app(None);
    app.lang = umadev_i18n::Lang::En;
    app.open_config_overlay();
    let body = app.overlay.take().unwrap().lines.join("\n");
    assert!(body.contains("Configuration"), "en config heading: {body}");

    // The SAME overlay must render Chinese, not the English literal.
    app.lang = umadev_i18n::Lang::ZhCn;
    app.open_config_overlay();
    let body = app.overlay.take().unwrap().lines.join("\n");
    assert!(body.contains("配置"), "zh config heading: {body}");
    assert!(!body.contains("Configuration"), "no leaked English: {body}");

    app.open_status_overlay();
    let body = app.overlay.take().unwrap().lines.join("\n");
    assert!(body.contains("流水线状态"), "zh status heading: {body}");

    app.slash_design("");
    let body = app.overlay.take().unwrap().lines.join("\n");
    assert!(
        body.contains("设计体系"),
        "zh design picker heading: {body}"
    );
}

#[test]
fn preedit_cleanup_requests_invalidate_not_contamination() {
    // The IME/preedit cleanup is DRIFT: it routes to the erase-free in-place
    // repaint (HealMode::Invalidate) so it can't flash the whole screen on a
    // terminal without DEC 2026 sync output. Lock the one-shot flag machinery +
    // its heal routing (the event loop folds the flag into `invalidate_due`).
    let app = fresh_app(None);
    assert!(!app.take_invalidate_requested());
    app.request_invalidate();
    assert!(
        !app.take_terminal_contaminated(),
        "invalidate must NOT be contamination (that would erase → flash)"
    );
    assert!(app.take_invalidate_requested());
    assert!(!app.take_invalidate_requested(), "one-shot drain");
    // The two flags route to different heals.
    assert_eq!(crate::heal_mode(true, false), crate::HealMode::Invalidate);
    assert_eq!(crate::heal_mode(false, true), crate::HealMode::Erase);
}
