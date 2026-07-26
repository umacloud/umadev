//! Hermetic terminal-contract tests shared by Linux, macOS, and Windows CI.
//!
//! These tests deliberately use ratatui's in-memory backend and byte sinks:
//! they never launch a real base CLI, inspect OAuth state, or read/write HOME.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use ratatui::Terminal;
use unicode_width::UnicodeWidthStr;

use crate::app::{Action, App};
use crate::clipboard::{native_clipboard_plan, normalize_clipboard_newlines, NativeClipboardPlan};
use crate::config::UserConfig;

struct Fixture {
    app: App,
    _root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::TempDir::new().expect("temporary terminal fixture");
        let project_root = root.path().join("项目 with spaces");
        std::fs::create_dir(&project_root).expect("temporary project root");
        let mut app = App::new(
            "terminal-contract",
            UserConfig {
                backend: Some("offline".to_string()),
                ..UserConfig::default()
            },
            root.path().join("配置 with spaces.toml"),
            project_root,
        );
        app.lang = umadev_i18n::Lang::En;
        Self { app, _root: root }
    }
}

fn render_to_rows(app: &App, width: u16, height: u16) -> Vec<String> {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("in-memory terminal");
    terminal
        .draw(|frame| crate::ui::render(frame, app))
        .expect("in-memory render");
    (0..height)
        .map(|row| {
            (0..width)
                .map(|col| terminal.backend().buffer()[(col, row)].symbol())
                .collect::<String>()
        })
        .collect()
}

fn submit(app: &mut App, text: &str) -> Action {
    app.input = text.to_string();
    app.input_cursor = text.chars().count();
    app.apply_key(KeyCode::Enter)
}

#[cfg(windows)]
fn ansi(command: impl crossterm::Command) -> String {
    let mut output = String::new();
    command
        .write_ansi(&mut output)
        .expect("terminal command has a static ANSI representation");
    output
}

#[test]
fn terminal_contract_copy_feedback_is_status_only_and_expires() {
    let mut fixture = Fixture::new();
    let before_history = fixture.app.history.clone();
    let before_conversation = fixture.app.conversation.clone();
    let before_transcript = fixture.app.full_transcript.clone();
    let started = Instant::now();

    fixture.app.show_copy_toast_at(80, started);
    let expected = umadev_i18n::tf(fixture.app.lang, "tui.copied", &["80"]);
    let screen = render_to_rows(&fixture.app, 120, 32).join("\n");

    assert_eq!(screen.matches(&expected).count(), 1, "{screen}");
    assert!(fixture
        .app
        .transcript_rows
        .borrow()
        .iter()
        .all(|row| !row.contains(&expected)));
    assert_eq!(fixture.app.history, before_history);
    assert_eq!(fixture.app.conversation, before_conversation);
    assert_eq!(fixture.app.full_transcript, before_transcript);
    let just_before_expiry = crate::selection::COPY_TOAST_TTL
        .checked_sub(Duration::from_millis(1))
        .expect("copy toast TTL exceeds one millisecond");
    assert!(!fixture.app.expire_copy_toast(started + just_before_expiry));
    assert!(fixture
        .app
        .expire_copy_toast(started + crate::selection::COPY_TOAST_TTL));

    let expired = render_to_rows(&fixture.app, 120, 32).join("\n");
    assert!(!expired.contains(&expected), "{expired}");
}

#[test]
fn terminal_contract_deferred_chat_and_current_run_steer_are_visibly_distinct() {
    let mut fixture = Fixture::new();
    fixture.app.thinking = true;
    fixture.app.director_run_in_flight = true;

    assert_eq!(submit(&mut fixture.app, "把配色换成暗色"), Action::None);
    assert_eq!(submit(&mut fixture.app, "完成后再做登录"), Action::None);
    assert_eq!(fixture.app.queued_steer.len(), 1);
    assert_eq!(fixture.app.queued_chat.len(), 1);

    let steer_note = umadev_i18n::t(fixture.app.lang, "run.steer_queued");
    let deferred_note = umadev_i18n::t(fixture.app.lang, "run.deferred");
    assert!(fixture
        .app
        .history
        .iter()
        .any(|row| row.body() == steer_note));
    assert!(fixture
        .app
        .history
        .iter()
        .any(|row| row.body() == deferred_note));

    let screen = render_to_rows(&fixture.app, 140, 42).join("\n");
    assert!(screen.contains("[steer]"), "{screen}");
    assert!(screen.contains("[queued]"), "{screen}");
    assert!(screen.contains("[queued 2]"), "{screen}");
}

