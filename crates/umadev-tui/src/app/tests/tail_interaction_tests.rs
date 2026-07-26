use super::*;

// ── Ctrl+click → open URL / file under the cursor ─────────────────────

/// Seed a wide viewport whose row 0 holds a URL mid-sentence, pinned to
/// the top (no scroll), no gutters — so screen col == char index.
fn seed_link_geometry(a: &App, rows: &[&str]) {
    *a.transcript_rows.borrow_mut() = rows.iter().map(|s| (*s).to_string()).collect();
    a.transcript_gutters.borrow_mut().clear();
    a.transcript_row_wraps.borrow_mut().clear();
    a.transcript_area.set((0, 0, 60, 4));
    a.transcript_max_scroll.set(0);
    a.set_transcript_scroll(0);
    a.transcript_first_visible.set(0);
}

#[test]
fn ctrl_click_resolves_the_url_under_the_cursor() {
    let a = fresh_app(Some("offline"));
    seed_link_geometry(&a, &["preview at http://127.0.0.1:4173/ now"]);
    // Click on the '1' of `127` (screen col 18, row 0).
    assert_eq!(
        a.link_target_at(18, 0).as_deref(),
        Some("http://127.0.0.1:4173/"),
        "the URL under the cursor is recovered, boundary-trimmed"
    );
    // A click on plain prose hits nothing (silent no-op upstream).
    assert_eq!(a.link_target_at(2, 0), None, "plain words are not links");
    // A click past the end of the text hits nothing either.
    assert_eq!(
        a.link_target_at(55, 0),
        None,
        "blank right margin is a miss"
    );
}

#[test]
fn ctrl_click_resolves_only_existing_paths() {
    let a = fresh_app(Some("offline"));
    // A real file in the workspace + a non-existent sibling on row 1.
    std::fs::write(a.project_root.join("proof.png"), b"x").unwrap();
    seed_link_geometry(&a, &["shot: proof.png done", "shot: missing.png done"]);
    let hit = a
        .link_target_at(8, 0)
        .expect("existing workspace file resolves");
    assert!(
        hit.ends_with("proof.png"),
        "resolved to the canonicalized real file: {hit}"
    );
    assert_eq!(
        a.link_target_at(8, 1),
        None,
        "a path token that does not exist is a miss, not an open"
    );
}

#[test]
fn link_click_miss_is_silent_and_suppresses_the_gesture() {
    let mut a = fresh_app(Some("offline"));
    seed_link_geometry(&a, &["no links in this row at all"]);
    let before = a.history.len();
    a.link_click_open(3, 0);
    assert!(
        a.link_click_pending,
        "the gesture is armed even on a miss (drag/up stay suppressed)"
    );
    assert_eq!(a.history.len(), before, "a miss posts no status note");
    assert!(
        a.selection.is_none(),
        "no selection is opened by ctrl+click"
    );
    // The event loop clears the flag on mouse-up; a PLAIN click afterwards
    // must start a selection exactly as before (behavior unchanged).
    a.link_click_pending = false;
    a.selection_begin(3, 0);
    assert!(
        a.selection.is_some(),
        "plain click still begins a selection"
    );
    assert!(a.selection_dragging, "and still opens a drag");
    assert!(
        !a.link_click_pending,
        "a plain click never arms the link gesture"
    );
}

#[test]
fn ctrl_click_reads_a_url_across_soft_wrapped_rows() {
    let a = fresh_app(Some("offline"));
    // One logical line folded over two visual rows mid-URL.
    seed_link_geometry(&a, &["see http://127.0.0.1:41", "73/ done"]);
    *a.transcript_row_wraps.borrow_mut() = vec![false, true];
    // Click on the second visual row's `7` (screen col 0, row 1): the
    // rejoined logical line yields the WHOLE url.
    assert_eq!(
        a.link_target_at(0, 1).as_deref(),
        Some("http://127.0.0.1:4173/"),
        "soft-wrapped URL rejoins before extraction"
    );
}

#[test]
fn handle_paste_inserts_a_multiline_block_verbatim() {
    // The legacy + owned paths both end at `handle_paste` with the full text
    // (crossterm `Event::Paste` / a decoded bracketed paste). A small
    // multi-line paste must land in the input box as ONE block — embedded
    // newlines kept, nothing dropped — not fragmented into submitted lines.
    let mut a = fresh_app(Some("offline"));
    a.handle_paste("first line\nsecond line");
    assert_eq!(a.input, "first line\nsecond line");
    assert_eq!(a.input_cursor, a.input_len());
}

#[test]
fn drag_past_bottom_edge_auto_scrolls_and_extends() {
    let mut a = fresh_app(Some("offline"));
    seed_transcript_geometry(&a);
    // Scroll all the way UP first so rows 0..3 are visible and there is room
    // to auto-scroll DOWN toward the newer rows.
    a.set_transcript_scroll(6);
    a.transcript_first_visible.set(0);
    a.selection_begin(0, 0); // anchor at content row 0
    assert_eq!(a.selection.unwrap().anchor, (0, 0));
    // Drag STRICTLY below the bottom edge (screen row 4 == top+height): one
    // auto-scroll step downward + the end pins to the freshly revealed row.
    a.selection_extend(0, 4);
    assert_eq!(
        a.transcript_scroll(),
        5,
        "dragging past the bottom auto-scrolls one step"
    );
    assert_eq!(
        a.selection.unwrap().cursor.0,
        4,
        "the end extends to the row pulled into view below the old viewport",
    );
}

#[test]
fn slash_history_opens_overlay_with_messages() {
    let mut a = fresh_app(Some("offline"));
    for c in "/history".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let _ = a.apply_key(KeyCode::Enter);
    let ov = a.overlay.as_ref().unwrap();
    assert!(ov
        .lines
        .iter()
        .any(|l| l.contains("[umadev]") || l.contains("[system]")));
}

#[test]
fn overlay_esc_closes() {
    let mut a = fresh_app(Some("offline"));
    for c in "/spec".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let _ = a.apply_key(KeyCode::Enter);
    assert!(a.overlay.is_some());
    // Esc should close, NOT quit, when an overlay is open.
    let action = a.apply_key(KeyCode::Esc);
    assert_eq!(action, Action::None);
    assert!(a.overlay.is_none());
    assert!(!a.should_quit);
}

#[test]
fn overlay_scroll_keys() {
    let mut a = fresh_app(Some("offline"));
    for c in "/spec".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let _ = a.apply_key(KeyCode::Enter);
    // A real frame publishes `max_scroll` (the top-most reachable VISUAL row)
    // before any key is handled; simulate that so scroll_down has room to move.
    a.overlay.as_ref().unwrap().max_scroll.set(100);
    let initial = a.overlay.as_ref().unwrap().scroll;
    // Down + PageDown advance.
    let _ = a.apply_key(KeyCode::Down);
    assert!(a.overlay.as_ref().unwrap().scroll > initial);
    let after_j = a.overlay.as_ref().unwrap().scroll;
    let _ = a.apply_key(KeyCode::PageDown);
    assert!(a.overlay.as_ref().unwrap().scroll > after_j);
    // Up rewinds.
    let _ = a.apply_key(KeyCode::Up);
    // Home resets to 0.
    let _ = a.apply_key(KeyCode::Home);
    assert_eq!(a.overlay.as_ref().unwrap().scroll, 0);
    // End jumps to the published last reachable row (not a logical-line guess).
    let _ = a.apply_key(KeyCode::End);
    assert_eq!(a.overlay.as_ref().unwrap().scroll, 100);
}

#[test]
fn overlay_wheel_scrolls_overlay_not_transcript() {
    let mut a = fresh_app(Some("offline"));
    // A tall transcript so a mis-routed wheel WOULD visibly move it.
    a.transcript_max_scroll.set(100);
    a.set_transcript_scroll(0);
    // Open an overlay (taller than the viewport — publish a non-zero max).
    for c in "/spec".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let _ = a.apply_key(KeyCode::Enter);
    assert!(a.overlay.is_some());
    a.overlay.as_ref().unwrap().max_scroll.set(100);
    // Wheel DOWN with the overlay open scrolls the OVERLAY, not the transcript.
    assert!(a.mouse_wheel(false, 3));
    assert_eq!(a.overlay.as_ref().unwrap().scroll, 3);
    assert_eq!(
        a.transcript_scroll(),
        0,
        "transcript stays pinned while an overlay is open"
    );
    // PageDown (key path) advances further; End clamps to the published last row.
    let _ = a.apply_key(KeyCode::PageDown);
    assert!(a.overlay.as_ref().unwrap().scroll > 3);
    let _ = a.apply_key(KeyCode::End);
    assert_eq!(a.overlay.as_ref().unwrap().scroll, 100, "End clamps to max");
    // Wheeling past the end stays clamped — never overruns the last visual row.
    assert!(a.mouse_wheel(false, 50));
    assert_eq!(a.overlay.as_ref().unwrap().scroll, 100);
    // The modal owns the wheel even when the `/mouse` toggle is OFF.
    a.mouse_scroll = false;
    let _ = a.apply_key(KeyCode::Home);
    assert_eq!(a.overlay.as_ref().unwrap().scroll, 0);
    a.overlay.as_ref().unwrap().max_scroll.set(100);
    assert!(a.mouse_wheel(false, 5));
    assert_eq!(
        a.overlay.as_ref().unwrap().scroll,
        5,
        "overlay scrolls regardless of the /mouse wheel-capture toggle"
    );
    // With the overlay CLOSED, the wheel falls back to the transcript.
    a.overlay = None;
    a.mouse_scroll = true;
    a.transcript_max_scroll.set(100);
    a.set_transcript_scroll(0);
    assert!(a.mouse_wheel(true, 4));
    assert_eq!(
        a.transcript_scroll(),
        4,
        "no overlay → the wheel scrolls the transcript"
    );
}

#[test]
fn slash_plan_includes_full_team_review_section() {
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::PlanPosted {
        statuses: vec![],
        steps: vec![
            "s1 · scaffold (frontend)".into(),
            "s2 · login route (backend)".into(),
        ],
        done: 1,
        total: 2,
    });
    a.apply_engine(EngineEvent::CriticVerdict {
        seat: "architect".into(),
        accepts: true,
        blocking: vec![],
        remediation: vec![],
        advisory: vec!["consider a cache".into()],
    });
    a.apply_engine(EngineEvent::CriticVerdict {
        seat: "qa".into(),
        accepts: false,
        blocking: vec!["no tests for login".into(), "no error handling".into()],
        remediation: vec![],
        advisory: vec![],
    });
    assert_eq!(a.critic_verdicts.len(), 2);
    let before = a.history.len();
    let action = a.try_slash_command("/plan").unwrap();
    assert_eq!(action, Action::None);
    let out: String = a
        .history
        .iter()
        .skip(before)
        .map(|m| m.body().clone())
        .collect();
    // Plan steps still render.
    assert!(out.contains("s1") && out.contains("s2"), "plan steps shown");
    // EVERY seat's verdict is listed (truthful "/plan for all").
    assert!(out.contains("[architect]"), "accepting seat shown: {out}");
    assert!(out.contains("[qa]"), "blocking seat shown: {out}");
    // A blocking seat's FULL findings are listed, not just the first.
    assert!(out.contains("no tests for login"), "first finding: {out}");
    assert!(out.contains("no error handling"), "second finding: {out}");
}

#[test]
fn team_command_registered_and_dispatchable() {
    // Wave C: `/team` is one registry row (so the palette + help advertise it)
    // AND has a dispatch arm — the lockstep parity test relies on both.
    assert!(
        App::COMMANDS.iter().any(|c| c.name == "team"),
        "/team is in COMMANDS"
    );
    assert!(
        dispatch_arm_verbs().iter().any(|v| v == "team"),
        "/team has a dispatch arm"
    );
}

#[test]
fn slash_team_no_run_shows_roster_and_convene_hint() {
    // No plan, no verdicts, no output dir → roster + the "convenes on a build"
    // hint, never the run section.
    let mut a = fresh_app(Some("offline"));
    let before = a.history.len();
    let action = a.try_slash_command("/team").unwrap();
    assert_eq!(action, Action::None);
    let out: String = a
        .history
        .iter()
        .skip(before)
        .map(|m| m.body().clone())
        .collect();
    // Roster: the title + every seat's role→deliverable line is present.
    assert!(
        out.contains(umadev_i18n::t(a.lang, "team.title")),
        "title: {out}"
    );
    for key in TEAM_ROSTER {
        assert!(
            out.contains(umadev_i18n::t(a.lang, key)),
            "roster row {key} present: {out}"
        );
    }
    // No run context → the convene hint, NOT the run header.
    assert!(
        out.contains(umadev_i18n::t(a.lang, "team.no_run")),
        "hint: {out}"
    );
    assert!(
        !out.contains(umadev_i18n::t(a.lang, "team.run.header")),
        "no run section without context: {out}"
    );
}

#[test]
fn slash_team_with_verdicts_shows_per_seat_verdicts() {
    // Recorded critic verdicts are run context. With NO plan steps the
    // convened roster is empty, so the run section falls back to naming each
    // reviewing seat (by its short display name) with its verdict.
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::CriticVerdict {
        seat: "architect".into(),
        accepts: true,
        blocking: vec![],
        remediation: vec![],
        advisory: vec![],
    });
    a.apply_engine(EngineEvent::CriticVerdict {
        seat: "qa".into(),
        accepts: false,
        blocking: vec!["no tests for login".into()],
        remediation: vec![],
        advisory: vec![],
    });
    let before = a.history.len();
    let _ = a.try_slash_command("/team").unwrap();
    let out: String = a
        .history
        .iter()
        .skip(before)
        .map(|m| m.body().clone())
        .collect();
    assert!(
        out.contains(umadev_i18n::t(a.lang, "team.run.header")),
        "run header: {out}"
    );
    assert!(
        out.contains(&seat_display_name(a.lang, "architect")),
        "accepting seat: {out}"
    );
    assert!(
        out.contains(&seat_display_name(a.lang, "qa")),
        "blocking seat: {out}"
    );
    // The verdict wording rides along (accept + must-fix).
    assert!(out.contains(umadev_i18n::t(a.lang, "plan.review.accept")));
}

#[test]
fn slash_team_reports_produced_vs_pending_deliverables() {
    // A deliverable that exists on disk renders `produced`; one that does not
    // renders `pending`. Need run context for the deliverables block to show.
    let mut a = fresh_app(Some("offline"));
    let out_dir = a.project_root.join("output");
    std::fs::create_dir_all(&out_dir).unwrap();
    std::fs::write(out_dir.join("demo-prd.md"), "# PRD").unwrap();
    a.apply_engine(EngineEvent::CriticVerdict {
        seat: "pm".into(),
        accepts: true,
        blocking: vec![],
        remediation: vec![],
        advisory: vec![],
    });
    let before = a.history.len();
    let _ = a.try_slash_command("/team").unwrap();
    let out: String = a
        .history
        .iter()
        .skip(before)
        .map(|m| m.body().clone())
        .collect();
    let produced = umadev_i18n::t(a.lang, "team.run.produced");
    let pending = umadev_i18n::t(a.lang, "team.run.pending");
    let prd = umadev_i18n::t(a.lang, "team.deliverable.prd");
    let deploy = umadev_i18n::t(a.lang, "team.deliverable.deploy");
    // The written PRD shows produced; the absent deploy proof shows pending.
    assert!(
        out.contains(&format!("{produced} {prd}")),
        "PRD produced: {out}"
    );
    assert!(
        out.contains(&format!("{pending} {deploy}")),
        "deploy proof pending: {out}"
    );
}

#[test]
fn constitution_command_registered_and_dispatchable() {
    // Wave C: `/constitution` is one registry row (palette + help advertise it)
    // AND has a dispatch arm — the lockstep parity test relies on both.
    assert!(
        App::COMMANDS.iter().any(|c| c.name == "constitution"),
        "/constitution is in COMMANDS"
    );
    assert!(
        dispatch_arm_verbs().iter().any(|v| v == "constitution"),
        "/constitution has a dispatch arm"
    );
    // The `/charter` alias resolves back to it.
    assert_eq!(
        App::resolve_command("charter").map(|c| c.name),
        Some("constitution")
    );
}

#[test]
fn slash_constitution_generates_and_shows_the_charter() {
    // First use with no file → generate the charter, open it in the overlay
    // with the real non-negotiables, and note where the user can edit it.
    let mut a = fresh_app(Some("offline"));
    let before = a.history.len();
    let action = a.try_slash_command("/constitution").unwrap();
    assert_eq!(action, Action::None);
    // The charter is shown in the overlay and names the enforced rules.
    let body = a
        .overlay
        .as_ref()
        .expect("charter overlay opened")
        .lines
        .join("\n");
    assert!(body.contains("UD-CODE-001"), "charter shown: {body}");
    // The file was actually generated on disk and not clobbered on a re-open.
    let path = a.project_root.join(umadev_agent::constitution_rel_path());
    assert!(path.exists(), "charter file generated");
    // A System note tells the user where to edit it (path surfaced).
    let notes: String = a
        .history
        .iter()
        .skip(before)
        .map(|m| m.body().clone())
        .collect();
    assert!(
        notes.contains(&path.display().to_string()),
        "edit hint names the path: {notes}"
    );
}

