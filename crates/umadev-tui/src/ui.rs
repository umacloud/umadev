//! Ratatui rendering — pure function of [`App`] state.
//!
//! Two screens, dispatched on [`AppMode`]:
//!
//! - **Picker** — first-launch backend chooser.
//! - **Chat** — persistent input box + scrolling conversation history,
//!   modelled after Claude Code's REPL feel.

use ratatui::layout::{Constraint, Direction, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};

// ─── Theme tokens — UmaDev brand cyan, dark + light aware ─────────────────
// The brand color is cyan (#06b6d4 / #0891b2), chosen because it reads as
// modern + developer-tool (Vercel/Linear/Deno family) and doesn't collide
// with Claude Code's orange. Colors resolve at runtime to a dark or light
// palette by probing the COLORFGBG env var (set by most modern terminals),
// so the TUI adapts to light backgrounds instead of washing out.
#[allow(dead_code, non_snake_case)] // palette complete; UPPER_CASE mirrors CSS color tokens.
mod theme {
    use ratatui::style::Color;

    /// Two complete palettes. Each tuple is (dark, light) so callers pick by
    /// [`is_dark()`]. Brand cyan anchors both: bright cyan on dark, deeper
    /// cyan on light (for contrast against a white bg).
    struct Pair {
        dark: Color,
        light: Color,
    }

    const fn rgb(r: u8, g: u8, b: u8) -> Color {
        Color::Rgb(r, g, b)
    }

    // Light/dark is probed ONCE in lib::setup_terminal() (OSC 11 + COLORFGBG,
    // before raw mode) and stored here. Default dark until probed.
    use std::sync::atomic::{AtomicBool, Ordering};
    static IS_LIGHT: AtomicBool = AtomicBool::new(false);

    /// Called once at launch (before raw mode) with the OSC 11 probe result.
    pub fn set_light_theme(is_light: bool) {
        IS_LIGHT.store(is_light, Ordering::Relaxed);
    }

    fn is_dark() -> bool {
        !IS_LIGHT.load(Ordering::Relaxed)
    }

    fn pick(p: &Pair) -> Color {
        if is_dark() {
            p.dark
        } else {
            p.light
        }
    }

    // ── Palette definitions (dark, light) ──
    const BG_PANEL_P: Pair = Pair {
        dark: rgb(0x1e, 0x20, 0x30),
        light: rgb(0xee, 0xf2, 0xf7),
    };
    const BG_ELEMENT_P: Pair = Pair {
        dark: rgb(0x22, 0x24, 0x36),
        light: rgb(0xe2, 0xe8, 0xf0),
    };
    const BORDER_P: Pair = Pair {
        dark: rgb(0x54, 0x5c, 0x7e),
        light: rgb(0xcb, 0xd5, 0xe1),
    };
    const BORDER_ACTIVE_P: Pair = Pair {
        dark: rgb(0x73, 0x7a, 0xa2),
        light: rgb(0x94, 0xa3, 0xb8),
    };
    const BORDER_STRONG_P: Pair = Pair {
        dark: rgb(0x90, 0x99, 0xb2),
        light: rgb(0x64, 0x74, 0x8b),
    };
    // Brand: cyan. Bright on dark, deep on light for contrast.
    const PRIMARY_P: Pair = Pair {
        dark: rgb(0x22, 0xd3, 0xee),  // cyan-400 — wordmark, primary action
        light: rgb(0x08, 0x91, 0xb2), // cyan-600
    };
    const SECONDARY_P: Pair = Pair {
        dark: rgb(0xa8, 0x55, 0xf7),  // purple-500
        light: rgb(0x7e, 0x22, 0xce), // purple-700
    };
    const ACCENT_P: Pair = Pair {
        dark: rgb(0x06, 0xb6, 0xd4),  // cyan-500 — the brand accent (icon)
        light: rgb(0x0e, 0x74, 0x9c), // cyan-700
    };
    const SUCCESS_P: Pair = Pair {
        dark: rgb(0x34, 0xd3, 0x99),  // emerald-400
        light: rgb(0x05, 0x96, 0x69), // emerald-600
    };
    const WARNING_P: Pair = Pair {
        dark: rgb(0xfb, 0xbb, 0x24),  // amber-400
        light: rgb(0xd9, 0x77, 0x06), // amber-600
    };
    const ERROR_P: Pair = Pair {
        dark: rgb(0xf8, 0x71, 0x71),  // red-400
        light: rgb(0xdc, 0x26, 0x26), // red-600
    };
    const INFO_P: Pair = Pair {
        dark: rgb(0x38, 0xbd, 0xf8),  // sky-400
        light: rgb(0x02, 0x86, 0xca), // sky-600
    };
    const TEXT_P: Pair = Pair {
        dark: rgb(0xe2, 0xe8, 0xf0),  // slate-200
        light: rgb(0x1e, 0x29, 0x3b), // slate-800
    };
    const TEXT_MUTED_P: Pair = Pair {
        dark: rgb(0x94, 0xa3, 0xb8),  // slate-400
        light: rgb(0x64, 0x74, 0x8b), // slate-500
    };

    // ── Public accessors (runtime-resolved) ──
    pub fn BG_PANEL() -> Color {
        pick(&BG_PANEL_P)
    }
    pub fn BG_ELEMENT() -> Color {
        pick(&BG_ELEMENT_P)
    }
    pub fn BORDER() -> Color {
        pick(&BORDER_P)
    }
    pub fn BORDER_ACTIVE() -> Color {
        pick(&BORDER_ACTIVE_P)
    }
    pub fn BORDER_STRONG() -> Color {
        pick(&BORDER_STRONG_P)
    }
    pub fn PRIMARY() -> Color {
        pick(&PRIMARY_P)
    }
    pub fn SECONDARY() -> Color {
        pick(&SECONDARY_P)
    }
    pub fn ACCENT() -> Color {
        pick(&ACCENT_P)
    }
    pub fn SUCCESS() -> Color {
        pick(&SUCCESS_P)
    }
    pub fn WARNING() -> Color {
        pick(&WARNING_P)
    }
    pub fn ERROR() -> Color {
        pick(&ERROR_P)
    }
    pub fn INFO() -> Color {
        pick(&INFO_P)
    }
    pub fn TEXT() -> Color {
        pick(&TEXT_P)
    }
    pub fn TEXT_MUTED() -> Color {
        pick(&TEXT_MUTED_P)
    }
    /// User message background — Claude Code uses rgb(55,55,55) dark /
    /// rgb(240,240,240) light for the user message tint bar.
    pub fn USER_MSG_BG() -> Color {
        if IS_LIGHT.load(Ordering::Relaxed) {
            Color::Rgb(240, 240, 242)
        } else {
            Color::Rgb(42, 42, 52)
        }
    }
    pub fn MD_HEADING() -> Color {
        pick(&SECONDARY_P)
    }
    pub fn MD_CODE() -> Color {
        pick(&SUCCESS_P)
    }
    pub fn MD_LINK() -> Color {
        pick(&PRIMARY_P)
    }

    /// Per-role left-bar color — brand cyan for `UmaDev` itself, semantic
    /// accents for the other speakers.
    pub fn role_bar(role: crate::app::ChatRole) -> Color {
        use crate::app::ChatRole;
        match role {
            ChatRole::You => PRIMARY(),
            ChatRole::UmaDev => ACCENT(),
            ChatRole::Host => SUCCESS(),
            ChatRole::Gate => WARNING(),
            ChatRole::System => BORDER_ACTIVE(),
        }
    }
}
use ratatui::Frame;

use crate::app::{App, AppMode, ChatRole};

/// Set the terminal's light/dark classification, probed once at launch
/// (OSC 11 + COLORFGBG) before raw mode. Re-exported from [`theme`].
pub fn set_light_theme(is_light: bool) {
    theme::set_light_theme(is_light);
}

/// Draw one full frame — dispatches on the current screen.
pub fn render(frame: &mut Frame, app: &App) {
    match app.mode {
        AppMode::Picker => render_picker(frame, app),
        AppMode::Chat => render_chat(frame, app),
    }
    // Overlay precedence: scrollable content overlay wins over help.
    if let Some(ov) = &app.overlay {
        render_scroll_overlay(frame, ov);
    } else if app.show_help {
        render_help_overlay(frame, app);
    }
}

/// Render a scrollable, near-fullscreen overlay used by `/spec`,
/// `/verify`, `/doctor`, `/diff`, `/history`.
fn render_scroll_overlay(frame: &mut Frame, ov: &crate::app::Overlay) {
    let area = centered_rect(frame.area(), 88, 88);
    frame.render_widget(Clear, area);

    let inner_height = area.height.saturating_sub(3) as usize;
    let total = ov.lines.len();
    let from = ov.scroll;
    let to = (from + inner_height).min(total);
    let visible: Vec<Line<'static>> = ov
        .lines
        .iter()
        .skip(from)
        .take(to - from)
        .map(|l| Line::from(l.clone()))
        .collect();

    let lang = umadev_i18n::current();
    let progress = if total == 0 {
        format!(" {} ", umadev_i18n::t(lang, "tui.overlay.empty"))
    } else {
        let pct = if total <= inner_height {
            100
        } else {
            ((from + inner_height).min(total) * 100) / total
        };
        umadev_i18n::tf(
            lang,
            "tui.overlay.progress",
            &[
                &(from + 1).to_string(),
                &to.to_string(),
                &total.to_string(),
                &pct.to_string(),
            ],
        )
    };
    let title_full = format!("{}{progress}", ov.title);

    let body = Paragraph::new(visible)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title_full)
                .border_style(Style::default().fg(theme::BORDER_ACTIVE())),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(body, area);
}

/// Simple per-line code syntax highlighting (keywords=accent, strings=green,
/// comments=muted, rest=green). Zero dependencies.
fn colorize_code_line(line: &str) -> Vec<Span<'static>> {
    let trimmed = line.trim_start();
    // Comment lines.
    if trimmed.starts_with("//") || (trimmed.starts_with('#') && !trimmed.starts_with("#{")) {
        return vec![Span::styled(
            line.to_string(),
            Style::default().fg(theme::TEXT_MUTED()),
        )];
    }
    // Default: green, but colorize string literals brighter.
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    for ch in line.chars() {
        if ch == '"' {
            if !buf.is_empty() {
                spans.push(Span::styled(
                    std::mem::take(&mut buf),
                    Style::default().fg(theme::SUCCESS()),
                ));
            }
            buf.push(ch);
            spans.push(Span::styled(
                std::mem::take(&mut buf),
                Style::default().fg(theme::WARNING()),
            ));
        } else {
            buf.push(ch);
        }
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, Style::default().fg(theme::SUCCESS())));
    }
    spans
}

