use super::*;

/// Safety-net bound for turn/approval operations that are EXPECTED TO COMPLETE.
///
/// These `timeout(...)` wrappers exist only so a genuinely hung operation cannot
/// wedge the whole suite — the happy path settles in well under a second. On a
/// saturated Windows CI runner the single-thread `#[tokio::test]` timer gets
/// CPU-starved after the ~10-minute compile, so a sub-100ms operation can blow
/// past a tight 5s wall-clock deadline and flake. A real hang never returns and
/// is still caught here (at 60s); 60s ≫ the sub-second happy path, so a passing
/// turn is never affected. Assert-on-timeout tests (which want the timeout to
/// FIRE) deliberately keep their own short bounds and do NOT use this const.
const TURN_HANG_GUARD: Duration = Duration::from_secs(60);

async fn wait_for_pending_approval(holder: &ApprovalHolder) {
    tokio::time::timeout(TURN_HANG_GUARD, async {
        loop {
            if holder.lock().unwrap().is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the guarded consequential action must PAUSE");
}

/// Fix ⑤ (HEADLESS never blocks): the SAME `AskUserQuestion` on a NON-interactive
/// turn must NOT park — it keeps today's observe-stash-and-continue behaviour and
/// runs through to `TurnDone`. A run with no user to answer can never wedge.
#[tokio::test]
async fn headless_askuserquestion_does_not_park_auto_continues() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sink, mut _engine_rx) = ChannelSink::new();
    let sink = Arc::new(sink);
    let (route_tx, mut route_rx) = tokio::sync::mpsc::unbounded_channel();
    let ask = umadev_runtime::SessionEvent::ToolCall {
        name: "AskUserQuestion".into(),
        input: serde_json::json!({"questions": [{
            "header": "Auth", "question": "Which auth method?",
            "options": [{"label": "Email"}]
        }]}),
    };
    let (fake, _sent, ended) = FakeChatSession::new(vec![vec![
        ask,
        umadev_runtime::SessionEvent::TurnDone {
            status: umadev_runtime::TurnStatus::Completed,
            usage: None,
        },
    ]]);
    let holder: ChatSessionHolder = ChatSessionHolder::from_mutex(tokio::sync::Mutex::new(Some(
        ResidentChat::Primed(Box::new(fake)),
    )));

    // HEADLESS: interactive = false via struct-update.
    let turn = ChatSessionTurn {
        interactive: false,
        ..chat_turn(
            "set up auth",
            holder.clone(),
            sink.clone(),
            route_tx.clone(),
            tmp.path().to_path_buf(),
        )
    };
    tokio::time::timeout(TURN_HANG_GUARD, drive_chat_session_turn(turn))
        .await
        .expect("a headless question turn must auto-continue, never block");

    assert!(
        !ended.load(std::sync::atomic::Ordering::SeqCst),
        "headless must NOT interrupt/park — it observes + continues to TurnDone"
    );
    assert!(
        matches!(route_rx.try_recv(), Ok(RouteDecision::AgenticDone { .. })),
        "the headless turn ran through to its own TurnDone"
    );
}

/// Fix ③ (INTERACTIVE): a Guarded consequential action (a shell command the floor
/// would otherwise auto-allow) PAUSES and asks the user. On approval the base is
/// answered `Allow` and the class is remembered — so the SAME action on a later turn
/// is auto-allowed with NO second pause (the ledger suppresses the re-ask).
#[tokio::test]
async fn guarded_interactive_pauses_then_ledger_suppresses_reask() {
    let tmp = tempfile::TempDir::new().unwrap();
    // This test isolates approval/ledger behavior. A routed Build now always
    // enters QC even without a Write tool; keep that QC clean so it does not
    // legitimately consume the second scripted approval as a repair turn.
    std::fs::write(tmp.path().join("app.ts"), "export const x = 1;\n").unwrap();
    let (sink, mut _engine_rx) = ChannelSink::new();
    let sink = Arc::new(sink);
    let (route_tx, mut route_rx) = tokio::sync::mpsc::unbounded_channel();
    let responded = Arc::new(std::sync::Mutex::new(Vec::new()));
    let approve = || umadev_runtime::SessionEvent::NeedApproval {
        req_id: "r1".into(),
        action: "npm run build".into(), // a local shell → consequential, not a read
        target: String::new(),
    };
    let done = || umadev_runtime::SessionEvent::TurnDone {
        status: umadev_runtime::TurnStatus::Completed,
        usage: None,
    };
    let (fake, _sent, _ended) =
        FakeChatSession::new(vec![vec![approve(), done()], vec![approve(), done()]]);
    let fake = fake.with_responses(responded.clone());
    let holder: ChatSessionHolder = ChatSessionHolder::from_mutex(tokio::sync::Mutex::new(Some(
        ResidentChat::Primed(Box::new(fake)),
    )));
    let approval_holder: ApprovalHolder = Arc::new(std::sync::Mutex::new(None));

    // Turn 1: PAUSES. Drive it as a task; the event loop's role (routing the user's
    // decision) is played by the test: poll for the pause, then approve.
    let t1 = tokio::spawn(drive_chat_session_turn(ChatSessionTurn {
        approval_holder: approval_holder.clone(),
        ..chat_turn(
            "build it",
            holder.clone(),
            sink.clone(),
            route_tx.clone(),
            tmp.path().to_path_buf(),
        )
    }));
    // Wait for the drain to register the pause, then answer Allow.
    wait_for_pending_approval(&approval_holder).await;
    let p = approval_holder.lock().unwrap().take().unwrap();
    p.reply_tx.send(ApprovalReply::Allow).unwrap();
    tokio::time::timeout(TURN_HANG_GUARD, t1)
        .await
        .expect("turn 1 must resume after approval")
        .unwrap();
    assert_eq!(
        *responded.lock().unwrap(),
        vec![umadev_runtime::ApprovalDecision::Allow],
        "the approved action is answered Allow to the base"
    );
    assert!(
        umadev_agent::TrustLedger::load(tmp.path()).remembers_rooted(
            "npm run build",
            "",
            tmp.path()
        ),
        "the approved class is remembered for this project"
    );
    assert!(matches!(
        route_rx.try_recv(),
        Ok(RouteDecision::AgenticDone { .. })
    ));

    // Turn 2: the SAME action must NOT pause (ledger suppresses). If it blocked, this
    // timeout would fire — no one is injecting a decision this time.
    tokio::time::timeout(
        TURN_HANG_GUARD,
        drive_chat_session_turn(ChatSessionTurn {
            approval_holder: approval_holder.clone(),
            ..chat_turn(
                "build again",
                holder.clone(),
                sink.clone(),
                route_tx.clone(),
                tmp.path().to_path_buf(),
            )
        }),
    )
    .await
    .expect("a remembered class must auto-allow with NO second pause");
    assert!(
        approval_holder.lock().unwrap().is_none(),
        "no pause was registered on the remembered-class turn"
    );
    assert_eq!(
        *responded.lock().unwrap(),
        vec![
            umadev_runtime::ApprovalDecision::Allow,
            umadev_runtime::ApprovalDecision::Allow
        ],
        "turn 2 auto-allowed the remembered class"
    );
}

/// Fix ③ (HEADLESS never blocks): the SAME Guarded consequential `NeedApproval` on a
/// NON-interactive turn must NOT pause — it auto-decides on the floor (a reversible
/// local shell is allowed) and runs straight through. A userless guarded run can
/// never wedge waiting on a human.
#[tokio::test]
async fn guarded_headless_needapproval_does_not_pause() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sink, mut _engine_rx) = ChannelSink::new();
    let sink = Arc::new(sink);
    let (route_tx, mut route_rx) = tokio::sync::mpsc::unbounded_channel();
    let responded = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (fake, _sent, _ended) = FakeChatSession::new(vec![vec![
        umadev_runtime::SessionEvent::NeedApproval {
            req_id: "r1".into(),
            action: "npm run build".into(),
            target: String::new(),
        },
        umadev_runtime::SessionEvent::TurnDone {
            status: umadev_runtime::TurnStatus::Completed,
            usage: None,
        },
    ]]);
    let fake = fake.with_responses(responded.clone());
    let holder: ChatSessionHolder = ChatSessionHolder::from_mutex(tokio::sync::Mutex::new(Some(
        ResidentChat::Primed(Box::new(fake)),
    )));
    let approval_holder: ApprovalHolder = Arc::new(std::sync::Mutex::new(None));

    tokio::time::timeout(
        TURN_HANG_GUARD,
        drive_chat_session_turn(ChatSessionTurn {
            interactive: false,
            approval_holder: approval_holder.clone(),
            ..chat_turn(
                "build it",
                holder.clone(),
                sink.clone(),
                route_tx.clone(),
                tmp.path().to_path_buf(),
            )
        }),
    )
    .await
    .expect("a headless guarded turn must auto-decide, never block");

    assert!(
        approval_holder.lock().unwrap().is_none(),
        "headless must NEVER register an approval pause"
    );
    assert_eq!(
        *responded.lock().unwrap(),
        vec![umadev_runtime::ApprovalDecision::Allow],
        "the reversible local shell is auto-allowed on the floor (unchanged headless)"
    );
    assert!(matches!(
        route_rx.try_recv(),
        Ok(RouteDecision::AgenticDone { .. })
    ));
}

/// Fix ③ fail-open: if the pause is abandoned while blocked — Esc / cancel / a dead
/// session drops the reply channel (here: the holder is cleared, as the Cancel arm
/// and `interactive_user_present`-off paths do) — the drain must fail-open to DENY
/// and resume, NEVER hang.
#[tokio::test]
async fn approval_pause_fails_open_to_deny_when_abandoned() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sink, mut _engine_rx) = ChannelSink::new();
    let sink = Arc::new(sink);
    let (route_tx, mut route_rx) = tokio::sync::mpsc::unbounded_channel();
    let responded = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (fake, _sent, _ended) = FakeChatSession::new(vec![vec![
        umadev_runtime::SessionEvent::NeedApproval {
            req_id: "r1".into(),
            action: "npm run build".into(),
            target: String::new(),
        },
        umadev_runtime::SessionEvent::TurnDone {
            status: umadev_runtime::TurnStatus::Completed,
            usage: None,
        },
    ]]);
    let fake = fake.with_responses(responded.clone());
    let holder: ChatSessionHolder = ChatSessionHolder::from_mutex(tokio::sync::Mutex::new(Some(
        ResidentChat::Primed(Box::new(fake)),
    )));
    let approval_holder: ApprovalHolder = Arc::new(std::sync::Mutex::new(None));

    let t = tokio::spawn(drive_chat_session_turn(ChatSessionTurn {
        approval_holder: approval_holder.clone(),
        ..chat_turn(
            "build it",
            holder.clone(),
            sink.clone(),
            route_tx.clone(),
            tmp.path().to_path_buf(),
        )
    }));
    // Wait for the pause, then ABANDON it (drop the sender) — the cancel / dead-session
    // fail-open path.
    wait_for_pending_approval(&approval_holder).await;
    clear_pending_approval(&approval_holder);
    tokio::time::timeout(TURN_HANG_GUARD, t)
        .await
        .expect("abandoning the wait must fail-open, never hang")
        .unwrap();
    assert_eq!(
        *responded.lock().unwrap(),
        vec![umadev_runtime::ApprovalDecision::Deny],
        "an abandoned approval fails open to DENY (the base is never left hanging)"
    );
    assert!(matches!(
        route_rx.try_recv(),
        Ok(RouteDecision::AgenticDone { .. })
    ));
}

/// Fix A: a `TurnStatus::Failed` whose base process ACTUALLY died
/// (`try_exit_status` is `Some`) is a genuine teardown — the session is end()-ed
/// and the holder cleared so the next turn re-opens fresh.
#[cfg(unix)]
#[tokio::test]
async fn chat_failed_turn_on_dead_base_ends_and_clears_holder() {
    use std::os::unix::process::ExitStatusExt;
    let tmp = tempfile::TempDir::new().unwrap();
    let (sink, mut _engine_rx) = ChannelSink::new();
    let sink = Arc::new(sink);
    let (route_tx, mut route_rx) = tokio::sync::mpsc::unbounded_channel();

    // A failed turn AND a base process that exited → not recoverable.
    let (fake, _sent, ended) =
        FakeChatSession::new(vec![vec![umadev_runtime::SessionEvent::TurnDone {
            status: umadev_runtime::TurnStatus::Failed("fatal: base crashed".into()),
            usage: None,
        }]]);
    let fake = fake.with_exit_status(std::process::ExitStatus::from_raw(256)); // exit code 1
    let holder: ChatSessionHolder = ChatSessionHolder::from_mutex(tokio::sync::Mutex::new(Some(
        ResidentChat::Primed(Box::new(fake)),
    )));

    drive_chat_session_turn(chat_turn(
        "hello",
        holder.clone(),
        sink.clone(),
        route_tx.clone(),
        tmp.path().to_path_buf(),
    ))
    .await;

    assert!(
        matches!(route_rx.try_recv(), Ok(RouteDecision::Failed(_))),
        "the failure is surfaced"
    );
    // A genuinely-dead base IS torn down + the holder cleared (fresh re-open next).
    assert!(
        holder.lock().await.is_none(),
        "a dead base must be end()-ed and the holder cleared for a fresh re-open"
    );
    assert!(
        ended.load(std::sync::atomic::Ordering::SeqCst),
        "a dead base's session must be end()-ed"
    );
}