#[test]
fn terminal_contract_resize_reflows_cjk_emoji_combining_and_neutralizes_ansi() {
    let mut fixture = Fixture::new();
    let body = concat!(
        "组合 e\u{301} 中文宽字符 emoji 🧑🏽‍💻 👨‍👩‍👧‍👦 ",
        "Windows 路径 C:\\Users\\微优\\项目 空格\\src\\main.rs ",
        "\u{1b}[31mred\u{1b}[0m"
    );
    fixture.app.push_workspace_notice(body);

    // Stay above the product's 40-column minimum so this exercises transcript
    // reflow rather than the intentional "resize terminal" fallback card.
    let backend = TestBackend::new(44, 24);
    let mut terminal = Terminal::new(backend).expect("in-memory terminal");
    terminal
        .draw(|frame| crate::ui::render(frame, &fixture.app))
        .expect("narrow render");
    let narrow_rows = fixture.app.transcript_rows.borrow().clone();
    let narrow_width = usize::from(fixture.app.transcript_area.get().2);
    assert!(narrow_rows.len() > 1, "the fixture must wrap when narrow");
    assert!(narrow_rows
        .iter()
        .all(|row| UnicodeWidthStr::width(row.as_str()) <= narrow_width));

    terminal.backend_mut().resize(78, 30);
    terminal
        .resize(Rect::new(0, 0, 78, 30))
        .expect("resize in-memory terminal");
    terminal
        .draw(|frame| crate::ui::render(frame, &fixture.app))
        .expect("wide render");
    let wide_rows = fixture.app.transcript_rows.borrow().clone();
    let wide_width = usize::from(fixture.app.transcript_area.get().2);
    assert!(wide_rows.len() < narrow_rows.len());
    assert!(wide_rows
        .iter()
        .all(|row| UnicodeWidthStr::width(row.as_str()) <= wide_width));

    let screen_text = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>();
    assert!(!screen_text.contains('\u{1b}'));
    assert!(fixture
        .app
        .history
        .iter()
        .any(|row| row.body().contains("e\u{301}")));
    assert!(fixture
        .app
        .history
        .iter()
        .any(|row| row.body().contains("C:\\Users\\微优\\项目 空格")));
}

#[test]
fn terminal_contract_plan_snapshot_is_lossless_and_scrollable_at_44x12() {
    let mut fixture = Fixture::new();
    fixture.app.backend = Some("grok-build".to_string());

    let seats = [
        "product-manager",
        "architect",
        "uiux-designer",
        "frontend-engineer",
        "backend-engineer",
        "qa-engineer",
        "security-engineer",
        "devops-engineer",
    ];
    fixture
        .app
        .apply_engine(umadev_agent::EngineEvent::PlanPosted {
            statuses: vec!["active".into(); 20],
            steps: (0..20)
                .map(|index| {
                    format!(
                        "s{index} · preserve complete 窄终端计划 step {index} ({})",
                        seats[index % seats.len()]
                    )
                })
                .collect(),
            done: 0,
            total: 20,
        });
    fixture
        .app
        .apply_engine(umadev_agent::EngineEvent::BaseSessionState {
            backend_id: "grok-build".to_string(),
            update: umadev_runtime::SessionStateUpdate::PlanReplaced {
                entries: vec![umadev_runtime::SessionPlanEntry {
                    content: "native base plan remains reachable".to_string(),
                    priority: umadev_runtime::SessionPlanEntryPriority::High,
                    status: umadev_runtime::SessionPlanEntryStatus::InProgress,
                }],
            },
        });
    fixture
        .app
        .apply_engine(umadev_agent::EngineEvent::CriticVerdict {
            seat: "security-engineer".to_string(),
            accepts: false,
            blocking: vec!["review blocker remains reachable".to_string()],
            remediation: vec!["review fix remains reachable".to_string()],
            advisory: vec!["review advisory remains reachable".to_string()],
        });

    assert_eq!(submit(&mut fixture.app, "/plan"), Action::None);
    let _ = render_to_rows(&fixture.app, 44, 12);

    let transcript_rows = fixture.app.transcript_rows.borrow().clone();
    let transcript = transcript_rows.join("\n");
    let compact_transcript = transcript.split_whitespace().collect::<Vec<_>>().join(" ");
    for expected in [
        "s19",
        "Base plan",
        "native base plan remains reachable",
        "Team",
        "Security",
        "review blocker remains reachable",
        "review fix remains reachable",
        "review advisory remains reachable",
    ] {
        assert!(
            compact_transcript.contains(expected),
            "missing {expected}: {transcript}"
        );
    }

    let transcript_width = usize::from(fixture.app.transcript_area.get().2);
    assert!(transcript_width > 0);
    assert!(transcript_rows
        .iter()
        .all(|row| UnicodeWidthStr::width(row.as_str()) <= transcript_width));
    assert!(
        fixture.app.transcript_max_scroll.get() > 0,
        "the narrow fixture must overflow"
    );

    fixture.app.transcript_scroll_to_top();
    let _ = render_to_rows(&fixture.app, 44, 12);
    assert_eq!(
        fixture.app.transcript_scroll(),
        fixture.app.transcript_max_scroll.get()
    );
    fixture.app.transcript_scroll_to_bottom();
    let _ = render_to_rows(&fixture.app, 44, 12);
    assert_eq!(fixture.app.transcript_scroll(), 0);
}