/// Lightweight markdown → styled Lines renderer. Handles the most common
/// patterns the worker outputs: headings (#/##/###), code blocks (```...```),
/// bullet lists (-/*/•), numbered lists, and inline `code`. No external
/// dependency — just pattern matching per line. Returns styled Lines that
/// the chat history renderer can splice in.
fn markdown_to_lines(text: &str, base_color: Color) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut in_code_block = false;
    for raw in text.lines() {
        let trimmed = raw.trim();
        // Code fence toggle.
        if trimmed.starts_with("```") {
            in_code_block = !in_code_block;
            lines.push(Line::from(Span::styled(
                if in_code_block {
                    "  ┌── code ──".to_string()
                } else {
                    "  └──────────".to_string()
                },
                Style::default().fg(theme::TEXT_MUTED()),
            )));
            continue;
        }
        if in_code_block {
            // Simple syntax highlight: keywords (accent), strings (green),
            // comments (muted), rest (green).
            let colored = colorize_code_line(raw);
            let mut spans: Vec<Span<'static>> = vec![Span::raw("  ")];
            spans.extend(colored);
            lines.push(Line::from(spans));
            continue;
        }
        // Headings — purple/magenta headings, like opencode's markdownHeading.
        if let Some(h) = trimmed.strip_prefix("### ") {
            lines.push(Line::from(Span::styled(
                format!("  {h}"),
                Style::default()
                    .fg(theme::MD_HEADING())
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            )));
            continue;
        }
        if let Some(h) = trimmed.strip_prefix("## ") {
            lines.push(Line::from(Span::styled(
                format!(" {h}"),
                Style::default()
                    .fg(theme::MD_HEADING())
                    .add_modifier(Modifier::BOLD),
            )));
            continue;
        }
        if let Some(h) = trimmed.strip_prefix("# ") {
            lines.push(Line::from(Span::styled(
                format!(" {h}"),
                Style::default()
                    .fg(theme::MD_HEADING())
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            )));
            continue;
        }
        // Bullet list. Use strip_prefix (not a hardcoded byte slice) — the `•`
        // marker is 3 bytes, so `&trimmed[2..]` would slice mid-char and panic.
        if let Some(content) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
            .or_else(|| trimmed.strip_prefix("• "))
        {
            lines.push(Line::from(vec![
                Span::styled("  • ", Style::default().fg(theme::INFO())),
                Span::styled(content.to_string(), Style::default().fg(base_color)),
            ]));
            continue;
        }
        // Numbered list (1. 2. etc).
        if let Some(pos) = trimmed.find(". ") {
            if pos <= 3 && trimmed[..pos].chars().all(|c| c.is_ascii_digit()) {
                let num = &trimmed[..pos];
                let content = &trimmed[pos + 2..];
                lines.push(Line::from(vec![
                    Span::styled(format!("  {num}. "), Style::default().fg(theme::INFO())),
                    Span::styled(content.to_string(), Style::default().fg(base_color)),
                ]));
                continue;
            }
        }
        // Inline code spans (simple: wrap `code` in the markdown-code color).
        if trimmed.contains('`') {
            let mut spans: Vec<Span<'static>> = vec![Span::raw(" ")];
            let mut in_code = false;
            for part in raw.split('`') {
                if in_code {
                    spans.push(Span::styled(
                        part.to_string(),
                        Style::default().fg(theme::MD_CODE()),
                    ));
                } else if !part.is_empty() {
                    spans.push(Span::styled(
                        part.to_string(),
                        Style::default().fg(base_color),
                    ));
                }
                in_code = !in_code;
            }
            lines.push(Line::from(spans));
            continue;
        }
        // Empty line → spacer.
        if trimmed.is_empty() {
            lines.push(Line::from(""));
            continue;
        }
        // Plain text.
        lines.push(Line::from(Span::styled(
            format!(" {raw}"),
            Style::default().fg(base_color),
        )));
    }
    lines
}

// ---------- Picker (first launch) -----------------------------------------

fn render_picker(frame: &mut Frame, app: &App) {
    let total = frame.area();

    // ── Layout: vertically centered, opencode-home-style flex column ──
    // Growing spacers above and below push the content block to the optical
    // center. The block itself is fixed-height so it never jitters as the
    // list resizes.
    // ── Adaptive list sizing ── few items (language / mode / base CLI) get
    // spacious rows; a long list (the ~14 API providers) goes compact and
    // WINDOWS around the selection so the card never outgrows the terminal.
    let count = app.picker_items.len();
    let spacious = count <= 5;
    let per_item = if spacious { 2usize } else { 1 };
    let avail_rows = usize::from(total.height.saturating_sub(12)).max(4);
    let max_items = (avail_rows / per_item).max(3);
    let windowed = count > max_items;
    let win_start = if windowed {
        app.picker_selected
            .saturating_sub(max_items / 2)
            .min(count - max_items)
    } else {
        0
    };
    let win_end = (win_start + max_items).min(count);
    // 1 leading blank + per_item rows per visible item + 2 scroll hints.
    let list_height =
        u16::try_from(1 + per_item * (win_end - win_start) + usize::from(windowed) * 2)
            .unwrap_or(8);
    let card_height = list_height + 2; // +title +border
    let center = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(2),              // top spacer (grows)
            Constraint::Length(4),           // logo row (icon + wordmark) + breathing gap
            Constraint::Length(1),           // tagline
            Constraint::Length(1),           // gap
            Constraint::Length(card_height), // selection card
            Constraint::Length(1),           // gap
            Constraint::Length(2),           // footer hint (wraps to 2 lines)
            Constraint::Min(1),              // bottom spacer (grows)
        ])
        .split(total);

    // ── Logo — terminal-window `>_` monogram + bold wordmark ──
    // A small terminal window with a `>_` prompt inside (brand orange, like
    // Claude's clawd_body) sits next to a bold wordmark (primary blue) + muted
    // version — the same compact horizontal layout Claude Code uses for
    // `[Clawd] Claude Code …`. The window is built from █ ▄ ▀ ▐ ▌ half-block
    // pixels so it renders sharp in any monospace terminal. The `>` and `_`
    // read as a shell prompt — instantly says "this is a dev tool".
    let icon = theme::ACCENT(); // brand orange window + prompt
    let word = theme::PRIMARY(); // primary blue wordmark
    let dim = theme::TEXT_MUTED();
    let logo_lines: Vec<Line> = vec![
        Line::from(vec![
            Span::styled(" ▟▀▀▀▀▀▙  ", Style::default().fg(icon)),
            Span::styled(
                "UmaDev",
                Style::default().fg(word).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled(" ▌ ", Style::default().fg(icon)),
            Span::styled(">", Style::default().fg(word).add_modifier(Modifier::BOLD)),
            Span::styled(" ", Style::default()),
            Span::styled("_", Style::default().fg(icon)),
            Span::styled(" ▐  ", Style::default().fg(icon)),
            Span::styled(
                format!("v{} ", env!("CARGO_PKG_VERSION")),
                Style::default().fg(dim),
            ),
            Span::styled(
                umadev_i18n::t(app.lang, "picker.logo_subtitle"),
                Style::default().fg(dim),
            ),
        ]),
        Line::from(vec![
            Span::styled(" ▜▄▄▄▄▄▛  ", Style::default().fg(icon)),
            Span::raw("            "),
        ]),
        // Breathing room between the logo box and the call-to-action tagline.
        Line::from(""),
    ];
    frame.render_widget(Paragraph::new(logo_lines), center[1]);

    // ── Tagline — the action line beneath the wordmark ──
    // (The version + “AI 编码的项目总监 Agent” already ride next to the wordmark on
    // row 2 of the logo block above. This row is the call-to-action.)
    let tagline = Line::from(Span::styled(
        umadev_i18n::t(app.lang, "picker.tagline"),
        Style::default().fg(theme::TEXT_MUTED()),
    ));
    frame.render_widget(Paragraph::new(tagline), center[2]);

    // ── Step card ── one card per step; the title carries the progress
    // ("Step N / 3 · <title>") so the guided flow reads as distinct steps.
    let step_title = match app.picker_step {
        crate::app::PickerStep::Language => umadev_i18n::t(app.lang, "setup.step.language"),
        crate::app::PickerStep::BaseCli => umadev_i18n::t(app.lang, "setup.step.base"),
    };
    let progress = umadev_i18n::tf(
        app.lang,
        "setup.progress",
        &[&app.picker_step.number().to_string()],
    );

    let mut items: Vec<ListItem> = vec![ListItem::new(Line::from(""))];
    // "N more above" indicator when the long list is scrolled.
    if windowed && win_start > 0 {
        items.push(ListItem::new(Line::from(Span::styled(
            format!(
                "    {}",
                umadev_i18n::tf(app.lang, "setup.more_above", &[&win_start.to_string()])
            ),
            Style::default().fg(theme::TEXT_MUTED()),
        ))));
    }
    for (idx, item) in app
        .picker_items
        .iter()
        .enumerate()
        .skip(win_start)
        .take(win_end - win_start)
    {
        let is_selected = idx == app.picker_selected;
        // Brand left-bar marks the selected row (Claude Code style).
        let bar = if is_selected { "▌" } else { "  " };
        // Only base-CLI rows carry a readiness mark; mode/language rows don't.
        let (icon, icon_color) = if item.backend_id.is_some() {
            if item.ready {
                ("[ok]", theme::SUCCESS())
            } else {
                ("·", theme::TEXT_MUTED())
            }
        } else {
            ("", theme::TEXT_MUTED())
        };
        let label_style = if is_selected {
            Style::default()
                .fg(theme::PRIMARY())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::TEXT())
        };
        items.push(ListItem::new(Line::from(vec![
            Span::styled(format!(" {bar} "), Style::default().fg(theme::PRIMARY())),
            Span::styled(format!("{:<26}", item.label), label_style),
            Span::styled(format!("{icon} "), Style::default().fg(icon_color)),
            Span::styled(
                item.detail.clone(),
                Style::default().fg(theme::TEXT_MUTED()),
            ),
        ])));
        if spacious {
            items.push(ListItem::new(Line::from("")));
        }
    }
    // "N more below" indicator.
    if windowed && win_end < count {
        items.push(ListItem::new(Line::from(Span::styled(
            format!(
                "    {}",
                umadev_i18n::tf(
                    app.lang,
                    "setup.more_below",
                    &[&(count - win_end).to_string()]
                )
            ),
            Style::default().fg(theme::TEXT_MUTED()),
        ))));
    }

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(Span::styled(
                    format!(" {progress} · {step_title} "),
                    Style::default()
                        .fg(theme::PRIMARY())
                        .add_modifier(Modifier::BOLD),
                ))
                .border_style(Style::default().fg(theme::BORDER())),
        )
        .highlight_style(Style::default().bg(theme::BG_ELEMENT()));
    frame.render_widget(list, center[4]);

    // ── Footer hint — show a rejection notice inline (so selecting an
    // un-installed host gives visible feedback ON the picker, not on the
    // chat screen the user can't see yet). ──
    let footer = if let Some(notice) = &app.picker_notice {
        Line::from(Span::styled(
            format!("  {notice}"),
            Style::default().fg(theme::WARNING()),
        ))
    } else {
        Line::from(Span::styled(
            format!("  {}", umadev_i18n::t(app.lang, "setup.nav_hint")),
            Style::default().fg(theme::TEXT_MUTED()),
        ))
    };
    frame.render_widget(
        Paragraph::new(footer).wrap(ratatui::widgets::Wrap { trim: true }),
        center[6],
    );
}

// ---------- Chat (main loop) ----------------------------------------------

/// The chat screen is laid out like opencode's session route: a borderless
/// content column with a thin title row on top, the scrolling transcript in
/// the middle, and a left-bar prompt pinned to the bottom. No outer chrome
/// boxes — the visual rhythm comes from the per-message left bars and the
/// background-tinted prompt, exactly like the reference.
/// Minimum terminal size the chat screen needs to lay out the title, at least
/// one transcript row, the input box and the status row without pushing a fixed
/// region off-screen. Below this we show a "make the window bigger" card.
const MIN_CHAT_WIDTH: u16 = 40;
const MIN_CHAT_HEIGHT: u16 = 10;