#[test]
fn slash_constitution_shows_a_user_edited_file_without_clobbering() {
    // An existing (user-edited) charter is shown verbatim and never rewritten.
    let mut a = fresh_app(Some("offline"));
    let path = a.project_root.join(umadev_agent::constitution_rel_path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let edited = "# Our rules\n\n- We pair on every PR.\n";
    std::fs::write(&path, edited).unwrap();
    let _ = a.try_slash_command("/constitution").unwrap();
    let body = a
        .overlay
        .as_ref()
        .expect("charter overlay opened")
        .lines
        .join("\n");
    assert!(body.contains("pair on every PR"), "user file shown: {body}");
    // On disk it is untouched.
    assert_eq!(std::fs::read_to_string(&path).unwrap(), edited);
}

#[test]
fn host_output_groups_into_single_bubble() {
    let mut a = fresh_app(Some("offline"));
    let before = a.history.len();
    a.apply_engine(EngineEvent::HostOutput {
        phase: Phase::Research,
        line: "# header".into(),
    });
    a.apply_engine(EngineEvent::HostOutput {
        phase: Phase::Research,
        line: "## section".into(),
    });
    a.apply_engine(EngineEvent::HostOutput {
        phase: Phase::Research,
        line: "body line".into(),
    });
    // All three lines collapse into one Host message.
    let host_msgs: Vec<_> = a
        .history
        .iter()
        .skip(before)
        .filter(|m| m.role == ChatRole::Host)
        .collect();
    assert_eq!(host_msgs.len(), 1);
    let body = &host_msgs[0].body();
    assert!(body.contains("# header"));
    assert!(body.contains("## section"));
    assert!(body.contains("body line"));
}

#[test]
fn host_output_starts_new_bubble_after_umadev_break() {
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::HostOutput {
        phase: Phase::Research,
        line: "research line".into(),
    });
    // A UmaDev message between the two streams must break the group.
    a.apply_engine(EngineEvent::PhaseCompleted {
        phase: Phase::Research,
    });
    a.apply_engine(EngineEvent::HostOutput {
        phase: Phase::Docs,
        line: "docs line".into(),
    });
    let host_msgs: Vec<_> = a
        .history
        .iter()
        .filter(|m| m.role == ChatRole::Host)
        .collect();
    assert_eq!(host_msgs.len(), 2);
}

#[test]
fn status_bar_contains_phase_dots() {
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::PhaseStarted {
        phase: Phase::Research,
    });
    // Phase progress is a compact geometric bar after the backend label.
    // With research running (first of 9): ◐○○○○○○○○ 0/9.
    assert!(a.status.contains("◐○○○○○○○○"));
    assert!(a.status.contains("0/9"));
}

#[test]
fn status_bar_dots_advance_as_phases_complete() {
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::PhaseStarted {
        phase: Phase::Research,
    });
    a.apply_engine(EngineEvent::PhaseCompleted {
        phase: Phase::Research,
    });
    a.apply_engine(EngineEvent::PhaseStarted { phase: Phase::Docs });
    // After research done + docs running: ●◐○○○○○○○ 1/9.
    assert!(a.status.contains("●◐○○○○○○○"));
    assert!(a.status.contains("1/9"));
}

#[test]
fn spinner_cycles() {
    let mut a = fresh_app(Some("offline"));
    let first = a.spinner();
    // 10 braille frames × 2 ticks each = 20 ticks per cycle.
    for _ in 0..20 {
        a.tick();
    }
    assert_eq!(a.spinner(), first);
}

#[test]
fn p5c_thinking_collapses_to_one_summary_row() {
    // P5c: a burst of Thinking events opens exactly ONE placeholder row; the
    // next real content collapses it to a single `正在思考… · N.Ns` summary
    // instead of leaving a stack of orphan `[thinking]` rows.
    let mut a = fresh_app(Some("offline"));
    let before = a.history.len();
    for _ in 0..5 {
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::Thinking,
        });
    }
    // Only ONE placeholder row was pushed despite five Thinking events.
    assert_eq!(
        a.history.len(),
        before + 1,
        "a thinking burst must not stack rows"
    );
    let placeholder_idx = a.history.len() - 1;
    assert!(a
        .history
        .back()
        .unwrap()
        .body()
        .contains(THINKING_PLACEHOLDER_TAG));
    assert!(a.thinking_block_idx.is_some(), "a reasoning block is open");
    // Real content arrives → the placeholder collapses to a summary in place
    // (no new row added for the collapse itself).
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::Text {
            delta: "here is the answer".into(),
        },
    });
    assert!(a.thinking_block_idx.is_none(), "block closed after content");
    let collapsed = a.history.get(placeholder_idx).unwrap().body().into_owned();
    assert!(
        !collapsed.contains(THINKING_PLACEHOLDER_TAG),
        "placeholder tag is gone after collapse: {collapsed:?}"
    );
    // The summary carries the thinking label + a seconds figure (`· N.Ns`).
    assert!(
        collapsed.contains('·') && collapsed.contains('s'),
        "summary shows elapsed seconds: {collapsed:?}"
    );
}

#[test]
fn p5c_collapse_failopen_without_timing() {
    // Fail-open: if the block-start timestamp is missing, the collapse still
    // rewrites the placeholder (to a no-seconds completion marker), never
    // leaving an orphan `[thinking]` row.
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::Thinking,
    });
    let idx = a.thinking_block_idx.unwrap();
    a.thinking_block_start = None; // simulate lost timing
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ToolResult {
            ok: true,
            summary: "done".into(),
        },
    });
    let row = a.history.get(idx).unwrap().body().into_owned();
    assert!(
        !row.contains(THINKING_PLACEHOLDER_TAG),
        "placeholder still collapsed without timing: {row:?}"
    );
}

#[test]
fn thinking_deltas_accumulate_into_one_collapsed_block() {
    // Phase-2-C-P0: a stream of reasoning deltas must build ONE foldable
    // `[thinking]` block (not a row per delta), default collapsed, and the
    // reasoning text must be preserved in that single row.
    let mut a = fresh_app(Some("offline"));
    let before = a.history.len();
    for chunk in ["Let me ", "think about ", "the architecture."] {
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::ThinkingDelta(chunk.into()),
        });
    }
    // Exactly ONE new row despite three deltas.
    assert_eq!(
        a.history.len(),
        before + 1,
        "reasoning deltas must fold into one block, not a row per delta"
    );
    let idx = a.thinking_block_idx.expect("a reasoning block is open");
    let body = a.history.get(idx).unwrap().body().into_owned();
    assert!(
        body.starts_with(THINKING_PLACEHOLDER_TAG),
        "header tag: {body:?}"
    );
    assert!(
        body.contains("Let me think about the architecture."),
        "the full reasoning text is accumulated: {body:?}"
    );
    // Default collapsed, and recognized as a foldable reasoning block.
    let msg = a.history.get(idx).unwrap();
    assert!(msg.collapsed, "the reasoning block defaults to collapsed");
    assert!(
        crate::app::is_thinking_reasoning_block(msg.role, body.as_str()),
        "row is a foldable [thinking] reasoning block"
    );
    // Real content closes the block but KEEPS the reasoning + the expandable tag.
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::Text {
            delta: "Here is the plan.".into(),
        },
    });
    assert!(a.thinking_block_idx.is_none(), "block closed after content");
    let after = a.history.get(idx).unwrap();
    let after_body = after.body().into_owned();
    assert!(
        after_body.starts_with(THINKING_PLACEHOLDER_TAG),
        "a reasoning block keeps its expandable tag after collapse: {after_body:?}"
    );
    assert!(
        after_body.contains("Let me think about the architecture."),
        "the reasoning survives collapse so it can be expanded: {after_body:?}"
    );
    assert!(after.collapsed, "still collapsed (expand with Ctrl+O)");
}

#[test]
fn thinking_delta_at_history_cap_never_corrupts_a_shifted_row() {
    // Fix 3 — `thinking_block_idx` is an ABSOLUTE `history` index. While a
    // reasoning block is open, a non-collapsing push at `HISTORY_CAP`
    // `pop_front`s the front row and shifts every index down by one, so the
    // stored idx lands on an UNRELATED row. The delta-append must re-validate
    // the placeholder tag (like the collapse path) and never write reasoning
    // into that shifted-onto row.
    let mut a = fresh_app(Some("offline"));
    // Fill history to exactly the cap so the NEXT push evicts a front row.
    for i in 0..HISTORY_CAP {
        a.push(ChatRole::System, format!("filler {i}"));
    }
    assert_eq!(a.history.len(), HISTORY_CAP);
    // Open a reasoning block + accumulate one delta (opening pushes the
    // placeholder at the back, evicting one filler).
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ThinkingDelta("first reasoning".into()),
    });
    let stale_idx = a.thinking_block_idx.expect("a reasoning block is open");
    // A non-collapsing push AT cap shifts every index down by one, so
    // `stale_idx` now points at THIS new (unrelated) row.
    a.push(ChatRole::You, "a new user line");
    let shifted = a.history.get(stale_idx).unwrap();
    assert_eq!(
        shifted.role,
        ChatRole::You,
        "the stored index was shifted onto an unrelated row"
    );
    let before = shifted.body().into_owned();
    // A further reasoning delta must NOT be appended into that unrelated row.
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ThinkingDelta("STRAY".into()),
    });
    let after = a.history.get(stale_idx).unwrap().body().into_owned();
    assert_eq!(
        before, after,
        "a reasoning delta must not corrupt the shifted-onto row"
    );
    assert!(
        !after.contains("STRAY"),
        "no reasoning leaked into the user row"
    );
    // Self-heal: the stale index was dropped, and the NEXT delta re-opens a
    // fresh reasoning block rather than chasing the moved row.
    assert!(a.thinking_block_idx.is_none(), "the stale index is dropped");
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ThinkingDelta("fresh".into()),
    });
    let new_idx = a
        .thinking_block_idx
        .expect("a fresh reasoning block re-opened");
    let new_body = a.history.get(new_idx).unwrap().body().into_owned();
    assert!(
        new_body.starts_with(THINKING_PLACEHOLDER_TAG) && new_body.contains("fresh"),
        "the next delta re-opens a fresh reasoning block: {new_body:?}"
    );
}

#[test]
fn a_turn_with_no_thinking_shows_no_reasoning_block() {
    // A plain answer with no reasoning deltas must add NO `[thinking]` block.
    let mut a = fresh_app(Some("offline"));
    let before = a.history.len();
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::Text {
            delta: "just an answer".into(),
        },
    });
    assert!(a.thinking_block_idx.is_none(), "no block opened");
    assert!(
        a.history
            .iter()
            .skip(before)
            .all(|m| !m.body().contains(THINKING_PLACEHOLDER_TAG)),
        "no [thinking] row when the turn never reasoned"
    );
}

#[test]
fn p5d_spinner_frame_static_when_animations_off() {
    // P5d: animations off → a single static glyph, never a strobing frame.
    for tick in 0..30u8 {
        assert_eq!(
            spinner_frame(tick, false, false),
            SPINNER_STATIC,
            "animations off must be static at tick {tick}"
        );
    }
}

#[test]
fn p5d_spinner_frame_freezes_on_stall() {
    // P5d: a stall FREEZES the spinner on one frame (the status surface paints
    // it the warning color) — it must not keep fake-spinning.
    let frozen = spinner_frame(0, true, true);
    for tick in 0..30u8 {
        assert_eq!(
            spinner_frame(tick, true, true),
            frozen,
            "stalled spinner must not advance (tick {tick})"
        );
    }
    // And while NOT stalled it does advance through the braille frames.
    let mut seen = std::collections::HashSet::new();
    for tick in 0..10u8 {
        seen.insert(spinner_frame(tick, true, false));
    }
    assert_eq!(seen.len(), SPINNER_FRAMES.len(), "all frames appear");
}

#[test]
fn p5d_app_spinner_uses_shared_frames() {
    // The App-level spinner funnels through the shared frame source, so a
    // non-animated app shows the static glyph and an animated one rotates.
    let mut a = fresh_app(Some("offline"));
    a.animations = false;
    assert_eq!(a.spinner(), SPINNER_STATIC);
    a.animations = true;
    assert_eq!(
        a.spinner(),
        SPINNER_FRAMES[a.tick as usize % SPINNER_FRAMES.len()]
    );
}

#[test]
fn running_circle_animates_through_its_frames() {
    // The in-progress phase circle must ROTATE (◐◓◑◒) as the tick advances,
    // not sit static — that rotation is what proves the bar is alive even
    // when the bottom-bar spinner is off-attention. One frame per 2 ticks.
    let mut a = fresh_app(Some("offline"));
    assert_eq!(a.running_circle(), '◐', "frame 0 at tick 0");
    let mut seen = std::collections::HashSet::new();
    for _ in 0..8 {
        seen.insert(a.running_circle());
        a.tick();
        a.tick(); // advance one full circle frame (~160ms)
    }
    // All four quarter-circle glyphs must appear as it rotates.
    for g in ['◐', '◓', '◑', '◒'] {
        assert!(seen.contains(&g), "running circle must show {g}: {seen:?}");
    }
}

#[test]
fn running_phase_circle_in_status_bar_rotates() {
    // The progress bar (in app.status) shows the rotating circle for the
    // running phase, not a frozen ◐.
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::PhaseStarted {
        phase: Phase::Research,
    });
    // tick=0 → ◐ (frame 0).
    assert!(a.status.contains('◐'), "tick 0 shows ◐: {}", a.status);
    // Advance two ticks (one circle frame) → the running glyph becomes ◓.
    a.tick();
    a.tick();
    assert!(
        a.status.contains('◓'),
        "after 2 ticks the running circle rotates to ◓: {}",
        a.status
    );
}

#[test]
fn stall_after_threshold_then_clears_on_output() {
    // Honest stall signal: a running phase with no output past the 60s
    // threshold reads as stalled (status painted red by the UI); any fresh
    // output clears it. A short quiet spell (the base thinking) is NOT a stall.
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::PhaseStarted {
        phase: Phase::Research,
    });
    // Just started → not stalled (spin-up grace).
    assert!(!a.is_stalled(), "a just-started phase is not stalled");
    // A 30s quiet spell is normal base thinking, NOT a stall.
    a.last_output_at = std::time::Instant::now().checked_sub(std::time::Duration::from_secs(30));
    assert!(!a.is_stalled(), "a sub-60s quiet spell is not a stall");
    // Backdate the last-output clock past the 60s threshold.
    a.last_output_at = std::time::Instant::now().checked_sub(std::time::Duration::from_secs(90));
    assert!(a.is_stalled(), "no output for >60s must read as stalled");
    // A worker stream event is a sign of life → stall clears immediately.
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::Text {
            delta: "back to work".into(),
        },
    });
    assert!(!a.is_stalled(), "fresh output must clear the stall signal");
}

#[test]
fn tool_call_in_flight_is_not_a_stall() {
    // A long tool call (e.g. a multi-minute npm install) is WORK, not a stall
    // — the red signal must stay suppressed while a ToolUse has no ToolResult
    // yet, even past the 60s threshold; the ToolResult re-arms the stall clock.
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::PhaseStarted {
        phase: Phase::Backend,
    });
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ToolUse {
            name: "Bash".into(),
            detail: "npm install".into(),
            edit: None,
        },
    });
    assert!(a.tool_in_progress, "ToolUse marks a tool in flight");
    // Even with a clock well past the 60s threshold, an in-flight tool is not
    // a stall (otherwise a long `npm install` would falsely flash red).
    a.last_output_at = std::time::Instant::now().checked_sub(std::time::Duration::from_secs(120));
    assert!(
        !a.is_stalled(),
        "an in-flight tool call must NOT read as stalled"
    );
    // The result returns → tool no longer in flight; the stall clock applies.
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ToolResult {
            ok: true,
            summary: "added 200 packages".into(),
        },
    });
    assert!(!a.tool_in_progress, "ToolResult clears the in-flight flag");
    // (The result itself just marked output, so still not stalled now.)
    assert!(!a.is_stalled());
}

#[test]
fn not_stalled_at_a_gate_or_when_idle() {
    // At a gate (paused for the user) phase_started_at is cleared, so the
    // status must never falsely flash red while waiting on a human.
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::PhaseStarted { phase: Phase::Docs });
    a.last_output_at = std::time::Instant::now().checked_sub(std::time::Duration::from_secs(90));
    a.apply_engine(EngineEvent::GateOpened {
        gate: umadev_agent::gates::Gate::DocsConfirm,
        choice: None,
    });
    assert!(
        !a.is_stalled(),
        "a gate pause (no running phase) is not a stall"
    );
    // A brand-new app with nothing running is never stalled either.
    let idle = fresh_app(Some("offline"));
    assert!(!idle.is_stalled());
}

#[test]
fn pre_phase_window_stalls_after_three_seconds() {
    // THE 0/9 WINDOW: a run has STARTED (PipelineStarted) but no `Running`
    // phase has begun yet (cold index build / intake / vector build). Here
    // `phase_started_at` is None and `thinking` is false — the old judge
    // would NEVER go red, so a silent freeze in this window read as smooth.
    // The structural backstop must paint it red past 60s.
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::PipelineStarted {
        slug: "demo".into(),
        requirement: "build it".into(),
    });
    assert!(a.phase_started_at.is_none(), "no Running phase yet (0/9)");
    assert!(!a.thinking, "not a chat-thinking turn");
    // Just launched → not stalled (spin-up grace).
    assert!(!a.is_stalled(), "a just-launched run is not stalled");
    // Backdate the run start past the 60s threshold (no output has arrived).
    a.run_started_at = std::time::Instant::now().checked_sub(std::time::Duration::from_secs(90));
    assert!(
        a.is_stalled(),
        "a silent pre-phase 0/9 window past 60s MUST read as stalled"
    );
}

#[test]
fn pre_phase_gate_pause_is_not_stalled() {
    // The pre-phase backstop must NOT misfire at a gate: GateOpened clears
    // run_started_at and sets active_gate, so a human pause never flashes red.
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::PipelineStarted {
        slug: "demo".into(),
        requirement: "build it".into(),
    });
    a.apply_engine(EngineEvent::GateOpened {
        gate: umadev_agent::gates::Gate::DocsConfirm,
        choice: None,
    });
    // Even with a very stale clock, a gate pause is not a stall.
    a.run_started_at = std::time::Instant::now().checked_sub(std::time::Duration::from_secs(90));
    assert!(
        !a.is_stalled(),
        "a gate pause in the pre-phase window must not read as stalled"
    );
    // A finished/aborted run is likewise never stalled.
    let mut done = fresh_app(Some("offline"));
    done.run_started = true;
    done.aborted = true;
    done.run_started_at = std::time::Instant::now().checked_sub(std::time::Duration::from_secs(90));
    assert!(!done.is_stalled(), "an aborted run is not stalled");
}