/// Fix A: an idle hang on a base whose process is still alive parks the session
/// (after interrupting the hung turn) instead of tearing it down — same recovery
/// as the turn-failure path, so the next follow-up reuses the bare session.
#[tokio::test]
async fn chat_idle_hang_on_live_base_parks_session() {
    let _env = CHAT_IDLE_ENV_LOCK.lock().await;
    let _idle = EnvRestore::set("UMADEV_IDLE_TIMEOUT_SECS", "1");

    let tmp = tempfile::TempDir::new().unwrap();
    let (sink, mut _engine_rx) = ChannelSink::new();
    let sink = Arc::new(sink);
    let (route_tx, mut route_rx) = tokio::sync::mpsc::unbounded_channel();
    // HangingChatSession stays ALIVE (try_exit_status defaults to None).
    let holder: ChatSessionHolder = ChatSessionHolder::from_mutex(tokio::sync::Mutex::new(Some(
        ResidentChat::Primed(Box::new(HangingChatSession)),
    )));

    drive_chat_session_turn(chat_turn(
        "explain this code",
        holder.clone(),
        sink.clone(),
        route_tx.clone(),
        tmp.path().to_path_buf(),
    ))
    .await;

    // The idle settle is still surfaced as a failure.
    assert!(
        matches!(route_rx.try_recv(), Ok(RouteDecision::Failed(_))),
        "the idle settle is surfaced"
    );
    // The still-alive base is PARKED back for the next turn, not dropped.
    assert!(
        holder.lock().await.is_some(),
        "an idle hang on a still-alive base must PARK the session for the next turn"
    );
}

/// The core latency-fix invariant: two chat turns REUSE the one held session
/// (never re-open / cold-start), and the session is PARKED back after each turn
/// for the next message. A reused session gets the small per-turn authority
/// contract, but no firmware/transcript re-injection (that remains one-time).
#[tokio::test]
async fn chat_reuses_one_resident_session_across_turns() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sink, mut engine_rx) = ChannelSink::new();
    let sink = Arc::new(sink);
    let (route_tx, mut route_rx) = tokio::sync::mpsc::unbounded_channel();
    let reported_usage = umadev_runtime::Usage::exact(1_200, 300);

    // Two scripted turns: each a plain text reply then a clean TurnDone.
    let (fake, sent, ended) = FakeChatSession::new(vec![
        vec![
            umadev_runtime::SessionEvent::TextDelta("hi there".into()),
            umadev_runtime::SessionEvent::TurnDone {
                status: umadev_runtime::TurnStatus::Completed,
                usage: Some(reported_usage),
            },
        ],
        vec![
            umadev_runtime::SessionEvent::TextDelta("still here".into()),
            umadev_runtime::SessionEvent::TurnDone {
                status: umadev_runtime::TurnStatus::Completed,
                usage: None,
            },
        ],
    ]);
    // Pre-load the holder with a PRIMED session → `drive_chat_session_turn` takes
    // it on the reuse path, so `session_for` is NEVER called (no cold start).
    let holder: ChatSessionHolder = ChatSessionHolder::from_mutex(tokio::sync::Mutex::new(Some(
        ResidentChat::Primed(Box::new(fake)),
    )));

    // Turn 1.
    drive_chat_session_turn(chat_turn(
        "你好",
        holder.clone(),
        sink.clone(),
        route_tx.clone(),
        tmp.path().to_path_buf(),
    ))
    .await;
    // The live session was parked back for reuse.
    assert!(
        holder.lock().await.is_some(),
        "session must be parked back after a clean turn"
    );
    // First turn settles as a pure chat (not a build).
    match route_rx.try_recv() {
        Ok(RouteDecision::AgenticDone {
            reply,
            director_build,
            ..
        }) => {
            assert_eq!(reply, "hi there");
            assert!(!director_build, "a pure reply is a chat, never a build");
        }
        other => panic!("expected AgenticDone, got {other:?}"),
    }

    // Turn 2 — the SAME held session is reused (no re-open).
    drive_chat_session_turn(chat_turn(
        "再说一句",
        holder.clone(),
        sink.clone(),
        route_tx.clone(),
        tmp.path().to_path_buf(),
    ))
    .await;
    assert!(holder.lock().await.is_some(), "session parked again");

    // The ONE session saw BOTH user turns with a fresh authority boundary, but
    // no firmware/transcript prefix was re-injected.
    let sent = sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 2, "both turns went to the SAME session");
    assert!(sent[0].contains("## Current-turn authority") && sent[0].contains("你好"));
    assert!(sent[1].contains("## Current-turn authority") && sent[1].contains("再说一句"));
    assert!(sent.iter().all(|directive| {
        directive.contains(
        "Prior conversation, plans, TODOs, project documents, and remembered facts are context only"
    )
    }));
    // It was never ended/interrupted (it lives on across the conversation).
    assert!(
        !ended.load(std::sync::atomic::Ordering::SeqCst),
        "a resident chat session is not closed between turns"
    );

    // No chat intent card was ever emitted (the user removed it) — only worker
    // stream text, no `IntentDecided`.
    let mut saw_intent = false;
    let mut saw_usage = false;
    while let Ok(ev) = engine_rx.try_recv() {
        if matches!(&ev, EngineEvent::IntentDecided { .. }) {
            saw_intent = true;
        }
        if matches!(&ev, EngineEvent::TurnUsage { usage: Some(usage), .. } if *usage == reported_usage)
        {
            saw_usage = true;
        }
    }
    assert!(
        !saw_intent,
        "a pure chat turn emits NO intent card (chat card removed)"
    );
    assert!(
        saw_usage,
        "resident chat forwards terminal usage to the TUI"
    );
}

/// Cross-session base memory (step 2): a host chat turn captures the LIVE base's
/// OWN resumable session id and carries it back on the terminal `AgenticDone`, so
/// the event loop can persist it onto the saved chat (a relaunch then `--resume`s
/// the base's deep context). A base WITHOUT a resumable id (opencode) carries
/// `None` — fail-open to today's fresh-session behavior.
#[tokio::test]
async fn chat_turn_carries_back_the_base_session_id() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sink, _engine_rx) = ChannelSink::new();
    let sink = Arc::new(sink);
    let (route_tx, mut route_rx) = tokio::sync::mpsc::unbounded_channel();

    // A primed session that exposes a resumable id (claude / codex behaviour).
    let (fake, _sent, _ended) = FakeChatSession::new(vec![vec![
        umadev_runtime::SessionEvent::TextDelta("ok".into()),
        umadev_runtime::SessionEvent::TurnDone {
            status: umadev_runtime::TurnStatus::Completed,
            usage: None,
        },
    ]]);
    let holder: ChatSessionHolder = ChatSessionHolder::from_mutex(tokio::sync::Mutex::new(Some(
        ResidentChat::Primed(Box::new(fake.with_id("base-sess-42"))),
    )));
    drive_chat_session_turn(chat_turn(
        "你好",
        holder.clone(),
        sink.clone(),
        route_tx.clone(),
        tmp.path().to_path_buf(),
    ))
    .await;
    match route_rx.try_recv() {
        Ok(RouteDecision::AgenticDone {
            base_session_id, ..
        }) => assert_eq!(
            base_session_id.as_deref(),
            Some("base-sess-42"),
            "the live base session id rides back on the terminal decision"
        ),
        other => panic!("expected AgenticDone, got {other:?}"),
    }

    // A base with NO resumable id (opencode / default) carries `None` — fail-open.
    let (fake2, _s2, _e2) = FakeChatSession::new(vec![vec![
        umadev_runtime::SessionEvent::TextDelta("ok".into()),
        umadev_runtime::SessionEvent::TurnDone {
            status: umadev_runtime::TurnStatus::Completed,
            usage: None,
        },
    ]]);
    let holder2: ChatSessionHolder = ChatSessionHolder::from_mutex(tokio::sync::Mutex::new(Some(
        ResidentChat::Primed(Box::new(fake2)),
    )));
    drive_chat_session_turn(chat_turn(
        "再来",
        holder2.clone(),
        sink.clone(),
        route_tx.clone(),
        tmp.path().to_path_buf(),
    ))
    .await;
    match route_rx.try_recv() {
        Ok(RouteDecision::AgenticDone {
            base_session_id, ..
        }) => assert_eq!(
            base_session_id, None,
            "a base with no resumable id is fail-open (None)"
        ),
        other => panic!("expected AgenticDone, got {other:?}"),
    }
}

/// The API-error surfacing fix: a chat turn whose base reports a `Failed` status
/// (an API error like a 429 rate limit) must SURFACE that error — a
/// `RouteDecision::Failed` carrying the actionable classifier line + the base's
/// raw error text — and must NOT read as a clean "[agentic] 完成" (no
/// `AgenticDone`) nor emit a "本轮无文件变更" note. The screenshot bug, end to end
/// on the chat path.
#[tokio::test]
async fn chat_failed_turn_surfaces_api_error_not_a_false_done() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sink, mut engine_rx) = ChannelSink::new();
    let sink = Arc::new(sink);
    let (route_tx, mut route_rx) = tokio::sync::mpsc::unbounded_channel();

    // The base hits a 429 mid-turn: it ends the turn with a Failed status whose
    // message is the base's OWN error text (exactly what claude's `parse_result`
    // now produces from an `is_error:true` result line).
    let api_err = "API Error: Request rejected (429) · You have exceeded the 5-hour usage quota. It will reset at 2026-06-28 18:59:37.";
    let (fake, _sent, ended) =
        FakeChatSession::new(vec![vec![umadev_runtime::SessionEvent::TurnDone {
            status: umadev_runtime::TurnStatus::Failed(api_err.to_string()),
            usage: None,
        }]]);
    let holder: ChatSessionHolder = ChatSessionHolder::from_mutex(tokio::sync::Mutex::new(Some(
        ResidentChat::Primed(Box::new(fake)),
    )));

    drive_chat_session_turn(chat_turn(
        "现在还有哪些任务没有完成",
        holder.clone(),
        sink.clone(),
        route_tx,
        tmp.path().to_path_buf(),
    ))
    .await;

    // The turn surfaced as a FAILURE (never a false AgenticDone / "完成").
    match route_rx.try_recv() {
        Ok(RouteDecision::Failed(note)) => {
            // The base's RAW error text reaches the user (never swallowed).
            assert!(note.contains("429"), "the raw 429 error is shown: {note}");
            assert!(
                note.contains("usage quota"),
                "the full base error is shown: {note}"
            );
            // The actionable rate-limit classifier line is prepended.
            assert!(
                note.contains(umadev_i18n::tl("base.fail.ratelimit")),
                "the rate-limit diagnosis is prepended: {note}"
            );
        }
        other => panic!("expected RouteDecision::Failed, got {other:?}"),
    }
    // No SECOND decision (a Failed turn is terminal — no false AgenticDone too).
    assert!(
        route_rx.try_recv().is_err(),
        "a failed turn emits exactly one terminal decision"
    );
    // The failure was surfaced, but the base PROCESS is still alive
    // (try_exit_status None), so the session is PARKED back as `Primed` for the
    // next turn (Fix A: a recoverable 429 blip no longer tears the resident
    // session down + forces a re-scan/re-open) — NOT end()-ed.
    assert!(
        !ended.load(std::sync::atomic::Ordering::SeqCst),
        "a recoverable failure on a LIVE base parks the session, it does not end it"
    );
    assert!(
        holder.lock().await.is_some(),
        "the live session is parked back for reuse after a surfaced failure"
    );
    // CRUCIAL: no "本轮无文件变更 / no file changes" Note was emitted — the swallow.
    while let Ok(ev) = engine_rx.try_recv() {
        if let EngineEvent::Note(n) = ev {
            assert!(
                !n.contains("无文件变更") && !n.contains("no file changes"),
                "a failed turn must NOT emit the no-file-changes note: {n}"
            );
        }
    }
}

/// H2: an OBSERVED write IS a real build completion. The reactive engine — not a
/// pre-action verdict — is the governance driver now, so a turn the base answered by
/// writing real code carries `director_build: true` (the full-build completion card /
/// task-Done / session hand-back), exactly what `ReactiveBuild::became_build` documents.
#[tokio::test]
async fn chat_session_reacts_to_first_write_as_build() {
    let tmp = tempfile::TempDir::new().unwrap();
    let target = tmp.path().join("src/main.rs");
    let (sink, mut engine_rx) = ChannelSink::new();
    let sink = Arc::new(sink);
    let (route_tx, mut route_rx) = tokio::sync::mpsc::unbounded_channel();

    let (fake, _sent, _ended) = FakeChatSession::new(vec![vec![
        umadev_runtime::SessionEvent::ToolCall {
            name: "Write".into(),
            input: serde_json::json!({ "file_path": "src/main.rs" }),
        },
        umadev_runtime::SessionEvent::TextDelta("created the file".into()),
        umadev_runtime::SessionEvent::TurnDone {
            status: umadev_runtime::TurnStatus::Completed,
            usage: None,
        },
    ]]);
    // The Write tool lands a real file (the send effect), so the source-present honesty
    // floor sees real code — the observed build is genuine, not a claimed-empty one.
    let fake = fake.with_send_effect(move || {
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, "fn main() {}\n").unwrap();
    });
    let holder: ChatSessionHolder = ChatSessionHolder::from_mutex(tokio::sync::Mutex::new(Some(
        ResidentChat::Primed(Box::new(fake)),
    )));

    drive_chat_session_turn(chat_turn(
        "建个 main",
        holder,
        sink.clone(),
        route_tx,
        tmp.path().to_path_buf(),
    ))
    .await;

    // The observed write is a real build completion (H2).
    match route_rx.try_recv() {
        Ok(RouteDecision::AgenticDone { director_build, .. }) => {
            assert!(
                director_build,
                "an observed write is a real build completion (director_build true)"
            );
        }
        other => panic!("expected AgenticDone, got {other:?}"),
    }
    // The conservative fallback route is surfaced, but terminal truth still
    // distinguishes that route label from actual Director execution.
    let mut saw_route_card = false;
    let mut saw_write = false;
    while let Ok(ev) = engine_rx.try_recv() {
        match ev {
            EngineEvent::IntentDecided { class, .. } if class == "quick_edit" => {
                saw_route_card = true;
            }
            EngineEvent::IntentDecided { class, .. } if class == "build" => {
                saw_route_card = true;
            }
            EngineEvent::WorkerStream {
                event: umadev_runtime::StreamEvent::ToolUse { name, .. },
            } if name == "Write" => saw_write = true,
            _ => {}
        }
    }
    assert!(
        saw_route_card,
        "the first write surfaces its route while terminal truth stays separate"
    );
    assert!(saw_write, "the write tool call streams live");
}