fn render_chat(frame: &mut Frame, app: &App) {
    let area = frame.area();
    // Tiny-terminal guard — when the window is too short/narrow the vertical
    // solver would crush the transcript to 0 and clip the input + status bar OUT
    // of view (the "looks frozen / can't see the bottom" failure). Show a
    // centered hint and bail BEFORE laying out, so the fixed regions never fall
    // off the screen.
    if area.height < MIN_CHAT_HEIGHT || area.width < MIN_CHAT_WIDTH {
        render_too_small(frame, area);
        return;
    }
    // Horizontal indent: both opencode and Claude Code indent their content
    // column by 2 cols on each side (paddingLeft/paddingRight = 2). Doing it
    // once here means the title, transcript, prompt and status row all line
    // up on the same gutters instead of kissing the terminal edges.
    let inner = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .split(area)[1];
    // Prompt height tracks the wrapped input: 1 row when empty/short, growing
    // (capped) as the user types or it wraps — underline sits right under it.
    // CLAMP it so the title(1) + at least one transcript row + status(1) always
    // fit: `area.height - 3` is the most the prompt may take. Without this a
    // tall multi-line input on a short terminal would shove the status row (and
    // the bottom of the input) past the viewport edge.
    let prompt_h = prompt_block_height(&app.input, inner.width, mode_prefix_width(app))
        .min(inner.height.saturating_sub(3))
        .max(2);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),        // title row (borderless)
            Constraint::Min(1),           // transcript (grows; ≥1 guaranteed)
            Constraint::Length(prompt_h), // prompt: input(N) + border(1) + meta(1)
            Constraint::Length(1),        // status row
        ])
        .split(inner);

    render_title_row(frame, chunks[0], app);
    render_transcript(frame, chunks[1], app);
    render_prompt(frame, chunks[2], app);
    render_status_row(frame, chunks[3], app);

    // Slash-command palette popover floats above the prompt when the user is
    // typing a `/`-prefixed command with at least one match.
    let palette = app.palette_matches();
    if !palette.is_empty() {
        render_palette_popover(frame, chunks[2], app, &palette);
    }
}

/// A centered "terminal too small" card, shown when the window is below
/// [`MIN_CHAT_WIDTH`] × [`MIN_CHAT_HEIGHT`]. Keeping the whole chat layout off
/// the screen in this state is what stops the input box / status bar from being
/// clipped out of view (the "looks frozen" symptom). The message itself wraps
/// and degrades gracefully even at 1×1.
fn render_too_small(frame: &mut Frame, area: Rect) {
    frame.render_widget(Clear, area);
    let lang = umadev_i18n::current();
    let lines = vec![
        Line::from(Span::styled(
            umadev_i18n::t(lang, "tui.too_small.title"),
            Style::default()
                .fg(theme::WARNING())
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            umadev_i18n::tf(
                lang,
                "tui.too_small.resize",
                &[&MIN_CHAT_WIDTH.to_string(), &MIN_CHAT_HEIGHT.to_string()],
            ),
            Style::default().fg(theme::TEXT_MUTED()),
        )),
        Line::from(Span::styled(
            umadev_i18n::tf(
                lang,
                "tui.too_small.now",
                &[&area.width.to_string(), &area.height.to_string()],
            ),
            Style::default().fg(theme::TEXT_MUTED()),
        )),
    ];
    // Vertically center the 3-line card; horizontally center each line.
    let card_h = u16::try_from(lines.len()).unwrap_or(3);
    let top = area.height.saturating_sub(card_h) / 2;
    let card = Rect {
        x: area.x,
        y: area
            .y
            .saturating_add(top)
            .min(area.y + area.height.saturating_sub(1)),
        width: area.width,
        height: card_h.min(area.height),
    };
    frame.render_widget(
        Paragraph::new(lines)
            .alignment(ratatui::layout::Alignment::Center)
            .wrap(Wrap { trim: true }),
        card,
    );
}

/// Thin, borderless title row: `UmaDev · <slug> · <phase>` with a subtle
/// bottom rule. Replaces the heavy bordered header that made the screen feel
/// boxed-in.
fn render_title_row(frame: &mut Frame, area: Rect, app: &App) {
    let title = Span::styled(
        format!(" UmaDev {} ", env!("CARGO_PKG_VERSION")),
        Style::default()
            .fg(theme::PRIMARY())
            .add_modifier(Modifier::BOLD),
    );
    let slug = Span::styled(
        format!(
            " {} ",
            if app.slug.is_empty() {
                umadev_i18n::t(app.lang, "tui.title.workspace_placeholder")
            } else {
                &app.slug
            }
        ),
        Style::default().fg(theme::TEXT_MUTED()),
    );
    // Honest stall signal: when a phase is running but the base has gone quiet
    // past the threshold, paint the status RED — a truthful "about to hang" cue
    // beats a fake-smooth spinner. Normal accent color the rest of the time.
    let phase_color = if app.is_stalled() {
        theme::ERROR()
    } else {
        theme::ACCENT()
    };
    let phase = Span::styled(
        format!(" {} ", app.status),
        Style::default().fg(phase_color),
    );
    let line = Line::from(vec![
        title,
        Span::styled("·", Style::default().fg(theme::BORDER())),
        slug,
        Span::styled("·", Style::default().fg(theme::BORDER())),
        phase,
    ]);
    // Fill the rest of the row with a faint rule so it reads as a divider.
    let mut rule = String::new();
    for _ in 0..area.width.saturating_sub(40) {
        rule.push('─');
    }
    let para = Paragraph::new(vec![
        line,
        Line::from(Span::styled(rule, Style::default().fg(theme::BORDER()))),
    ]);
    frame.render_widget(para, area);
}

/// The scrolling transcript — opencode-style. Each message is a block with a
/// tinted left bar (the speaker's accent color) and an indented body. No
/// container border around the whole thing; the per-message bars carry the
/// structure. Gates render as a compact left-bar warning panel instead of
/// the old ASCII box art.
/// The session welcome banner — the `>_` monogram + wordmark + tagline, drawn
/// ONCE at the very top of the transcript so a fresh chat doesn't feel empty.
/// It is prepended to the scrolling content (not pinned), so it scrolls away
/// naturally as the conversation grows — the Claude Code model.
fn welcome_lines(app: &App) -> Vec<Line<'static>> {
    let icon = theme::ACCENT();
    let word = theme::PRIMARY();
    let dim = theme::TEXT_MUTED();
    vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(" ▟▀▀▀▀▀▙  ", Style::default().fg(icon)),
            Span::styled(
                "UmaDev",
                Style::default().fg(word).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  v{}", env!("CARGO_PKG_VERSION")),
                Style::default().fg(dim),
            ),
        ]),
        Line::from(vec![
            Span::styled(" ▌ ", Style::default().fg(icon)),
            Span::styled(">", Style::default().fg(word).add_modifier(Modifier::BOLD)),
            Span::raw(" "),
            Span::styled("_", Style::default().fg(icon)),
            Span::styled(" ▐  ", Style::default().fg(icon)),
            Span::styled(
                umadev_i18n::t(app.lang, "picker.tagline"),
                Style::default().fg(dim),
            ),
        ]),
        Line::from(Span::styled(" ▜▄▄▄▄▄▛  ", Style::default().fg(icon))),
        Line::from(""),
    ]
}