#[test]
fn build_brain_failure_drives_aborted_terminal_state() {
    // Fix 3: a `build_brain` init failure (unknown backend / driver build
    // error) carries the ABORT_SENTINEL like the other terminal paths, so the
    // bar flips to `[aborted]` instead of sitting at a fake idle "0/9".
    let mut app = fresh_app(Some("offline"));
    app.run_started = true;
    app.run_started_at = Some(std::time::Instant::now());
    // Emulate the wrapped terminal note spawn_block now emits on init failure.
    app.apply_engine(EngineEvent::Note(format!(
        "{}{}",
        crate::ABORT_SENTINEL,
        umadev_i18n::tlf("worker.init_failed", &["claude", "not on PATH"])
    )));
    assert!(
        app.aborted,
        "an init-failure sentinel note flips to aborted"
    );
    assert!(
        !app.is_pipeline_active(),
        "the failed run is no longer active"
    );
    app.refresh_status();
    assert!(
        app.status.contains("aborted"),
        "the bar shows [aborted], not a fake idle 0/9"
    );
}

#[test]
fn config_save_failure_pushes_a_note_on_lang_change() {
    // Fix 4: a persist failure on `/lang` must surface a note, not silently
    // claim success and revert on next launch. Point config_path under a
    // regular FILE so `create_dir_all(parent)` inside save_to fails.
    let mut app = fresh_app(Some("offline"));
    let tmp = tempfile::TempDir::new().unwrap();
    let blocker = tmp.path().join("cfg-blocker");
    std::fs::write(&blocker, b"x").unwrap();
    app.config_path = blocker.join("nested").join("config.toml");
    let before = app.history.len();
    let _ = app.slash_lang("en");
    let pushed: Vec<String> = app
        .history
        .iter()
        .skip(before)
        .map(|m| m.body().into_owned())
        .collect();
    assert!(
        pushed.iter().any(|b| b.contains("[warn]")),
        "a config persist failure must push a warning note: {pushed:?}"
    );
    // The language still changed for this session (fail-open).
    assert_eq!(app.lang, umadev_i18n::Lang::En);
}

#[test]
fn chat_reply_claiming_edits_gets_an_unverified_warning() {
    // Fix 5: a pure-chat reply that recites an edit ("已修改…/新增了…") —
    // with no run and no agentic tool calls — gets a reality-anchor note.
    let mut app = fresh_app(Some("offline"));
    app.record_chat_reply("我已修改了 app.rs 并新增了一个函数".to_string());
    assert!(
        app.history.iter().any(|m| m.body().contains("[warn]")),
        "a chat reply claiming code changes must get a verify warning"
    );
    // A benign chat reply (no change claim) must NOT be warned.
    let mut benign = fresh_app(Some("offline"));
    benign.record_chat_reply("你好,有什么可以帮你的?".to_string());
    assert!(
        !benign.history.iter().any(|m| m.body().contains("[warn]")),
        "a plain chat reply must not trigger the warning"
    );
}

#[test]
fn clarify_answer_write_failure_does_not_claim_recorded() {
    // Fix 6: when the clarify answer can't be persisted, the user must be
    // told it was NOT recorded — never the false "已记录" line.
    let mut app = fresh_app(Some("offline"));
    // Point the project root at a regular FILE so the output/ dir can't be
    // created and the answer write fails.
    let tmp = tempfile::TempDir::new().unwrap();
    let blocker = tmp.path().join("clarify-blocker");
    std::fs::write(&blocker, b"x").unwrap();
    app.project_root = blocker.clone();
    app.active_gate = Some(Gate::ClarifyGate);
    for c in "use postgres".chars() {
        let _ = app.apply_key(KeyCode::Char(c));
    }
    let _ = app.apply_key(KeyCode::Enter);
    let last = app.history.back().unwrap();
    assert!(
        !last.body().contains("已记录"),
        "a failed write must NOT claim the answer was recorded: {}",
        last.body()
    );
    assert!(
        last.body().contains("[warn]"),
        "a failed clarify write must surface a warning: {}",
        last.body()
    );
}

// ---- WorkerStream rendering tests ----

#[test]
fn text_delta_creates_host_message() {
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::Text {
            delta: "Hello world".into(),
        },
    });
    let last = a.history.back().unwrap();
    assert_eq!(last.role, ChatRole::Host);
    assert!(last.body().contains("Hello world"));
    assert!(
        a.stream_text_active,
        "stream_text_active should be true after first text"
    );
}

#[test]
fn consecutive_text_deltas_append_not_push() {
    let mut a = fresh_app(Some("offline"));
    // First delta → new message
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::Text {
            delta: "Part 1".into(),
        },
    });
    // Second delta → append to same message (typewriter)
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::Text {
            delta: " Part 2".into(),
        },
    });
    let host_msgs: Vec<_> = a
        .history
        .iter()
        .filter(|m| m.role == ChatRole::Host)
        .collect();
    assert_eq!(
        host_msgs.len(),
        1,
        "two consecutive text deltas should be one message"
    );
    assert_eq!(host_msgs[0].body(), "Part 1 Part 2");
}

#[test]
fn long_stream_is_never_truncated_only_segmented() {
    // The bug: a long streamed reply was hard-capped at 2000 bytes and the
    // rest silenced with `…` (CJK hit that in a few sentences). The fix keeps
    // EVERY byte — once a segment fills, the reply rolls into a fresh Host
    // bubble. Stream ~20 KB of CJK in many deltas and assert nothing is lost
    // and no `…` truncation marker is appended.
    let mut a = fresh_app(Some("offline"));
    let chunk = "这是一段很长的中文回复内容用来测试不被截断"; // 21 chars
    let n = 500;
    for _ in 0..n {
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::Text {
                delta: chunk.into(),
            },
        });
    }
    let host_total: usize = a
        .history
        .iter()
        .filter(|m| m.role == ChatRole::Host)
        .map(|m| m.body().chars().count())
        .sum();
    let expected = chunk.chars().count() * n;
    assert_eq!(
        host_total, expected,
        "every streamed char must survive — no truncation"
    );
    // It segmented into more than one bubble (proof the rollover ran), and no
    // segment carries the old truncation ellipsis.
    let host_msgs: Vec<_> = a
        .history
        .iter()
        .filter(|m| m.role == ChatRole::Host)
        .collect();
    assert!(
        host_msgs.len() > 1,
        "a 20 KB reply must roll over into multiple segments"
    );
    for m in &host_msgs {
        assert!(
            !m.body().contains('…'),
            "no segment should be truncated with an ellipsis: {}",
            m.body()
        );
    }
}

#[test]
fn tool_use_resets_text_append() {
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::Text {
            delta: "Some text".into(),
        },
    });
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ToolUse {
            name: "Read".into(),
            detail: "Cargo.toml".into(),
            edit: None,
        },
    });
    // Text after tool should be a NEW message, not appended to tool line
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::Text {
            delta: "New text".into(),
        },
    });
    assert!(!a.stream_text_active || a.history.back().unwrap().body() == "New text");
}

#[test]
fn same_tool_type_batches() {
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ToolUse {
            name: "Read".into(),
            detail: "file1".into(),
            edit: None,
        },
    });
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ToolUse {
            name: "Read".into(),
            detail: "file2".into(),
            edit: None,
        },
    });
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ToolUse {
            name: "Read".into(),
            detail: "file3".into(),
            edit: None,
        },
    });
    let host_msgs: Vec<_> = a
        .history
        .iter()
        .filter(|m| m.role == ChatRole::Host)
        .collect();
    assert_eq!(
        host_msgs.len(),
        1,
        "3 same-type read calls should merge into 1 structured tool row"
    );
    // The merged row is a STRUCTURED tool call, not a flattened sentence.
    let MessageBody::Tool(t) = &host_msgs[0].kind else {
        panic!("merged read batch must be a Tool body, got Text");
    };
    assert!(t.merged, "low-signal reads merge into one batch row");
    assert_eq!(t.count, 3, "the count tracks all three reads");
    assert_eq!(t.status, ToolStatus::Running, "still in flight");
    // The flat text still surfaces the count for export / history.
    assert!(
        host_msgs[0].body().contains('3'),
        "flat text carries the count: {}",
        host_msgs[0].body()
    );
}

#[test]
fn different_tool_type_resets_batch() {
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ToolUse {
            name: "Read".into(),
            detail: "file1".into(),
            edit: None,
        },
    });
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ToolUse {
            name: "Bash".into(),
            detail: "npm test".into(),
            edit: None,
        },
    });
    let host_msgs: Vec<_> = a
        .history
        .iter()
        .filter(|m| m.role == ChatRole::Host)
        .collect();
    assert_eq!(
        host_msgs.len(),
        2,
        "different tool types should be separate messages"
    );
    // The Bash row is a single, un-merged tool row (its result IS signal).
    let MessageBody::Tool(bash) = &host_msgs[1].kind else {
        panic!("Bash must render as a structured tool row");
    };
    assert!(!bash.merged, "Bash is a single-row tool, never merged");
    assert_eq!(bash.name, "Bash");
}

// ---- P4: structured tool rows ----------------------------------------

#[test]
fn tool_use_pushes_structured_tool_row_not_a_sentence() {
    // A ToolUse no longer flattens into a `[write] Edit `path`` string — it
    // becomes a typed `MessageBody::Tool` the renderer draws as one status
    // line. Guards against regressing to the "tool call reads like prose" bug.
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ToolUse {
            name: "Edit".into(),
            detail: "src/main.rs".into(),
            edit: None,
        },
    });
    let last = a.history.back().unwrap();
    let MessageBody::Tool(t) = &last.kind else {
        panic!("a tool use must produce a Tool body, not Text");
    };
    assert_eq!(t.name, "Edit");
    assert_eq!(t.arg, "src/main.rs");
    assert_eq!(t.status, ToolStatus::Running);
    assert!(t.result.is_none(), "no result yet while in flight");
}

#[test]
fn edit_with_content_pushes_a_diff_card_in_real_time() {
    // P1: a Write/Edit carrying structured content renders a diff card the
    // moment the tool_use arrives — we don't wait for the result.
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ToolUse {
            name: "Edit".into(),
            detail: "src/lib.rs".into(),
            edit: Some(umadev_runtime::ToolEdit {
                path: "src/lib.rs".into(),
                before: "let x = 1;\nlet y = 2;\n".into(),
                after: "let x = 1;\nlet y = 3;\n".into(),
            }),
        },
    });
    let MessageBody::Diff(d) = &a.history.back().unwrap().kind else {
        panic!("an edit with content must produce a Diff card, not a Tool row");
    };
    assert_eq!(d.path, "src/lib.rs");
    assert_eq!(d.added, 1, "one line changed → one added");
    assert_eq!(d.removed, 1, "…and one removed");
    // The unchanged `let x = 1;` is kept as ±context around the change.
    let all: Vec<(char, &str)> = d
        .hunks
        .iter()
        .flat_map(|h| h.lines.iter())
        .map(|l| (l.tag, l.text.as_str()))
        .collect();
    assert!(
        all.contains(&(' ', "let x = 1;")),
        "context line kept: {all:?}"
    );
    assert!(all.contains(&('-', "let y = 2;")), "deletion kept: {all:?}");
    assert!(all.contains(&('+', "let y = 3;")), "addition kept: {all:?}");
}

#[test]
fn identical_diff_card_is_not_rendered_twice_in_a_row() {
    // A base can surface the same edit both in its narration AND as the structured
    // tool call (or an opencode tool part can arrive under two ids), landing a
    // byte-identical diff card right after the last - the reported duplicate. The
    // guard collapses it, while a different follow-up edit still renders its card.
    let mut a = fresh_app(Some("offline"));
    for _ in 0..2 {
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::ToolUse {
                name: "Edit".into(),
                detail: "src/lib.rs".into(),
                edit: Some(umadev_runtime::ToolEdit {
                    path: "src/lib.rs".into(),
                    before: "let x = 1;\nlet y = 2;\n".into(),
                    after: "let x = 1;\nlet y = 3;\n".into(),
                }),
            },
        });
    }
    let one = a
        .history
        .iter()
        .filter(|m| matches!(m.kind, MessageBody::Diff(_)))
        .count();
    assert_eq!(one, 1, "an identical consecutive diff must render once");
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ToolUse {
            name: "Edit".into(),
            detail: "src/lib.rs".into(),
            edit: Some(umadev_runtime::ToolEdit {
                path: "src/lib.rs".into(),
                before: "let y = 3;\n".into(),
                after: "let y = 4;\n".into(),
            }),
        },
    });
    let two = a
        .history
        .iter()
        .filter(|m| matches!(m.kind, MessageBody::Diff(_)))
        .count();
    assert_eq!(
        two, 2,
        "a distinct follow-up edit still renders its own card"
    );
}

#[test]
fn write_renders_as_all_additions_diff() {
    // A Write is a fresh file: every line is an addition, none removed.
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ToolUse {
            name: "Write".into(),
            detail: "src/new.rs".into(),
            edit: Some(umadev_runtime::ToolEdit {
                path: "src/new.rs".into(),
                before: String::new(),
                after: "fn a() {}\nfn b() {}\n".into(),
            }),
        },
    });
    let MessageBody::Diff(d) = &a.history.back().unwrap().kind else {
        panic!("a Write with content must produce a Diff card");
    };
    assert_eq!(d.added, 2);
    assert_eq!(d.removed, 0);
    assert!(
        d.hunks
            .iter()
            .flat_map(|h| h.lines.iter())
            .all(|l| l.tag == '+'),
        "every line of a fresh Write is an addition"
    );
}

#[test]
fn diff_card_keeps_only_three_context_lines() {
    // ±DIFF_CONTEXT: a far-away unchanged line is NOT kept in the hunk.
    use std::fmt::Write as _;
    let mut before = String::new();
    let mut after = String::new();
    for i in 0..20 {
        let _ = writeln!(before, "line{i}");
        if i == 10 {
            after.push_str("line10-CHANGED\n");
        } else {
            let _ = writeln!(after, "line{i}");
        }
    }
    let d = FileDiff::from_tool_edit(&umadev_runtime::ToolEdit {
        path: "x.txt".into(),
        before,
        after,
    });
    let texts: Vec<&str> = d
        .hunks
        .iter()
        .flat_map(|h| h.lines.iter())
        .map(|l| l.text.as_str())
        .collect();
    // line7 is within 3 of the change (line10) → kept; line0 is far → dropped.
    assert!(texts.contains(&"line7"), "±3 context kept: {texts:?}");
    assert!(!texts.contains(&"line0"), "far line dropped");
    assert_eq!(DIFF_CONTEXT, 3);
}

#[test]
fn noop_edit_falls_open_to_a_plain_tool_row() {
    // Fail-open: an edit whose before==after (no real change → zero hunks)
    // degrades to a plain tool row, never an empty diff card.
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ToolUse {
            name: "Edit".into(),
            detail: "same.rs".into(),
            edit: Some(umadev_runtime::ToolEdit {
                path: "same.rs".into(),
                before: "unchanged\n".into(),
                after: "unchanged\n".into(),
            }),
        },
    });
    assert!(
        matches!(a.history.back().unwrap().kind, MessageBody::Tool(_)),
        "a no-op edit degrades to a plain tool row"
    );
}

#[test]
fn diff_card_handles_cjk_content_without_panic() {
    // CJK lines must not panic the diff builder (char-boundary safe) and must
    // round-trip their content.
    let d = FileDiff::from_tool_edit(&umadev_runtime::ToolEdit {
        path: "说明.md".into(),
        before: "第一行\n第二行\n".into(),
        after: "第一行\n第二行改\n".into(),
    });
    assert_eq!(d.added, 1);
    assert_eq!(d.removed, 1);
    assert!(d
        .hunks
        .iter()
        .flat_map(|h| h.lines.iter())
        .any(|l| l.text == "第二行改"));
}

#[test]
fn diff_card_absorbs_a_success_result_silently() {
    // After a diff card, a SUCCESS ToolResult is implied by the card itself —
    // no redundant `[ok]` line is appended.
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ToolUse {
            name: "Write".into(),
            detail: "f.rs".into(),
            edit: Some(umadev_runtime::ToolEdit {
                path: "f.rs".into(),
                before: String::new(),
                after: "fn x() {}\n".into(),
            }),
        },
    });
    let before = a.history.len();
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ToolResult {
            ok: true,
            summary: "File created successfully".into(),
        },
    });
    assert_eq!(
        a.history.len(),
        before,
        "a success after a diff card adds no extra line"
    );
    assert!(matches!(
        a.history.back().unwrap().kind,
        MessageBody::Diff(_)
    ));
}

#[test]
fn big_diff_defaults_collapsed_and_ctrl_r_toggles() {
    // A diff over the fold threshold defaults collapsed; Ctrl+R expands it
    // (reusing the P6 fold lever).
    use std::fmt::Write as _;
    let before = String::new();
    let mut after = String::new();
    for i in 0..40 {
        let _ = writeln!(after, "row{i}");
    }
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ToolUse {
            name: "Write".into(),
            detail: "big.rs".into(),
            edit: Some(umadev_runtime::ToolEdit {
                path: "big.rs".into(),
                before,
                after,
            }),
        },
    });
    {
        let MessageBody::Diff(d) = &a.history.back().unwrap().kind else {
            panic!("Diff card");
        };
        assert!(d.collapsed, "a big diff defaults collapsed");
        assert!(d.total_rows() > DIFF_FOLD_THRESHOLD);
    }
    // Ctrl+R toggles the most-recent collapsible row → expanded.
    let _ = a.apply_key_with_mods(
        crossterm::event::KeyCode::Char('r'),
        crossterm::event::KeyModifiers::CONTROL,
    );
    let MessageBody::Diff(d) = &a.history.back().unwrap().kind else {
        panic!("Diff card");
    };
    assert!(!d.collapsed, "Ctrl+R expands the folded diff");
}