/// An observed write records real build truth: it carries `director_build: true` (H2 —
/// the reactive engine drives completion now), while the flagship team QC stays gated on
/// the reactive-QC switch (unset here → no QC). Build-completion semantics and the QC
/// pass are separate facts.
#[tokio::test]
async fn fallback_write_records_build_truth_without_flagship_qc() {
    let tmp = tempfile::TempDir::new().unwrap();
    // Pre-seed a real, slop-free source file so the source-present honesty floor
    // PASSES and the governance scan is clean — the QC pass runs and settles clean.
    std::fs::write(tmp.path().join("app.ts"), "export const x = 1;").unwrap();
    let (sink, mut engine_rx) = ChannelSink::new();
    let sink = Arc::new(sink);
    let (route_tx, mut route_rx) = tokio::sync::mpsc::unbounded_channel();

    // The base writes a file (flips to a build) then reports done.
    let (fake, _sent, _ended) = FakeChatSession::new(vec![vec![
        umadev_runtime::SessionEvent::ToolCall {
            name: "Write".into(),
            input: serde_json::json!({ "file_path": "app.ts" }),
        },
        umadev_runtime::SessionEvent::TextDelta("built the page".into()),
        umadev_runtime::SessionEvent::TurnDone {
            status: umadev_runtime::TurnStatus::Completed,
            usage: None,
        },
    ]]);
    let holder: ChatSessionHolder = ChatSessionHolder::from_mutex(tokio::sync::Mutex::new(Some(
        ResidentChat::Primed(Box::new(fake)),
    )));

    drive_chat_session_turn(chat_turn(
        "做个落地页",
        holder.clone(),
        sink.clone(),
        route_tx,
        tmp.path().to_path_buf(),
    ))
    .await;

    // The observed write is a real build completion (H2).
    match route_rx.try_recv() {
        Ok(RouteDecision::AgenticDone { director_build, .. }) => {
            assert!(
                director_build,
                "an observed write is a real build completion (director_build true)"
            );
        }
        other => panic!("expected AgenticDone, got {other:?}"),
    }
    // The build completion and the flagship QC are separate facts — with the reactive-QC
    // switch unset, the observed build runs no team review.
    let mut saw_qc = false;
    while let Ok(ev) = engine_rx.try_recv() {
        if let EngineEvent::Note(n) = ev {
            if n.contains("构建完成") || n.contains("honesty + QC") || n.contains("team ·") {
                saw_qc = true;
            }
        }
    }
    assert!(!saw_qc, "fallback writes must not launch flagship QC");
    // The live session is parked back for reuse after the scoped turn.
    assert!(
        holder.lock().await.is_some(),
        "the session is parked back after the scoped fallback turn"
    );
}

/// Plan trust mode is enforced by the session-permission layer, INDEPENDENT of
/// routing. Stage B removed the pre-action verdict, so there is no model class to
/// "cap" — a build-shaped request under Plan simply runs a resident read-only reply
/// and never enters the Director or writes Director workflow state. This proves the
/// Plan guardrail still holds without the barrier.
#[tokio::test]
async fn plan_mode_build_request_stays_a_resident_read_only_reply() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sink, _engine_rx) = ChannelSink::new();
    let sink = Arc::new(sink);
    let (route_tx, mut route_rx) = tokio::sync::mpsc::unbounded_channel();
    // A real Plan-mode base cannot write, so it just replies — no write tool.
    let (fake, sent, _ended) = FakeChatSession::new(vec![vec![
        umadev_runtime::SessionEvent::TextDelta("当前是规划模式，我先说明实现方案。".into()),
        umadev_runtime::SessionEvent::TurnDone {
            status: umadev_runtime::TurnStatus::Completed,
            usage: None,
        },
    ]]);
    // Pin the parked permissions to Plan so the resident is REUSED (identity match).
    let holder: ChatSessionHolder = ChatSessionHolder::from_mutex_with_permissions(
        tokio::sync::Mutex::new(Some(ResidentChat::Primed(Box::new(fake)))),
        umadev_runtime::BasePermissionProfile::Plan,
    );
    let mut turn = chat_turn(
        "构建一个完整应用",
        holder,
        sink,
        route_tx,
        tmp.path().to_path_buf(),
    );
    turn.mode = umadev_agent::TrustMode::Plan;
    turn.permissions = umadev_runtime::BasePermissionProfile::Plan;

    drive_chat_session_turn(turn).await;

    match route_rx.try_recv() {
        Ok(RouteDecision::AgenticDone {
            director_build,
            reply,
            ..
        }) => {
            assert!(!director_build, "Plan mode cannot enter the Director");
            assert!(reply.contains("规划模式"));
        }
        other => panic!("expected a read-only Plan reply, got {other:?}"),
    }
    assert!(
        route_rx.try_recv().is_err(),
        "Plan mode emits no DirectorStarted transition"
    );
    assert_eq!(
        sent.lock().unwrap().len(),
        1,
        "one resident reply, no QC re-drive"
    );
    assert!(
        !tmp.path().join(".umadev/run.lock").exists(),
        "a read-only reply takes no writer lock"
    );
    assert!(
        !tmp.path().join(".umadev/workflow-state.json").exists(),
        "Plan-mode natural language creates no Director workflow state"
    );
}

/// B1 + B2: a build-shaped request with NO explicit `/run` no longer forks to
/// `run_director_loop` before acting. It runs on the resident writer session and is
/// upgraded REACTIVELY the moment the base writes real code — the Build-shaped card,
/// the reactive governance QC (with the flag on), and the governance context all fire
/// off the OBSERVED write, not a pre-action verdict, and NO `DirectorStarted`
/// transition is ever emitted from chat.
#[tokio::test]
async fn build_shaped_chat_reaches_resident_writer_and_upgrades_on_first_write() {
    let _qc = ReactiveQcOverride::force(true);
    let tmp = tempfile::TempDir::new().unwrap();
    let (events, routes) = drive_resident_turn(
        tmp.path(),
        "做一个完整的后台管理系统",
        write_tool_turn("src/admin.tsx"),
        Some("src/admin.tsx"),
    )
    .await;

    assert!(
        !routes
            .iter()
            .any(|r| matches!(r, RouteDecision::DirectorStarted { .. })),
        "an implicit build never dispatches the Director from chat: {routes:?}"
    );
    assert!(
        routes.iter().any(|r| matches!(
            r,
            RouteDecision::AgenticDone {
                director_build: true,
                ..
            }
        )),
        "the observed build is a real build completion (H2), though it never dispatched \
         the Director loop from chat: {routes:?}"
    );
    assert!(
        saw_build_intent_card(&events),
        "the observed write surfaces a Build-shaped card: {events:?}"
    );
    assert!(
        ran_governance_qc(&events),
        "the observed build runs the reactive governance QC: {events:?}"
    );
    assert!(
        tmp.path().join(".umadev/governance-context.json").exists(),
        "the governance context was persisted before the first write"
    );
}

/// B4 — the ONE routing rule kept from the removed barrier: an EXPLICIT read-only
/// request ("只分析，不要修改任何文件") forces a non-mutating, non-isolating turn in EVERY
/// trust mode (Guarded AND Auto), so the base is jailed read-only and cannot write even
/// under Auto. Observed via the read-only park disposition plus the absence of any
/// write-side governance (no writer lock, no persisted governance context).
#[tokio::test]
async fn explicit_read_only_request_stays_read_only_in_guarded_and_auto() {
    for (mode, perms) in [
        (
            umadev_agent::TrustMode::Guarded,
            umadev_runtime::BasePermissionProfile::Guarded,
        ),
        (
            umadev_agent::TrustMode::Auto,
            umadev_runtime::BasePermissionProfile::Auto,
        ),
    ] {
        let tmp = tempfile::TempDir::new().unwrap();
        let (sink, _engine_rx) = ChannelSink::new();
        let sink = Arc::new(sink);
        let (route_tx, mut route_rx) = tokio::sync::mpsc::unbounded_channel();
        // Start from a read-only-primed resident so the read-only verdict is KEPT: a
        // write-capable fake would (in production) be reopened as a real PLAN session, a
        // reopen unit tests deliberately short-circuit.
        let (fake, _sent, _ended) = FakeChatSession::new(vec![vec![
            umadev_runtime::SessionEvent::TextDelta("这是只读分析结论……".into()),
            umadev_runtime::SessionEvent::TurnDone {
                status: umadev_runtime::TurnStatus::Completed,
                usage: None,
            },
        ]]);
        // Pin the parked permissions to this mode so the read-only-primed resident is
        // REUSED (identity match), not treated as stale and reopened.
        let holder: ChatSessionHolder = ChatSessionHolder::from_mutex_with_permissions(
            tokio::sync::Mutex::new(Some(ResidentChat::ReadOnlyPrimed(Box::new(fake)))),
            perms,
        );
        let mut turn = chat_turn(
            "只分析，不要修改任何文件",
            holder.clone(),
            sink,
            route_tx,
            tmp.path().to_path_buf(),
        );
        turn.mode = mode;
        turn.permissions = perms;

        drive_chat_session_turn(turn).await;

        assert!(
            matches!(
                route_rx.try_recv(),
                Ok(RouteDecision::AgenticDone {
                    director_build: false,
                    ..
                })
            ),
            "an explicit read-only turn settles as a resident reply in {mode:?}"
        );
        assert!(
            matches!(*holder.lock().await, Some(ResidentChat::ReadOnlyPrimed(_))),
            "the turn stayed read-only (parked read-only) in {mode:?}"
        );
        assert!(
            !tmp.path().join(".umadev/run.lock").exists(),
            "a read-only turn takes no writer lock in {mode:?}"
        );
        assert!(
            !tmp.path().join(".umadev/governance-context.json").exists(),
            "a read-only turn persists no write-side governance context in {mode:?}"
        );
    }
}

/// The reported bug: a read-only "find the repo" request used to be stranded — a
/// low-confidence fallback verdict jailed the base in plan mode, where it could not use
/// tools and re-planned forever. Under the reactive model the base just ACTS on a
/// write-capable resident session, so it freely uses read-only tools (no plan-mode jail,
/// no ExitPlanMode deadlock) and settles as an ordinary resident reply.
#[tokio::test]
async fn find_the_repo_read_only_request_runs_and_is_not_stranded() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sink, _engine_rx) = ChannelSink::new();
    let sink = Arc::new(sink);
    let (route_tx, mut route_rx) = tokio::sync::mpsc::unbounded_channel();
    // The base inspects with a read-only tool (Grep), then answers — a healthy "find X"
    // turn, NOT a stranded "I can't use tools this turn" dead-end.
    let (fake, sent, _ended) = FakeChatSession::new(vec![vec![
        umadev_runtime::SessionEvent::ToolCall {
            name: "Grep".into(),
            input: serde_json::json!({ "pattern": "fn main" }),
        },
        umadev_runtime::SessionEvent::ToolResult {
            ok: true,
            summary: "src/main.rs".into(),
        },
        umadev_runtime::SessionEvent::TextDelta("源码在 src/main.rs。".into()),
        umadev_runtime::SessionEvent::TurnDone {
            status: umadev_runtime::TurnStatus::Completed,
            usage: None,
        },
    ]]);
    let holder: ChatSessionHolder = ChatSessionHolder::from_mutex(tokio::sync::Mutex::new(Some(
        ResidentChat::Primed(Box::new(fake)),
    )));

    drive_chat_session_turn(chat_turn(
        "找 tm 的源码在哪里",
        holder.clone(),
        sink,
        route_tx,
        tmp.path().to_path_buf(),
    ))
    .await;

    match route_rx.try_recv() {
        Ok(RouteDecision::AgenticDone {
            director_build,
            reply,
            ..
        }) => {
            assert!(!director_build);
            assert_eq!(reply, "源码在 src/main.rs。");
        }
        other => panic!("a read-only inspection must run, not strand, got {other:?}"),
    }
    assert_eq!(
        sent.lock().unwrap().len(),
        1,
        "one resident turn, no reroute"
    );
    assert!(
        !tmp.path().join(".umadev/run.lock").exists(),
        "a read-only inspection takes no writer lock"
    );
}