#[test]
fn terminal_contract_conpty_interrupt_preserves_chat_and_restore_is_symmetric() {
    let mut fixture = Fixture::new();
    fixture.app.thinking = true;
    fixture.app.queued_chat.push_back("完成后解释结果".into());

    let action = fixture
        .app
        .apply_key_with_mods(KeyCode::Char('\u{3}'), KeyModifiers::NONE);
    assert_eq!(
        action,
        Action::Cancel,
        "ConPTY literal Ctrl-C must normalize"
    );
    fixture.app.cancel_run();
    assert!(!fixture.app.thinking);
    assert_eq!(
        fixture.app.queued_chat.front().map(String::as_str),
        Some("完成后解释结果")
    );

    let mut enabled = Vec::new();
    super::enable_terminal_modes(&mut enabled, true).expect("Vec writer");
    let enabled = String::from_utf8(enabled).expect("terminal bytes are UTF-8 escapes");
    let mut restored = Vec::new();
    super::restore_sequence_inner(&mut restored, true);
    let restored = String::from_utf8(restored).expect("terminal bytes are UTF-8 escapes");

    // On Windows, crossterm deliberately executes mouse and alternate-screen
    // commands through WinAPI instead of serializing them to the supplied byte
    // sink. Exercise the real helpers above, then compare their platform-neutral
    // protocol representations here. A real ConPTY remains an explicit OS gap.
    #[cfg(unix)]
    let (enabled_protocol, restored_protocol) = (enabled.as_str(), restored.as_str());
    #[cfg(windows)]
    let (enabled_protocol, restored_protocol) = {
        use crossterm::cursor::Show;
        use crossterm::event::{
            DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
            EnableFocusChange, EnableMouseCapture,
        };
        use crossterm::style::ResetColor;
        use crossterm::terminal::{
            DisableLineWrap, EnableLineWrap, EndSynchronizedUpdate, LeaveAlternateScreen,
        };

        assert!(enabled.is_ascii() && restored.is_ascii());
        let on = [
            ansi(DisableLineWrap),
            ansi(EnableBracketedPaste),
            ansi(EnableMouseCapture),
            ansi(EnableFocusChange),
            ansi(Show),
        ]
        .concat();
        let off = [
            ansi(LeaveAlternateScreen),
            ansi(EnableLineWrap),
            ansi(DisableMouseCapture),
            ansi(DisableFocusChange),
            ansi(DisableBracketedPaste),
            ansi(EndSynchronizedUpdate),
            ansi(Show),
            ansi(ResetColor),
        ]
        .concat();
        (on, off)
    };

    for (on, off) in [
        ("\u{1b}[?7l", "\u{1b}[?7h"),
        ("\u{1b}[?1000h", "\u{1b}[?1000l"),
        ("\u{1b}[?1002h", "\u{1b}[?1002l"),
        ("\u{1b}[?1003h", "\u{1b}[?1003l"),
        ("\u{1b}[?1004h", "\u{1b}[?1004l"),
        ("\u{1b}[?1006h", "\u{1b}[?1006l"),
        ("\u{1b}[?2004h", "\u{1b}[?2004l"),
    ] {
        assert!(enabled_protocol.contains(on), "missing setup mode {on:?}");
        assert!(
            restored_protocol.contains(off),
            "missing restore mode {off:?}"
        );
    }
    let leave = restored_protocol
        .find("\u{1b}[?1049l")
        .expect("leave alt screen");
    let wrap = restored_protocol
        .find("\u{1b}[?7h")
        .expect("restore line wrap");
    assert!(
        leave < wrap,
        "line wrap must be restored on the primary screen"
    );
    assert!(super::resume_gap_elapsed(
        Duration::from_secs(5),
        Duration::from_secs(5)
    ));
    assert!(!super::resume_gap_elapsed(
        Duration::from_millis(250),
        Duration::from_secs(5)
    ));
}

#[test]
fn terminal_contract_clipboard_crlf_and_platform_routing_are_deterministic() {
    let mixed = "C:\\Users\\微优\\项目 空格\\a.rs\r\n第二行\r第三行\n";
    assert_eq!(
        normalize_clipboard_newlines(mixed, true),
        "C:\\Users\\微优\\项目 空格\\a.rs\r\n第二行\r\n第三行\r\n"
    );
    assert_eq!(
        normalize_clipboard_newlines(mixed, false),
        "C:\\Users\\微优\\项目 空格\\a.rs\n第二行\n第三行\n"
    );
    assert_eq!(
        native_clipboard_plan("windows"),
        NativeClipboardPlan::Windows
    );
    assert_eq!(native_clipboard_plan("macos"), NativeClipboardPlan::Macos);
    assert_eq!(
        native_clipboard_plan("linux"),
        NativeClipboardPlan::UnixLike
    );
}