#[test]
fn word_diff_marks_only_the_changed_token_on_each_side() {
    // `const oldName = compute(input);` → `const newName = compute(input);`
    // — only `oldName`/`newName` should carry a `changed` byte range; the
    // surrounding tokens stay unchanged (empty around them). The rename is a
    // small fraction of the line, well under the 0.4 rewrite threshold.
    let d = FileDiff::from_tool_edit(&umadev_runtime::ToolEdit {
        path: "x.ts".into(),
        before: "const oldName = compute(input);\n".into(),
        after: "const newName = compute(input);\n".into(),
    });
    let lines: Vec<&DiffLine> = d.hunks.iter().flat_map(|h| h.lines.iter()).collect();
    let del = lines.iter().find(|l| l.tag == '-').expect("a - line");
    let ins = lines.iter().find(|l| l.tag == '+').expect("a + line");
    // Each side has exactly one changed region, and it covers the renamed
    // identifier — not the whole line.
    assert_eq!(del.changed.len(), 1, "one changed region on the - line");
    assert_eq!(ins.changed.len(), 1, "one changed region on the + line");
    let (ds, de) = del.changed[0];
    let (is, ie) = ins.changed[0];
    assert_eq!(&del.text[ds..de], "oldName", "the deleted word is marked");
    assert_eq!(&ins.text[is..ie], "newName", "the inserted word is marked");
    // The unchanged prefix `let ` and suffix ` = 1;` are NOT inside a range.
    assert!(ds >= "let ".len(), "the leading `let ` stays unchanged");
}

#[test]
fn word_diff_falls_back_to_whole_line_on_a_near_total_rewrite() {
    // A line replaced wholesale (almost no shared tokens) trips the 0.4
    // rewrite ratio → both `changed` vecs come back empty so the renderer
    // whole-line-highlights instead of confetti.
    let d = FileDiff::from_tool_edit(&umadev_runtime::ToolEdit {
        path: "x.rs".into(),
        before: "alpha beta gamma delta\n".into(),
        after: "one two three four five\n".into(),
    });
    let lines: Vec<&DiffLine> = d.hunks.iter().flat_map(|h| h.lines.iter()).collect();
    for l in lines.iter().filter(|l| l.tag != ' ') {
        assert!(
            l.changed.is_empty(),
            "a near-total rewrite drops the word signal: {:?}",
            l.changed
        );
    }
}

#[test]
fn word_diff_is_cjk_byte_safe() {
    // A single CJK token changed inside an otherwise-equal line: the byte
    // ranges must land on char boundaries (slicing must not panic) and cover
    // exactly the changed CJK run.
    let d = FileDiff::from_tool_edit(&umadev_runtime::ToolEdit {
        path: "x.md".into(),
        before: "前缀 旧值 后缀\n".into(),
        after: "前缀 新值 后缀\n".into(),
    });
    let lines: Vec<&DiffLine> = d.hunks.iter().flat_map(|h| h.lines.iter()).collect();
    let del = lines.iter().find(|l| l.tag == '-').expect("a - line");
    let ins = lines.iter().find(|l| l.tag == '+').expect("a + line");
    // Every range must be on a char boundary (no panic when sliced).
    for l in [del, ins] {
        for &(s, e) in &l.changed {
            assert!(l.text.is_char_boundary(s) && l.text.is_char_boundary(e));
            let _ = &l.text[s..e]; // would panic if mis-aligned
        }
    }
    assert!(
        del.changed
            .iter()
            .any(|&(s, e)| del.text[s..e].contains('旧')),
        "the changed CJK token is marked on the - side"
    );
    assert!(
        ins.changed
            .iter()
            .any(|&(s, e)| ins.text[s..e].contains('新')),
        "the changed CJK token is marked on the + side"
    );
}

#[test]
fn tool_output_delta_stays_running_until_terminal_result() {
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ToolUse {
            name: "Bash".into(),
            detail: "cargo test -q".into(),
            edit: None,
        },
    });
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ToolOutputDelta {
            delta: "running tests...\n".into(),
        },
    });
    let MessageBody::Tool(tool) = &a.history.back().unwrap().kind else {
        panic!("expected a tool row");
    };
    assert_eq!(tool.status, ToolStatus::Running);
    assert_eq!(tool.result.as_deref(), Some("running tests...\n"));
    assert!(a.tool_in_progress, "a delta must not settle the spinner");

    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ToolResult {
            ok: false,
            summary: "test failed".into(),
        },
    });
    let MessageBody::Tool(tool) = &a.history.back().unwrap().kind else {
        panic!("expected the same tool row");
    };
    assert_eq!(tool.status, ToolStatus::Fail);
    assert_eq!(tool.result.as_deref(), Some("test failed"));
    assert!(!a.tool_in_progress);
}

#[test]
fn tool_output_snapshot_replaces_and_can_clear_a_running_log() {
    let mut app = fresh_app(Some("offline"));
    app.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ToolUse {
            name: "Bash".into(),
            detail: "cargo test".into(),
            edit: None,
        },
    });
    app.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ToolOutputDelta {
            delta: "old output".into(),
        },
    });
    app.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ToolOutputSnapshot {
            output: "replacement \u{4e16}\u{754c}".into(),
        },
    });
    let MessageBody::Tool(tool) = &app.history.back().unwrap().kind else {
        panic!("expected a tool row");
    };
    assert_eq!(tool.status, ToolStatus::Running);
    assert_eq!(tool.result.as_deref(), Some("replacement \u{4e16}\u{754c}"));
    assert!(app.tool_in_progress);

    app.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ToolOutputSnapshot {
            output: String::new(),
        },
    });
    let MessageBody::Tool(tool) = &app.history.back().unwrap().kind else {
        panic!("expected the same tool row");
    };
    assert_eq!(tool.status, ToolStatus::Running);
    assert_eq!(tool.result, None);
    assert!(app.tool_in_progress, "a snapshot must not settle the tool");
}

#[test]
fn correlated_tool_progress_and_results_never_cross_interleaved_rows() {
    let mut app = fresh_app(Some("kimi-code"));
    for (call_id, detail) in [("tool-a", "cargo test"), ("tool-b", "cargo build")] {
        app.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::ToolUseCorrelated {
                call_id: call_id.into(),
                name: "Bash".into(),
                detail: detail.into(),
                edit: None,
            },
        });
    }
    app.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ToolProgressCorrelated {
            call_id: "tool-a".into(),
            title: "Running unit tests".into(),
        },
    });
    app.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ToolOutputDeltaCorrelated {
            call_id: "tool-b".into(),
            delta: "compiling crate_b\n".into(),
        },
    });
    app.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ToolResultCorrelated {
            call_id: "tool-a".into(),
            ok: true,
            summary: "tests passed".into(),
        },
    });

    let tools = app
        .history
        .iter()
        .filter_map(|message| match &message.kind {
            MessageBody::Tool(tool) => Some(tool),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(tools.len(), 2, "one stable row per correlated tool call");
    let a = tools
        .iter()
        .find(|tool| tool.call_id.as_deref() == Some("tool-a"))
        .unwrap();
    let b = tools
        .iter()
        .find(|tool| tool.call_id.as_deref() == Some("tool-b"))
        .unwrap();
    assert_eq!(a.status, ToolStatus::Ok);
    assert_eq!(a.progress, None, "terminal result clears transient title");
    assert_eq!(a.result.as_deref(), Some("tests passed"));
    assert_eq!(b.status, ToolStatus::Running);
    assert_eq!(b.progress, None);
    assert_eq!(b.result.as_deref(), Some("compiling crate_b\n"));
    assert!(
        app.tool_in_progress,
        "settling one correlated call must not stop another spinner"
    );
}

#[test]
fn tool_result_attaches_to_the_running_row_and_auto_collapses_on_ok() {
    // A successful result flips the SAME row to Ok (not a new line) and
    // auto-collapses it; a row height stays stable pending→done.
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ToolUse {
            name: "Write".into(),
            detail: "README.md".into(),
            edit: None,
        },
    });
    let before = a.history.len();
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ToolResult {
            ok: true,
            summary: "wrote 12 lines".into(),
        },
    });
    assert_eq!(
        a.history.len(),
        before,
        "result updates in place, no new row"
    );
    let MessageBody::Tool(t) = &a.history.back().unwrap().kind else {
        panic!("still a Tool row");
    };
    assert_eq!(t.status, ToolStatus::Ok);
    assert_eq!(t.result.as_deref(), Some("wrote 12 lines"));
    assert!(t.collapsed, "a finished OK call auto-collapses");
}

#[test]
fn subagent_terminal_sequence_reuses_and_settles_the_working_row() {
    let mut a = fresh_app(Some("claude-code"));
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ToolUse {
            name: "↳ 子代理 · audit · 工作中…".into(),
            detail: String::new(),
            edit: None,
        },
    });
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::Text {
            delta: "主代理阶段性输出".into(),
        },
    });
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ToolUse {
            name: "↳ 子代理 · audit".into(),
            detail: String::new(),
            edit: None,
        },
    });
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ThinkingDelta("核对结果".into()),
    });
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ToolResult {
            ok: true,
            summary: "↳ 子代理 · 完成审计".into(),
        },
    });

    let rows: Vec<&ToolCall> = a
        .history
        .iter()
        .filter_map(|message| match &message.kind {
            MessageBody::Tool(tool) if claude_subagent_row(&tool.name).is_some() => Some(tool),
            _ => None,
        })
        .collect();
    assert_eq!(rows.len(), 1, "同一子代理只能保留一条状态行");
    assert_eq!(rows[0].name, "↳ 子代理 · audit");
    assert_eq!(rows[0].status, ToolStatus::Ok);
    assert_eq!(rows[0].result.as_deref(), Some("↳ 子代理 · 完成审计"));
}

#[test]
fn failed_tool_result_stays_expanded() {
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ToolUse {
            name: "Bash".into(),
            detail: "cargo build".into(),
            edit: None,
        },
    });
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ToolResult {
            ok: false,
            summary: "error[E0308]".into(),
        },
    });
    let MessageBody::Tool(t) = &a.history.back().unwrap().kind else {
        panic!("Tool row");
    };
    assert_eq!(t.status, ToolStatus::Fail);
    assert!(!t.collapsed, "a failed call must never hide its error");
}

#[test]
fn logs_toggle_keeps_command_output_visible_and_off_clips_it() {
    // `/logs` ON: a long-running command's full output stays in the row AND the
    // row stays expanded, so the build log is visible as it streams. OFF (the
    // default): the tight 200-char clip + auto-collapse, exactly as before.
    // The renderer reads `self.show_process_logs` (a field, not the env), so this
    // is deterministic and never races a parallel test on the process env.
    let long_log: String = (0..60)
        .map(|_| "[INFO] compiling module")
        .collect::<Vec<_>>()
        .join("\n");

    // ── OFF (default) ──
    let mut off = fresh_app(Some("offline"));
    assert!(!off.show_process_logs, "off by default");
    off.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::tool_use("Bash", "mvn -q install"),
    });
    off.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ToolResult {
            ok: true,
            summary: long_log.clone(),
        },
    });
    let MessageBody::Tool(t) = &off.history.back().unwrap().kind else {
        panic!("Tool row");
    };
    assert!(t.collapsed, "OFF: a finished OK command auto-collapses");
    assert!(
        t.result.as_deref().unwrap_or("").chars().count() <= 200,
        "OFF: output is clipped to the tight preview"
    );

    // ── ON (via /logs) ──
    let mut on = fresh_app(Some("offline"));
    let _ = on.slash_logs();
    assert!(on.show_process_logs, "/logs turned it on");
    on.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::tool_use("Bash", "mvn -q install"),
    });
    on.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ToolResult {
            ok: true,
            summary: long_log.clone(),
        },
    });
    let MessageBody::Tool(t) = &on.history.back().unwrap().kind else {
        panic!("Tool row");
    };
    assert!(
        !t.collapsed,
        "ON: the command row stays expanded so the build log is visible"
    );
    let shown = t.result.as_deref().unwrap_or("");
    assert!(
        shown.contains("[INFO] compiling module"),
        "ON: the full build log reaches the transcript: {shown:?}"
    );
    assert!(
        shown.chars().count() > 200,
        "ON: the output is NOT clipped to 200 chars"
    );

    // Toggling /logs again turns it back off (and clears the shared flag — the
    // toggle drives thread-safe state now, never the process env).
    let _ = on.slash_logs();
    assert!(!on.show_process_logs, "/logs toggles back off");
}

/// Helper: a tool row whose `status` is whatever the caller passes, so the
/// settle tests can stand up a mix of in-flight + already-finished rows.
fn push_tool_row(a: &mut App, name: &str, status: ToolStatus) {
    a.history.push_back(ChatMessage {
        role: ChatRole::Host,
        kind: MessageBody::Tool(ToolCall {
            call_id: None,
            name: name.to_string(),
            arg: String::new(),
            status,
            result: None,
            progress: None,
            merged: false,
            count: 1,
            collapsed: false,
        }),
        collapsed: false,
    });
}

fn tool_statuses(a: &App) -> Vec<ToolStatus> {
    a.history
        .iter()
        .filter_map(|m| match &m.kind {
            MessageBody::Tool(t) => Some(t.status),
            _ => None,
        })
        .collect()
}

#[test]
fn abort_settles_in_flight_tool_rows_but_keeps_finished_ones() {
    // The user-reported bug: after the run aborts (idle settle / base error,
    // both arrive as an ABORT_SENTINEL note), a stack of base tool rows
    // (TaskCreate / Agent / Bash / Read / TaskUpdate) kept spinning forever
    // because the matching ToolResult never landed. They must now settle.
    let mut a = fresh_app(Some("offline"));
    a.run_started = true;
    push_tool_row(&mut a, "TaskCreate", ToolStatus::Running);
    push_tool_row(&mut a, "Read", ToolStatus::Ok); // already finished — keep it
    push_tool_row(&mut a, "Agent", ToolStatus::Running);
    push_tool_row(&mut a, "TaskUpdate", ToolStatus::Queued);

    a.apply_engine(EngineEvent::Note(format!(
        "{}本轮已中止:磁盘写入失败",
        crate::ABORT_SENTINEL
    )));

    assert!(a.aborted, "the sentinel flips the run into aborted");
    let statuses = tool_statuses(&a);
    // Every in-flight row is settled; NONE is left in-progress.
    assert!(
        statuses.iter().all(|s| s.is_terminal()),
        "no tool row may stay in-progress after an abort: {statuses:?}"
    );
    // The genuinely-finished Ok row is NOT downgraded to a fake abort.
    assert_eq!(
        statuses,
        vec![
            ToolStatus::Aborted,
            ToolStatus::Ok,
            ToolStatus::Aborted,
            ToolStatus::Aborted,
        ],
        "in-flight rows -> Aborted, the Ok row keeps its real success: {statuses:?}"
    );
}

#[test]
fn cancel_settles_in_flight_tool_rows() {
    // The user Cancel path (Esc/Ctrl-C -> cancel_run -> reset_for_new_run ->
    // clear_live_panels) must also stop any spinning tool row.
    let mut a = fresh_app(Some("offline"));
    a.run_started = true;
    push_tool_row(&mut a, "Bash", ToolStatus::Running);
    push_tool_row(&mut a, "Edit", ToolStatus::Fail); // finished — keep it

    a.cancel_run();

    let statuses = tool_statuses(&a);
    assert!(
        statuses.iter().all(|s| s.is_terminal()),
        "cancel must settle every in-flight tool row: {statuses:?}"
    );
    assert_eq!(
        statuses,
        vec![ToolStatus::Aborted, ToolStatus::Fail],
        "the Running row -> Aborted, the Fail row keeps its failure: {statuses:?}"
    );
}

#[test]
fn clean_finish_closes_dangling_in_flight_tool_row() {
    // Defensive: even a CLEAN delivery finish (finalize_live_panels, reached
    // here via the chat/Fast build completion card) must close any tool row
    // left dangling in-progress, so a settled run never keeps a spinner.
    let mut a = fresh_app(Some("offline"));
    push_tool_row(&mut a, "Write", ToolStatus::Running);
    push_tool_row(&mut a, "Read", ToolStatus::Ok);

    a.finalize_live_panels();

    let statuses = tool_statuses(&a);
    assert_eq!(
        statuses,
        vec![ToolStatus::Aborted, ToolStatus::Ok],
        "a clean finish closes the dangling Running row, keeps the Ok row: {statuses:?}"
    );
}

#[test]
fn read_only_grep_folds_a_metric_not_the_raw_dump() {
    // A merged read/grep batch keeps its `inspected N` headline and folds
    // the grep result into a `(N matches)` metric — never the raw output.
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ToolUse {
            name: "Grep".into(),
            detail: "TODO".into(),
            edit: None,
        },
    });
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ToolResult {
            ok: true,
            summary: "3 files\nsrc/a.rs\nsrc/b.rs\nsrc/c.rs".into(),
        },
    });
    let MessageBody::Tool(t) = &a.history.back().unwrap().kind else {
        panic!("Tool row");
    };
    assert!(t.merged, "a grep is a low-signal mergeable tool");
    // The metric folds in; the raw file list is NOT dumped into the result.
    let result = t.result.as_deref().unwrap_or("");
    assert!(result.contains('3'), "folds the count metric: {result}");
    assert!(
        !result.contains("src/a.rs"),
        "must not dump the raw output: {result}"
    );
}

#[test]
fn read_only_grep_never_treats_matched_numeric_content_as_a_count() {
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ToolUse {
            name: "Grep".into(),
            detail: "numeric literal".into(),
            edit: None,
        },
    });
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ToolResult {
            ok: true,
            summary: "3000000000000000\nsrc/data.rs:42".into(),
        },
    });

    let MessageBody::Tool(t) = &a.history.back().unwrap().kind else {
        panic!("Tool row");
    };
    assert_eq!(
        t.result, None,
        "matched repository data must not be relabelled as a search count"
    );
    assert_eq!(
        read_only_metric(umadev_i18n::Lang::ZhCn, "Grep", "build 53 found 3 files"),
        None,
        "an unrelated leading number is not an explicit count phrase"
    );
    assert_eq!(
        read_only_metric(umadev_i18n::Lang::ZhCn, "Grep", "Found 3 files\na\nb\nc"),
        Some("3 处匹配".to_string()),
        "an explicit provider count remains visible"
    );
}