/// Risk #1 — a build done through `Bash` (a heredoc / generator) emits NO Write/Edit
/// tool event, but a shell `ToolCall` still reads as a potential write, so
/// `react_to_first_write` fires off it: the turn is promoted to a build and, with the
/// reactive QC flag on, runs the SAME governance QC a `Write`-driven build does. A
/// Bash-only build is therefore never left ungoverned — and (H2) its terminal decision
/// carries `director_build: true`, so an OBSERVED build earns the full-build completion
/// semantics rather than reading as a plain streamed chat turn.
#[tokio::test]
async fn bash_heredoc_build_is_governed_by_the_reactive_engine() {
    let _qc = ReactiveQcOverride::force(true);
    let tmp = init_git_repo();
    let target = tmp.path().join("src/app.tsx");
    let (sink, mut engine_rx) = ChannelSink::new();
    let sink = Arc::new(sink);
    let (route_tx, mut route_rx) = tokio::sync::mpsc::unbounded_channel();
    // A Bash write with no Write/Edit tool — the base scaffolds via a heredoc and a real
    // file lands via the send effect (mirrors the pre-turn snapshot → post-turn git-diff
    // path production uses when no tool event names the write).
    let (writer, _sent, _ended) = FakeChatSession::new(vec![vec![
        umadev_runtime::SessionEvent::ToolCall {
            name: "Bash".into(),
            input: serde_json::json!({ "command": "cat > src/app.tsx <<'EOF'\n...\nEOF" }),
        },
        umadev_runtime::SessionEvent::ToolResult {
            ok: true,
            summary: "scaffolded src/app.tsx".into(),
        },
        umadev_runtime::SessionEvent::TextDelta("built it via bash".into()),
        umadev_runtime::SessionEvent::TurnDone {
            status: umadev_runtime::TurnStatus::Completed,
            usage: None,
        },
    ]]);
    let writer = writer.with_send_effect(move || {
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, "export const App = () => null;\n").unwrap();
    });
    let holder: ChatSessionHolder = ChatSessionHolder::from_mutex(tokio::sync::Mutex::new(Some(
        ResidentChat::Primed(Box::new(writer)),
    )));

    drive_chat_session_turn(chat_turn(
        "用脚本生成一个登录页面",
        holder,
        sink,
        route_tx,
        tmp.path().to_path_buf(),
    ))
    .await;

    assert!(matches!(
        route_rx.try_recv(),
        Ok(RouteDecision::AgenticDone {
            director_build: true,
            ..
        })
    ));
    let mut saw_qc = false;
    let mut saw_build_card = false;
    while let Ok(event) = engine_rx.try_recv() {
        match event {
            EngineEvent::Note(n) if n.contains("构建完成") => saw_qc = true,
            EngineEvent::IntentDecided { class, .. } if class == "build" => saw_build_card = true,
            _ => {}
        }
    }
    assert!(
        saw_build_card,
        "a Bash write promotes the turn to a build (react_to_first_write fires off the shell tool)"
    );
    assert!(
        saw_qc,
        "a Bash-only build still runs the reactive governance QC"
    );
}

#[tokio::test]
async fn deterministic_fallback_build_stays_resident_without_flagship_qc() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(tmp.path().join("app.ts"), "export const x = 1;\n").unwrap();
    let (sink, mut engine_rx) = ChannelSink::new();
    let sink = Arc::new(sink);
    let (route_tx, mut route_rx) = tokio::sync::mpsc::unbounded_channel();
    // No scripted fork: semantic routing is unavailable, so the clear Build
    // wording is only an availability fallback. One resident answer must not
    // earn a DirectorStarted transition or a broad QC re-drive.
    let (writer, sent, _ended) = FakeChatSession::new(vec![vec![
        umadev_runtime::SessionEvent::TextDelta("I inspected the request.".into()),
        umadev_runtime::SessionEvent::TurnDone {
            status: umadev_runtime::TurnStatus::Completed,
            usage: None,
        },
    ]]);
    let holder: ChatSessionHolder = ChatSessionHolder::from_mutex(tokio::sync::Mutex::new(Some(
        ResidentChat::Primed(Box::new(writer)),
    )));

    drive_chat_session_turn(chat_turn(
        "构建一个完整应用",
        holder,
        sink,
        route_tx,
        tmp.path().to_path_buf(),
    ))
    .await;

    match route_rx.try_recv() {
        Ok(RouteDecision::AgenticDone { director_build, .. }) => assert!(
            !director_build,
            "a no-write fallback verdict cannot claim a governed build"
        ),
        other => panic!("expected one resident completion, got {other:?}"),
    }
    assert!(
        route_rx.try_recv().is_err(),
        "no DirectorStarted transition"
    );
    assert_eq!(sent.lock().unwrap().len(), 1, "no flagship QC re-drive");
    while let Ok(event) = engine_rx.try_recv() {
        if let EngineEvent::Note(note) = event {
            assert!(
                !(note.contains("honesty + QC") || note.contains("team ·")),
                "availability fallback must not launch team QC: {note}"
            );
        }
    }
}

#[tokio::test]
async fn bash_file_change_is_post_turn_work_but_quick_edit_skips_flagship_qc() {
    let tmp = init_git_repo();
    let target = tmp.path().join("src/generated.rs");
    let (sink, mut engine_rx) = ChannelSink::new();
    let sink = Arc::new(sink);
    let (route_tx, mut route_rx) = tokio::sync::mpsc::unbounded_channel();

    // No routing fork under the reactive model: the base just acts. A Bash write is
    // real work (the post-turn git delta proves it), but with no reactive-QC flag it
    // never launches the flagship team QC.
    let (writer, sent, _ended) = FakeChatSession::new(vec![vec![
        umadev_runtime::SessionEvent::ToolCall {
            name: "Bash".into(),
            input: serde_json::json!({"command": "generate src/generated.rs"}),
        },
        umadev_runtime::SessionEvent::ToolResult {
            ok: true,
            summary: "generated source".into(),
        },
        umadev_runtime::SessionEvent::ToolCall {
            name: "Bash".into(),
            input: serde_json::json!({"command": "cargo check"}),
        },
        umadev_runtime::SessionEvent::ToolResult {
            ok: true,
            summary: "cargo check passed".into(),
        },
        umadev_runtime::SessionEvent::TextDelta("created src/generated.rs".into()),
        umadev_runtime::SessionEvent::TurnDone {
            status: umadev_runtime::TurnStatus::Completed,
            usage: None,
        },
    ]]);
    let writer = writer.with_send_effect(move || {
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(target, "pub fn generated() {}\n").unwrap();
    });
    let holder: ChatSessionHolder = ChatSessionHolder::from_mutex(tokio::sync::Mutex::new(Some(
        ResidentChat::Primed(Box::new(writer)),
    )));

    drive_chat_session_turn(chat_turn(
        "生成一个很小的 Rust 文件",
        holder,
        sink,
        route_tx,
        tmp.path().to_path_buf(),
    ))
    .await;

    // The Bash write is an observed build → a real build completion (H2). The flagship
    // team QC is a separate fact and stays gated on the reactive-QC switch (unset here).
    assert!(matches!(
        route_rx.try_recv(),
        Ok(RouteDecision::AgenticDone {
            director_build: true,
            ..
        })
    ));
    assert_eq!(
        sent.lock().unwrap().len(),
        1,
        "a Bash-backed change is real work but, with no reactive-QC flag, never launches \
         the flagship team QC"
    );
    let mut saw_real_change = false;
    while let Ok(event) = engine_rx.try_recv() {
        if let EngineEvent::Note(note) = event {
            // Porcelain v1 may collapse a wholly-untracked directory to
            // `src/`; either spelling is objective evidence of this write.
            saw_real_change |= note.contains("src/generated.rs") || note.contains("src/");
            assert!(
                !(note.contains("构建完成")
                    || note.contains("honesty + QC")
                    || note.contains("team ·")),
                "an unflagged reactive write runs no flagship QC: {note}"
            );
        }
    }
    assert!(
        saw_real_change,
        "the objective git delta must surface the Bash-created code path"
    );
}

#[tokio::test]
async fn docs_only_write_does_not_become_code_build_or_run_flagship_qc() {
    let tmp = init_git_repo();
    let target = tmp.path().join("README.md");
    let (sink, mut engine_rx) = ChannelSink::new();
    let sink = Arc::new(sink);
    let (route_tx, mut route_rx) = tokio::sync::mpsc::unbounded_channel();

    // No routing fork under the reactive model. A single doc/markdown write is
    // legitimate pre-development work: the `is_doc_artifact_path` latch keeps
    // `react_to_first_write` from firing, so the turn never becomes a code build.
    let (writer, sent, _ended) = FakeChatSession::new(vec![vec![
        umadev_runtime::SessionEvent::ToolCall {
            name: "Write".into(),
            input: serde_json::json!({"file_path": "README.md"}),
        },
        umadev_runtime::SessionEvent::TextDelta("updated README.md".into()),
        umadev_runtime::SessionEvent::TurnDone {
            status: umadev_runtime::TurnStatus::Completed,
            usage: None,
        },
    ]]);
    let writer =
        writer.with_send_effect(move || std::fs::write(target, "# Documentation\n").unwrap());
    let holder: ChatSessionHolder = ChatSessionHolder::from_mutex(tokio::sync::Mutex::new(Some(
        ResidentChat::Primed(Box::new(writer)),
    )));

    drive_chat_session_turn(chat_turn(
        "只更新 README 文档",
        holder,
        sink,
        route_tx,
        tmp.path().to_path_buf(),
    ))
    .await;

    assert!(matches!(
        route_rx.try_recv(),
        Ok(RouteDecision::AgenticDone {
            director_build: false,
            ..
        })
    ));
    assert_eq!(sent.lock().unwrap().len(), 1, "a doc write has no QC turn");
    // A doc-only write never latches a build, so — like a pure chat — it opens no
    // durable writer-ledger run (the up-front entry task was keyed off a mutating seed
    // route, which no longer exists).
    assert!(
        umadev_agent::task_lifecycle::recent_agent_runs(tmp.path(), 1).is_empty(),
        "a doc-only write does not enter the durable writer ledger"
    );
    while let Ok(event) = engine_rx.try_recv() {
        if let EngineEvent::Note(note) = event {
            assert!(
                !(note.contains(ABORT_SENTINEL)
                    || note.contains("构建完成")
                    || note.contains("honesty + QC")
                    || note.contains("team ·")),
                "DocsOnly must not enter the code-build gates: {note}"
            );
        }
    }
}

/// The other half of the unification invariant: a PURE chat reply (no write, no
/// `became_build`) must NOT run the post-build QC pass — it stays light + fast,
/// with no `team · …` QC notes and no extra fix directives. This guards the
/// latency: conversation is never slowed by the build-only QC machinery.
#[tokio::test]
async fn pure_chat_reply_skips_the_post_build_qc_pass() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sink, mut engine_rx) = ChannelSink::new();
    let sink = Arc::new(sink);
    let (route_tx, mut route_rx) = tokio::sync::mpsc::unbounded_channel();

    // A pure text reply — no write tool, so `became_build` stays false.
    let (fake, sent, _ended) = FakeChatSession::new(vec![vec![
        umadev_runtime::SessionEvent::TextDelta("here is my answer".into()),
        umadev_runtime::SessionEvent::TurnDone {
            status: umadev_runtime::TurnStatus::Completed,
            usage: None,
        },
    ]]);
    let holder: ChatSessionHolder = ChatSessionHolder::from_mutex(tokio::sync::Mutex::new(Some(
        ResidentChat::Primed(Box::new(fake)),
    )));

    drive_chat_session_turn(chat_turn(
        "你好,解释一下闭包",
        holder.clone(),
        sink.clone(),
        route_tx,
        tmp.path().to_path_buf(),
    ))
    .await;

    // Settles as a pure chat (not a build).
    match route_rx.try_recv() {
        Ok(RouteDecision::AgenticDone {
            director_build,
            reply,
            ..
        }) => {
            assert!(!director_build, "a pure reply is a chat, never a build");
            assert_eq!(reply, "here is my answer");
        }
        other => panic!("expected AgenticDone, got {other:?}"),
    }
    // NO post-build QC note fired — the conversation stayed on the light path.
    while let Ok(ev) = engine_rx.try_recv() {
        if let EngineEvent::Note(n) = ev {
            assert!(
                !(n.contains("构建完成") || n.contains("honesty + QC")),
                "a pure chat reply must NOT run the post-build QC pass: {n:?}"
            );
        }
    }
    // EXACTLY one directive was sent (the user turn) — no QC fix directive was
    // ever folded back, so a pure chat is never slowed by rework.
    assert_eq!(
        sent.lock().unwrap().len(),
        1,
        "a pure chat reply drives exactly one directive — no QC rework"
    );
    assert!(
        umadev_agent::task_lifecycle::recent_agent_runs(tmp.path(), 1).is_empty(),
        "Chat/Explain must not enter the durable writer ledger"
    );
}