fn render_transcript(frame: &mut Frame, area: Rect, app: &App) {
    const MAX_RENDER_LINES: usize = 500;
    let inner_height = area.height as usize;

    // Welcome banner first — it scrolls away as the conversation fills in.
    let mut rendered: Vec<Line<'static>> = welcome_lines(app);
    for (msg_idx, msg) in app.history.iter().enumerate() {
        // Top gap before each message for breathing room (Claude Code: marginTop=1).
        if msg_idx > 0 {
            rendered.push(Line::from(""));
        }

        if msg.role == ChatRole::Gate {
            render_gate_block(&msg.body, theme::WARNING(), &mut rendered);
            continue;
        }

        match msg.role {
            // **User messages** — full-width tinted background bar (Claude Code:
            // userMessageBackground = rgb(55,55,55)), no leading dot.
            ChatRole::You => {
                for line in msg.body.lines() {
                    rendered.push(Line::from(Span::styled(
                        format!(" {line}"),
                        Style::default().fg(theme::TEXT()).bg(theme::USER_MSG_BG()),
                    )));
                }
            }
            // **Assistant/Host messages** — leading `●` bullet + plain text
            // on terminal background (Claude Code: AssistantTextMessage).
            ChatRole::UmaDev | ChatRole::Host => {
                let body_lines = markdown_to_lines(&msg.body, theme::TEXT());
                for (i, bl) in body_lines.into_iter().enumerate() {
                    let mut spans: Vec<Span<'static>> = Vec::new();
                    if i == 0 {
                        spans.push(Span::styled("● ", Style::default().fg(theme::ACCENT())));
                    } else {
                        spans.push(Span::raw("  "));
                    }
                    spans.extend(bl.spans);
                    rendered.push(Line::from(spans));
                }
            }
            // **System messages** — dim/muted, no bullet.
            ChatRole::System => {
                for line in msg.body.lines() {
                    rendered.push(Line::from(Span::styled(
                        format!("  {line}"),
                        Style::default().fg(theme::TEXT_MUTED()),
                    )));
                }
            }
            ChatRole::Gate => unreachable!(),
        }
    }
    // Live waiting indicator — an animated spinner + verb + ticking elapsed,
    // pinned just above the input while the base replies, so a sent message
    // visibly "works" instead of looking frozen.
    if app.thinking {
        let secs = app.thinking_elapsed_secs();
        let elapsed = if secs > 0 {
            format!(
                "  ({}s · {})",
                secs,
                umadev_i18n::t(app.lang, "status.esc_cancel")
            )
        } else {
            String::new()
        };
        rendered.push(Line::from(""));
        rendered.push(Line::from(vec![
            Span::styled(
                format!("{} ", app.spinner()),
                Style::default().fg(theme::ACCENT()),
            ),
            Span::styled(
                umadev_i18n::t(app.lang, "status.thinking").to_string(),
                Style::default()
                    .fg(theme::ACCENT())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(elapsed, Style::default().fg(theme::TEXT_MUTED())),
        ]));
    }
    if rendered.len() > MAX_RENDER_LINES {
        rendered = rendered.split_off(rendered.len() - MAX_RENDER_LINES);
    }

    // Wrap long lines to the CURRENT width so content REFLOWS on resize (a
    // narrower window re-wraps instead of clipping), then stick to the bottom by
    // scrolling past the overflow. We estimate the wrapped height from each
    // line's display width (exact for the common short-line case; `line_count`
    // is private in ratatui).
    let w = usize::from(area.width).max(1);
    let total: usize = rendered
        .iter()
        .map(|l| {
            let lw: usize = l.spans.iter().map(|s| disp_width(s.content.as_ref())).sum();
            lw.div_ceil(w).max(1)
        })
        .sum();
    let para = Paragraph::new(rendered).wrap(Wrap { trim: false });
    let hidden_above = total.saturating_sub(inner_height);

    // Publish the scroll bounds for the key handlers (Home/End, Page, Ctrl-U/D,
    // Shift+↑/↓, mouse wheel) — they clamp `transcript_scroll` against these
    // width-aware numbers instead of guessing. `transcript_scroll` counts rows
    // ABOVE the bottom; clamp it here so a stale value (e.g. after the window
    // grew and content now fits) can't push the view off the end.
    app.transcript_max_scroll.set(hidden_above);
    app.transcript_viewport_rows.set(inner_height);
    let user_offset = app.transcript_scroll.min(hidden_above);

    // Effective scroll: bottom-pinned is `hidden_above`; scrolling up SUBTRACTS
    // the user's offset so older content comes into view. At offset 0 the view
    // auto-sticks to the newest line (the default).
    let scroll_rows = hidden_above.saturating_sub(user_offset);
    let scroll = u16::try_from(scroll_rows).unwrap_or(u16::MAX);
    let para = para.scroll((scroll, 0));

    // Two-way scroll indicator: how many rows are hidden ABOVE the current view
    // and BELOW it, so the user always knows there's more in either direction
    // and which keys bring it on screen.
    let rows_above = scroll_rows; // rows scrolled past, above the viewport
    let rows_below = user_offset; // rows hidden below when scrolled up
    let para = if hidden_above == 0 {
        para
    } else {
        let hint = if rows_below > 0 {
            umadev_i18n::tf(
                app.lang,
                "tui.scroll.both",
                &[&rows_above.to_string(), &rows_below.to_string()],
            )
        } else {
            umadev_i18n::tf(app.lang, "tui.scroll.above", &[&rows_above.to_string()])
        };
        para.block(
            Block::default()
                .title_top(Span::styled(hint, Style::default().fg(theme::TEXT_MUTED())))
                .title_alignment(ratatui::layout::Alignment::Right),
        )
    };
    frame.render_widget(para, area);
}

/// Gate messages render as a single bordered warning panel — a compact,
/// unmissable "pause and decide" card with a yellow left bar. Replaces the
/// old full-width ASCII box-drawing art that read as amateur.
fn render_gate_block(body: &str, bar: Color, rendered: &mut Vec<Line<'static>>) {
    let lang = umadev_i18n::current();
    let title = Line::from(vec![
        Span::styled("▎ ", Style::default().fg(bar)),
        Span::styled(
            umadev_i18n::t(lang, "tui.gate_block.title"),
            Style::default()
                .fg(theme::WARNING())
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    rendered.push(title);
    for line in body.lines() {
        rendered.push(Line::from(vec![
            Span::styled("  ", Style::default().fg(bar)),
            Span::styled(line.to_string(), Style::default().fg(theme::TEXT())),
        ]));
    }
    rendered.push(Line::from(vec![
        Span::styled("  ", Style::default().fg(bar)),
        Span::styled(
            umadev_i18n::t(lang, "tui.gate_block.hint"),
            Style::default().fg(theme::TEXT_MUTED()),
        ),
    ]));
}

/// The prompt — opencode-style. A panel with the agent-tinted left bar, the
/// Check if a char is double-width (CJK, emoji, etc.) for terminal display.
/// Used to calculate cursor position correctly — without this, typing
/// Chinese/Japanese/Korean would put the cursor in the wrong place.
trait CjkWide {
    fn is_cjk_wide(&self) -> bool;
}

impl CjkWide for char {
    fn is_cjk_wide(&self) -> bool {
        let c = *self as u32;
        // Common CJK ranges (double-width in monospace terminals).
        c >= 0x1100
            && (
                // Hangul Jamo, CJK Radicals, Kangxi
                c <= 0x115F || // Hangul Jamo
            c == 0x2329 || c == 0x232A ||
            (0x2E80..=0xA4CF).contains(&c) || // CJK Radicals + Yi
            (0xAC00..=0xD7A3).contains(&c) || // Hangul Syllables
            (0xF900..=0xFAFF).contains(&c) || // CJK Compatibility Ideographs
            (0xFE30..=0xFE4F).contains(&c) || // CJK Compatibility Forms
            (0xFF00..=0xFF60).contains(&c) || // Fullwidth Forms
            (0xFFE0..=0xFFE6).contains(&c) || // Fullwidth Signs
            (0x1F300..=0x1FAFF).contains(&c) || // Emoji + CJK Symbols
            (0x20000..=0x3FFFD).contains(&c)
                // CJK Extensions B-F
            )
    }
}

/// Display columns a string occupies (ASCII = 1, CJK/wide = 2).
fn disp_width(s: &str) -> usize {
    s.chars().map(|c| usize::from(c.is_cjk_wide()) + 1).sum()
}

/// Truncate `s` to at most `max` display columns (CJK = 2), char-aligned so a
/// wide glyph is never split. Returns the kept prefix; the caller decides
/// whether to add an ellipsis. Used by the status row so a long CJK phase
/// string can never overflow a narrow terminal.
fn truncate_to_width(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut col = 0usize;
    for c in s.chars() {
        let cw = usize::from(c.is_cjk_wide()) + 1;
        if col + cw > max {
            break;
        }
        out.push(c);
        col += cw;
    }
    out
}

/// Hard-wrap `text` into rows of at most `width` display columns (char-level,
/// like a terminal), honoring explicit `\n`. Always returns at least one row.
/// This is what lets the input box GROW with content and put the underline
/// right under the last line.
fn wrap_input_rows(text: &str, width: u16) -> Vec<String> {
    let w = (width.max(1)) as usize;
    let mut rows: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut col = 0usize;
    for c in text.chars() {
        if c == '\n' {
            rows.push(std::mem::take(&mut cur));
            col = 0;
            continue;
        }
        let cw = usize::from(c.is_cjk_wide()) + 1;
        if col + cw > w && col > 0 {
            rows.push(std::mem::take(&mut cur));
            col = 0;
        }
        cur.push(c);
        col += cw;
    }
    rows.push(cur);
    rows
}

/// Display width of the input's row-0 prefix (mode marker + one space):
/// `>_ ` (idle) = 3, `[run] ` = 6, `[gate] ` = 7. The wrap width, box height,
/// continuation indent and cursor ALL derive from this so they stay in lockstep
/// at any terminal width — otherwise the wider run/gate markers push the text
/// past the right edge on a narrow terminal.
fn mode_prefix_width(app: &App) -> u16 {
    if app.active_gate.is_some() {
        7
    } else if app.run_started && !app.finished {
        6
    } else {
        3
    }
}

/// The width available to the input TEXT after the mode prefix, given the prompt
/// area width.
fn input_text_width(area_width: u16, prefix: u16) -> u16 {
    area_width.saturating_sub(prefix).max(1)
}

/// Max visible rows the input box grows to before it starts scrolling.
const INPUT_MAX_ROWS: u16 = 6;

/// How many rows the prompt block needs: visible input rows + underline + meta.
/// Used by `render_chat` to size the layout BEFORE rendering, so the box grows.
fn prompt_block_height(input: &str, area_width: u16, prefix: u16) -> u16 {
    let total = wrap_input_rows(input, input_text_width(area_width, prefix)).len();
    let visible = (u16::try_from(total).unwrap_or(INPUT_MAX_ROWS)).clamp(1, INPUT_MAX_ROWS);
    visible + 2 // + underline + meta row
}

fn render_prompt(frame: &mut Frame, area: Rect, app: &App) {
    let text_width = input_text_width(area.width, mode_prefix_width(app));
    // Wrap the real input so the box height + underline track the content.
    let all_rows = wrap_input_rows(&app.input, text_width);
    let total_rows = u16::try_from(all_rows.len()).unwrap_or(INPUT_MAX_ROWS);
    let visible_rows = total_rows.clamp(1, INPUT_MAX_ROWS);
    // Scroll so the LAST visible_rows (where the cursor is) stay on screen.
    let scroll = total_rows.saturating_sub(visible_rows);
    let prompt_chunks = Layout::default()
        .direction(Direction::Vertical)
        // input rows + bottom border, then the meta row.
        .constraints([Constraint::Length(visible_rows + 1), Constraint::Length(1)])
        .split(area);

    // Border color: muted gray normally (Claude Code's promptBorder
    // rgb(136,136,136)), warm yellow at a gate.
    let border_color = if app.active_gate.is_some() {
        theme::WARNING()
    } else {
        theme::BORDER_ACTIVE()
    };

    // Mode indicator: `>_` when idle, `[run]` when running, `[gate]` at gate.
    // The `>_` is a terminal-window icon. The run/gate markers drop the trailing
    // `_` faux-cursor — with the real terminal cursor now sitting in the input
    // (you can type to queue while running), a second `_` read as a stray cursor.
    // An aborted block is NOT "running": it bailed before any phase. Showing
    // `[run]` there would contradict the `[aborted]` status bar and lie about a
    // dead round. `is_pipeline_active()` already excludes both finished AND
    // aborted, so the run marker only shows for a genuinely live run.
    let mode_icon = if app.active_gate.is_some() {
        "[gate]"
    } else if app.is_pipeline_active() {
        "[run]"
    } else {
        ">_"
    };
    // Single source of truth for the row-0 prefix width — continuation rows, the
    // cursor and the wrap width (above) all use it, so they can never drift apart
    // as the terminal resizes or the mode marker changes.
    let prefix_w = usize::from(mode_prefix_width(app));
    let mode_color = if app.active_gate.is_some() {
        theme::WARNING()
    } else {
        theme::PRIMARY()
    };

    // Placeholder (Claude Code style: dim hint when empty). Localized.
    let placeholder = if app.active_gate.is_some() {
        umadev_i18n::t(app.lang, "input.gate")
    } else if app.finished {
        umadev_i18n::t(app.lang, "input.finished")
    } else if app.aborted {
        // The round bailed — tell the user to re-enter a requirement, NOT that a
        // run is still in flight (which the bare `run_started` branch below would
        // wrongly imply, since `run_started` stays set on an aborted block).
        umadev_i18n::t(app.lang, "input.aborted")
    } else if app.run_started {
        umadev_i18n::t(app.lang, "input.running")
    } else {
        umadev_i18n::t(app.lang, "input.idle")
    };

    // Build the wrapped input: row 0 carries the `>_ ` mode prefix; wrapped
    // continuation rows are indented 3 cols so they align under the text.
    let lines: Vec<Line> = if app.input.is_empty() {
        vec![Line::from(vec![
            Span::styled(mode_icon, Style::default().fg(mode_color)),
            Span::raw(" "),
            Span::styled(placeholder, Style::default().fg(theme::TEXT_MUTED())),
        ])]
    } else {
        all_rows
            .iter()
            .enumerate()
            .map(|(i, row)| {
                if i == 0 {
                    Line::from(vec![
                        Span::styled(mode_icon, Style::default().fg(mode_color)),
                        Span::raw(" "),
                        Span::styled(row.clone(), Style::default().fg(theme::TEXT())),
                    ])
                } else {
                    Line::from(vec![
                        Span::raw(" ".repeat(prefix_w)),
                        Span::styled(row.clone(), Style::default().fg(theme::TEXT())),
                    ])
                }
            })
            .collect()
    };

    // Bottom-only border = the underline. With the input area sized to the
    // content (visible_rows + 1), the border sits directly under the last line.
    let input_block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(border_color));
    let input_panel = Paragraph::new(lines).scroll((scroll, 0)).block(input_block);
    frame.render_widget(input_panel, prompt_chunks[0]);

    // Cursor: derive its wrapped (row, col) by wrapping the text UP TO the
    // cursor the same way. Display width (CJK = 2 cols) keeps it aligned.
    let input_area = prompt_chunks[0];
    let pre: String = app.input.chars().take(app.input_cursor).collect();
    let pre_rows = wrap_input_rows(&pre, text_width);
    let cursor_row_abs = u16::try_from(pre_rows.len().saturating_sub(1)).unwrap_or(0);
    let cursor_col = pre_rows.last().map_or(0, |r| disp_width(r));
    let cursor_row_vis = cursor_row_abs.saturating_sub(scroll);
    if app.overlay.is_none() && !app.show_help {
        frame.set_cursor_position((
            input_area
                .x
                .saturating_add(u16::try_from(prefix_w).unwrap_or(3))
                .saturating_add(u16::try_from(cursor_col).unwrap_or(u16::MAX)),
            input_area.y.saturating_add(cursor_row_vis),
        ));
    }

    // Context line beneath the input box: model / backend / hints.
    let backend = app.backend.as_deref().unwrap_or("offline");
    let hint: String = if app.input.starts_with('/') {
        umadev_i18n::t(app.lang, "tui.hint.palette").into()
    } else if let Some(gate) = app.active_gate {
        return meta_row(
            frame,
            prompt_chunks[1],
            border_color,
            &[
                (
                    umadev_i18n::tf(app.lang, "tui.hint.gate_tag", &[gate.id_str()]),
                    theme::WARNING(),
                ),
                (
                    umadev_i18n::t(app.lang, "tui.hint.gate_action").into(),
                    theme::TEXT_MUTED(),
                ),
                ("· ".into(), theme::BORDER()),
                (backend.into(), theme::TEXT_MUTED()),
            ],
        );
    } else if app.input.contains('\n') {
        umadev_i18n::t(app.lang, "tui.hint.multiline").into()
    } else if !app.input.is_empty() {
        umadev_i18n::t(app.lang, "tui.hint.typed").into()
    } else if app.finished {
        umadev_i18n::t(app.lang, "tui.hint.finished").into()
    } else if app.aborted {
        // Aborted round — the hint must match the `[aborted]` status, not the
        // "wait for the next gate" line a live run shows.
        umadev_i18n::t(app.lang, "tui.hint.aborted").into()
    } else if app.run_started {
        umadev_i18n::t(app.lang, "tui.hint.running").into()
    } else {
        umadev_i18n::t(app.lang, "tui.hint.idle").into()
    };
    // Trust-tier chip: plan (read-only) / guarded (review each gate) / auto.
    let mode = app.effective_trust_mode();
    let mode_color = match mode {
        umadev_agent::TrustMode::Auto => theme::SUCCESS(),
        umadev_agent::TrustMode::Guarded => theme::WARNING(),
        umadev_agent::TrustMode::Plan => theme::INFO(),
    };
    let mode_chip = umadev_i18n::t(app.lang, mode.chip_key());
    let mut parts: Vec<(String, Color)> = vec![
        ("UmaDev".into(), theme::ACCENT()),
        ("·".into(), theme::BORDER()),
        (backend.into(), theme::TEXT_MUTED()),
        ("·".into(), theme::BORDER()),
        (mode_chip.into(), mode_color),
    ];
    // Persistent "queued N" chip — stays visible the whole time input is parked
    // (a routed turn still in flight, or a steer waiting on a gate), so the user
    // never has to remember a one-off System note that has since scrolled away.
    // Bracket-tag style (emoji-free) to match the existing [gate] / [queued]
    // markers. Hidden when nothing is queued.
    let queued = app.queued_count();
    if queued > 0 {
        parts.push(("·".into(), theme::BORDER()));
        parts.push((format!("[queued {queued}]"), theme::WARNING()));
    }
    parts.push(("·".into(), theme::BORDER()));
    parts.push((hint, theme::TEXT_MUTED()));
    meta_row(frame, prompt_chunks[1], border_color, &parts);
}

/// Helper to render the meta row as a sequence of styled spans, left-aligned.
fn meta_row(frame: &mut Frame, area: Rect, _bar: Color, parts: &[(String, Color)]) {
    let mut spans: Vec<Span<'static>> = vec![Span::raw(" ")];
    for (text, color) in parts {
        spans.push(Span::styled(text.clone(), Style::default().fg(*color)));
        spans.push(Span::raw(" "));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// A short popover above the input box that lists matching slash commands.
fn render_palette_popover(
    frame: &mut Frame,
    input_area: Rect,
    app: &App,
    matches: &[(&'static str, &'static str)],
) {
    let total = matches.len();
    if total == 0 {
        return;
    }
    // Show as many rows as fit ABOVE the input (the popover floats upward),
    // capped at 12. Crucially, WINDOW the list around the current selection so
    // the user can scroll through ALL commands with ↑↓ — the old code only ever
    // rendered the first 6 of ~45, so most commands (and the selection itself,
    // once scrolled past row 6) were invisible.
    let avail_above = usize::from(input_area.y).saturating_sub(2);
    let max_rows = total.min(avail_above).clamp(1, 12);
    let selected = app.palette_selected.min(total - 1);
    let win_start = if total > max_rows {
        selected.saturating_sub(max_rows / 2).min(total - max_rows)
    } else {
        0
    };
    let win_end = (win_start + max_rows).min(total);

    let rows = u16::try_from(win_end - win_start).unwrap_or(6);
    let height = rows + 2; // borders
    let width = input_area.width.min(56);
    let x = input_area.x;
    let y = input_area.y.saturating_sub(height);
    // CLAMP to the frame: this is a hand-built Rect (unlike the Layout-clamped
    // overlays), so on a short/narrow terminal it would extend past the buffer
    // and `Clear` would index out of bounds and panic the whole TUI.
    let area = Rect {
        x,
        y,
        width,
        height,
    }
    .intersection(frame.area());
    if area.is_empty() {
        return;
    }
    frame.render_widget(Clear, area);

    let items: Vec<ListItem> = matches[win_start..win_end]
        .iter()
        .enumerate()
        .map(|(i, (verb, hint))| {
            let idx = win_start + i;
            let arrow = if idx == selected { "›" } else { " " };
            let row_style = if idx == selected {
                Style::default()
                    .fg(theme::PRIMARY())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme::TEXT())
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {arrow} "), row_style),
                Span::styled(format!("/{verb:<12}"), row_style),
                Span::styled(
                    (*hint).to_string(),
                    Style::default().fg(theme::TEXT_MUTED()),
                ),
            ]))
        })
        .collect();
    // Title carries the position + total so the user KNOWS there are more
    // (e.g. "8/45 · ↑↓ Tab") — the previous popover gave no hint that the list
    // was truncated.
    let title = format!(
        " {} · {}/{} · ↑↓ ",
        umadev_i18n::t(app.lang, "tui.palette.title"),
        selected + 1,
        total
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            title,
            Style::default().fg(theme::BORDER_STRONG()),
        ))
        .border_style(Style::default().fg(theme::BORDER()));
    frame.render_widget(List::new(items).block(block), area);
}

/// Bottom status row — directory on the left, phase/status on the right,
/// both muted, separated by flexible whitespace. No box.
fn render_status_row(frame: &mut Frame, area: Rect, app: &App) {
    let dir_name = app
        .project_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("~");
    let backend_label = app.backend.as_deref().unwrap_or("offline");
    let phase_info = if app.thinking {
        // Animated so a sent message never looks frozen while the base replies.
        format!(
            "{} {}",
            app.spinner(),
            umadev_i18n::t(app.lang, "status.thinking")
        )
    } else if app.aborted {
        // Dedicated terminal branch — an aborted round reads as `[aborted]` here
        // DIRECTLY, instead of leaning on `app.status` carrying the right text.
        // That coupling was fragile: a future `refresh_status` change could
        // silently make a wedged run show stale phase progress. Checked before
        // `run_started` because `mark_block_aborted` leaves `run_started` set.
        format!("[aborted] {}", umadev_i18n::t(app.lang, "status.aborted"))
    } else if app.run_started {
        // While a slow phase's heartbeat is live, show its in-place "still
        // working (mm:ss)" reassurance HERE (overwritten each beat) instead of
        // letting it pile up in the transcript. The spinner + phase timer in
        // `app.status` still prove motion; this just makes the wait explicit and
        // reminds the user the long phase is interruptible (ESC).
        match &app.transient_status {
            Some(beat) => format!(
                "{} · {beat} · {}",
                app.status,
                umadev_i18n::t(app.lang, "status.esc_cancel")
            ),
            None => app.status.clone(),
        }
    } else if app.finished {
        umadev_i18n::t(app.lang, "tui.status.complete").to_string()
    } else {
        umadev_i18n::t(app.lang, "status.ready").to_string()
    };
    let left = Line::from(vec![
        Span::styled(
            format!(" {dir_name} "),
            Style::default().fg(theme::TEXT_MUTED()),
        ),
        Span::styled("·", Style::default().fg(theme::BORDER())),
        Span::styled(
            format!(" {backend_label} "),
            Style::default().fg(theme::TEXT_MUTED()),
        ),
        Span::styled("·", Style::default().fg(theme::BORDER())),
        Span::styled(" /help ", Style::default().fg(theme::TEXT_MUTED())),
    ]);
    let mut right_spans: Vec<Span<'static>> = Vec::new();
    // Right-align by DISPLAY width, not byte length — a CJK glyph is 3 bytes but
    // occupies 2 columns, so `.len()` over-counts ~3x and used to saturate the
    // pad to 0 (status text glued to the left) under a Chinese locale. The left
    // chrome is `" {dir} " · " {backend} " · " /help "`: each padded label adds
    // its display width + 2 spaces, plus 1+1 for the two `·` and 7 for ` /help `.
    let left_width = disp_width(dir_name) + 2 + 1 + disp_width(backend_label) + 2 + 1 + 7;
    // On a narrow terminal the phase string itself can be wider than the space
    // left after the chrome — clip it (by display width) so it never wraps or
    // overruns the row. Keep a trailing column for the ` ` we append below.
    let avail_right = usize::from(area.width)
        .saturating_sub(left_width)
        .saturating_sub(1);
    let phase_info = if disp_width(&phase_info) > avail_right {
        truncate_to_width(&phase_info, avail_right)
    } else {
        phase_info
    };
    // Pad with spaces to right-align the (possibly clipped) phase status.
    let phase_width = disp_width(&phase_info) + 1; // + the trailing space we add
    let pad = usize::from(area.width)
        .saturating_sub(left_width)
        .saturating_sub(phase_width);
    for _ in 0..pad {
        right_spans.push(Span::raw(" "));
    }
    // Stall → red (honest "about to hang"); otherwise the normal info color.
    let info_color = if app.is_stalled() {
        theme::ERROR()
    } else {
        theme::INFO()
    };
    right_spans.push(Span::styled(
        format!("{phase_info} "),
        Style::default().fg(info_color),
    ));
    let mut all = left.spans;
    all.extend(right_spans);
    frame.render_widget(Paragraph::new(Line::from(all)), area);
}

// ---------- Help overlay (both modes) -------------------------------------

fn render_help_overlay(frame: &mut Frame, app: &App) {
    let area = centered_rect(frame.area(), 72, 80);
    frame.render_widget(Clear, area);

    let header = match app.mode {
        AppMode::Picker => umadev_i18n::t(app.lang, "tui.help.header_picker"),
        AppMode::Chat => umadev_i18n::t(app.lang, "tui.help.header_chat"),
    };
    let lang = app.lang;

    let mut items: Vec<ListItem> = Vec::new();
    items.push(ListItem::new(Line::from(Span::styled(
        umadev_i18n::t(app.lang, "help.overlay_subtitle"),
        Style::default()
            .fg(theme::INFO())
            .add_modifier(Modifier::BOLD),
    ))));
    items.push(ListItem::new(Line::from("")));

    match app.mode {
        AppMode::Picker => {
            push_help_group(
                &mut items,
                umadev_i18n::t(lang, "tui.help.group.navigation"),
                &[
                    ("↑ / ↓", umadev_i18n::t(lang, "tui.help.nav.move")),
                    ("Enter", umadev_i18n::t(lang, "tui.help.nav.confirm")),
                    ("F1", umadev_i18n::t(lang, "tui.help.nav.toggle")),
                    ("Esc", umadev_i18n::t(lang, "tui.help.nav.quit")),
                ],
            );
        }
        AppMode::Chat => {
            // Worker group derived from the REAL driver registry — never list a
            // backend that isn't wired. (The old hard-coded list advertised
            // /gemini /qwen /kimi etc. which don't exist and error on use.)
            let mut worker_owned: Vec<(String, String)> = umadev_host::BACKEND_IDS
                .iter()
                .map(|id| {
                    let display = umadev_host::driver_for(id)
                        .map_or_else(|| (*id).to_string(), |d| d.display_name().to_string());
                    (format!("/{id}"), display)
                })
                .collect();
            worker_owned.push((
                "/offline".to_string(),
                umadev_i18n::t(lang, "tui.help.worker.offline").to_string(),
            ));
            worker_owned.push((
                "/model <id>".to_string(),
                umadev_i18n::t(lang, "tui.help.worker.model").to_string(),
            ));
            let worker_rows: Vec<(&str, &str)> = worker_owned
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            push_help_group(
                &mut items,
                umadev_i18n::t(lang, "tui.help.group.worker"),
                &worker_rows,
            );
            push_help_group(
                &mut items,
                umadev_i18n::t(lang, "tui.help.group.pipeline"),
                &[
                    ("Enter", umadev_i18n::t(lang, "tui.help.pipe.enter")),
                    (
                        "/continue or c",
                        umadev_i18n::t(lang, "tui.help.pipe.continue"),
                    ),
                    (
                        "/revise <txt>",
                        umadev_i18n::t(lang, "tui.help.pipe.revise"),
                    ),
                    ("/manual", umadev_i18n::t(lang, "tui.help.pipe.manual")),
                    ("/auto", umadev_i18n::t(lang, "tui.help.pipe.auto")),
                    (
                        "/diff [artifact]",
                        umadev_i18n::t(lang, "tui.help.pipe.diff"),
                    ),
                    (
                        "/run [slug] <req>",
                        umadev_i18n::t(lang, "tui.help.pipe.run"),
                    ),
                    ("/quick <task>", umadev_i18n::t(lang, "tui.help.pipe.quick")),
                    ("/redo [phase]", umadev_i18n::t(lang, "tui.help.pipe.redo")),
                    ("/rewind [id]", umadev_i18n::t(lang, "tui.help.pipe.rewind")),
                    ("/init", umadev_i18n::t(lang, "tui.help.pipe.init")),
                ],
            );
            push_help_group(
                &mut items,
                umadev_i18n::t(lang, "tui.help.group.ship"),
                &[
                    ("/preview", umadev_i18n::t(lang, "tui.help.ship.preview")),
                    (
                        "/stop-preview",
                        umadev_i18n::t(lang, "tui.help.ship.stop_preview"),
                    ),
                    ("/deploy", umadev_i18n::t(lang, "tui.help.ship.deploy")),
                    ("/pr", umadev_i18n::t(lang, "tui.help.ship.pr")),
                    ("/export", umadev_i18n::t(lang, "tui.help.ship.export")),
                ],
            );
            push_help_group(
                &mut items,
                umadev_i18n::t(lang, "tui.help.group.inspect"),
                &[
                    (
                        "/design <name>",
                        umadev_i18n::t(lang, "tui.help.inspect.design"),
                    ),
                    (
                        "/template <name>",
                        umadev_i18n::t(lang, "tui.help.inspect.template"),
                    ),
                    ("/status", umadev_i18n::t(lang, "tui.help.inspect.status")),
                    (
                        "/pitfalls",
                        umadev_i18n::t(lang, "tui.help.inspect.pitfalls"),
                    ),
                    ("/runs", umadev_i18n::t(lang, "tui.help.inspect.runs")),
                    (
                        "/knowledge",
                        umadev_i18n::t(lang, "tui.help.inspect.knowledge"),
                    ),
                    ("/mcp", umadev_i18n::t(lang, "tui.help.inspect.mcp")),
                    ("/skill", umadev_i18n::t(lang, "tui.help.inspect.skill")),
                    ("/usage", umadev_i18n::t(lang, "tui.help.inspect.usage")),
                    ("/spec", umadev_i18n::t(lang, "tui.help.inspect.spec")),
                    ("/verify", umadev_i18n::t(lang, "tui.help.inspect.verify")),
                    ("/config", umadev_i18n::t(lang, "tui.help.inspect.config")),
                    ("/doctor", umadev_i18n::t(lang, "tui.help.inspect.doctor")),
                    ("/history", umadev_i18n::t(lang, "tui.help.inspect.history")),
                    ("/version", umadev_i18n::t(lang, "tui.help.inspect.version")),
                    (
                        "/changelog",
                        umadev_i18n::t(lang, "tui.help.inspect.changelog"),
                    ),
                    ("/bug", umadev_i18n::t(lang, "tui.help.inspect.bug")),
                ],
            );
            push_help_group(
                &mut items,
                umadev_i18n::t(lang, "tui.help.group.editing"),
                &[
                    ("Shift+Enter", umadev_i18n::t(lang, "tui.help.edit.newline")),
                    ("↑ / ↓", umadev_i18n::t(lang, "tui.help.edit.recall")),
                    ("Tab", umadev_i18n::t(lang, "tui.help.edit.autocomplete")),
                    ("/clear", umadev_i18n::t(lang, "tui.help.edit.clear")),
                    ("/help or /?", umadev_i18n::t(lang, "tui.help.edit.help")),
                    ("/quit or q", umadev_i18n::t(lang, "tui.help.edit.quit")),
                    ("F1", umadev_i18n::t(lang, "tui.help.edit.toggle")),
                    ("Esc", umadev_i18n::t(lang, "tui.help.edit.esc")),
                ],
            );
        }
    }

    // Scrollable window — the help can exceed the overlay height on small
    // terminals; slice to the visible rows and show a position indicator so
    // the bottom groups (e.g. "Editing & exit") are never silently cropped.
    let inner_h = area.height.saturating_sub(2) as usize;
    let total = items.len();
    let max_scroll = total.saturating_sub(inner_h);
    let scroll = (app.help_scroll as usize).min(max_scroll);
    let shown = inner_h.min(total.saturating_sub(scroll));
    let title = if max_scroll > 0 {
        format!(
            "{header} · {}-{}/{total} · ↑↓ {}",
            scroll + 1,
            scroll + shown,
            umadev_i18n::t(app.lang, "tui.help.scroll_hint")
        )
    } else {
        header.to_string()
    };
    let visible: Vec<ListItem> = items.into_iter().skip(scroll).take(inner_h).collect();
    let list = List::new(visible).block(
        Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(
                title,
                Style::default().fg(theme::BORDER_STRONG()),
            ))
            .border_style(Style::default().fg(theme::BORDER())),
    );
    frame.render_widget(list, area);
}