#[test]
fn contiguous_low_signal_reads_merge_with_increasing_count() {
    // Five reads in a row collapse to one row with count 5 — and the count
    // is greatest-seen, so it can never visibly jump backwards.
    let mut a = fresh_app(Some("offline"));
    for i in 0..5 {
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::ToolUse {
                name: "Read".into(),
                detail: format!("file{i}"),
                edit: None,
            },
        });
    }
    let host_rows: Vec<_> = a
        .history
        .iter()
        .filter(|m| m.role == ChatRole::Host)
        .collect();
    assert_eq!(host_rows.len(), 1, "five reads merge into one row");
    let MessageBody::Tool(t) = &host_rows[0].kind else {
        panic!("Tool row");
    };
    assert_eq!(t.count, 5);
    assert!(t.merged);
}

#[test]
fn a_write_breaks_the_read_batch_so_the_next_read_starts_fresh() {
    let mut a = fresh_app(Some("offline"));
    for ev in [
        ("Read", "a"),
        ("Read", "b"),
        ("Write", "out.txt"),
        ("Read", "c"),
    ] {
        a.apply_engine(EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::ToolUse {
                name: ev.0.into(),
                detail: ev.1.into(),
                edit: None,
            },
        });
    }
    let host_rows: Vec<_> = a
        .history
        .iter()
        .filter(|m| m.role == ChatRole::Host)
        .collect();
    // batch(a,b) · write · batch(c) → 3 rows.
    assert_eq!(host_rows.len(), 3, "a write splits the read batch");
}

// ---- P6: long-output folding -----------------------------------------

#[test]
fn a_long_host_reply_is_collapsible_and_ctrl_r_toggles_it() {
    let mut a = fresh_app(Some("offline"));
    // A 50-line Host reply — well past the fold threshold.
    let wall: String = (0..50)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    a.push(ChatRole::Host, wall);
    let idx = a.history.len() - 1;
    assert!(
        message_is_collapsible(&a.history[idx]),
        "a 50-line wall is foldable"
    );
    assert!(!a.history[idx].collapsed, "starts expanded");
    // Ctrl+R folds the most recent collapsible row.
    let _ = a.apply_key_with_mods(KeyCode::Char('r'), crossterm::event::KeyModifiers::CONTROL);
    assert!(a.history[idx].collapsed, "Ctrl+R collapsed the wall");
    // Ctrl+R again expands it.
    let _ = a.apply_key_with_mods(KeyCode::Char('r'), crossterm::event::KeyModifiers::CONTROL);
    assert!(!a.history[idx].collapsed, "Ctrl+R re-expanded the wall");
}

#[test]
fn a_short_reply_is_not_collapsible() {
    let mut a = fresh_app(Some("offline"));
    a.push(ChatRole::Host, "just one short line");
    let last = a.history.back().unwrap();
    assert!(
        !message_is_collapsible(last),
        "a short reply is never folded"
    );
}

#[test]
fn ctrl_r_is_a_noop_when_nothing_is_foldable() {
    let mut a = fresh_app(Some("offline"));
    a.push(ChatRole::Host, "short");
    a.input_history.clear(); // no prompt history to search either
    let before = a.clone();
    let _ = a.apply_key_with_mods(KeyCode::Char('r'), crossterm::event::KeyModifiers::CONTROL);
    // Nothing foldable AND no prompt history → Ctrl+R stays a no-op
    // (fail-open): no fold, and no history-search mode opens.
    assert_eq!(a.history.len(), before.history.len());
    assert!(!a.history.back().unwrap().collapsed);
    assert!(
        a.history_search.is_none(),
        "no history → no reverse-search mode"
    );
}

// ---- I3 — reverse prompt-history search (Ctrl+R) ----

/// Seed the prompt-history ring directly (front→back == oldest→newest) and
/// drop any transcript rows so nothing is foldable — the state in which
/// Ctrl+R opens the reverse history search.
fn seed_history(a: &mut App, prompts: &[&str]) {
    a.history.clear();
    a.input_history.clear();
    for p in prompts {
        a.input_history.push_back((*p).to_string());
    }
}

#[test]
fn ctrl_r_opens_history_search_finds_and_cycles() {
    let mut a = fresh_app(Some("offline"));
    seed_history(&mut a, &["alpha one", "beta two", "alpha three"]);
    // Ctrl+R opens the reverse history search (nothing foldable in view).
    let _ = a.apply_key_with_mods(KeyCode::Char('r'), crossterm::event::KeyModifiers::CONTROL);
    assert!(
        a.history_search.is_some(),
        "Ctrl+R opened reverse history search"
    );
    // Empty query previews the NEWEST entry.
    assert_eq!(a.history_search_preview(), Some("alpha three"));
    // Typing narrows to the matching entries, newest-first.
    for c in "alpha".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    assert_eq!(
        a.history_search_preview(),
        Some("alpha three"),
        "newest 'alpha' match is previewed"
    );
    // ↓ steps to the OLDER match; wraps back to the newest.
    let _ = a.apply_key(KeyCode::Down);
    assert_eq!(
        a.history_search_preview(),
        Some("alpha one"),
        "cycled older"
    );
    let _ = a.apply_key(KeyCode::Down);
    assert_eq!(
        a.history_search_preview(),
        Some("alpha three"),
        "wrapped to newest"
    );
    // Ctrl+R inside the mode also cycles older (readline convention).
    let _ = a.apply_key_with_mods(KeyCode::Char('r'), crossterm::event::KeyModifiers::CONTROL);
    assert_eq!(
        a.history_search_preview(),
        Some("alpha one"),
        "Ctrl+R cycled older"
    );
}

#[test]
fn history_search_enter_loads_match_into_input() {
    let mut a = fresh_app(Some("offline"));
    seed_history(&mut a, &["fix the bug", "add a feature"]);
    a.open_history_search();
    for c in "bug".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    assert_eq!(a.history_search_preview(), Some("fix the bug"));
    let _ = a.apply_key(KeyCode::Enter);
    assert!(a.history_search.is_none(), "Enter closed the mode");
    assert_eq!(
        a.input, "fix the bug",
        "Enter loaded the match into the input box"
    );
    assert_eq!(a.input_cursor, a.input_len(), "caret lands at the end");
}

#[test]
fn history_search_windows_bs_deletes_query_char() {
    let mut a = fresh_app(Some("offline"));
    seed_history(&mut a, &["fix the bug", "add a feature"]);
    a.open_history_search();
    for c in "bug".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let _ = a.apply_key(KeyCode::Char('\u{8}'));
    assert_eq!(a.history_search.as_ref().unwrap().query, "bu");
}

#[test]
fn history_search_esc_cancels_without_touching_input() {
    let mut a = fresh_app(Some("offline"));
    seed_history(&mut a, &["an old prompt"]);
    a.input = "draft".to_string();
    a.input_cursor = a.input_len();
    let _ = a.apply_key_with_mods(KeyCode::Char('r'), crossterm::event::KeyModifiers::CONTROL);
    assert!(a.history_search.is_some(), "opened over a non-empty draft");
    let _ = a.apply_key(KeyCode::Esc);
    assert!(a.history_search.is_none(), "Esc closed the mode");
    assert_eq!(a.input, "draft", "Esc left the prompt untouched");
}

#[test]
fn history_search_dedups_repeated_entries() {
    let mut a = fresh_app(Some("offline"));
    seed_history(&mut a, &["run tests", "run tests", "deploy", "run tests"]);
    a.open_history_search();
    let entries = &a.history_search.as_ref().unwrap().entries;
    // Deduped + newest-first: one "run tests" (its most-recent position), then
    // "deploy".
    assert_eq!(
        entries,
        &vec!["run tests".to_string(), "deploy".to_string()],
        "repeated entries collapse to one, newest-first: {entries:?}"
    );
}

#[test]
fn history_search_does_not_open_while_palette_owns_keys() {
    let mut a = fresh_app(Some("offline"));
    seed_history(&mut a, &["past prompt"]);
    // Type a slash command so the palette owns the keyboard.
    for c in "/cl".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    assert!(!a.palette_matches().is_empty(), "palette is active");
    let _ = a.apply_key_with_mods(KeyCode::Char('r'), crossterm::event::KeyModifiers::CONTROL);
    assert!(
        a.history_search.is_none(),
        "Ctrl+R suppressed while the palette owns keys"
    );
}

#[test]
fn history_search_owns_keys_so_other_modes_cannot_open() {
    // Once open, the mode is mutually exclusive: a Ctrl+F that would normally
    // open the transcript search is swallowed (typing/nav only).
    let mut a = fresh_app(Some("offline"));
    seed_history(&mut a, &["something"]);
    a.open_history_search();
    let _ = a.apply_key_with_mods(KeyCode::Char('f'), crossterm::event::KeyModifiers::CONTROL);
    assert!(
        a.search.is_none(),
        "transcript search did not open under history search"
    );
    assert!(
        a.history_search.is_some(),
        "history search still owns the keyboard"
    );
}

#[test]
fn ctrl_o_toggles_the_global_verbose_flag() {
    // UX maturity Fix B: a single global key flips `verbose`, which every
    // collapsible renderer reads so ALL collapsed output reveals/hides at
    // once — not just the most-recent row that Ctrl+R reaches.
    let mut a = fresh_app(Some("offline"));
    assert!(
        !a.verbose,
        "verbose defaults off (everything at its per-row state)"
    );
    let _ = a.apply_key_with_mods(KeyCode::Char('o'), crossterm::event::KeyModifiers::CONTROL);
    assert!(a.verbose, "Ctrl+O turns the global expand-all on");
    let _ = a.apply_key_with_mods(KeyCode::Char('o'), crossterm::event::KeyModifiers::CONTROL);
    assert!(!a.verbose, "Ctrl+O toggles it back off");
}

// ---- backward-compat: plain Text rows ---------------------------------

#[test]
fn plain_push_stays_a_text_body_and_body_reads_through() {
    // Every existing `push(role, String)` call still produces a Text body
    // and `body()` reads it back verbatim — the upgrade is invisible to the
    // dozens of plain-message call sites.
    let mut a = fresh_app(Some("offline"));
    a.push(ChatRole::System, "hello world");
    let last = a.history.back().unwrap();
    assert!(matches!(last.kind, MessageBody::Text(_)));
    assert_eq!(last.body(), "hello world");
    assert!(!last.collapsed);
}

#[test]
fn thinking_indicator_shows() {
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::Thinking,
    });
    let last = a.history.back().unwrap();
    assert_eq!(last.role, ChatRole::System);
    assert!(
        last.body().contains("thinking"),
        "should show thinking indicator: {}",
        last.body()
    );
}

#[test]
fn tool_result_shows_checkmark() {
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ToolResult {
            ok: true,
            summary: "version = 4.6.0".into(),
        },
    });
    let last = a.history.back().unwrap();
    assert!(
        last.body().contains("[ok]"),
        "success should show checkmark"
    );
    assert!(last.body().contains("4.6.0"));
}

#[test]
fn tool_result_error_shows_cross() {
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::ToolResult {
            ok: false,
            summary: "file not found".into(),
        },
    });
    let last = a.history.back().unwrap();
    assert!(last.body().contains("[fail]"), "error should show cross");
}

#[test]
fn empty_text_delta_ignored() {
    let mut a = fresh_app(Some("offline"));
    let before = a.history.len();
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::Text {
            delta: "   ".into(),
        },
    });
    assert_eq!(
        a.history.len(),
        before,
        "empty/whitespace text delta should not push"
    );
}

#[test]
fn transient_warning_goes_to_status_not_transcript() {
    // A RECOVERABLE hiccup (rate-limit / retry / overloaded) surfaces as ONE
    // muted live status line, NOT a permanent transcript row — so a flurry of
    // retries doesn't read like the turn is erroring next to the thinking timer
    // ("时间会乱弹错误"). The turn keeps running; only a terminal ABORT settles it.
    let mut a = fresh_app(Some("offline"));
    let before = a.history.len();
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::Warning {
            message: "rate limited".into(),
        },
    });
    assert_eq!(
        a.history.len(),
        before,
        "a transient warning must not be pushed to the transcript"
    );
    assert!(
        a.transient_status
            .as_deref()
            .unwrap_or("")
            .contains("rate limited"),
        "it surfaces as a transient live status line instead"
    );
}

#[test]
fn notable_warning_still_shows_in_transcript() {
    // A non-transient warning stays a transcript row as before.
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::WorkerStream {
        event: umadev_runtime::StreamEvent::Warning {
            message: "disk almost full".into(),
        },
    });
    let last = a.history.back().unwrap();
    assert!(last.body().contains("disk almost full"));
}

#[test]
fn default_trust_mode_is_guarded() {
    // fresh_app writes `.umadevrc` with auto_approve_gates = false, so the
    // default tier is the existing human-in-the-loop behaviour.
    let a = fresh_app(Some("offline"));
    assert_eq!(a.effective_trust_mode(), umadev_agent::TrustMode::Guarded);
    assert!(!a.auto_approve_on());
}

#[test]
fn shift_tab_toggles_guarded_auto_and_never_enters_read_only_plan() {
    // BackTab/Shift+Tab toggles ONLY the two WRITABLE tiers (Guarded ↔ Auto),
    // matching the footer chips. It never ENTERS read-only Plan (the reported bug:
    // Auto → Plan stranded the base with no write permission); from Plan it EXITS
    // to Guarded so a `/mode plan` user can escape read-only. Plan stays reachable
    // only via `/mode plan`.
    let mut a = fresh_app(Some("claude-code"));
    a.set_trust_mode(umadev_agent::TrustMode::Plan);
    a.chat_session_id = Some("plan-session".to_string());
    a.chat_resume_identity = crate::session_slot::requested_resume_identity(
        "claude-code",
        &a.project_root,
        umadev_runtime::BasePermissionProfile::Plan,
    );
    a.chat_session_dirty = false;
    // Plan → Guarded (exit read-only). A real tier change rebuilds the session.
    a.cycle_approval_mode();
    assert!(
        a.chat_session_dirty,
        "a real tier change rebuilds the session"
    );
    assert_eq!(a.chat_session_id, None);
    assert_eq!(a.chat_resume_identity, None);
    assert_eq!(a.effective_trust_mode(), umadev_agent::TrustMode::Guarded);
    // Guarded → Auto.
    a.cycle_approval_mode();
    assert_eq!(a.effective_trust_mode(), umadev_agent::TrustMode::Auto);
    // Auto → Guarded (NOT Plan): Shift+Tab must never make the base read-only.
    a.cycle_approval_mode();
    assert_eq!(
        a.effective_trust_mode(),
        umadev_agent::TrustMode::Guarded,
        "Auto wraps back to the writable Guarded tier, never read-only Plan"
    );
    // Same tier (Guarded → Guarded) must NOT rebuild the session.
    a.chat_session_dirty = false;
    a.set_trust_mode(umadev_agent::TrustMode::Guarded);
    assert!(
        !a.chat_session_dirty,
        "same tier must not rebuild the session"
    );
}

#[test]
fn config_trust_mode_is_cached_not_re_read_per_call() {
    // P2-B: `effective_trust_mode` runs in the render hot path (~12/s). It
    // must NOT `load_project_config` (a disk read) on every call. Proof: the
    // first call memoises `Guarded`; rewriting `.umadevrc` to auto ON DISK is
    // then IGNORED (cache still serves `Guarded`) — i.e. no per-call read.
    // Only after an explicit invalidation does it pick up the new value.
    let a = fresh_app(Some("offline"));
    assert_eq!(a.effective_trust_mode(), umadev_agent::TrustMode::Guarded);

    // Flip the on-disk config behind the running app's back.
    std::fs::write(
        a.project_root.join(".umadevrc"),
        "[pipeline]\nauto_approve_gates = true\n",
    )
    .unwrap();

    // No session override is set, so without a cache this would re-read disk
    // and flip to Auto. The cache means it stays Guarded — that is the proof
    // the hot path no longer touches the filesystem.
    assert_eq!(
        a.effective_trust_mode(),
        umadev_agent::TrustMode::Guarded,
        "config-derived tier must come from the process cache, not a fresh disk read"
    );

    // After an explicit invalidation, the next call re-reads and sees Auto.
    a.invalidate_trust_cache();
    assert_eq!(
        a.effective_trust_mode(),
        umadev_agent::TrustMode::Auto,
        "invalidation must let the next call pick up the new on-disk config"
    );
}

#[test]
fn gate_card_health_labels_are_localized() {
    // P2-D: the artifact health labels ([warn] MISSING / SCAFFOLD / SHORT /
    // [ok], and the dark-mode marker) were hard-coded English, so a zh-CN
    // user saw English jammed into an otherwise localized card. They now come
    // from the catalog.
    let app = fresh_app(Some("offline"));
    // No output/ artifacts exist for this fresh workspace → every doc is
    // MISSING, exercising the `lines == 0` label.
    let card = gate_card(
        Gate::DocsConfirm,
        &app.slug,
        &app.project_root,
        umadev_i18n::Lang::ZhCn,
    );
    // Localized "missing" label is present; the old raw English is gone.
    assert!(
        card.contains("缺失"),
        "zh-CN gate card should use the localized MISSING label: {card}"
    );
    assert!(
        !card.contains("MISSING") && !card.contains("SCAFFOLD") && !card.contains("SHORT"),
        "no hard-coded English health labels should leak into a zh-CN card: {card}"
    );

    // English locale still shows the English labels (round-trips the key).
    let card_en = gate_card(
        Gate::DocsConfirm,
        &app.slug,
        &app.project_root,
        umadev_i18n::Lang::En,
    );
    assert!(
        card_en.contains("MISSING"),
        "en gate card should render the English MISSING label: {card_en}"
    );
}

#[test]
fn session_override_wins_over_cache() {
    // A `/mode` override always beats the cached config tier and the override
    // path never consults the cache at all (it returns before the disk path).
    let mut a = fresh_app(Some("offline"));
    assert_eq!(a.effective_trust_mode(), umadev_agent::TrustMode::Guarded); // primes cache
    a.set_trust_mode(umadev_agent::TrustMode::Plan);
    assert_eq!(a.effective_trust_mode(), umadev_agent::TrustMode::Plan);
}