/// An interrupted turn (ESC reflected by the base as `TurnStatus::Interrupted`)
/// PARKS the still-alive session back for reuse and settles `thinking` via a
/// (non-build) terminal decision — it does NOT close the resident session.
#[tokio::test]
async fn chat_session_interrupt_parks_session_for_reuse() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sink, _engine_rx) = ChannelSink::new();
    let sink = Arc::new(sink);
    let (route_tx, mut route_rx) = tokio::sync::mpsc::unbounded_channel();

    let (fake, _sent, ended) = FakeChatSession::new(vec![vec![
        umadev_runtime::SessionEvent::TextDelta("partial".into()),
        umadev_runtime::SessionEvent::TurnDone {
            status: umadev_runtime::TurnStatus::Interrupted,
            usage: None,
        },
    ]]);
    let holder: ChatSessionHolder = ChatSessionHolder::from_mutex(tokio::sync::Mutex::new(Some(
        ResidentChat::Primed(Box::new(fake)),
    )));

    drive_chat_session_turn(chat_turn(
        "停",
        holder.clone(),
        sink,
        route_tx,
        tmp.path().to_path_buf(),
    ))
    .await;

    assert!(
        holder.lock().await.is_some(),
        "an interrupted turn parks the live session back for reuse"
    );
    assert!(
        !ended.load(std::sync::atomic::Ordering::SeqCst),
        "interrupt does NOT close the resident session"
    );
    match route_rx.try_recv() {
        Ok(RouteDecision::AgenticDone { director_build, .. }) => {
            assert!(!director_build, "an interrupted turn settles as a chat");
        }
        other => panic!("expected AgenticDone, got {other:?}"),
    }
}

/// A PRE-LOADED warm session (the latency fix): the holder already carries a
/// `Warm` session by the time the user sends, so the FIRST turn does NOT
/// cold-start (`session_for` is never called) — it only sends the first directive
/// into the already-resident base and parks it back PRIMED for reuse. The first
/// directive front-loads the bounded conversation transcript so the warm session
/// inherits the prior dialogue.
#[tokio::test]
async fn preloaded_warm_session_is_used_without_a_cold_start() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sink, _engine_rx) = ChannelSink::new();
    let sink = Arc::new(sink);
    let (route_tx, mut route_rx) = tokio::sync::mpsc::unbounded_channel();

    let (fake, sent, ended) = FakeChatSession::new(vec![vec![
        umadev_runtime::SessionEvent::TextDelta("warm reply".into()),
        umadev_runtime::SessionEvent::TurnDone {
            status: umadev_runtime::TurnStatus::Completed,
            usage: None,
        },
    ]]);
    // Park a WARM session (claude → no firmware prefix on the first directive)
    // exactly as the background pre-load would have.
    let holder: ChatSessionHolder = ChatSessionHolder::from_mutex(tokio::sync::Mutex::new(Some(
        ResidentChat::Warm(WarmChatSession {
            session: Box::new(fake),
            firmware: None,
            backend: "claude-code".to_string(),
            permissions: umadev_runtime::BasePermissionProfile::Guarded,
            generation: 0,
        }),
    )));

    // Drive a turn whose snapshot carries a one-line prior conversation so we can
    // assert the FIRST directive front-loads it (the warm session is fresh memory).
    let mut turn = chat_turn(
        "继续",
        holder.clone(),
        sink,
        route_tx,
        tmp.path().to_path_buf(),
    );
    turn.conversation = vec![
        umadev_runtime::Message {
            role: "user".into(),
            content: "之前的问题".into(),
        },
        umadev_runtime::Message {
            role: "assistant".into(),
            content: "之前的回答".into(),
        },
    ];
    drive_chat_session_turn(turn).await;

    // The warm session was consumed and re-parked as `Primed` (alive, reusable).
    assert!(
        matches!(*holder.lock().await, Some(ResidentChat::Primed(_))),
        "a warm session becomes primed after its first turn"
    );
    assert!(
        !ended.load(std::sync::atomic::Ordering::SeqCst),
        "the warm session is reused, never closed, after the first turn"
    );
    // The FIRST directive front-loaded the prior dialogue (warm session has no
    // native memory of it yet) — so it is NOT the bare user turn.
    let sent = sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 1, "exactly one directive into the warm session");
    assert!(
        sent[0].contains("继续") && sent[0].contains("之前的回答"),
        "first directive front-loads the transcript onto the warm session: {:?}",
        sent[0]
    );
    match route_rx.try_recv() {
        Ok(RouteDecision::AgenticDone {
            reply,
            director_build,
            ..
        }) => {
            assert_eq!(reply, "warm reply");
            assert!(!director_build);
        }
        other => panic!("expected AgenticDone, got {other:?}"),
    }
}

/// The first directive for a warm session: claude gets history ONLY (firmware is
/// native via `--append-system-prompt`); a non-claude base (no native system
/// slot) gets the firmware re-prefixed onto the directive too.
#[test]
fn first_chat_directive_prefixes_firmware_only_for_non_claude() {
    let convo: Vec<Message> = Vec::new();
    // claude: firmware present but NEVER restated on the directive.
    let route = umadev_agent::deterministic_route("做个登录页");
    let claude = first_chat_directive(
        Some("FW-BLOCK"),
        "claude-code",
        &convo,
        "做个登录页",
        "做个登录页",
        &route,
    );
    assert!(
        !claude.contains("FW-BLOCK"),
        "claude firmware is native — never re-prefixed: {claude:?}"
    );
    assert!(claude.contains("做个登录页"));
    // codex: no native system slot → firmware is prefixed onto the directive.
    let codex = first_chat_directive(
        Some("FW-BLOCK"),
        "codex",
        &convo,
        "做个登录页",
        "做个登录页",
        &route,
    );
    assert!(
        codex.starts_with("FW-BLOCK"),
        "non-claude firmware is front-loaded onto the first directive: {codex:?}"
    );
    assert!(codex.contains("做个登录页"));
    // No firmware → bare goal regardless of base.
    let bare = first_chat_directive(None, "opencode", &convo, "做个登录页", "做个登录页", &route);
    assert!(bare.contains("## Current-turn authority"));
    assert!(bare.contains("做个登录页"));
}

#[test]
fn scope_contract_is_route_sized_and_history_is_not_authority() {
    // The toolless Chat lane. A deterministic fallback no longer PICKS Chat for a bare
    // question ("what is Rust?" now floors to read-only Explain so the base may look), so
    // exercise the toolless-Chat directive via an explicit Chat class — the lane text is
    // what this asserts, plus the shared "context only" authority framing.
    let mut chat_route = umadev_agent::deterministic_route("what is Rust?");
    chat_route.class = umadev_agent::RouteClass::Chat;
    let read = scoped_chat_directive("what is Rust?", &chat_route);
    assert!(read.contains("without tools, commands, file writes, reviews, or QC"));
    assert!(read.contains("context only"));

    // The fix at the directive boundary: a bare question now floors to Explain (read/
    // search allowed), NOT toolless Chat, so a read-only "where is X" is never stranded.
    assert_eq!(
        umadev_agent::deterministic_route("what is Rust?").class,
        umadev_agent::RouteClass::Explain
    );

    let mut explain_route = umadev_agent::deterministic_route("what is Rust?");
    explain_route.class = umadev_agent::RouteClass::Explain;
    let explain = scoped_chat_directive("解释现有代码", &explain_route);
    assert!(explain.contains("necessary read/search tools"));
    assert!(explain.contains("do not run mutating commands, write files"));

    let edit = scoped_chat_directive(
        "改个文案，把标题改成 Welcome",
        &umadev_agent::deterministic_route("改个文案，把标题改成 Welcome"),
    );
    assert!(edit.contains("smallest necessary edit"));
    assert!(edit.contains("Do not launch a team or broad review"));
    assert!(edit.contains("## Latest request\n改个文案，把标题改成 Welcome"));
}

/// The turn-time resident guard (the post-`/backend`-switch ordering race): a
/// parked WARM session pinned to ANOTHER base is rejected as stale (the caller
/// closes it and lazily opens the right base), while a matching warm session and
/// any primed session pass through untouched.
#[test]
fn resident_for_turn_rejects_a_warm_session_from_another_base() {
    let workspace = tempfile::TempDir::new().unwrap();
    let claude_identity = SessionIdentity::for_launch(
        "claude-code",
        workspace.path(),
        umadev_runtime::BasePermissionProfile::Guarded,
    )
    .unwrap();
    let codex_identity = SessionIdentity::for_launch(
        "codex",
        workspace.path(),
        umadev_runtime::BasePermissionProfile::Guarded,
    )
    .unwrap();
    // Stale: warm claude parked, but the turn now runs on codex.
    let (fake, _s, _e) = FakeChatSession::new(vec![]);
    let parked = Some(ResidentChat::Warm(WarmChatSession {
        session: Box::new(fake),
        firmware: Some("FW".into()),
        backend: "claude-code".into(),
        permissions: umadev_runtime::BasePermissionProfile::Guarded,
        generation: 0,
    }));
    let (usable, stale) =
        resident_for_turn(parked, Some(&codex_identity), Some(&claude_identity), 0);
    assert!(
        usable.is_none(),
        "a wrong-base warm session is never served"
    );
    assert!(
        matches!(stale, Some(ResidentChat::Warm(_))),
        "the stale warm session is returned for closing"
    );
    // Matching: warm codex serves a codex turn.
    let (fake, _s, _e) = FakeChatSession::new(vec![]);
    let parked = Some(ResidentChat::Warm(WarmChatSession {
        session: Box::new(fake),
        firmware: None,
        backend: "codex".into(),
        permissions: umadev_runtime::BasePermissionProfile::Guarded,
        generation: 0,
    }));
    let (usable, stale) =
        resident_for_turn(parked, Some(&codex_identity), Some(&codex_identity), 0);
    assert!(matches!(usable, Some(ResidentChat::Warm(_))));
    assert!(stale.is_none());
    // Primed is trusted only under the holder's exact permission profile;
    // generation invalidation prevents an older turn from parking one here.
    let (fake, _s, _e) = FakeChatSession::new(vec![]);
    let (usable, stale) = resident_for_turn(
        Some(ResidentChat::Primed(Box::new(fake))),
        Some(&codex_identity),
        Some(&codex_identity),
        0,
    );
    assert!(matches!(usable, Some(ResidentChat::Primed(_))));
    assert!(stale.is_none());
    // A canonical workspace mismatch is just as stale as a backend mismatch.
    let other_workspace = tempfile::TempDir::new().unwrap();
    let other_identity = SessionIdentity::for_launch(
        "codex",
        other_workspace.path(),
        umadev_runtime::BasePermissionProfile::Guarded,
    )
    .unwrap();
    let (fake, _s, _e) = FakeChatSession::new(vec![]);
    let (usable, stale) = resident_for_turn(
        Some(ResidentChat::Primed(Box::new(fake))),
        Some(&other_identity),
        Some(&codex_identity),
        0,
    );
    assert!(usable.is_none());
    assert!(matches!(stale, Some(ResidentChat::Primed(_))));
    // Empty holder stays empty.
    let (usable, stale) = resident_for_turn(None, None, None, 0);
    assert!(usable.is_none() && stale.is_none());
}

#[test]
fn resident_for_turn_rejects_stale_generation_and_permission_profiles() {
    let workspace = tempfile::TempDir::new().unwrap();
    let requested_plan = SessionIdentity::for_launch(
        "codex",
        workspace.path(),
        umadev_runtime::BasePermissionProfile::Plan,
    )
    .unwrap();
    let parked_auto = SessionIdentity::for_launch(
        "codex",
        workspace.path(),
        umadev_runtime::BasePermissionProfile::Auto,
    )
    .unwrap();
    let (fake, _sent, _ended) = FakeChatSession::new(vec![]);
    let warm = ResidentChat::Warm(WarmChatSession {
        session: Box::new(fake),
        firmware: None,
        backend: "codex".into(),
        permissions: umadev_runtime::BasePermissionProfile::Auto,
        generation: 4,
    });
    let (usable, stale) =
        resident_for_turn(Some(warm), Some(&requested_plan), Some(&parked_auto), 4);
    assert!(usable.is_none());
    assert!(matches!(stale, Some(ResidentChat::Warm(_))));

    let (fake, _sent, _ended) = FakeChatSession::new(vec![]);
    let warm = ResidentChat::Warm(WarmChatSession {
        session: Box::new(fake),
        firmware: None,
        backend: "codex".into(),
        permissions: umadev_runtime::BasePermissionProfile::Guarded,
        generation: 3,
    });
    let guarded_identity = SessionIdentity::for_launch(
        "codex",
        workspace.path(),
        umadev_runtime::BasePermissionProfile::Guarded,
    )
    .unwrap();
    let (usable, stale) = resident_for_turn(
        Some(warm),
        Some(&guarded_identity),
        Some(&guarded_identity),
        4,
    );
    assert!(usable.is_none());
    assert!(matches!(stale, Some(ResidentChat::Warm(_))));

    let (fake, _sent, _ended) = FakeChatSession::new(vec![]);
    let (usable, stale) = resident_for_turn(
        Some(ResidentChat::Primed(Box::new(fake))),
        Some(&requested_plan),
        Some(&parked_auto),
        4,
    );
    assert!(usable.is_none());
    assert!(matches!(stale, Some(ResidentChat::Primed(_))));
}