fn push_help_group(items: &mut Vec<ListItem<'_>>, title: &str, rows: &[(&str, &str)]) {
    items.push(ListItem::new(Line::from(Span::styled(
        format!(" {title}"),
        Style::default()
            .fg(theme::INFO())
            .add_modifier(Modifier::BOLD),
    ))));
    for (key, desc) in rows {
        items.push(ListItem::new(Line::from(vec![
            Span::styled(
                format!("  {key:<22} "),
                Style::default()
                    .fg(theme::WARNING())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                (*desc).to_string(),
                Style::default().fg(theme::TEXT_MUTED()),
            ),
        ])));
    }
    items.push(ListItem::new(Line::from("")));
}

fn centered_rect(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(percent_y)])
        .flex(Flex::Center)
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(percent_x)])
        .flex(Flex::Center)
        .split(vertical[0])[0]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::config::UserConfig;
    use crossterm::event::KeyCode;
    use ratatui::backend::Backend;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use umadev_agent::{EngineEvent, Gate};
    use umadev_spec::Phase;

    #[test]
    fn markdown_to_lines_handles_unicode_bullets_without_panic() {
        // Regression: `•` (U+2022) is 3 bytes; a hardcoded `&trimmed[2..]`
        // sliced mid-char and panicked. Worker/chat replies routinely emit
        // `•`-prefixed lists, so this must render, not crash.
        let md = "结果:\n• 第一步\n• 第二步 with 中文\n- dash 项\n* star 项";
        let lines = markdown_to_lines(md, Color::White);
        assert!(lines.len() >= 5);
        // The bullet content is preserved (rendered under a normalized "• ").
        let joined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(joined.contains("第一步"));
        assert!(joined.contains("第二步 with 中文"));
    }

    fn render_to_string(app: &App) -> String {
        // Tall enough that the full grouped help overlay (which has grown with
        // the command set) renders without the bottom session group being clipped.
        let backend = TestBackend::new(120, 110);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        buffer
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    fn app_with(backend: Option<&str>) -> App {
        App::new(
            "demo",
            UserConfig {
                backend: backend.map(str::to_string),
                model: None,
                ..Default::default()
            },
            std::path::PathBuf::from("/tmp/sd-test-config.toml"),
            std::path::PathBuf::from("/tmp/sd-test-workspace"),
        )
    }

    // --- Picker ---

    #[test]
    fn picker_renders_all_workers() {
        let mut app = app_with(None);
        // Base CLIs live in step 3 of the guided setup.
        app.goto_picker_step(crate::app::PickerStep::BaseCli);
        let out = render_to_string(&app);
        assert!(out.contains("Claude Code CLI") || out.contains("Claude Code"));
        assert!(out.contains("Codex CLI") || out.contains("Codex"));
    }

    #[test]
    fn picker_renders_umadev_logo() {
        // Terminal-window `>_` monogram + bold wordmark.
        let app = app_with(None);
        let out = render_to_string(&app);
        // The wordmark text renders.
        assert!(out.contains("UmaDev"), "logo wordmark missing: {out}");
        // The prompt glyphs (> and _) and the window border render.
        assert!(out.contains('>'), "logo prompt > missing: {out}");
        assert!(out.contains('_'), "logo prompt _ missing: {out}");
    }

    #[test]
    fn picker_marks_current_selection() {
        let app = app_with(None);
        let out = render_to_string(&app);
        // The selected row carries the brand left-bar marker.
        assert!(out.contains('▌'));
    }

    // --- Chat ---

    #[test]
    fn chat_shows_greeting() {
        let app = app_with(Some("offline"));
        let out = render_to_string(&app);
        // Title row + greeting render.
        assert!(out.contains("UmaDev"));
        assert!(out.contains("AI"));
        // The prompt's placeholder + meta row render.
        assert!(out.contains("输入需求") || out.contains("help"));
    }

    #[test]
    fn chat_input_box_shows_input() {
        let mut app = app_with(Some("offline"));
        for c in "hello".chars() {
            let _ = app.apply_key(KeyCode::Char(c));
        }
        let out = render_to_string(&app);
        assert!(out.contains("hello"), "input text should be visible");
        // The old fake `▌` cursor char is gone — we now use a real terminal
        // cursor via set_cursor_position. The render_to_string buffer may
        // not capture cursor position, so we just assert the text shows.
    }

    #[test]
    fn chat_slash_input_shows_palette_popover() {
        let mut app = app_with(Some("offline"));
        // Pin English so the assertion is locale-independent: wide CJK glyphs get
        // split across cells in the test buffer, so an ASCII title matches cleanly.
        app.lang = umadev_i18n::Lang::En;
        for c in "/cla".chars() {
            let _ = app.apply_key(KeyCode::Char(c));
        }
        let out = render_to_string(&app);
        // Popover lists matching commands above the input.
        assert!(out.contains(umadev_i18n::t(app.lang, "tui.palette.title")));
        assert!(out.contains("/claude"));
        // Selection chevron is on the first match by default.
        assert!(out.contains("›"));
    }

    #[test]
    fn chat_input_box_title_changes_with_state() {
        let mut app = app_with(Some("offline"));
        let empty = render_to_string(&app);
        assert!(
            empty.contains("输入需求")
                || empty.contains("type requirement")
                || empty.contains("help")
        );
        // Some normal text → the localized "typed" meta hint. Assert against the
        // resolved value (and its language-neutral key glyph) so this passes in
        // any UI locale, not just English.
        for c in "hello".chars() {
            let _ = app.apply_key(KeyCode::Char(c));
        }
        let typed = render_to_string(&app);
        let typed_hint = umadev_i18n::t(app.lang, "tui.hint.typed");
        // The hint mentions Enter + Shift+Enter in every locale (key names stay
        // literal); a substring of the resolved value must appear on screen.
        assert!(typed_hint.contains("Enter"));
        assert!(typed.contains("Shift+Enter"));
    }

    #[test]
    fn chat_history_title_surfaces_scrolloff_count() {
        let mut app = app_with(Some("offline"));
        // Render at a small viewport so plenty of lines spill above.
        for i in 0..40 {
            app.apply_engine(umadev_agent::EngineEvent::Note(format!(
                "scroll-content-line-{i}"
            )));
        }
        let backend = TestBackend::new(80, 18);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| crate::ui::render(f, &app)).unwrap();
        let buf = term.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area().height {
            for x in 0..buf.area().width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        // The transcript surfaces the scrolloff count via a top-right hint.
        // The hint text is localized with a `{}` count placeholder; assert that
        // the static suffix after the count (locale-specific, but stable) renders.
        let suffix = umadev_i18n::t(app.lang, "tui.scroll.above")
            .rsplit("{}")
            .next()
            .unwrap()
            .trim();
        assert!(out.contains(suffix), "rendered: {out}");
    }

    #[test]
    fn input_title_shows_gate_hint_when_paused() {
        let mut app = app_with(Some("offline"));
        app.apply_engine(umadev_agent::EngineEvent::GateOpened {
            gate: umadev_agent::Gate::DocsConfirm,
        });
        let out = render_to_string(&app);
        // The input status hint is gate-aware.
        assert!(out.contains("gate"));
        assert!(out.contains("docs_confirm"));
    }

    #[test]
    fn input_title_shows_running_hint_when_pipeline_active() {
        let mut app = app_with(Some("offline"));
        app.apply_engine(umadev_agent::EngineEvent::PipelineStarted {
            slug: "demo".into(),
            requirement: "x".into(),
        });
        let out = render_to_string(&app);
        // The running-state meta hint is localized; assert against its resolved
        // value (which carries the language-neutral [wait] tag) so the check
        // holds in any UI locale.
        let running_hint = umadev_i18n::t(app.lang, "tui.hint.running");
        assert!(out.contains("[wait]"), "running hint tag missing: {out}");
        assert!(running_hint.contains("[wait]"));
    }

    #[test]
    fn cjk_backspace_deletes_one_whole_char_no_residue() {
        // Regression for the reported bug: type "你是" (2 chars / 6 bytes), one
        // Backspace must leave exactly "你" — never a byte-sliced fragment and
        // never a stale char bleeding in from a previous message. The char-cursor
        // + byte_index splice must stay on UTF-8 boundaries.
        let mut app = app_with(Some("offline"));
        for c in "你是".chars() {
            let _ = app.apply_key(KeyCode::Char(c));
        }
        assert_eq!(app.input, "你是");
        assert_eq!(app.input_cursor, 2);
        let _ = app.apply_key(KeyCode::Backspace);
        assert_eq!(
            app.input, "你",
            "backspace must delete the whole last CJK char"
        );
        assert_eq!(app.input_cursor, 1);
        // The rendered cursor column lands right after the single remaining
        // wide char: gutter(2) + prefix `>_ `(3) + 2 cols for "你" = x 7.
        let backend = TestBackend::new(60, 20);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| render(f, &app)).unwrap();
        let cur = term.backend_mut().get_cursor_position().unwrap();
        assert_eq!(cur.x, 7, "cursor must sit just past the wide char");
    }

    #[test]
    fn pasted_text_inserts_atomically_and_strips_escapes() {
        // Bracketed paste / CJK IME commit: a whole string lands at the cursor in
        // one shot. Newlines survive (multi-line prompts); other control chars
        // (here a stray ESC) are dropped so a pasted escape can't corrupt render.
        let mut app = app_with(Some("offline"));
        app.insert_str_at_cursor("你好\x1b世界\n第二行");
        assert_eq!(app.input, "你好世界\n第二行");
        // cursor advanced by the number of chars actually inserted (ESC skipped).
        assert_eq!(app.input_cursor, app.input_len());
    }

    #[test]
    fn cursor_position_tracks_arrow_keys() {
        let mut app = app_with(Some("offline"));
        for c in "abc".chars() {
            let _ = app.apply_key(KeyCode::Char(c));
        }
        // Cursor at end (position 3).
        assert_eq!(app.input_cursor, 3);
        // Left arrow moves cursor between b and c (position 2).
        let _ = app.apply_key(KeyCode::Left);
        assert_eq!(app.input_cursor, 2);
        // The input text is still "abc" — only cursor position changed.
        assert_eq!(app.input, "abc");
    }

    #[test]
    fn chat_renders_gate_message_with_gate_role() {
        let mut app = app_with(Some("offline"));
        app.apply_engine(EngineEvent::GateOpened {
            gate: Gate::DocsConfirm,
        });
        let out = render_to_string(&app);
        assert!(out.contains("gate"));
        assert!(out.contains("docs_confirm"));
    }

    #[test]
    fn chat_renders_host_output_with_worker_label() {
        let mut app = app_with(Some("offline"));
        app.apply_engine(EngineEvent::HostOutput {
            phase: Phase::Research,
            line: "## Similar products".into(),
        });
        let out = render_to_string(&app);
        // Worker output shows via left-bar message (no "worker" label tag anymore).
        assert!(out.contains("Similar products"));
    }

    // --- Help overlay ---

    #[test]
    fn help_overlay_in_picker_lists_navigation_keys() {
        let mut app = app_with(None);
        // Pin English: wide CJK glyphs split across cells in the test buffer, so
        // assert the resolved (ASCII) values render contiguously.
        app.lang = umadev_i18n::Lang::En;
        let _ = app.apply_key(KeyCode::F(1));
        let out = render_to_string(&app);
        assert!(out.contains(umadev_i18n::t(app.lang, "tui.help.header_picker").trim()));
        assert!(out.contains(umadev_i18n::t(app.lang, "tui.help.nav.move")));
    }

    #[test]
    fn help_overlay_in_chat_lists_slash_commands() {
        let mut app = app_with(Some("offline"));
        app.lang = umadev_i18n::Lang::En;
        let _ = app.apply_key(KeyCode::F(1));
        let out = render_to_string(&app);
        assert!(out.contains(umadev_i18n::t(app.lang, "tui.help.header_chat").trim()));
        assert!(out.contains("/claude"));
        assert!(out.contains("/quit"));
    }

    #[test]
    fn help_overlay_in_chat_uses_grouped_sections() {
        let mut app = app_with(Some("offline"));
        // Pin English so the localized group titles render as contiguous ASCII in
        // the test buffer (wide CJK glyphs would be split across cells).
        app.lang = umadev_i18n::Lang::En;
        let _ = app.apply_key(KeyCode::F(1));
        // Render at a terminal tall enough that the full overlay fits (so all
        // groups are visible without scrolling).
        let backend = TestBackend::new(120, 90);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| crate::ui::render(f, &app)).unwrap();
        let buf = term.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area().height {
            for x in 0..buf.area().width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        // Each group header appears, and verbs are sorted under them. The group
        // titles are localized — assert against the resolved values so the test
        // holds in any UI locale.
        assert!(out.contains(umadev_i18n::t(app.lang, "tui.help.group.worker").trim()));
        assert!(out.contains(umadev_i18n::t(app.lang, "tui.help.group.pipeline").trim()));
        assert!(out.contains(umadev_i18n::t(app.lang, "tui.help.group.ship").trim()));
        assert!(out.contains(umadev_i18n::t(app.lang, "tui.help.group.inspect").trim()));
        assert!(out.contains(umadev_i18n::t(app.lang, "tui.help.group.editing").trim()));
        // Primary ship-it verbs that used to be missing from /help.
        assert!(out.contains("/preview"));
        assert!(out.contains("/deploy"));
        // Surfaces live in the right groups.
        assert!(out.contains("/model"));
        assert!(out.contains("/version"));
        assert!(out.contains("Shift+Enter"));
    }

    #[test]
    fn help_overlay_does_not_advertise_phantom_backends() {
        // The worker list is derived from the real driver registry — none of
        // the old hard-coded phantom CLIs should appear.
        let mut app = app_with(Some("offline"));
        let _ = app.apply_key(KeyCode::F(1));
        let backend = TestBackend::new(120, 90);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| crate::ui::render(f, &app)).unwrap();
        let buf = term.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area().height {
            for x in 0..buf.area().width {
                out.push_str(buf[(x, y)].symbol());
            }
        }
        for phantom in ["/gemini", "/droid", "/qwen", "/copilot", "/kimi", "/qoder"] {
            assert!(!out.contains(phantom), "help still lists phantom {phantom}");
        }
    }

    // --- Transcript scrollback (P0 viewport hardening) ---

    /// Render the chat at an explicit size, returning the whole buffer as a
    /// single string (row-major) so assertions can look for on-screen text.
    fn render_chat_at(app: &App, w: u16, h: u16) -> String {
        let backend = TestBackend::new(w, h);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| render(f, app)).unwrap();
        let buf = term.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area().height {
            for x in 0..buf.area().width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn app_with_long_transcript(rows: usize) -> App {
        let mut app = app_with(Some("offline"));
        for i in 0..rows {
            app.apply_engine(umadev_agent::EngineEvent::Note(format!(
                "scroll-content-line-{i}"
            )));
        }
        app
    }

    #[test]
    fn transcript_scroll_clamps_to_hidden_above() {
        // After a render publishes the max scroll, scrolling far past it must
        // clamp to exactly the rows hidden above — never overshoot.
        let mut app = app_with_long_transcript(60);
        let _ = render_chat_at(&app, 80, 18); // publishes transcript_max_scroll
        let max = app.transcript_max_scroll.get();
        assert!(
            max > 0,
            "long transcript should overflow an 18-row terminal"
        );
        // Ask for far more than exists.
        app.transcript_scroll_up(10_000);
        assert_eq!(
            app.transcript_scroll, max,
            "scroll-up must clamp at hidden_above"
        );
        // Scrolling back down past 0 re-pins to the bottom.
        app.transcript_scroll_down(10_000);
        assert_eq!(app.transcript_scroll, 0);
    }

    #[test]
    fn scrolling_up_stops_auto_stick_and_shows_down_indicator() {
        // The bug: once content overflows, the view was hard-pinned to the
        // bottom so older lines were unreachable. Scrolling up must reveal the
        // top AND surface a "↓ more" indicator pointing back to the bottom.
        let mut app = app_with_long_transcript(60);
        // Bottom-pinned first: recent content is visible, the welcome banner at
        // the very top of the session has scrolled off.
        let bottom = render_chat_at(&app, 80, 18);
        assert!(
            bottom.contains("scroll-content-line-57"),
            "bottom: {bottom}"
        );
        assert!(
            !bottom.contains("▟▀▀▀▀▀▙"),
            "welcome banner must be scrolled off when pinned to bottom"
        );
        // Scroll all the way up — the session's oldest content (the welcome
        // banner) comes on screen, and the recent notes scroll off.
        app.transcript_scroll_to_top();
        let top = render_chat_at(&app, 80, 18);
        assert!(top.contains("▟▀▀▀▀▀▙"), "top of session not shown: {top}");
        assert!(!top.contains("scroll-content-line-57"));
        // The two-way indicator now offers the way back down. The hint prose is
        // localized, but it always names the `End` key (kept literal in every
        // locale), so assert on that locale-independent token.
        assert!(top.contains("End"), "missing down hint: {top}");
        // End re-pins to the bottom (auto-stick resumes): recent content is back
        // and the welcome banner is hidden again.
        app.transcript_scroll_to_bottom();
        assert_eq!(app.transcript_scroll, 0);
        let back = render_chat_at(&app, 80, 18);
        assert!(back.contains("scroll-content-line-57"));
        assert!(!back.contains("▟▀▀▀▀▀▙"));
    }

    #[test]
    fn tiny_terminal_shows_resize_card_not_a_clipped_layout() {
        // Below the min size the chat layout would crush the transcript to 0 and
        // clip the input/status off-screen. Instead we show a resize hint and
        // never lay out the chat — so the fixed regions can't fall out of view.
        let app = app_with(Some("offline"));
        let out = render_chat_at(&app, 30, 6); // < MIN_CHAT_WIDTH × MIN_CHAT_HEIGHT
                                               // The resize-card prose is localized (and wide CJK chars get split across
                                               // cells in the test buffer), so assert on the locale-independent target
                                               // dimensions string, which the card always renders verbatim.
        let target = format!("{MIN_CHAT_WIDTH}×{MIN_CHAT_HEIGHT}");
        assert!(out.contains(&target), "expected resize card: {out}");
        // A roomy terminal lays out normally (no resize card).
        let ok = render_chat_at(&app, 80, 24);
        assert!(!ok.contains(&target));
    }

    #[test]
    fn prompt_height_is_clamped_so_status_row_stays_on_screen() {
        // A tall multi-line input on a short terminal must not push the prompt
        // past `area.height - 3`; the status row (and the input bottom) stay on
        // screen. We assert the clamp arithmetic directly.
        let inner_h: u16 = 12; // a short content column
                               // A would-be very tall prompt (e.g. INPUT_MAX_ROWS + 2).
        let raw = INPUT_MAX_ROWS + 2;
        let clamped = raw.min(inner_h.saturating_sub(3)).max(2);
        assert!(
            clamped <= inner_h.saturating_sub(3),
            "prompt must leave room for title + ≥1 transcript row + status"
        );
        assert!(clamped >= 2, "prompt keeps at least input + meta rows");
        // And it renders without panicking even with a multi-line input on a
        // short terminal (regression guard for the clip-out-of-view bug).
        let mut app = app_with(Some("offline"));
        app.insert_str_at_cursor("a\nb\nc\nd\ne\nf\ng\nh");
        let out = render_chat_at(&app, 50, 12);
        // The bottom status row (directory breadcrumb + /help) is still drawn.
        assert!(out.contains("/help"), "status row clipped: {out}");
    }

    #[test]
    fn help_overlay_scrolls_to_reveal_bottom_on_short_terminal() {
        // Regression for the crop bug: on a short terminal the bottom group is
        // below the fold at offset 0, but PageDown must reveal it.
        let mut app = app_with(Some("offline"));
        // Pin English so the localized group title renders as contiguous ASCII.
        app.lang = umadev_i18n::Lang::En;
        let _ = app.apply_key(KeyCode::F(1));
        let render = |app: &App| {
            let backend = TestBackend::new(100, 28);
            let mut term = Terminal::new(backend).unwrap();
            term.draw(|f| crate::ui::render(f, app)).unwrap();
            let buf = term.backend().buffer();
            let mut out = String::new();
            for y in 0..buf.area().height {
                for x in 0..buf.area().width {
                    out.push_str(buf[(x, y)].symbol());
                }
            }
            out
        };
        // At the top, the bottom group is cropped… (the group title is localized,
        // so assert against the resolved value).
        let editing = umadev_i18n::t(app.lang, "tui.help.group.editing")
            .trim()
            .to_string();
        assert!(!render(&app).contains(&editing));
        // …but scrolling down reveals it.
        for _ in 0..6 {
            let _ = app.apply_key(KeyCode::PageDown);
        }
        assert!(render(&app).contains(&editing));
    }

    #[test]
    fn queued_chip_appears_in_meta_row_when_queue_non_empty() {
        let mut app = app_with(Some("offline"));
        // Nothing queued → no chip.
        assert!(
            !render_to_string(&app).contains("queued"),
            "no queued chip when the queue is empty"
        );
        // Park two chat turns → the persistent chip shows the count.
        app.queued_chat.push_back("a".into());
        app.queued_chat.push_back("b".into());
        let out = render_to_string(&app);
        assert!(
            out.contains("[queued 2]"),
            "the meta row must show a persistent queued count: {out}"
        );
    }

    #[test]
    fn queued_chip_disappears_when_queue_drains() {
        let mut app = app_with(Some("offline"));
        app.queued_chat.push_back("a".into());
        assert!(render_to_string(&app).contains("[queued 1]"));
        // Drain it → the chip is gone again (purely display-driven, no residue).
        let _ = app.take_next_queued_chat();
        assert!(
            !render_to_string(&app).contains("queued"),
            "the chip must disappear once the queue empties"
        );
    }

    // --- P2-A: status-row display-width alignment (CJK) ---

    /// Render JUST the status row at `width` cols and return its single row as a
    /// per-cell `Vec<String>` (one entry per terminal column). A wide CJK glyph
    /// occupies one cell + a following skip cell, so column indices are exact —
    /// the honest way to assert alignment without re-measuring a flattened string.
    fn render_status_cells(app: &App, width: u16) -> Vec<String> {
        let backend = TestBackend::new(width, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render_status_row(f, f.area(), app))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        buffer
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect()
    }

    /// Column index of the first cell whose symbol contains `needle`.
    fn col_of(cells: &[String], needle: &str) -> Option<usize> {
        cells.iter().position(|s| s.contains(needle))
    }

    #[test]
    fn disp_width_counts_cjk_as_two_columns() {
        assert_eq!(disp_width("ab"), 2);
        assert_eq!(disp_width("正在思考"), 8); // 4 CJK glyphs × 2 cols
        assert_eq!(disp_width("a正b"), 4);
    }

    #[test]
    fn truncate_to_width_never_splits_a_wide_glyph() {
        // 5 cols of room: "正在" = 4 cols, the 3rd glyph would push to 6 → dropped.
        assert_eq!(truncate_to_width("正在思考", 5), "正在");
        // Exact fit keeps everything.
        assert_eq!(truncate_to_width("正在思考", 8), "正在思考");
        // ASCII truncates per-column.
        assert_eq!(truncate_to_width("abcdef", 3), "abc");
        assert_eq!(truncate_to_width("anything", 0), "");
    }

    #[test]
    fn status_row_right_aligns_cjk_without_overflow_or_collision() {
        // A Chinese phase string (`正在思考`, 8 display cols but 12 bytes) used to
        // over-count via `.len()`, saturate the pad to 0, and glue the status to
        // the left chrome. With display-width padding it sits flush RIGHT instead.
        let mut app = app_with(Some("offline"));
        app.lang = umadev_i18n::Lang::ZhCn;
        app.thinking = true; // → phase_info = "<spinner> 正在思考"
        let width = 80u16;
        let cells = render_status_cells(&app, width);
        // The buffer is exactly `width` cells (a wide glyph + its skip cell) — so
        // by construction nothing overran. Assert the phase is RIGHT-aligned:
        // its first glyph starts in the right portion of the row, well past the
        // left chrome (which used to be where it got glued under the byte-len bug).
        let phase_col = col_of(&cells, "正").expect("CJK phase glyph renders");
        let help_col = col_of(&cells, "h").expect("/help chrome renders"); // ` /help `
        assert!(
            phase_col > help_col + 10,
            "phase not right-aligned (phase col {phase_col} vs help col {help_col})"
        );
        // The last glyph of the phase (`考`, 2 cols wide) ends near the right
        // edge — its right edge (start col + 2 wide cells) is within one trailing
        // space of the boundary. Anything larger means the pad over-shrank again.
        let last_col = cells
            .iter()
            .rposition(|s| s.contains('考'))
            .expect("last CJK glyph renders");
        let glyph_right_edge = last_col + 2; // wide glyph occupies last_col + skip cell
        assert!(
            (width as usize) - glyph_right_edge <= 2,
            "phase not flush-right: last glyph ends at {glyph_right_edge}, width {width}"
        );
    }

    #[test]
    fn status_row_aborted_branch_is_independent_of_app_status() {
        // P2-F: an aborted round must render `[aborted]` from a DEDICATED status
        // branch, not by relying on `app.status` text. Proof: blank out app.status
        // entirely — the row still shows the aborted marker, so the two are
        // decoupled and a future refresh_status change can't silently break it.
        let mut app = app_with(Some("offline"));
        app.lang = umadev_i18n::Lang::ZhCn;
        app.aborted = true;
        app.run_started = true; // mark_block_aborted leaves this set
        app.status = String::new(); // the fragile coupling, deliberately removed
        let cells = render_status_cells(&app, 80);
        let joined: String = cells.join("");
        assert!(
            joined.contains("aborted") || joined.contains("中止"),
            "aborted status must render from its own branch, not app.status: {joined:?}"
        );
    }

    #[test]
    fn status_row_clips_overlong_cjk_on_a_narrow_terminal() {
        // On a very narrow terminal the phase string is wider than the space left
        // after the chrome — it must be clipped (by display width) so it never
        // wraps or overruns. The render itself is the proof: a TestBackend of
        // width W always yields exactly W cells; the assertion verifies the
        // chrome + clipped phase still fit (no panic, phase head still visible).
        let mut app = app_with(Some("offline"));
        app.lang = umadev_i18n::Lang::ZhCn;
        app.aborted = true; // → "[aborted] 本轮已中止"
        let width = 30u16;
        let cells = render_status_cells(&app, width);
        assert_eq!(cells.len(), width as usize, "row is exactly the width");
        // The `[aborted]` tag (left of the phase text) still renders even clipped.
        assert!(
            col_of(&cells, "a").is_some(),
            "aborted status head should still render at a narrow width"
        );
    }
}