#[test]
fn slash_mode_switches_tier_and_keeps_legacy_toggle_consistent() {
    let mut a = fresh_app(Some("offline"));
    a.slash_mode("auto");
    assert_eq!(a.effective_trust_mode(), umadev_agent::TrustMode::Auto);
    assert!(a.auto_approve_on(), "legacy toggle tracks the tier");

    a.thinking = true;
    a.slash_mode("guarded");
    a.slash_set_review_mode(false);
    assert_eq!(a.effective_trust_mode(), umadev_agent::TrustMode::Auto);
    a.thinking = false;

    a.slash_mode("plan");
    assert_eq!(a.effective_trust_mode(), umadev_agent::TrustMode::Plan);
    // plan is read-only → gates do NOT auto-approve.
    assert!(!a.auto_approve_on());

    a.slash_mode("guarded");
    assert_eq!(a.effective_trust_mode(), umadev_agent::TrustMode::Guarded);

    // Unknown arg is rejected without changing the tier.
    a.slash_mode("nonsense");
    assert_eq!(a.effective_trust_mode(), umadev_agent::TrustMode::Guarded);
    assert!(a
        .history
        .iter()
        .any(|m| m.body().contains("nonsense") || m.body().contains("未知")));
}

#[test]
fn active_codex_turn_rejects_a_mode_change_that_its_fixed_sandbox_cannot_adopt() {
    let mut a = fresh_app(Some("codex"));
    assert_eq!(a.effective_trust_mode(), umadev_agent::TrustMode::Guarded);
    a.thinking = true;

    assert_eq!(a.slash_mode("auto"), Action::None);

    assert_eq!(a.effective_trust_mode(), umadev_agent::TrustMode::Guarded);
    assert!(a.history.iter().any(|message| {
        let body = message.body();
        body.contains("cancel") || body.contains("取消")
    }));
}

/// Serialize the `/sandbox` tests that mutate the process-wide thread-safe
/// codex sandbox override so they can't observe each other's writes when the
/// suite runs multi-threaded. Each test restores the override on exit. (The
/// override is shared state, NOT the process env: a runtime setenv racing the
/// codex driver's getenv would be UB.)
static SANDBOX_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn sandbox_env_restore(prev: Option<String>) {
    umadev_host::codex_session::set_codex_sandbox(prev.as_deref());
}

#[test]
fn sandbox_verb_is_registered_and_dispatchable() {
    // The unified-registry contract (mirrors the /model lockstep guard): the
    // palette, help overlay, and dispatcher all read App::COMMANDS, so
    // `/sandbox` must be a registry row AND have a real dispatch arm.
    assert!(
        App::COMMANDS.iter().any(|c| c.name == "sandbox"),
        "/sandbox is registered"
    );
    assert!(
        dispatch_arm_verbs().iter().any(|v| v == "sandbox"),
        "/sandbox has a dispatch arm"
    );
}

#[test]
fn slash_sandbox_no_arg_shows_current_mode_and_all_options() {
    let _guard = SANDBOX_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let prev = umadev_host::codex_session::codex_sandbox_override();
    // Pin a known tier so App::new can't emit a startup danger warning.
    umadev_host::codex_session::set_codex_sandbox(Some("workspace-write"));
    let mut a = fresh_app(Some("codex"));
    let before = a.history.len();
    a.slash_sandbox("");
    let body = a
        .history
        .iter()
        .skip(before)
        .map(|m| m.body().into_owned())
        .collect::<Vec<_>>()
        .join("\n");
    // Current tier + all three options + a usage line are shown.
    assert!(
        body.contains("workspace-write"),
        "shows current tier: {body}"
    );
    assert!(body.contains("read-only"), "lists read-only: {body}");
    assert!(
        body.contains("danger-full-access"),
        "lists danger-full-access: {body}"
    );
    assert!(body.contains("/sandbox"), "shows usage: {body}");
    sandbox_env_restore(prev);
}

#[test]
fn permission_help_and_config_report_the_effective_codex_launch_policy() {
    let _guard = SANDBOX_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let prev = umadev_host::codex_session::codex_sandbox_override();
    // Guarded clamps this raw high-risk preference to workspace-write. Every
    // user-facing surface must report the actual launch sandbox, not the raw
    // configured preference.
    umadev_host::codex_session::set_codex_sandbox(Some("danger-full-access"));
    let mut a = fresh_app(Some("codex"));
    a.lang = umadev_i18n::Lang::ZhCn;
    assert!(
        !a.history.iter().any(|m| matches!(m.role, ChatRole::Error)),
        "startup must not warn that danger is active when Guarded clamps it"
    );
    a.thinking = true;
    a.director_run_in_flight = true;

    let before = a.history.len();
    assert_eq!(a.submit_text("怎么给你权限？".into()), Action::None);
    let answer = a
        .history
        .iter()
        .skip(before)
        .map(|m| m.body().into_owned())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(answer.contains("按回合"), "per-turn authority: {answer}");
    assert!(
        answer.contains("不会锁死"),
        "first turn is not sticky: {answer}"
    );
    assert!(answer.contains("无需重启"), "no restart fiction: {answer}");
    assert!(
        answer.contains("guarded"),
        "trust mode is explicit: {answer}"
    );
    assert!(
        answer.contains("workspace-write"),
        "reports host-resolved sandbox: {answer}"
    );
    assert!(
        a.thinking && a.director_run_in_flight,
        "permission help must not interrupt the in-flight base turn"
    );
    assert!(
        a.queued_chat.is_empty() && a.queued_steer.is_empty(),
        "permission help is answered locally without queueing another base turn"
    );

    a.open_config_overlay();
    let config = a.overlay.take().unwrap().lines.join("\n");
    assert!(
        config.contains("信任模式") && config.contains("guarded"),
        "{config}"
    );
    assert!(
        config.contains("Codex 实际沙箱") && config.contains("workspace-write"),
        "{config}"
    );
    sandbox_env_restore(prev);
}

#[test]
fn permission_help_gives_sandbox_command_only_when_codex_is_actually_read_only() {
    let _guard = SANDBOX_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let prev = umadev_host::codex_session::codex_sandbox_override();
    umadev_host::codex_session::set_codex_sandbox(Some("read-only"));
    let mut a = fresh_app(Some("codex"));
    a.lang = umadev_i18n::Lang::ZhCn;
    let before = a.history.len();

    assert_eq!(a.submit_text("为什么只读？".into()), Action::None);
    let answer = a
        .history
        .iter()
        .skip(before)
        .map(|m| m.body().into_owned())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(answer.contains("read-only"), "{answer}");
    assert!(answer.contains("/sandbox workspace-write"), "{answer}");
    assert!(answer.contains("无需重启"), "{answer}");
    sandbox_env_restore(prev);
}

#[test]
fn slash_sandbox_reports_guarded_clamp_then_warns_only_when_danger_is_effective() {
    let _guard = SANDBOX_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let prev = umadev_host::codex_session::codex_sandbox_override();
    umadev_host::codex_session::set_codex_sandbox(Some("workspace-write"));
    let mut a = fresh_app(Some("codex"));
    let action = a.slash_sandbox("danger-full-access");
    assert_eq!(
        action,
        Action::SandboxChanged,
        "an active Codex base must be rebuilt with the new sandbox"
    );
    // (1) the shared override is published for the next codex turn (same
    // mechanism as startup) — thread-safe state, not the process env.
    assert_eq!(
        umadev_host::codex_session::codex_sandbox_override().as_deref(),
        Some("danger-full-access"),
        "publishes the new tier to the shared override"
    );
    // (2) persisted to .umadevrc so it survives a restart.
    let cfg = umadev_agent::config::load_project_config(&a.project_root);
    assert_eq!(
        cfg.codex.resolved_sandbox(),
        umadev_agent::config::CodexSandbox::DangerFullAccess,
        "persists to .umadevrc [codex] sandbox_mode"
    );
    // Guarded clamps the configured danger preference to workspace-write, so
    // the UI must report that truth instead of claiming full access.
    assert!(
        !a.history.iter().any(|m| matches!(m.role, ChatRole::Error)),
        "a clamped preference is not an active danger sandbox"
    );
    let guarded_body = a
        .history
        .iter()
        .map(|m| m.body().into_owned())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(guarded_body.contains("guarded"), "{guarded_body}");
    assert!(guarded_body.contains("workspace-write"), "{guarded_body}");
    assert!(guarded_body.contains("/mode auto"), "{guarded_body}");

    // Once the user explicitly switches to Auto, the same configured sandbox
    // becomes the real launch sandbox and the red liability warning is honest.
    a.set_trust_mode(umadev_agent::TrustMode::Auto);
    let before = a.history.len();
    assert_eq!(
        a.slash_sandbox("danger-full-access"),
        Action::SandboxChanged
    );
    assert!(
        a.history
            .iter()
            .skip(before)
            .any(|m| matches!(m.role, ChatRole::Error)),
        "effective danger reuses the red liability warning"
    );
    sandbox_env_restore(prev);
}

#[test]
fn slash_sandbox_garbage_shows_usage_and_leaves_env_untouched() {
    let _guard = SANDBOX_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let prev = umadev_host::codex_session::codex_sandbox_override();
    umadev_host::codex_session::set_codex_sandbox(Some("workspace-write"));
    let mut a = fresh_app(Some("codex"));
    let before = a.history.len();
    a.slash_sandbox("yolo-root");
    // Garbage never silently widens/narrows the sandbox.
    assert_eq!(
        umadev_host::codex_session::codex_sandbox_override().as_deref(),
        Some("workspace-write"),
        "garbage leaves the shared override unchanged"
    );
    let body = a
        .history
        .iter()
        .skip(before)
        .map(|m| m.body().into_owned())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(body.contains("/sandbox"), "garbage shows usage: {body}");
    sandbox_env_restore(prev);
}

#[test]
fn slash_sandbox_persist_failure_is_fail_open_env_still_set() {
    let _guard = SANDBOX_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let prev = umadev_host::codex_session::codex_sandbox_override();
    umadev_host::codex_session::set_codex_sandbox(Some("workspace-write"));
    let mut a = fresh_app(Some("codex"));
    // Corrupt .umadevrc so the persist is REFUSED (returns Err) — the shared
    // override must STILL be set (fail-open) and the user warned it didn't save.
    std::fs::write(a.project_root.join(".umadevrc"), "= = not valid toml").unwrap();
    a.slash_sandbox("read-only");
    assert_eq!(
        umadev_host::codex_session::codex_sandbox_override().as_deref(),
        Some("read-only"),
        "fail-open: shared override set even though the persist failed"
    );
    let body = a
        .history
        .iter()
        .map(|m| m.body().into_owned())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        body.contains(".umadevrc"),
        "warns the persist failed: {body}"
    );
    sandbox_env_restore(prev);
}

#[test]
fn plan_mode_does_not_auto_continue_at_gate() {
    let mut a = fresh_app(Some("offline"));
    a.run_started = true;
    a.slash_mode("plan");
    a.apply_engine(EngineEvent::GateOpened {
        gate: Gate::DocsConfirm,
        choice: None,
    });
    // Plan is read-only: the gate pauses, never auto-continues.
    assert!(
        a.pending_auto_continue.is_none(),
        "plan mode must not auto-advance the gate"
    );
    assert_eq!(a.active_gate, Some(Gate::DocsConfirm));
}

/// Reset any persisted trust state so a leftover `.umadev/trust.json` from a
/// previous run of the (reused) test workspace can't skew the counters.
fn reset_trust(a: &mut App) {
    let _ = std::fs::remove_file(a.project_root.join(".umadev").join("trust.json"));
    a.trust_ledger = umadev_agent::TrustLedger::default();
}

#[test]
fn auto_mode_auto_continues_and_records_trust() {
    let mut a = fresh_app(Some("offline"));
    reset_trust(&mut a);
    a.run_started = true;
    a.slash_mode("auto");
    a.apply_engine(EngineEvent::GateOpened {
        gate: Gate::DocsConfirm,
        choice: None,
    });
    // Auto tier auto-advances AND books a trust pass for the gate.
    assert_eq!(a.pending_auto_continue, Some(Gate::DocsConfirm));
    assert_eq!(a.trust_ledger.consecutive("docs_confirm"), 1);
}

#[test]
fn manual_approval_builds_trust_and_suggests_at_threshold() {
    let mut a = fresh_app(Some("offline"));
    reset_trust(&mut a);
    // Guarded default: manually approve the docs gate enough times in a row
    // that the ledger surfaces a one-time auto-advance suggestion.
    for _ in 0..umadev_agent::trust::SUGGEST_THRESHOLD {
        a.active_gate = Some(Gate::DocsConfirm);
        let action = a.submit_text("c".to_string());
        assert_eq!(action, Action::Continue(Gate::DocsConfirm));
    }
    assert_eq!(
        a.trust_ledger.consecutive("docs_confirm"),
        umadev_agent::trust::SUGGEST_THRESHOLD
    );
    assert!(
        a.history.iter().any(|m| m.body().contains("[trust]")),
        "a trust suggestion should have fired once at the threshold"
    );
}

#[test]
fn revision_resets_trust_streak() {
    let mut a = fresh_app(Some("offline"));
    reset_trust(&mut a);
    a.active_gate = Some(Gate::PreviewConfirm);
    let _ = a.submit_text("c".to_string());
    assert_eq!(a.trust_ledger.consecutive("preview_confirm"), 1);
    // A revision at the gate walks back the accumulated trust.
    a.active_gate = Some(Gate::PreviewConfirm);
    let _ = a.submit_text("把图标换成 lucide".to_string());
    assert_eq!(a.trust_ledger.consecutive("preview_confirm"), 0);
}

// ---- input-correctness hardening (wave 3) ----------------------------

#[test]
fn unrelated_note_does_not_clear_thinking_but_route_result_does() {
    let mut a = fresh_app(Some("offline"));
    // A routed chat turn is in flight.
    a.thinking = true;
    a.thinking_started = Some(std::time::Instant::now());
    // An UNRELATED progress note (heartbeat / resume-retry / governance)
    // must NOT extinguish the animation — the route is still running.
    a.apply_engine(EngineEvent::Note("route.resume_retry: retrying".into()));
    assert!(
        a.thinking,
        "a bare progress Note must not clear thinking while a route is in flight"
    );
    // A TERMINAL route outcome DOES clear it: first the failure path…
    a.record_route_failed("route failed: boom".into(), FailedRouteOrigin::Chat);
    assert!(!a.thinking, "a failed route result clears thinking");
    assert!(a.thinking_started.is_none());
    // …and the normal reply path too.
    a.thinking = true;
    a.thinking_started = Some(std::time::Instant::now());
    a.record_chat_reply("hello back".into());
    assert!(!a.thinking, "a chat reply clears thinking");
    assert!(a.thinking_started.is_none());
}

#[test]
fn submitting_while_thinking_queues_instead_of_routing_concurrently() {
    let mut a = fresh_app(Some("offline"));
    // First turn: nothing running → routes, and marks thinking.
    let first = a.submit_text("first message".to_string());
    assert!(matches!(first, Action::Route(_)), "first turn routes");
    assert!(a.thinking, "first routed turn marks thinking");
    assert!(a.queued_chat.is_empty());
    // Second turn WHILE thinking: must NOT spawn a second route — it parks.
    let second = a.submit_text("second message".to_string());
    assert_eq!(
        second,
        Action::None,
        "a turn submitted while thinking must not route concurrently"
    );
    assert_eq!(a.queued_chat.len(), 1, "the extra turn is queued");
    assert_eq!(
        a.queued_chat.front().map(String::as_str),
        Some("second message")
    );
    // A third also queues (FIFO order preserved).
    let _ = a.submit_text("third message".to_string());
    assert_eq!(a.queued_chat.len(), 2);
    assert_eq!(a.take_next_queued_chat().as_deref(), Some("second message"));
    assert_eq!(a.take_next_queued_chat().as_deref(), Some("third message"));
}

#[test]
fn identical_queued_display_text_never_moves_an_attachment_to_another_turn() {
    let mut app = fresh_app(Some("offline"));
    let display = "same visible text".to_string();
    let text_only = SubmittedTurn::text(display.clone());
    let attached = SubmittedTurn {
        text: display.clone(),
        input: TurnInput::new(vec![
            TurnInputBlock::Text {
                text: display.clone(),
            },
            TurnInputBlock::File {
                path: std::path::PathBuf::from("report.txt"),
                mode: FileInputMode::MaterializeText,
            },
        ]),
    };

    app.queue_chat_turn(text_only);
    app.queue_chat_turn(attached);

    assert_eq!(
        app.take_next_queued_chat().as_deref(),
        Some(display.as_str())
    );
    let first = app.take_route_input(&display);
    assert!(
        !first.has_attachments(),
        "the first text-only turn must not steal the later attachment"
    );

    assert_eq!(
        app.take_next_queued_chat().as_deref(),
        Some(display.as_str())
    );
    let second = app.take_route_input(&display);
    assert!(
        second.has_attachments(),
        "the attachment stays bound to its exact FIFO snapshot"
    );
}

#[test]
fn recalling_duplicate_text_uses_the_newest_structured_snapshot() {
    let mut app = fresh_app(Some("offline"));
    let display = "duplicate".to_string();
    app.queue_chat_turn(SubmittedTurn {
        text: display.clone(),
        input: TurnInput::new(vec![TurnInputBlock::File {
            path: std::path::PathBuf::from("older.png"),
            mode: FileInputMode::MaterializeText,
        }]),
    });
    app.queue_chat_turn(SubmittedTurn::text(display.clone()));

    assert!(app.recall_queued_chat());
    assert_eq!(app.input, display);
    assert!(
        app.file_attachments.is_empty() && app.attachments.is_empty(),
        "recall must restore the newest text-only snapshot, not the older attachment"
    );
    assert_eq!(app.queued_chat.len(), 1);
    assert_eq!(app.queued_turn_inputs.len(), 1);
    assert!(app.queued_turn_inputs.front().unwrap().has_attachments());
}