#[tokio::test]
async fn resident_holder_never_parks_a_cancelled_generation() {
    let holder = ChatSessionHolder::new(None);
    let started_generation = holder.generation();
    holder.invalidate();
    let (fake, _sent, ended) = FakeChatSession::new(vec![]);
    let workspace = tempfile::TempDir::new().unwrap();
    let identity = SessionIdentity::for_launch(
        "codex",
        workspace.path(),
        umadev_runtime::BasePermissionProfile::Auto,
    )
    .unwrap();
    let parked = holder
        .park_if_current(
            started_generation,
            identity,
            ResidentChat::Primed(Box::new(fake)),
        )
        .await;
    assert!(!parked, "a cancelled generation has no right to re-park");
    assert!(holder.lock().await.is_none());
    tokio::time::timeout(TURN_HANG_GUARD, async {
        while !ended.load(std::sync::atomic::Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("a rejected stale process is closed off-path");
}

/// The transient-failure park disposition: a FIRST front-loaded directive that
/// streamed NOTHING re-parks `Warm` (the next turn re-feeds the transcript — the
/// base may never have absorbed it); streamed evidence or a bare `Primed`
/// acquire re-parks `Primed` (the pre-existing behavior).
#[tokio::test]
async fn park_after_transient_failure_reparks_warm_only_for_an_unabsorbed_first_directive() {
    // First directive + nothing streamed → Warm (full re-feed next turn).
    let front = AttemptDirective::FrontLoaded {
        firmware: Some("FW".into()),
    };
    let (fake, _s, _e) = FakeChatSession::new(vec![]);
    let parked = park_after_transient_failure(
        Box::new(fake),
        &front,
        false,
        "codex",
        umadev_runtime::BasePermissionProfile::Guarded,
        7,
    );
    match parked {
        ResidentChat::Warm(w) => {
            assert_eq!(w.firmware.as_deref(), Some("FW"), "the firmware is carried");
            assert_eq!(w.backend, "codex");
        }
        ResidentChat::Primed(_) | ResidentChat::ReadOnlyPrimed(_) => {
            panic!("an unabsorbed first directive must re-park Warm")
        }
    }
    // First directive but the base DID stream → Primed (it absorbed the history).
    let (fake, _s, _e) = FakeChatSession::new(vec![]);
    let parked = park_after_transient_failure(
        Box::new(fake),
        &front,
        true,
        "codex",
        umadev_runtime::BasePermissionProfile::Guarded,
        7,
    );
    assert!(matches!(parked, ResidentChat::Primed(_)));
    // A bare Primed reuse (no first directive this attempt) stays Primed.
    let (fake, _s, _e) = FakeChatSession::new(vec![]);
    let parked = park_after_transient_failure(
        Box::new(fake),
        &AttemptDirective::Bare,
        false,
        "codex",
        umadev_runtime::BasePermissionProfile::Guarded,
        7,
    );
    assert!(matches!(parked, ResidentChat::Primed(_)));
}

/// End-to-end amnesia regression on the resident chat path: the FIRST
/// front-loaded directive fails with a KNOWN-transient error (429) and ZERO
/// events streamed — the base never absorbed the transcript. The session must
/// re-park `Warm` so the NEXT turn re-feeds the full front-load (firmware +
/// prior dialogue), instead of going out bare into an empty brain.
#[tokio::test]
async fn unabsorbed_first_directive_failure_refeeds_the_transcript_on_the_next_turn() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sink, _engine_rx) = ChannelSink::new();
    let sink = Arc::new(sink);
    let (route_tx, mut route_rx) = tokio::sync::mpsc::unbounded_channel();

    // Turn 1: an immediate KNOWN-transient failure (no auto-redrive, base still
    // alive), with NO events before it. Turn 2: a clean completion.
    let (fake, sent, _ended) = FakeChatSession::new(vec![
        vec![umadev_runtime::SessionEvent::TurnDone {
            status: umadev_runtime::TurnStatus::Failed("429 Too Many Requests".into()),
            usage: None,
        }],
        vec![
            umadev_runtime::SessionEvent::TextDelta("有上下文的回答".into()),
            umadev_runtime::SessionEvent::TurnDone {
                status: umadev_runtime::TurnStatus::Completed,
                usage: None,
            },
        ],
    ]);
    let holder: ChatSessionHolder = ChatSessionHolder::from_mutex(tokio::sync::Mutex::new(Some(
        ResidentChat::Warm(WarmChatSession {
            session: Box::new(fake),
            firmware: Some("FW-CODEX".into()),
            backend: "codex".into(),
            permissions: umadev_runtime::BasePermissionProfile::Guarded,
            generation: 0,
        }),
    )));
    let prior = vec![
        umadev_runtime::Message {
            role: "user".into(),
            content: "MARKER-EARLIER 我们之前定了用 SQLite".into(),
        },
        umadev_runtime::Message {
            role: "assistant".into(),
            content: "好的,表结构已定".into(),
        },
    ];

    // Turn 1 — fails clean; the session must re-park WARM (not Primed).
    let mut turn = chat_turn(
        "继续实现",
        holder.clone(),
        sink.clone(),
        route_tx.clone(),
        tmp.path().to_path_buf(),
    );
    turn.backend = "codex".into();
    turn.conversation = prior.clone();
    drive_chat_session_turn(turn).await;
    assert!(
        matches!(route_rx.try_recv(), Ok(RouteDecision::Failed(_))),
        "the transient failure is surfaced honestly"
    );
    assert!(
        matches!(*holder.lock().await, Some(ResidentChat::Warm(_))),
        "an unabsorbed first directive re-parks the session WARM for a full re-feed"
    );

    // Turn 2 — the re-fed first directive carries the current route-sized
    // identity/authority overlay AND the prior dialogue again (the amnesia
    // fix), then completes and parks Primed. The old warm placeholder firmware
    // is intentionally replaced after model routing.
    let mut turn = chat_turn(
        "再试一次",
        holder.clone(),
        sink,
        route_tx,
        tmp.path().to_path_buf(),
    );
    turn.backend = "codex".into();
    turn.conversation = prior;
    drive_chat_session_turn(turn).await;
    let sent = sent.lock().unwrap().clone();
    assert_eq!(
        sent.len(),
        2,
        "two directives into the same session: {sent:?}"
    );
    assert!(
        sent[0].contains("Current-turn authority") && sent[0].contains("MARKER-EARLIER"),
        "the first attempt front-loaded route firmware + transcript: {:?}",
        sent[0]
    );
    assert!(
        sent[1].contains("Current-turn authority") && sent[1].contains("MARKER-EARLIER"),
        "the retry turn RE-FEEDS the full front-load (no bare amnesia turn): {:?}",
        sent[1]
    );
    assert!(
        matches!(*holder.lock().await, Some(ResidentChat::Primed(_))),
        "a completed turn parks Primed as before"
    );
}

/// The background pre-load is a NO-OP for a non-host (offline) brain — there is no
/// resident process to keep, so the holder stays empty and the first chat turn
/// lazily opens exactly as before. (Hermetic: an offline id never spawns a base.)
#[tokio::test]
async fn preload_is_a_noop_for_a_non_host_backend() {
    let tmp = tempfile::TempDir::new().unwrap();
    let holder = ChatSessionHolder::new(None);
    spawn_chat_session_preload(
        Some("offline"),
        String::new(),
        tmp.path().to_path_buf(),
        umadev_runtime::BasePermissionProfile::Guarded,
        None,
        holder.clone(),
    );
    // Also a `None` backend (no base configured) — both must leave the holder empty.
    spawn_chat_session_preload(
        None,
        String::new(),
        tmp.path().to_path_buf(),
        umadev_runtime::BasePermissionProfile::Guarded,
        None,
        holder.clone(),
    );
    // Give any (wrongly-)spawned task a chance to run, then assert nothing landed.
    tokio::task::yield_now().await;
    assert!(
        holder.lock().await.is_none(),
        "a non-host / unconfigured pre-load never lands a session"
    );
}

/// `ResidentChat::end` releases the underlying base in BOTH states (warm + primed)
/// — the cleanup the cancel / `/clear` / backend-switch / quit paths rely on.
#[tokio::test]
async fn resident_chat_end_closes_warm_and_primed() {
    let (warm_fake, _s, warm_ended) = FakeChatSession::new(vec![]);
    ResidentChat::Warm(WarmChatSession {
        session: Box::new(warm_fake),
        firmware: None,
        backend: "claude-code".to_string(),
        permissions: umadev_runtime::BasePermissionProfile::Guarded,
        generation: 0,
    })
    .end()
    .await;
    assert!(
        warm_ended.load(std::sync::atomic::Ordering::SeqCst),
        "ending a warm resident closes its base"
    );
    let (primed_fake, _s2, primed_ended) = FakeChatSession::new(vec![]);
    ResidentChat::Primed(Box::new(primed_fake)).end().await;
    assert!(
        primed_ended.load(std::sync::atomic::Ordering::SeqCst),
        "ending a primed resident closes its base"
    );
}

// --- Rendering self-heal (P0 every-frame repaint / P2 probe / P3
// contamination) ------------------------------------------------------------

#[test]
fn size_poll_detects_a_lost_resize_event_only_on_a_real_change() {
    // No baseline yet (startup / first poll) → record only, never heal: the
    // initial paint must not be preceded by a spurious clear.
    assert!(
        !size_poll_detected_resize(None, Some((120, 30))),
        "the first size reading is a baseline, not a resize"
    );
    // Unchanged size → no heal. This is the idle steady state — the 80ms tick
    // polls forever, so an identical reading MUST stay silent (the
    // no-per-frame-clear anti-flicker contract).
    assert!(
        !size_poll_detected_resize(Some((120, 30)), Some((120, 30))),
        "an unchanged size must never trigger a clear (idle = no flicker)"
    );
    // Width shrink — the fullscreen/drag case: rows painted at the stale wider
    // width overflow the new terminal, autowrap spills the status bar's tail
    // down the left column. The poll must catch it even with no Resize event.
    assert!(
        size_poll_detected_resize(Some((160, 40)), Some((120, 40))),
        "a width change with no delivered Resize event must heal"
    );
    // Growth and a height-only change count too.
    assert!(size_poll_detected_resize(Some((120, 30)), Some((160, 30))));
    assert!(size_poll_detected_resize(Some((120, 30)), Some((120, 31))));
    // A failed backend size query fabricates nothing (fail-open), with or
    // without a baseline — and it must not erase the baseline either (the
    // caller keeps the old one so a later good reading still compares).
    assert!(!size_poll_detected_resize(Some((120, 30)), None));
    assert!(!size_poll_detected_resize(None, None));
}

#[test]
fn poll_detected_resize_runs_the_same_heal_as_an_event_resize() {
    // The shared reaction (`apply_resize_heal`) — used by BOTH a delivered
    // Event::Resize and the tick-time size-poll fallback — opens the
    // RESIZE_HEAL_WINDOW, so every frame for a short spell repaints IN PLACE
    // (HealMode::Invalidate) and the terminal's multi-frame buffer settle heals
    // too, not just one frame. It deliberately does NOT contaminate: a resize
    // shows OUR cells at the wrong geometry (drift), not foreign bytes, so it
    // must not pay an ED(2) erase + its (0,0) cursor sweep.
    let (mut terminal, _tap) = recording_terminal(20, 3);
    let mut last_resize_at = None;
    apply_resize_heal(&mut terminal, false, &mut last_resize_at);
    assert!(
        last_resize_at.is_some_and(|t| t.elapsed() < RESIZE_HEAL_WINDOW),
        "a detected resize opens the resize heal window for the settle frames"
    );
}

#[test]
fn a_resize_re_fires_the_background_theme_query_when_allowed() {
    // Trigger #2 — a resize can accompany a live terminal-theme switch (dragging
    // the window to a display with a different profile, an OS day/night flip that
    // also reflowed), so the shared resize reaction re-fires the OSC 11
    // background query. With the guard satisfied (`allow = true`) the query bytes
    // hit the wire; the throttle-free discrete event always probes.
    let (mut terminal, tap) = recording_terminal(20, 3);
    let mut last_resize_at = None;
    apply_resize_heal(&mut terminal, true, &mut last_resize_at);
    assert!(
        tap.drain().contains("\x1b]11;?"),
        "a resize must re-fire the OSC 11 background-color query"
    );

    // …and when the guard is NOT satisfied (`allow = false` — a pinned theme, the
    // legacy reader, or an explicit UMADEV_THEME) no query is emitted.
    let (mut terminal, tap) = recording_terminal(20, 3);
    let mut last_resize_at = None;
    apply_resize_heal(&mut terminal, false, &mut last_resize_at);
    assert!(
        !tap.drain().contains("\x1b]11;?"),
        "a pinned / non-owned theme must NOT be re-probed on resize"
    );
}

// --- The heal split: drift repaints in place, contamination erases -----------

#[test]
fn heal_mode_erases_only_for_contamination_and_invalidates_for_drift() {
    // Drift (the streaming cadence / the resize + focus settle windows): repaint
    // every cell IN PLACE. No ED(2), no (0,0) cursor park, no flash, and no
    // dependence on the terminal honoring DEC 2026.
    assert_eq!(
        heal_mode(true, false),
        HealMode::Invalidate,
        "drift heals in place — never an erase"
    );
    // True contamination (an out-of-band write / Ctrl+L / /redraw): the screen
    // holds bytes we never wrote, so only an erase is honest.
    assert_eq!(
        heal_mode(false, true),
        HealMode::Erase,
        "contamination erases"
    );
    // Contamination wins when both are pending — the erase subsumes the repaint,
    // so a frame never pays two heals.
    assert_eq!(
        heal_mode(true, true),
        HealMode::Erase,
        "contamination subsumes a concurrent drift heal (exactly one heal per frame)"
    );
    // The steady state — a pure scroll, an idle screen, a prompt being typed at:
    // NOTHING. This is the anti-flicker contract: no per-frame heal, ever.
    assert_eq!(
        heal_mode(false, false),
        HealMode::None,
        "no drift and no contamination → plain incremental diff (no flicker)"
    );
}

/// A `Write` sink that keeps its bytes reachable — `CrosstermBackend`'s own
/// writer is private, so a recording backend has to own the tap itself.
#[derive(Clone, Default)]
struct Tap(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);

impl Tap {
    /// Take everything written so far, as a lossy string.
    fn drain(&self) -> String {
        let bytes = std::mem::take(&mut *self.0.borrow_mut());
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

impl std::io::Write for Tap {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A `Terminal` over the REAL `AnchoredBackend` + `CrosstermBackend`, writing its
/// escape sequences into a [`Tap`] instead of a TTY — so a test can assert on the
/// EXACT bytes a frame put on the wire.
fn recording_terminal(w: u16, h: u16) -> (ratatui::Terminal<AnchoredBackend<Tap>>, Tap) {
    let tap = Tap::default();
    let backend = AnchoredBackend::new(CrosstermBackend::new(tap.clone()));
    let terminal = ratatui::Terminal::with_options(
        backend,
        ratatui::TerminalOptions {
            viewport: ratatui::Viewport::Fixed(ratatui::layout::Rect::new(0, 0, w, h)),
        },
    )
    .expect("a Tap-backed terminal cannot fail");
    (terminal, tap)
}

/// Paint `text` at (0, 0) — a tiny stand-in for a real frame.
fn draw_text(terminal: &mut ratatui::Terminal<AnchoredBackend<Tap>>, text: &str) {
    terminal
        .draw(|f| {
            f.render_widget(
                ratatui::widgets::Paragraph::new(text),
                ratatui::layout::Rect::new(0, 0, f.area().width, 1),
            );
        })
        .expect("a Tap-backed draw cannot fail");
}

/// Whether the wire carries ANY screen-erase op — the thing a drift heal must
/// never emit. `ED(2)` (`\x1b[2J`, what `Clear(All)` sends on a fullscreen
/// viewport) and `ED(0)` (`\x1b[J` / `\x1b[0J`, the erase-to-end a fixed
/// viewport's per-row clear sends) both blank real cells and, on Windows, park
/// the cursor at (0, 0) — the flash + the cursor sweep.
fn erases_the_screen(wire: &str) -> bool {
    wire.contains("\x1b[2J") || wire.contains("\x1b[J") || wire.contains("\x1b[0J")
}

#[test]
fn a_drift_heal_repaints_without_erasing_while_contamination_still_erases() {
    // THE central behavioral claim of the heal split, asserted on the wire.
    let (mut terminal, tap) = recording_terminal(12, 2);
    draw_text(&mut terminal, "hello");
    let _first = tap.drain();

    // A second identical frame with NO heal: ratatui's own-buffer diff is empty,
    // so nothing is repainted. (This is exactly why drift can never self-heal —
    // and why the heal below has to exist at all.)
    draw_text(&mut terminal, "hello");
    let steady = tap.drain();
    assert!(
        !steady.contains('h'),
        "an unchanged frame emits no cells (the empty diff that lets drift persist): {steady:?}"
    );

    // DRIFT heal → repaint every cell IN PLACE. No erase, and the cells come back.
    apply_heal(&mut terminal, HealMode::Invalidate);
    draw_text(&mut terminal, "hello");
    let healed = tap.drain();
    assert!(
        !erases_the_screen(&healed),
        "a drift heal must emit NO erase op — the erase is the flash + the (0,0) cursor sweep: {healed:?}"
    );
    assert!(
        healed.contains("hello"),
        "a drift heal must re-emit the frame's cells in place: {healed:?}"
    );

    // CONTAMINATION heal → the erase the caller actually asked for.
    apply_heal(&mut terminal, HealMode::Erase);
    draw_text(&mut terminal, "hello");
    let erased = tap.drain();
    assert!(
        erases_the_screen(&erased),
        "contamination must still erase the screen: {erased:?}"
    );
    assert!(erased.contains("hello"), "…and repaint: {erased:?}");

    // And HealMode::None touches nothing.
    apply_heal(&mut terminal, HealMode::None);
    draw_text(&mut terminal, "hello");
    let none = tap.drain();
    assert!(
        !erases_the_screen(&none) && !none.contains('h'),
        "no heal → plain (here: empty) incremental diff: {none:?}"
    );
}

#[test]
fn a_drift_heal_repaints_cells_that_are_blank_in_the_new_frame() {
    // The trap in a plain `Buffer::reset()`-based invalidation: reset fills with
    // `Cell::EMPTY` (a space in the default style), so a cell that is ALSO blank
    // in the new frame diffs EQUAL and is SKIPPED — and whatever garbage the
    // drift left in that cell survives the "full" repaint. `invalidate_frame`
    // poisons the previous buffer with a symbol no real cell can hold, so every
    // cell — blanks included — is re-emitted. Assert it on the wire: a heal frame
    // whose content is entirely blank must still write spaces over the screen.
    let (mut terminal, tap) = recording_terminal(6, 1);
    draw_text(&mut terminal, "abcdef");
    let _ = tap.drain();

    apply_heal(&mut terminal, HealMode::Invalidate);
    // The new frame is BLANK — every cell is a default-styled space.
    draw_text(&mut terminal, "");
    let healed = tap.drain();
    assert!(
        healed.contains("      "),
        "a drift heal must paint the blank cells too, or stale glyphs survive it: {healed:?}"
    );
    assert!(
        !erases_the_screen(&healed),
        "…and still without an erase: {healed:?}"
    );
}

// --- Cursor-advance re-anchoring (the ambiguous-width root cause) ------------

#[test]
fn the_backend_re_anchors_the_cursor_after_a_non_ascii_cell() {
    use ratatui::backend::Backend as _;
    use ratatui::buffer::Cell;

    // ratatui's stock crossterm backend suppresses the MoveTo whenever the next
    // cell sits at `prev.x + 1` — it ASSUMES every printed cell advanced the real
    // cursor exactly one column. For an East-Asian AMBIGUOUS-width glyph (`·`,
    // `─`, `—`, `…`) `unicode-width` says 1 but a CJK-locale terminal renders 2,
    // so the real cursor ends up one column further right and EVERY later cell in
    // the row lands in the wrong place. `AnchoredBackend` re-emits an explicit
    // MoveTo after any non-ASCII cell, so the disagreement self-corrects at the
    // very next cell instead of cascading.
    let tap = Tap::default();
    let mut backend = AnchoredBackend::new(CrosstermBackend::new(tap.clone()));
    let cells: Vec<(u16, u16, Cell)> = "a·b"
        .chars()
        .enumerate()
        .map(|(i, ch)| {
            let mut c = Cell::EMPTY;
            c.set_symbol(&ch.to_string());
            (u16::try_from(i).unwrap(), 0, c)
        })
        .collect();
    backend
        .draw(cells.iter().map(|(x, y, c)| (*x, *y, c)))
        .expect("a Tap-backed draw cannot fail");
    let wire = tap.drain();

    // The cell AFTER the ambiguous-width `·` is re-anchored with an explicit
    // MoveTo (1-based CSI row;col H → column index 2 = `\x1b[1;3H`).
    assert!(
        wire.contains("\x1b[1;3H"),
        "the cell after a non-ASCII glyph must be re-anchored with an explicit MoveTo: {wire:?}"
    );
    // …while the cell after the pure-ASCII `a` still rides the contiguous-run
    // shortcut (no MoveTo at column index 1 → `\x1b[1;2H`), so a pure-ASCII frame
    // is byte-for-byte what stock ratatui would emit — the anchoring is free.
    assert!(
        !wire.contains("\x1b[1;2H"),
        "an ASCII predecessor must keep the MoveTo suppression (no per-cell cost): {wire:?}"
    );
    assert!(wire.contains('a') && wire.contains('·') && wire.contains('b'));
}

#[test]
fn the_backend_never_queries_stdin_for_a_cursor_position() {
    use ratatui::backend::Backend as _;
    use ratatui::layout::Position;

    let tap = Tap::default();
    let mut backend = AnchoredBackend::new(CrosstermBackend::new(tap.clone()));
    backend
        .set_cursor_position(Position { x: 7, y: 3 })
        .expect("a Tap-backed cursor move cannot fail");
    let _ = tap.drain();

    assert_eq!(
        backend.get_cursor_position().unwrap(),
        Position { x: 7, y: 3 }
    );
    assert!(
        !tap.drain().contains("\x1b[6n"),
        "cursor reads must use the owned logical position, never a blocking terminal query"
    );
}

#[test]
fn ascii_advance_is_the_only_certain_one() {
    assert!(cell_advance_is_certain("a"));
    assert!(cell_advance_is_certain(" "));
    // Every ambiguous-width glyph UmaDev's own chrome uses — the actual garble
    // sources — must force a re-anchor.
    for amb in ["·", "─", "—", "…", "│", "▸"] {
        assert!(
            !cell_advance_is_certain(amb),
            "{amb:?} is ambiguous/wide — its column advance is NOT certain"
        );
    }
    // …and so must a plain CJK glyph.
    assert!(!cell_advance_is_certain("中"));
}

#[test]
fn autowrap_is_disabled_on_enter_and_restored_on_exit() {
    // DECAWM off (`\x1b[?7l`) for the alt-screen session: with autowrap ON, one
    // glyph the terminal renders wider than `unicode-width` predicted pushes the
    // row's tail past the right margin, the terminal SPILLS it onto the next
    // line, and the corruption cascades down the whole screen — invisible to
    // ratatui's own-buffer diff, so it can never be repaired. With DECAWM off the
    // overflow is dropped at the margin and the damage cannot leave its row.
    let mut enable = Vec::new();
    enable_terminal_modes(&mut enable, true).expect("a Vec sink cannot fail");
    let enable = String::from_utf8_lossy(&enable).into_owned();
    assert!(
        enable.contains("\x1b[?7l"),
        "the enable block must disable autowrap: {enable:?}"
    );

    // …and the shell gets it back: a primary buffer with DECAWM off is unusable
    // (long command lines overtype themselves at the right margin).
    let mut restore = Vec::new();
    restore_sequence_inner(&mut restore, false);
    let restore = String::from_utf8_lossy(&restore).into_owned();
    assert!(
        restore.contains("\x1b[?7h"),
        "the restore sequence must re-enable autowrap: {restore:?}"
    );
    // On the PRIMARY buffer: the re-enable must land AFTER LeaveAlternateScreen
    // (`\x1b[?1049l`), or it would only restore the alt screen we are discarding.
    let leave = restore.find("\x1b[?1049l").expect("leaves the alt screen");
    let wrap_on = restore.find("\x1b[?7h").expect("re-enables autowrap");
    assert!(
        wrap_on > leave,
        "autowrap must be restored on the PRIMARY buffer, after the alt-screen leave"
    );
}

#[test]
fn focus_gain_reasserts_the_dec_modes_and_opens_the_heal_window() {
    // Windows Terminal / ConPTY STRIP DEC private modes while the window is
    // unfocused. Coming back, focus reporting (1004), bracketed paste (2004),
    // mouse capture and — now load-bearing — autowrap-OFF (?7l) may simply be
    // gone, so the very next ambiguous-width glyph would wrap and cascade again.
    // The focus-return reaction therefore re-asserts the WHOLE enable block
    // (idempotent, the same one startup uses) before it heals, and opens the
    // multi-frame heal window so the terminal's own settle-redraw can't win.
    let (mut terminal, tap) = recording_terminal(20, 3);
    let mut last_focus_gained_at = None;
    apply_focus_heal(&mut terminal, true, false, &mut last_focus_gained_at);
    let wire = tap.drain();

    assert!(
        wire.contains("\x1b[?7l"),
        "focus return must re-assert autowrap-OFF — ConPTY drops it while unfocused: {wire:?}"
    );
    assert!(
        wire.contains("\x1b[?1004h"),
        "…and focus reporting, or the NEXT focus return is never even delivered: {wire:?}"
    );
    assert!(
        wire.contains("\x1b[?2004h"),
        "…and bracketed paste: {wire:?}"
    );
    assert!(
        !wire.contains("\x1b[?1049h"),
        "focus return must not replay stateful alternate-screen entry: {wire:?}"
    );
    assert!(
        last_focus_gained_at.is_some_and(|t| t.elapsed() < FOCUS_HEAL_WINDOW),
        "focus return opens the heal window for the terminal's multi-frame settle"
    );
}

#[test]
fn focus_gain_re_fires_the_background_theme_query_when_allowed() {
    // Trigger #1 (the DOMINANT live-theme-switch case) — the user changed the
    // terminal theme (in prefs, or the OS flipped day/night) while the window was
    // unfocused, then returned. Focus-in must re-fire the OSC 11 background query
    // so the palette re-detects. With the guard satisfied (`allow = true`) the
    // query bytes hit the wire alongside the mode reassert + heal-window open.
    let (mut terminal, tap) = recording_terminal(20, 3);
    let mut last_focus_gained_at = None;
    apply_focus_heal(&mut terminal, true, true, &mut last_focus_gained_at);
    assert!(
        tap.drain().contains("\x1b]11;?"),
        "focus return must re-fire the OSC 11 background-color query"
    );

    // …and a pinned theme / non-owned reader (`allow = false`) still re-asserts
    // the DEC modes on focus return but sends NO query — the pin is respected.
    let (mut terminal, tap) = recording_terminal(20, 3);
    let mut last_focus_gained_at = None;
    apply_focus_heal(&mut terminal, true, false, &mut last_focus_gained_at);
    let wire = tap.drain();
    assert!(
        !wire.contains("\x1b]11;?"),
        "a pinned / non-owned theme must NOT be re-probed on focus return"
    );
    assert!(
        wire.contains("\x1b[?1004h"),
        "…but the focus-return mode reassert still runs regardless of the theme guard"
    );
}

#[test]
fn maybe_reprobe_background_only_writes_when_allowed_and_never_blocks() {
    // Trigger fail-open contract — the emit primitive is a best-effort write that
    // NEVER blocks: with `allow = true` the query is on the wire, with
    // `allow = false` nothing is written. A terminal that never answers simply
    // produces no reply (there is nothing here that awaits one), so a re-probe
    // against a silent terminal changes nothing and cannot stall the loop.
    let (mut terminal, tap) = recording_terminal(20, 3);
    maybe_reprobe_background(&mut terminal, true);
    assert!(
        tap.drain().contains("\x1b]11;?"),
        "an allowed re-probe emits the OSC 11 query"
    );
    let (mut terminal, tap) = recording_terminal(20, 3);
    maybe_reprobe_background(&mut terminal, false);
    assert!(
        tap.drain().is_empty(),
        "a disallowed re-probe emits nothing (guard respected, fail-open)"
    );
}

#[test]
fn should_reprobe_background_respects_owned_reader_and_the_pin() {
    // Only the OWNED reader can consume a reply, and a pinned palette (an explicit
    // UMADEV_THEME, or a manual `/theme light|dark` — both folded into `pinned` by
    // `theme_pinned`) must not even be queried — so NONE of the automatic triggers
    // can flip a palette the user pinned.
    assert!(
        should_reprobe_background(true, false),
        "owned + unpinned → probe"
    );
    assert!(
        !should_reprobe_background(false, false),
        "the legacy reader has no response lane → never probe"
    );
    assert!(
        !should_reprobe_background(true, true),
        "a pinned theme (UMADEV_THEME or /theme light|dark) → never probe on any trigger"
    );
}

#[test]
fn idle_background_reprobe_is_throttled_to_the_interval() {
    // Trigger #3 — the idle-tick safety net re-probes at a LOW cadence: the first
    // idle tick always probes, a second tick INSIDE the interval does not (two
    // ticks closer than the interval ⇒ exactly one query), and the next tick PAST
    // the interval probes again. Driven by the elapsed gap, the same way the
    // loop's other throttle predicates (`resume_gap_elapsed`, `preedit_cleanup_due`)
    // are tested — no real clock needed.
    assert!(
        theme_reprobe_due(None),
        "the first idle tick always re-probes"
    );
    assert!(
        !theme_reprobe_due(Some(THEME_REPROBE_INTERVAL / 2)),
        "a second tick inside the interval must NOT re-probe (throttled to one query)"
    );
    assert!(
        theme_reprobe_due(Some(THEME_REPROBE_INTERVAL)),
        "a tick at/after the interval re-probes again"
    );
}

#[test]
fn background_reply_flips_only_on_a_real_change_and_never_when_pinned() {
    // Trigger #4 + #5 — the reply-apply decision. A re-probe reply that MATCHES
    // the current classification is a NO-OP (no needless full repaint from the
    // idle re-probe); a reply that DIFFERS flips. And when the palette is pinned
    // (UMADEV_THEME or `/theme light|dark`, both folded into `pinned`), a reply of
    // the OPPOSITE theme is ignored — so a focus-in / idle re-probe can never
    // override the user's pin. Args: (reply_is_light, pinned, current_is_light);
    // current = dark here (`current_is_light = false`).
    assert!(
        background_reply_should_apply(true, false, false),
        "a differing reply (light vs current dark), unpinned → flip"
    );
    assert!(
        !background_reply_should_apply(false, false, false),
        "a reply that re-confirms the current theme → no-op (no repaint)"
    );
    assert!(
        !background_reply_should_apply(true, true, false),
        "the opposite theme is ignored while the palette is pinned (/theme or UMADEV_THEME)"
    );
}

#[test]
fn a_re_probe_that_never_gets_a_reply_changes_nothing() {
    // Fail-open — a re-probe against a terminal that never answers OSC 11 leaves
    // the source with no captured reply, so `apply_background_theme_reply` early-
    // returns: no palette change, no forced repaint, no panic, no block.
    use crate::input::reader::owned_test_source;
    let tmp = tempfile::TempDir::new().unwrap();
    let mut app = App::new(
        "offline",
        crate::config::UserConfig::default(),
        tmp.path().join("config.toml"),
        tmp.path().to_path_buf(),
    );
    let mut input = owned_test_source(None);
    let mut draw_now = false;
    apply_background_theme_reply(&mut app, &mut input, &mut draw_now);
    assert!(
        !draw_now,
        "a re-probe with no reply must not force a redraw (fail-open)"
    );
    assert!(
        !app.take_terminal_contaminated(),
        "…and must not contaminate the terminal / change the palette"
    );
}

#[test]
fn the_background_probe_runs_after_the_alternate_screen_is_up() {
    // A pre-alt capability query can stall ConPTY resize delivery. Lock the
    // startup order structurally; normalize CRLF for Windows checkouts.
    let source = include_str!("../lib.rs").replace("\r\n", "\n");
    let alt_screen = source
        .find("stdout.execute(EnterAlternateScreen)")
        .expect("setup_terminal enters the alternate screen once");
    let probe = source
        .find("request_background_color(terminal.backend_mut())")
        .expect("event_loop sends the OSC 11 query through its writer");
    assert!(
        alt_screen < probe,
        "alternate-screen entry must precede the OSC 11 query"
    );
}

#[test]
fn synchronized_output_brackets_are_emitted_unconditionally() {
    // DEC 2026 is a PRIVATE mode: a terminal that doesn't implement it silently
    // ignores the escape, and crossterm's Windows path has a literal no-op
    // `execute_winapi` for both. Emitting is therefore free — which is what makes
    // the whole env-allowlist + DECRQM-probe apparatus (deleted) unnecessary.
    // Locked here as a byte-level contract so nobody re-introduces a capability
    // gate around it.
    let mut buf = Vec::new();
    buf.execute(BeginSynchronizedUpdate)
        .expect("a Vec sink cannot fail");
    buf.execute(EndSynchronizedUpdate)
        .expect("a Vec sink cannot fail");
    let wire = String::from_utf8_lossy(&buf).into_owned();
    assert_eq!(
        wire, "\x1b[?2026h\x1b[?2026l",
        "BSU/ESU are 8 bytes each and are always safe to emit"
    );
}

#[test]
fn owned_focus_in_sequence_drives_a_full_repaint() {
    // End-to-end through the OWNED input pipeline (tokenizer → decoder): the
    // focus-in escape `\x1b[I` must decode to a focus-in event, which the reader
    // maps to `Event::FocusGained` and the event loop routes to
    // `App::contaminate_terminal` (P3) — one healing clear+repaint on return.
    // This is the owned-path (non-Windows default) counterpart to the native
    // `Event::FocusGained` the Windows `EventStream` delivers.
    use crate::input::decode::{Decoder, InputEvent};
    use crate::input::tokenize::Tokenizer;
    let mut tk = Tokenizer::for_stdin();
    let mut dec = Decoder::new();
    let mut got_focus_in = false;
    for token in tk.feed(b"\x1b[I") {
        for ev in dec.feed_token(token) {
            if ev == InputEvent::Focus(true) {
                got_focus_in = true;
            }
        }
    }
    assert!(
        got_focus_in,
        "the owned tokenizer decodes CSI I (`\\x1b[I`) to a focus-in event"
    );
}

#[test]
fn setup_enables_and_restore_disables_focus_change_reporting() {
    use crossterm::ExecutableCommand as _;
    // Setup turns focus-change reporting ON via `EnableFocusChange`
    // (DEC private mode 1004 = `\x1b[?1004h`), the exact escape `setup_terminal`
    // writes so the terminal reports focus in/out.
    let mut enable_buf: Vec<u8> = Vec::new();
    let _ = enable_buf.execute(EnableFocusChange);
    assert!(
        String::from_utf8_lossy(&enable_buf).contains("\x1b[?1004h"),
        "setup must enable focus-change reporting (mode 1004h)"
    );
    // Teardown / panic hook / mid-setup failure all route through
    // `restore_sequence`, which must turn it back OFF symmetrically so focus
    // reports never leak as `\x1b[I` / `\x1b[O` text at the restored shell.
    let mut restore_buf: Vec<u8> = Vec::new();
    restore_sequence(&mut restore_buf);
    assert!(
        String::from_utf8_lossy(&restore_buf).contains("\x1b[?1004l"),
        "restore must disable focus-change reporting (mode 1004l)"
    );
}

#[test]
fn r5_gap_detection_trips_only_past_the_threshold() {
    let threshold = Duration::from_secs(5);
    // A long gap (sleep/wake / re-attach) → reassert.
    assert!(
        resume_gap_elapsed(Duration::from_secs(5), threshold),
        "a gap at the threshold trips the reassert"
    );
    assert!(
        resume_gap_elapsed(Duration::from_secs(30), threshold),
        "a long gap trips the reassert"
    );
    // Normal typing cadence → no reassert.
    assert!(
        !resume_gap_elapsed(Duration::from_millis(200), threshold),
        "normal typing never trips the reassert"
    );
    assert!(
        !resume_gap_elapsed(Duration::from_secs(4), threshold),
        "a sub-threshold gap never trips the reassert"
    );
}

#[test]
fn settle_edge_contaminates_the_terminal_once() {
    // The live→settled true→false edge (a turn/run just ended) contaminates
    // the terminal so the final settled frame gets ONE clean full repaint on
    // a non-sync terminal — the drift a long streaming run accumulated must
    // not freeze on screen. Exercised through the same App flag the event
    // loop drains; steady states never contaminate.
    let (app, _tmp) = build_test_app();
    // The loop-top edge detector: contaminate ONLY on true→false.
    for (was, now, expect) in [
        (true, false, true),   // the settling edge → one heal
        (true, true, false),   // steady live run → no thrash
        (false, false, false), // steady idle → no thrash
        (false, true, false),  // starting a turn is not a settle
    ] {
        if was && !now {
            app.contaminate_terminal();
        }
        assert_eq!(
            app.take_terminal_contaminated(),
            expect,
            "was_live={was} now_live={now}"
        );
    }
    // And the drain is one-shot: the healing repaint fires exactly once.
    assert!(
        !app.take_terminal_contaminated(),
        "contamination drains once"
    );
}

#[test]
fn idle_animation_tick_does_not_force_a_redraw() {
    let (mut app, _tmp) = build_test_app();

    assert!(
        !tick_needs_draw(&app, false),
        "a settled chat should not repaint every 80ms tick while the user reads scrollback"
    );

    app.thinking = true;
    assert!(
        tick_needs_draw(&app, false),
        "a live thinking spinner still needs tick-driven redraws"
    );
    app.thinking = false;

    app.register_run_task("long build");
    assert!(
        tick_needs_draw(&app, false),
        "a running background task keeps elapsed/status animation fresh"
    );
}

#[test]
fn transcript_reflow_repaints_on_rebase_and_shrink_but_not_steady_growth() {
    // The `MAX_RENDER_ROWS` front-trim FIRST crosses in (prev_cut 0 → cut > 0):
    // the whole retained window re-based → repaint once on the crossing.
    assert!(
        transcript_reflow_needs_repaint(8000, 8000, 0, 50),
        "the MAX_RENDER_ROWS split re-base forces a repaint"
    );
    // Already trimming and the trim merely advances by a row (cut 50 → 51) with
    // the total capped: the painted tail is identical → NO repaint (no thrash
    // over a marathon streaming run).
    assert!(
        !transcript_reflow_needs_repaint(8000, 8000, 50, 51),
        "a per-row trim advance past the cap does not thrash the repaint"
    );
    // The transcript SHRANK (a fold/collapse toggle, `/compact`, `/clear`, or
    // the live indicator removed at settle) → repaint (vacated rows below).
    assert!(
        transcript_reflow_needs_repaint(500, 480, 0, 0),
        "a transcript shrink forces a repaint"
    );
    // Steady bottom-pinned streaming GROWTH (total climbs, no trim yet) → the
    // diff paints the new tail cleanly → NO repaint.
    assert!(
        !transcript_reflow_needs_repaint(500, 512, 0, 0),
        "steady streaming growth never forces a repaint"
    );
    // A first frame (prev_total 0 → some) is growth, not a shrink → no repaint.
    assert!(
        !transcript_reflow_needs_repaint(0, 300, 0, 0),
        "the first populated frame does not spuriously repaint"
    );
}

#[test]
fn resume_gap_honors_env_override_and_floor() {
    // Default when unset.
    let _resume = EnvRestore::remove("UMADEV_RESUME_GAP_SECS");
    assert_eq!(
        resume_gap(),
        Duration::from_secs(5),
        "default resume gap 5s"
    );
    // A valid override is honored.
    std::env::set_var("UMADEV_RESUME_GAP_SECS", "10");
    assert_eq!(resume_gap(), Duration::from_secs(10), "resume override 10s");
    // Garbage is rejected by the `>= 1` floor → falls back to the default,
    // so a misconfig can't thrash the mode reassert on every keystroke.
    std::env::set_var("UMADEV_RESUME_GAP_SECS", "nonsense");
    assert_eq!(
        resume_gap(),
        Duration::from_secs(5),
        "garbage resume gap floors back to the default"
    );
}