#[test]
fn failed_route_skips_identical_queued_retry_but_keeps_distinct_followup() {
    let mut a = fresh_app(Some("offline"));
    // First turn routes and records the user turn in base-facing memory.
    let first = a.submit_text("same question".to_string());
    assert!(matches!(first, Action::Route(_)));
    assert!(a.thinking);
    // A duplicate Enter/re-send while thinking is parked, plus a real follow-up.
    let _ = a.submit_text("same question".to_string());
    let _ = a.submit_text("different follow-up".to_string());
    assert_eq!(a.queued_chat.len(), 2);

    a.record_route_failed("route failed".into(), FailedRouteOrigin::Chat);

    assert_eq!(
        a.queued_chat.len(),
        1,
        "only the exact duplicate retry is skipped"
    );
    assert_eq!(
        a.queued_chat.front().map(String::as_str),
        Some("different follow-up"),
        "a distinct queued turn remains ready to drain"
    );
    assert!(
        a.history
            .iter()
            .any(|m| m.body().contains("完全相同") || m.body().contains("identical")),
        "the transcript explains why the duplicate was skipped"
    );
    assert_eq!(
        a.conversation
            .iter()
            .filter(|m| m.role == "user" && m.content == "same question")
            .count(),
        1,
        "the skipped duplicate was never recorded into base memory"
    );
    assert_eq!(
        a.take_next_queued_chat().as_deref(),
        Some("different follow-up")
    );
    let roles = a
        .conversation
        .iter()
        .map(|turn| turn.role.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        roles,
        vec!["user", "assistant", "user"],
        "the failed turn is closed before the queued follow-up is dispatched"
    );
}

#[test]
fn director_failure_never_dedups_with_a_stale_chat_dispatch_key() {
    let mut a = fresh_app(Some("offline"));

    // Leave behind the exact key of a previously completed CHAT turn. An
    // explicit `/run` does not replace this key; a model-promoted Director
    // similarly crosses the ownership boundary after the chat dispatch.
    let prior = a.submit_text("inspect the cache".to_string());
    assert!(matches!(prior, Action::Route(_)));
    a.record_chat_reply("prior chat completed".into());
    assert_eq!(a.last_dispatched_chat.as_deref(), Some("inspect the cache"));

    // A legitimate later FIFO turn can happen to have the same text. The
    // Director failure must preserve it rather than treating the stale chat
    // key as proof of an accidental double-Enter.
    a.director_run_in_flight = true;
    a.thinking = true;
    a.queued_chat.push_back("inspect the cache".into());
    a.record_route_failed("director failed".into(), FailedRouteOrigin::Director);

    assert_eq!(
        a.queued_chat.front().map(String::as_str),
        Some("inspect the cache"),
        "Director failure preserves every deferred chat turn"
    );
    assert!(
        a.last_dispatched_chat.is_none(),
        "the stale chat dispatch key is cleared at the Director boundary"
    );
    assert!(
        !a.history
            .iter()
            .any(|m| m.body().contains("完全相同") || m.body().contains("identical")),
        "no duplicate-skipped claim is rendered for a Director failure"
    );

    assert_eq!(
        a.take_next_queued_chat().as_deref(),
        Some("inspect the cache")
    );
    assert_eq!(
        a.last_dispatched_chat.as_deref(),
        Some("inspect the cache"),
        "the key is restored only when the preserved FIFO turn actually dispatches"
    );
}

// ---- explicit retry authority + post-run refresh trigger ----------------------

#[test]
fn a_failed_turn_routes_the_first_explicit_retry_immediately() {
    let mut a = fresh_app(Some("offline"));
    let first = a.submit_text("retry me".to_string());
    assert!(matches!(first, Action::Route(_)));
    assert_eq!(a.last_dispatched_chat.as_deref(), Some("retry me"));
    a.record_route_failed("502 Bad Gateway".into(), FailedRouteOrigin::Chat);
    assert!(!a.thinking);

    let resend = a.submit_text("retry me".to_string());
    assert!(
        matches!(resend, Action::Route(text) if text == "retry me"),
        "the user's first explicit retry must never be swallowed"
    );
}

#[test]
fn a_distinct_follow_up_after_failure_routes_normally() {
    let mut a = fresh_app(Some("offline"));
    let _ = a.submit_text("first thing".to_string());
    a.record_route_failed("boom".into(), FailedRouteOrigin::Chat);
    let other = a.submit_text("a different message".to_string());
    assert!(
        matches!(other, Action::Route(_)),
        "a distinct follow-up routes normally"
    );
}

#[test]
fn a_gate_answer_after_a_failed_turn_is_still_honored() {
    let mut a = fresh_app(Some("offline"));
    let _ = a.submit_text("c".to_string());
    a.record_route_failed("boom".into(), FailedRouteOrigin::Chat);
    a.active_gate = Some(Gate::DocsConfirm);
    let action = a.submit_text("c".to_string());
    assert_eq!(
        action,
        Action::Continue(Gate::DocsConfirm),
        "a gate answer is honored, never suppressed by the dedup guard"
    );
}

#[test]
fn terminal_recorders_clear_the_run_marker_so_the_post_run_refresh_must_snapshot_it_first() {
    // The post-run resident-chat refresh (a `/run` leaves the idle chat session
    // stale) keys off `director_run_in_flight` — but BOTH terminal recorders CLEAR
    // it. So the event loop MUST snapshot `was_run` BEFORE recording; this locks the
    // invariant that makes the capture-before-record ordering load-bearing.
    let mut failed = fresh_app(Some("offline"));
    failed.director_run_in_flight = true;
    failed.record_route_failed("run failed".into(), FailedRouteOrigin::Director);
    assert!(
        !failed.director_run_in_flight,
        "a failed run clears the in-flight marker"
    );

    let mut done = fresh_app(Some("offline"));
    done.director_run_in_flight = true;
    done.record_agentic_done("built it".into(), true, None, None);
    assert!(
        !done.director_run_in_flight,
        "a completed run clears the in-flight marker"
    );
}

// ---- I6: editable queued-input recall ------------------------------------

#[test]
fn i6_up_on_empty_box_recalls_most_recent_queued_message_for_editing() {
    let mut a = fresh_app(Some("offline"));
    // A routed turn is in flight, with two more parked behind it (FIFO).
    let _ = a.submit_text("first message".to_string()); // routes, marks thinking
    assert!(a.thinking);
    let _ = a.submit_text("second message".to_string()); // queued
    let _ = a.submit_text("third message".to_string()); // queued
    assert_eq!(a.queued_chat.len(), 2);
    // Empty box → Up pulls the MOST RECENT queued message back for editing,
    // popping it (recall the queue BEFORE shell history).
    a.input.clear();
    a.input_cursor = 0;
    let act = a.apply_key(KeyCode::Up);
    assert_eq!(act, Action::None);
    assert_eq!(
        a.input, "third message",
        "the newest queued turn is recalled"
    );
    assert_eq!(a.queued_chat.len(), 1, "the recalled turn was popped");
    assert_eq!(
        a.queued_chat.front().map(String::as_str),
        Some("second message"),
        "the earlier queued turn stays parked"
    );
}

#[test]
fn i6_esc_on_empty_box_recalls_queued_message_before_rewind() {
    let mut a = fresh_app(Some("offline"));
    let _ = a.submit_text("first".to_string());
    let _ = a.submit_text("queued edit".to_string());
    assert_eq!(a.queued_chat.len(), 1);
    a.interrupt_armed_at = Some(std::time::Instant::now());
    a.input.clear();
    a.input_cursor = 0;
    // Esc with a parked queued turn recalls it (popping) instead of arming the
    // idle rewind gesture — the box was empty, so the queue wins.
    let act = a.apply_key(KeyCode::Esc);
    assert_eq!(act, Action::None);
    assert_eq!(a.input, "queued edit");
    assert!(
        a.queued_chat.is_empty(),
        "the queued turn was popped for editing"
    );
    assert!(
        !a.pending_rewind,
        "queue recall takes precedence over the rewind arm"
    );
    assert!(
        !a.interrupt_armed(),
        "queue recall clears a stale interrupt arm"
    );
}

#[test]
fn i6_up_with_no_queue_still_does_history_recall() {
    let mut a = fresh_app(Some("offline"));
    a.remember_submission("an earlier prompt");
    assert!(a.queued_chat.is_empty());
    a.input.clear();
    a.input_cursor = 0;
    // Empty box + NO queue → Up recalls shell history exactly as before.
    let act = a.apply_key(KeyCode::Up);
    assert_eq!(act, Action::None);
    assert_eq!(
        a.input, "an earlier prompt",
        "with no queue, history recall is unchanged"
    );
}

// ---- I9: first-run rotating example placeholder --------------------------

#[test]
fn i9_first_run_example_tip_shows_when_idle_empty_early() {
    let a = fresh_app(Some("offline"));
    // Fresh session: idle, empty box, nothing sent yet → a rotating example.
    let tip = a
        .first_run_example_tip()
        .expect("a first-run example shows");
    assert!(!tip.is_empty());
    assert_ne!(
        tip,
        umadev_i18n::t(a.lang, "input.idle"),
        "the tip is the example, layered above the plain idle hint"
    );
    // The empty test workspace has no source file → the generic token is used.
    let generic = umadev_i18n::t(a.lang, "input.example.file_generic");
    assert!(
        tip.contains(generic),
        "names a generic file when none is found: {tip}"
    );
}

#[test]
fn i9_example_tip_vanishes_on_typing_and_after_first_turn() {
    let mut a = fresh_app(Some("offline"));
    assert!(a.first_run_example_tip().is_some(), "shown at first-run");
    // The instant the user types, the box is non-empty → the tip is gone.
    let _ = a.apply_key(KeyCode::Char('h'));
    assert!(!a.input.is_empty());
    assert!(
        a.first_run_example_tip().is_none(),
        "vanishes the moment the user types"
    );
    // Cleared again, but still no submit this session → the tip returns.
    a.input.clear();
    a.input_cursor = 0;
    assert!(
        a.first_run_example_tip().is_some(),
        "empty again, no submit yet → still first-run"
    );
    // After an ACTUAL submit, the first-run window closes for the session.
    a.remember_submission("do a thing");
    a.input.clear();
    a.input_cursor = 0;
    assert!(
        a.first_run_example_tip().is_none(),
        "the first-run window closes after a submit"
    );
}

#[test]
fn i9_example_tip_rotates_by_session_stable_index() {
    let mut a = fresh_app(Some("offline"));
    let templates = [
        "input.example.refactor",
        "input.example.tests",
        "input.example.explain",
    ];
    let generic = umadev_i18n::t(a.lang, "input.example.file_generic").to_string();
    // Rotation index = the persisted prompt-history depth (stable across the
    // first-run window; `session_turns` stays 0 since we don't submit). No RNG.
    for depth in 0..6usize {
        a.input_history.clear();
        for i in 0..depth {
            a.input_history.push_back(format!("p{i}"));
        }
        let tip = a.first_run_example_tip().expect("idle+empty+early");
        let expected = umadev_i18n::tf(a.lang, templates[depth % 3], &[&generic]);
        assert_eq!(tip, expected, "depth {depth} picks template {}", depth % 3);
    }
}

#[test]
fn ctrl_c_interrupts_a_running_pipeline_even_with_nonempty_input() {
    let mut a = fresh_app(Some("offline"));
    a.apply_engine(EngineEvent::PipelineStarted {
        slug: "demo".into(),
        requirement: "build".into(),
    });
    assert!(a.is_pipeline_active());
    // Half-typed next message in the box.
    for c in "half typed".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    assert!(!a.input.is_empty());
    // Ctrl-C while running → INTERRUPT immediately (Claude Code parity),
    // not just clear the input.
    let action = a.apply_key_with_mods(KeyCode::Char('c'), crossterm::event::KeyModifiers::CONTROL);
    assert_eq!(
        action,
        Action::Cancel,
        "Ctrl-C interrupts a running pipeline"
    );
    assert!(
        a.input.is_empty(),
        "the half-typed input is dropped on interrupt"
    );
}

#[test]
fn paused_director_gate_is_interruptible_from_every_control_surface() {
    let paused = || {
        let mut app = fresh_app(Some("offline"));
        app.active_gate = Some(Gate::DocsConfirm);
        app.director_gate_paused = true;
        app
    };

    let mut command = paused();
    assert_eq!(command.try_slash_command("/cancel"), Some(Action::Cancel));

    let mut escape = paused();
    assert_eq!(escape.apply_key(KeyCode::Esc), Action::None);
    assert_eq!(escape.apply_key(KeyCode::Esc), Action::Cancel);

    let mut ctrl_c = paused();
    assert_eq!(
        ctrl_c.apply_key_with_mods(KeyCode::Char('c'), crossterm::event::KeyModifiers::CONTROL,),
        Action::Cancel
    );
}

#[test]
fn queued_turn_is_echoed_recorded_and_uses_chat_text() {
    let mut a = fresh_app(Some("offline"));
    // First turn starts a brain-driven turn (thinking).
    let _ = a.submit_text("first".to_string());
    assert!(a.thinking);
    let convo_before = a.conversation.len();
    let hist_before = a.history.len();
    // Second turn WHILE thinking: queued — but it must STILL be echoed to the
    // transcript (the user sees their message), recorded in conversation
    // memory (so the parked turn isn't lost from the base's context), and the
    // queue note must be the chat text, NOT the pipeline `run.queued` (no gate
    // exists here). This is the "second message looks like it did nothing" fix.
    let _ = a.submit_text("second".to_string());
    // Echoed: the user's "second" message is in the transcript.
    assert!(
        a.history.iter().any(|m| m.body() == "second"),
        "the queued user message is still echoed to the transcript"
    );
    // NOT YET recorded in conversation memory: a queued turn is recorded only
    // when it actually FIRES (in `take_next_queued_chat`), not when parked — so
    // an interrupt that clears the queue can't leave a dangling "user said X"
    // with no assistant reply in the base's context.
    assert_eq!(
        a.conversation.len(),
        convo_before,
        "a parked turn is recorded at drain time, not when queued"
    );
    // A chat.queued note was pushed (history grew by the You echo + the note).
    assert!(a.history.len() >= hist_before + 2);
    let note = umadev_i18n::t(a.lang, "chat.queued");
    assert!(
        a.history.iter().any(|m| m.body() == note),
        "the queue note uses chat.queued, not the gate-flavoured run.queued"
    );
    assert_eq!(a.queued_chat.len(), 1);
}

#[test]
fn ctrl_c_while_thinking_stops_spinner_and_preserves_the_queue() {
    let mut a = fresh_app(Some("offline"));
    // A route in flight, with extra turns parked behind it.
    a.thinking = true;
    a.thinking_started = Some(std::time::Instant::now());
    a.queued_chat.push_back("parked".into());
    for c in "typing".chars() {
        let _ = a.apply_key(KeyCode::Char(c));
    }
    let action = a.apply_key_with_mods(KeyCode::Char('c'), crossterm::event::KeyModifiers::CONTROL);
    assert_eq!(action, Action::Cancel);
    // The real event loop consumes the action through its canonical cancel
    // terminal before it drains the preserved FIFO.
    a.cancel_run();
    assert!(!a.thinking, "Ctrl-C while thinking stops the animation");
    assert!(a.thinking_started.is_none());
    assert_eq!(
        a.queued_chat.front().map(String::as_str),
        Some("parked"),
        "interrupting must not silently discard a parked user turn"
    );
    let expected = umadev_i18n::tf(a.lang, "chat.queued_preserved", &["1"]);
    assert!(
        a.history.iter().any(|message| message.body() == expected),
        "the preserved queue is surfaced visibly"
    );
    assert!(a.input.is_empty());
}

#[test]
fn ctrl_c_on_empty_idle_input_never_quits() {
    // Ctrl+C is universal muscle-memory for COPY, so on an idle EMPTY box it must
    // NOT quit and must NOT even arm a quit-confirm — it only hints to use /quit.
    // (Quitting stays deliberate: /quit, /q, /exit, Ctrl+D, or a double-Esc.)
    let mut a = fresh_app(Some("offline"));
    let action = a.apply_key_with_mods(KeyCode::Char('c'), crossterm::event::KeyModifiers::CONTROL);
    assert_eq!(action, Action::None);
    assert!(
        !a.pending_quit_confirm,
        "idle empty Ctrl-C does NOT arm a quit confirm"
    );
    assert!(!a.should_quit, "idle empty Ctrl-C does NOT quit the app");
    assert_eq!(
        a.history.back().expect("a hint was pushed").body(),
        umadev_i18n::t(a.lang, "quit.use_command"),
        "idle empty Ctrl-C hints to use /quit"
    );
}

#[test]
fn queued_count_reflects_chat_queue_and_steer() {
    let mut a = fresh_app(Some("offline"));
    assert_eq!(a.queued_count(), 0, "nothing queued initially");
    a.queued_chat.push_back("a".into());
    a.queued_chat.push_back("b".into());
    assert_eq!(a.queued_count(), 2, "chat queue counts");
    a.queued_steer.push_back("steer".into());
    assert_eq!(a.queued_count(), 3, "a pending steer adds to the count");
    a.queued_chat.clear();
    a.queued_steer.clear();
    assert_eq!(a.queued_count(), 0, "clears back to zero");
}

#[test]
fn history_recall_preserves_the_in_progress_draft() {
    let mut a = fresh_app(Some("offline"));
    a.remember_submission("first prompt");
    a.remember_submission("second prompt");
    // The user is mid-way through typing a fresh line.
    a.input = "draft I was typing".to_string();
    a.input_cursor = a.input_len();
    // Recall back through history…
    a.input_history_back();
    assert_eq!(a.input, "second prompt");
    a.input_history_back();
    assert_eq!(a.input, "first prompt");
    // …then step forward past the newest entry → the DRAFT is restored, not
    // cleared.
    a.input_history_forward();
    assert_eq!(a.input, "second prompt");
    a.input_history_forward();
    assert_eq!(
        a.input, "draft I was typing",
        "stepping forward past the newest entry restores the stashed draft"
    );
    assert_eq!(
        a.input_cursor,
        a.input_len(),
        "cursor lands at the draft end"
    );
    assert!(a.input_history_idx.is_none(), "recall is over");
}

#[test]
fn up_on_first_row_with_a_nonempty_draft_recalls_and_down_restores_it() {
    // Claude Code parity: ↑ on a first-visual-row caret recalls history EVEN
    // when the box holds a non-empty partial draft (the old gate required an
    // empty box, so ↑ did nothing). The draft is stashed and ↓ restores it.
    let mut a = fresh_app(Some("offline"));
    a.remember_submission("earlier prompt");
    // A non-empty, un-recalled single-line draft; the caret is on the first
    // (only) visual row, so `caret_move_up_wrapped` returns false and the key
    // handler falls through to history recall.
    a.input = "partial draft".to_string();
    a.input_cursor = a.input_len();
    a.input_text_cols.set(40);
    let act = a.apply_key(KeyCode::Up);
    assert_eq!(act, Action::None);
    assert_eq!(
        a.input, "earlier prompt",
        "↑ recalls history even over a non-empty draft"
    );
    assert!(a.input_history_idx.is_some(), "now paging history");
    // ↓ steps forward past the newest entry → the stashed partial returns.
    let _ = a.apply_key(KeyCode::Down);
    assert_eq!(
        a.input, "partial draft",
        "↓ restores the stashed partial draft"
    );
    assert!(a.input_history_idx.is_none(), "recall is over");
}

#[test]
fn multiline_submitted_entry_round_trips_through_persist_load_as_one_entry() {
    // A multi-line requirement (built with Ctrl+J) must survive a restart as a
    // SINGLE history entry. The old newline-joined format re-split it into one
    // entry per physical line; the JSON format keeps it whole.
    let mut a = fresh_app(Some("offline"));
    let entry = "build a login page\n- email + password\n- remember me";
    a.remember_submission(entry); // writes the ring to disk (JSON)
                                  // Simulate a fresh launch: drop the in-memory ring and reload from disk.
    a.input_history.clear();
    a.load_history();
    assert_eq!(
        a.input_history.len(),
        1,
        "the multi-line entry loads as ONE entry, not three lines"
    );
    assert_eq!(
        a.input_history.back().map(String::as_str),
        Some(entry),
        "the multi-line body round-trips verbatim"
    );
}

#[test]
fn legacy_newline_history_file_still_loads_fail_open() {
    // An existing pre-JSON history file (newline-delimited) must still load —
    // each physical line an entry — rather than being dropped.
    let mut a = fresh_app(Some("offline"));
    let path = a.history_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&path, "alpha\nbeta\ngamma").unwrap();
    a.load_history();
    assert_eq!(
        a.input_history.len(),
        3,
        "three legacy lines → three entries"
    );
    assert_eq!(a.input_history.back().map(String::as_str), Some("gamma"));
    assert_eq!(a.input_history.front().map(String::as_str), Some("alpha"));
}

#[test]
fn picker_enter_with_stale_index_does_not_panic() {
    let mut a = fresh_app(Some("offline"));
    a.mode = AppMode::Picker;
    // Force a selection index past the end of whatever the picker holds.
    a.picker_selected = a.picker_items.len() + 5;
    // Must fail-open to a no-op Action, never index-panic.
    let act = a.apply_key_with_mods(KeyCode::Enter, crossterm::event::KeyModifiers::NONE);
    assert!(matches!(act, Action::None));
}

#[test]
fn forward_delete_and_kill_to_eol_reset_palette_selected() {
    let mut a = fresh_app(Some("offline"));
    a.input = "abcdef".to_string();
    a.input_cursor = 2;
    a.palette_selected = 3;
    a.forward_delete();
    assert_eq!(a.palette_selected, 0, "forward_delete resets the palette");

    a.palette_selected = 4;
    a.delete_to_line_end();
    assert_eq!(
        a.palette_selected, 0,
        "delete_to_line_end resets the palette"
    );
}

// ---- /status reconciles with the persisted workflow state ----

#[test]
fn reconcile_phase_statuses_advances_to_persisted_phase() {
    // The plan / director-loop build emits no PhaseStarted/PhaseCompleted,
    // so the in-memory vector is all-Pending. Reconciled against a
    // workflow-state that reached `backend`, every phase up to and including
    // backend must read Done and quality/delivery must stay Pending.
    let rows: Vec<PhaseRow> = PHASE_CHAIN
        .iter()
        .map(|&phase| PhaseRow {
            phase,
            status: PhaseStatus::Pending,
        })
        .collect();
    let statuses = App::reconcile_phase_statuses(&rows, Some(Phase::Backend));
    let backend_i = PHASE_CHAIN
        .iter()
        .position(|&p| p == Phase::Backend)
        .unwrap();
    for (i, (row, status)) in rows.iter().zip(&statuses).enumerate() {
        if i <= backend_i {
            assert_eq!(
                *status,
                PhaseStatus::Done,
                "{} should be done",
                row.phase.id()
            );
        } else {
            assert_eq!(
                *status,
                PhaseStatus::Pending,
                "{} should be pending",
                row.phase.id()
            );
        }
    }
}

#[test]
fn reconcile_phase_statuses_fail_open_and_never_regresses() {
    // Legacy walk: research/docs/docs_confirm done, spec actively Running.
    let rows: Vec<PhaseRow> = PHASE_CHAIN
        .iter()
        .map(|&phase| {
            let status = match phase {
                Phase::Research | Phase::Docs | Phase::DocsConfirm => PhaseStatus::Done,
                Phase::Spec => PhaseStatus::Running,
                _ => PhaseStatus::Pending,
            };
            PhaseRow { phase, status }
        })
        .collect();
    let spec_i = PHASE_CHAIN.iter().position(|&p| p == Phase::Spec).unwrap();
    let backend_i = PHASE_CHAIN
        .iter()
        .position(|&p| p == Phase::Backend)
        .unwrap();
    let quality_i = PHASE_CHAIN
        .iter()
        .position(|&p| p == Phase::Quality)
        .unwrap();

    // No persisted phase → in-memory statuses returned verbatim (fail-open).
    let verbatim = App::reconcile_phase_statuses(&rows, None);
    assert_eq!(
        verbatim,
        rows.iter().map(|r| r.status).collect::<Vec<_>>(),
        "missing/unparseable state must fall back to in-memory only"
    );

    // File at the SAME furthest phase → keep spec's Running (active) marker.
    let same = App::reconcile_phase_statuses(&rows, Some(Phase::Spec));
    assert_eq!(same[spec_i], PhaseStatus::Running);

    // File AHEAD (backend) → spec subsumed into Done, backend Done, quality
    // still Pending.
    let ahead = App::reconcile_phase_statuses(&rows, Some(Phase::Backend));
    assert_eq!(ahead[spec_i], PhaseStatus::Done);
    assert_eq!(ahead[backend_i], PhaseStatus::Done);
    assert_eq!(ahead[quality_i], PhaseStatus::Pending);

    // File BEHIND (docs) → never regress; spec stays Running.
    let behind = App::reconcile_phase_statuses(&rows, Some(Phase::Docs));
    assert_eq!(behind[spec_i], PhaseStatus::Running, "never goes backward");
}

#[test]
fn status_overlay_reflects_persisted_phase_after_plan_run() {
    let tmp = tempfile::TempDir::new().unwrap();
    let state_dir = tmp.path().join(".umadev");
    std::fs::create_dir_all(&state_dir).unwrap();
    // A director-loop / plan build reached `backend` (phase persisted by the
    // run) but emitted no PhaseStarted/PhaseCompleted, so self.phases is
    // all-Pending and the raw table would lie.
    let state_json = r#"{
        "phase": "backend",
        "active_gate": "",
        "slug": "shop",
        "requirement": "做个电商后台",
        "last_transition_at": "2026-06-27T10:00:00Z",
        "note": "",
        "spec_version": "UMADEV_HOST_SPEC_V1"
    }"#;
    std::fs::write(state_dir.join("workflow-state.json"), state_json).unwrap();

    let cfg = UserConfig {
        backend: Some("offline".into()),
        lang: Some("zh-CN".into()),
        ..Default::default()
    };
    let mut app = App::new(
        "shop",
        cfg,
        tmp.path().join("config.toml"),
        tmp.path().to_path_buf(),
    );
    // Precondition: the in-memory phase vector is frozen all-Pending.
    assert!(
        app.phases.iter().all(|r| r.status == PhaseStatus::Pending),
        "the plan path leaves self.phases all-Pending"
    );

    app.open_status_overlay();
    let lines = app.overlay.as_ref().expect("overlay opened").lines.clone();
    // The pipeline-phases table precedes the knowledge table, so `find`
    // returns the pipeline row (the only one carrying a status icon).
    let row = |phase: &str| {
        lines
            .iter()
            .find(|l| l.contains(&format!("| {phase} |")))
            .cloned()
            .unwrap_or_default()
    };
    for done in [
        "research",
        "docs",
        "docs_confirm",
        "spec",
        "frontend",
        "preview_confirm",
        "backend",
    ] {
        assert!(
            row(done).contains("[ok]"),
            "{done} row should be done, got: {:?}",
            row(done)
        );
    }
    for pending in ["quality", "delivery"] {
        assert!(
            row(pending).contains("[pending]"),
            "{pending} row should be pending, got: {:?}",
            row(pending)
        );
    }
}

// ===== Feature A — completion notification (terminal bell) =====

#[test]
fn bell_env_parsing_default_on_and_falsy_off() {
    // Unset → default ON.
    assert!(bell_enabled_from_env(None));
    // Truthy / unrecognized → ON.
    assert!(bell_enabled_from_env(Some("1")));
    assert!(bell_enabled_from_env(Some("on")));
    assert!(bell_enabled_from_env(Some("")));
    // The documented OFF values (case-insensitive, trimmed).
    assert!(!bell_enabled_from_env(Some("0")));
    assert!(!bell_enabled_from_env(Some("false")));
    assert!(!bell_enabled_from_env(Some(" OFF ")));
    assert!(!bell_enabled_from_env(Some("No")));
}

/// Build an `Instant` `secs` in the past (saturating at "now" on the rare
/// host where the monotonic clock is younger than `secs`).
fn secs_ago(secs: u64) -> Option<std::time::Instant> {
    std::time::Instant::now()
        .checked_sub(std::time::Duration::from_secs(secs))
        .or_else(|| Some(std::time::Instant::now()))
}

#[test]
fn a_long_finished_run_rings_the_bell_a_quick_one_does_not() {
    // A run that's been going well past the threshold reaching delivery rings.
    let mut app = fresh_app(Some("offline"));
    app.bell_enabled = true;
    app.run_started = true;
    app.run_started_at = secs_ago(6);
    app.apply_engine(EngineEvent::BlockCompleted {
        final_phase: Phase::Delivery,
        paused_at: None,
    });
    assert!(app.finished);
    assert!(app.bell_pending, "a long finished run arms the bell");
    assert_eq!(app.bell_count, 1);
    // `take_bell` drains it (the event loop emits the BEL once).
    assert!(app.take_bell());
    assert!(!app.bell_pending);
    assert!(!app.take_bell(), "drained — no second beep");

    // A run that JUST started reaching delivery must not beep (quick turn).
    let mut quick = fresh_app(Some("offline"));
    quick.bell_enabled = true;
    quick.run_started = true;
    quick.run_started_at = Some(std::time::Instant::now());
    quick.apply_engine(EngineEvent::BlockCompleted {
        final_phase: Phase::Delivery,
        paused_at: None,
    });
    assert!(quick.finished);
    assert!(!quick.bell_pending, "a quick run does not beep");
    assert_eq!(quick.bell_count, 0);
}

#[test]
fn an_aborted_long_run_rings_and_umadev_bell_zero_silences() {
    // A long run that aborts (the ABORT_SENTINEL note) rings the away user.
    let mut app = fresh_app(Some("offline"));
    app.bell_enabled = true;
    app.run_started = true;
    app.run_started_at = secs_ago(7);
    app.apply_engine(EngineEvent::Note(format!("{}boom", crate::ABORT_SENTINEL)));
    assert!(app.aborted, "the sentinel note flips the run into aborted");
    assert!(app.bell_pending, "an aborted long run arms the bell");

    // bell_enabled = false (UMADEV_BELL=0) silences even a long abort.
    let mut silent = fresh_app(Some("offline"));
    silent.bell_enabled = false;
    silent.run_started = true;
    silent.run_started_at = secs_ago(7);
    silent.apply_engine(EngineEvent::Note(format!("{}boom", crate::ABORT_SENTINEL)));
    assert!(silent.aborted);
    assert!(!silent.bell_pending, "UMADEV_BELL=0 silences the bell");
    assert_eq!(silent.bell_count, 0);
}

#[test]
fn a_long_agentic_turn_rings_a_short_chat_reply_does_not() {
    // A long agentic turn settling (the common chat path) rings.
    let mut app = fresh_app(Some("offline"));
    app.bell_enabled = true;
    app.thinking = true;
    app.thinking_started = secs_ago(6);
    app.record_agentic_done("done".into(), false, None, None);
    assert!(app.bell_pending, "a long agentic turn arms the bell");
    assert_eq!(app.bell_count, 1);

    // A snappy chat reply (a second or two) does NOT beep.
    let mut quick = fresh_app(Some("offline"));
    quick.bell_enabled = true;
    quick.thinking = true;
    quick.thinking_started = Some(std::time::Instant::now());
    quick.record_agentic_done("hi".into(), false, None, None);
    assert!(!quick.bell_pending, "a quick reply does not beep");
    assert_eq!(quick.bell_count, 0);
}

// ===== Feature B — search-in-transcript =====

/// Seed the folded-row cache + scroll bounds the search normally reads off a
/// render, so search logic is testable without a terminal frame.
fn seed_transcript(app: &App, rows: &[&str]) {
    *app.transcript_rows.borrow_mut() = rows.iter().map(|s| (*s).to_string()).collect();
    *app.transcript_gutters.borrow_mut() = vec![0; rows.len()];
}

#[test]
fn search_finds_case_insensitive_matches_and_nav_wraps() {
    let mut app = fresh_app(Some("offline"));
    seed_transcript(
        &app,
        &["the quick brown fox", "jumps over the lazy dog", "THE END"],
    );
    // Renderer-published scroll bounds, so focus-into-view has math to do.
    app.transcript_max_scroll.set(10);
    app.transcript_viewport_rows.set(4);

    app.open_search();
    assert!(app.search.is_some());
    // Type "the" through the key path (routed to the modal search handler).
    for c in "the".chars() {
        let _ = app.apply_key(crossterm::event::KeyCode::Char(c));
    }
    {
        let s = app.search.as_ref().unwrap();
        assert_eq!(s.matches.len(), 3, "three case-insensitive matches");
        assert_eq!(s.current, 0);
        // Each match carries its (visual-row, char-span) coordinate.
        assert_eq!(
            (s.matches[0].row, s.matches[0].start, s.matches[0].end),
            (0, 0, 3)
        );
        assert_eq!(s.matches[1].row, 1);
        assert_eq!(s.matches[2].row, 2, "uppercase THE matched too");
    }

    // n/N (next/prev) cycle the current index and WRAP.
    app.search_next();
    assert_eq!(app.search.as_ref().unwrap().current, 1);
    app.search_next();
    assert_eq!(app.search.as_ref().unwrap().current, 2);
    app.search_next();
    assert_eq!(
        app.search.as_ref().unwrap().current,
        0,
        "next wraps past the end"
    );
    app.search_prev();
    assert_eq!(
        app.search.as_ref().unwrap().current,
        2,
        "prev wraps past the start"
    );

    // The current match's position is turned into a scroll offset that brings
    // its row into view, and navigating actually applied it.
    let row = app.search.as_ref().unwrap().matches[2].row;
    let off = app.search_scroll_offset_for(row);
    assert_eq!(
        off,
        app.transcript_scroll(),
        "focus set the transcript scroll"
    );
    // max(10) - (row 2 - viewport/2 (=2) → 0) = 10.
    assert_eq!(off, 10);

    // Esc clears search entirely.
    let _ = app.apply_key(crossterm::event::KeyCode::Esc);
    assert!(app.search.is_none(), "Esc closes + clears search");
}

#[test]
fn ctrl_f_opens_search_modally_and_swallows_typing() {
    let mut app = fresh_app(Some("offline"));
    seed_transcript(&app, &["alpha beta gamma"]);
    // Ctrl+F opens the bar.
    let _ = app.apply_key_with_mods(
        crossterm::event::KeyCode::Char('f'),
        crossterm::event::KeyModifiers::CONTROL,
    );
    assert!(app.search.is_some(), "Ctrl+F opens search");
    // While open, typing filters the query and never reaches the input box
    // (so it can't collide with the slash palette / @-mention popover).
    let before = app.input.clone();
    for c in "beta".chars() {
        let _ = app.apply_key(crossterm::event::KeyCode::Char(c));
    }
    assert_eq!(
        app.input, before,
        "typing goes to search, not the input box"
    );
    let s = app.search.as_ref().unwrap();
    assert_eq!(s.query, "beta");
    assert_eq!(s.matches.len(), 1);
    assert_eq!(s.matches[0].row, 0);

    let _ = app.apply_key(crossterm::event::KeyCode::Char('\u{8}'));
    assert_eq!(
        app.search.as_ref().unwrap().query,
        "bet",
        "Windows BS deletes from the search query"
    );
    let _ = app.apply_key(crossterm::event::KeyCode::Char('a'));

    // Enter advances to the next match (single match → stays put, no panic).
    let _ = app.apply_key(crossterm::event::KeyCode::Enter);
    assert_eq!(app.search.as_ref().unwrap().current, 0);

    // A query with no hits clears matches but keeps search open.
    for c in "ZZZ".chars() {
        let _ = app.apply_key(crossterm::event::KeyCode::Char(c));
    }
    assert!(app.search.as_ref().unwrap().matches.is_empty());
    assert!(app.search.is_some());
}
