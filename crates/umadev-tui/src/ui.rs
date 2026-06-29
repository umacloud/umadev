//! Ratatui rendering — pure function of [`App`] state.
//!
//! Two screens, dispatched on [`AppMode`]:
//!
//! - **Picker** — first-launch backend chooser.
//! - **Chat** — persistent input box + scrolling conversation history,
//!   modelled after Claude Code's REPL feel.

use ratatui::layout::{Alignment, Constraint, Direction, Flex, Layout, Rect};
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

    /// A stable id for the ACTIVE palette (P3 highlight-cache key component). The
    /// dark/light flip is the only thing that changes a highlighted line's colors,
    /// so a single bool fully identifies the theme generation. Returned as `u8`
    /// so the cache key stays a plain hashable tuple.
    pub(crate) fn theme_id() -> u8 {
        u8::from(IS_LIGHT.load(Ordering::Relaxed))
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
    /// Background tint for the in-app text selection highlight (the rows the
    /// user is dragging across). A muted brand-cyan wash — clearly distinct
    /// from the user-message tint and the diff backgrounds, but not so loud it
    /// drowns the text under it. Resolved here (no naked color at the call
    /// site) so a theme swap re-skins the selection too.
    pub fn SELECTION_BG() -> Color {
        if IS_LIGHT.load(Ordering::Relaxed) {
            // cyan mixed into the light panel bg — readable behind black text.
            Color::Rgb(0xbf, 0xe3, 0xef)
        } else {
            // cyan mixed into the dark panel bg — visible behind light text.
            Color::Rgb(0x1d, 0x4e, 0x5e)
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

    // ── Extra palette pairs for the syntax-role table ──
    // Roles that don't map onto an existing UI token get their own (dark, light)
    // pair here, drawn from the same Tailwind-family ramp as the rest of the
    // theme so the highlighter reads as part of the brand, not a foreign scheme.
    const SYN_NUMBER_P: Pair = Pair {
        dark: rgb(0xf0, 0x9b, 0x4e),  // orange-400 — numeric literals
        light: rgb(0xc2, 0x41, 0x0c), // orange-700
    };
    const SYN_TYPE_P: Pair = Pair {
        dark: rgb(0x5e, 0xea, 0xd4),  // teal-300 — type names
        light: rgb(0x0f, 0x76, 0x6e), // teal-700
    };
    const SYN_FUNCTION_P: Pair = Pair {
        dark: rgb(0x82, 0xa9, 0xff),  // indigo-300 — function / method names
        light: rgb(0x43, 0x38, 0xca), // indigo-700
    };
    const SYN_PUNCT_P: Pair = Pair {
        dark: rgb(0xb8, 0xc1, 0xd4),  // slate-300 — punctuation/operators
        light: rgb(0x47, 0x55, 0x69), // slate-600
    };
    const DIFF_ADD_P: Pair = Pair {
        dark: rgb(0x4a, 0xde, 0x80),  // green-400 — diff +
        light: rgb(0x15, 0x80, 0x3d), // green-700
    };
    const DIFF_DEL_P: Pair = Pair {
        dark: rgb(0xf8, 0x71, 0x71),  // red-400 — diff -
        light: rgb(0xb9, 0x1c, 0x1c), // red-700
    };
    // ── Word-level diff emphasis (the actually-changed tokens within a line) ──
    // Brighter than the base add/del fg so the changed word pops against the
    // surrounding (normally syntax-highlighted) unchanged text — Claude-Code's
    // word-diff read. Drawn from the same green/red ramp, one step toward white.
    const DIFF_ADD_WORD_P: Pair = Pair {
        dark: rgb(0x86, 0xef, 0xac),  // green-300 — changed token on a + line
        light: rgb(0x16, 0x65, 0x34), // green-800 (deeper for white-bg contrast)
    };
    const DIFF_DEL_WORD_P: Pair = Pair {
        dark: rgb(0xfc, 0xa5, 0xa5),  // red-300 — changed token on a - line
        light: rgb(0x99, 0x1b, 0x1b), // red-800
    };
    /// Full-width row-background tint for a `+` diff line. Pre-mixed RGB: a LOW
    /// blend of ANSI-green into the active panel background (dark uses a low
    /// alpha so the tint is barely-there; light uses an even lower alpha so it
    /// never washes out black text). NEVER a naked `Color::Green` — the value is
    /// resolved here so a theme swap re-skins the whole card.
    pub fn DIFF_ADD_BG() -> Color {
        if IS_LIGHT.load(Ordering::Relaxed) {
            // green mixed ~10% into the light panel bg (#eef2f7).
            Color::Rgb(0xdc, 0xf0, 0xe2)
        } else {
            // green mixed ~16% into the dark panel bg (#1e2030).
            Color::Rgb(0x1c, 0x2e, 0x2a)
        }
    }
    /// Full-width row-background tint for a `-` diff line — the red counterpart
    /// of [`DIFF_ADD_BG`], same low-alpha pre-mix policy. Never a naked color.
    pub fn DIFF_DEL_BG() -> Color {
        if IS_LIGHT.load(Ordering::Relaxed) {
            // red mixed ~10% into the light panel bg.
            Color::Rgb(0xf2, 0xdf, 0xe1)
        } else {
            // red mixed ~16% into the dark panel bg.
            Color::Rgb(0x30, 0x22, 0x29)
        }
    }
    /// Subtle background tint for fenced code blocks — distinct from the prose
    /// background without shouting. Sits between BG_PANEL and BG_ELEMENT.
    pub fn CODE_BG() -> Color {
        if IS_LIGHT.load(Ordering::Relaxed) {
            Color::Rgb(0xe7, 0xed, 0xf4)
        } else {
            Color::Rgb(0x18, 0x1a, 0x28)
        }
    }

    /// One semantic syntax/markdown role. Both the markdown compiler and the
    /// code tokenizer emit ONLY these tags — never a bare `Color` — so a theme
    /// swap means editing exactly one table (`syn_color`). This is the "no naked
    /// color" contract for rich text.
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub enum SynRole {
        /// Default prose / code identifier text.
        Text,
        /// Muted / secondary text.
        Muted,
        /// Language keyword (`fn`, `if`, `def`, `return`, …).
        Keyword,
        /// String / char literal.
        StringLit,
        /// Numeric literal.
        Number,
        /// Type / class / constructor name.
        Type,
        /// Function / method name.
        Function,
        /// Comment.
        Comment,
        /// Punctuation / operator.
        Punctuation,
        /// Markdown heading.
        Heading,
        /// Inline `code` span / fenced-code identifier text.
        InlineCode,
        /// Link / URL text.
        Link,
        /// Blockquote text + its left bar.
        Blockquote,
        /// List marker (bullet / number).
        ListMarker,
        /// Diff added line (`+ …`).
        DiffAdd,
        /// Diff removed line (`- …`).
        DiffDel,
        /// The actually-changed token(s) on a `+` line (word-level emphasis) —
        /// brighter than `DiffAdd` so the new word pops against unchanged text.
        DiffAddWord,
        /// The actually-changed token(s) on a `-` line (word-level emphasis).
        DiffDelWord,
    }

    /// The single source of truth mapping a [`SynRole`] to a runtime-resolved
    /// color. Change a theme by editing only this table. Resolves dark/light via
    /// the same `pick` path as every other token.
    pub fn syn_color(role: SynRole) -> Color {
        match role {
            SynRole::Text => TEXT(),
            // Muted / comment / blockquote all read as secondary text.
            SynRole::Muted | SynRole::Comment | SynRole::Blockquote => TEXT_MUTED(),
            // Keywords + headings share the secondary (purple) accent.
            SynRole::Keyword | SynRole::Heading => pick(&SECONDARY_P),
            SynRole::StringLit => pick(&SUCCESS_P),
            SynRole::Number => pick(&SYN_NUMBER_P),
            SynRole::Type => pick(&SYN_TYPE_P),
            SynRole::Function => pick(&SYN_FUNCTION_P),
            SynRole::Punctuation => pick(&SYN_PUNCT_P),
            // Inline code + links share the primary (cyan) brand color.
            SynRole::InlineCode | SynRole::Link => pick(&PRIMARY_P),
            SynRole::ListMarker => pick(&INFO_P),
            SynRole::DiffAdd => pick(&DIFF_ADD_P),
            SynRole::DiffDel => pick(&DIFF_DEL_P),
            SynRole::DiffAddWord => pick(&DIFF_ADD_WORD_P),
            SynRole::DiffDelWord => pick(&DIFF_DEL_WORD_P),
        }
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
            ChatRole::Error => ERROR(),
        }
    }
}
use ratatui::Frame;

use crate::app::{
    App, AppMode, ChatRole, CmdGroup, FileDiff, MessageBody, PaletteEntry, ToolCall, ToolStatus,
};

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

    let inner_height = area.height.saturating_sub(2) as usize; // minus top+bottom border
                                                               // Inner text width: total minus the two side borders (1 each) and a 1-col
                                                               // breathing pad on each side — must match what we hand the wrapper so the
                                                               // pre-fold row count equals exactly what paints (no `Paragraph::wrap` second
                                                               // guess that desyncs scroll from the painted rows, the long-line bug).
    let inner_width = area.width.saturating_sub(4).max(1);

    // Pre-fold every logical line to the inner width into the exact VISUAL rows it
    // occupies, then render WITHOUT `Paragraph::wrap`. This mirrors the transcript
    // path: the folded row count IS the scroll universe, so End / scroll_down can
    // reach a wrapped row hidden past the last logical line, and the progress %
    // counts real rows instead of lying about logical lines. An empty logical line
    // still occupies one visual row (keeps blank-line spacing intact).
    let folded: Vec<String> = ov
        .lines
        .iter()
        .flat_map(|l| {
            if l.is_empty() {
                vec![String::new()]
            } else {
                wrap_input_rows(l, inner_width)
            }
        })
        .collect();
    let total = folded.len();

    // Publish the top-most reachable visual row so the key handlers clamp `scroll`
    // against width-aware reality (End lands on the true last row, not a logical
    // guess). Clamp the live `scroll` here too so a stale value (overlay opened
    // wide, terminal then shrank) can't index past the end.
    let max_scroll = total.saturating_sub(inner_height.max(1));
    ov.max_scroll.set(max_scroll);
    let from = ov.scroll.min(max_scroll);
    let to = (from + inner_height).min(total);
    let visible: Vec<Line<'static>> = folded
        .iter()
        .skip(from)
        .take(to.saturating_sub(from))
        .map(|l| Line::from(l.clone()))
        .collect();

    let lang = umadev_i18n::current();
    let progress = if total == 0 {
        format!(" {} ", umadev_i18n::t(lang, "tui.overlay.empty"))
    } else {
        let pct = if total <= inner_height {
            100
        } else {
            (to * 100) / total
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
    // When the body is taller than the viewport, surface the scroll affordance in
    // the title so the user knows the overlay scrolls (the close hint is already in
    // each opener's title). `max_scroll == 0` means everything fits, so no hint.
    let title_full = if max_scroll > 0 {
        format!(
            "{}{progress}{} ",
            ov.title,
            umadev_i18n::t(lang, "tui.overlay.scroll_hint")
        )
    } else {
        format!("{}{progress}", ov.title)
    };

    // No `.wrap()` — the body is already folded to the inner width, so the painted
    // rows and the scroll offset agree exactly.
    let body = Paragraph::new(visible).block(
        Block::default()
            .borders(Borders::ALL)
            .title(title_full)
            .border_style(Style::default().fg(theme::BORDER_ACTIVE())),
    );
    frame.render_widget(body, area);
}

use theme::SynRole;

/// Convenience: a styled [`Span`] whose color comes from the semantic role table
/// (never a naked `Color`). `modifier` layers bold/italic/etc on top.
fn role_span(text: impl Into<String>, role: SynRole, modifier: Modifier) -> Span<'static> {
    Span::styled(
        text.into(),
        Style::default()
            .fg(theme::syn_color(role))
            .add_modifier(modifier),
    )
}

/// The single left-gutter width (display columns) for every transcript turn —
/// the role spine glyph (`▎`) + one space. Every speaker (You / Host / UmaDev /
/// System) hangs its wrapped body and aligns its continuation rows under this
/// same gutter, so the vertical skeleton is one width, not the old per-role
/// patchwork (You=1 / Host=2 / System=2 / Gate=2). Continuation rows are
/// indented by exactly this many columns (see [`prefold_line`]'s `spine` arg).
const GUTTER_W: usize = 2;

/// The role-spine glyph (`▎`, U+258E "left one-quarter block"), built from its
/// codepoint so the source carries no literal pictographic glyph (same policy
/// as [`assistant_marker`]). It is exactly one display column; followed by one
/// space it forms the [`GUTTER_W`]-wide left gutter under which a turn's body
/// and every wrapped continuation row align.
fn spine_glyph() -> char {
    char::from_u32(0x258E).unwrap_or('|')
}

/// The colored role-spine span for the FIRST row of a turn: `▎ ` (glyph + space,
/// [`GUTTER_W`] columns) tinted by [`theme::role_bar`]. Continuation rows get
/// the same spine re-painted by [`prefold_line`] when it folds the line, so a
/// multi-line reply reads as one unbroken vertical bar in the speaker's color.
fn role_spine_span(role: ChatRole) -> Span<'static> {
    let mut s = String::with_capacity(2);
    s.push(spine_glyph());
    s.push(' ');
    Span::styled(s, Style::default().fg(theme::role_bar(role)))
}

// ─── Markdown → styled Lines ──────────────────────────────────────────────
// A pulldown-cmark event stream compiled into ratatui `Line`/`Span`s. Replaces
// the old per-line `strip_prefix` renderer (no tables, no nested lists, no
// inline bold/italic, fence state per-message only). Every color is emitted as
// a semantic [`SynRole`] tag — never a naked `Color` — so a theme swap edits one
// table. CJK width is measured with `unicode-width` everywhere a column is laid
// out (tables), so Chinese text never misaligns. Fail-open: ANY panic in the
// compiler falls back to plain per-line text (see [`markdown_to_lines`]).

/// One frame on the inline style stack — a `Modifier` set plus an optional
/// role override (inline code / link recolor their text).
#[derive(Clone, Copy)]
struct StyleFrame {
    modifier: Modifier,
    role: Option<SynRole>,
}

/// Accumulates inline events (`Text`, `Code`, `Start(Strong)`, …) into a row of
/// styled spans, applying the current style stack. Tables and prose both feed
/// their inline content through one of these and then `take()` the spans.
struct InlineBuilder {
    spans: Vec<Span<'static>>,
    stack: Vec<StyleFrame>,
    base: SynRole,
}

impl InlineBuilder {
    fn new(base: SynRole) -> Self {
        Self {
            spans: Vec::new(),
            stack: Vec::new(),
            base,
        }
    }

    /// The effective `(modifier, role)` from the top of the stack down.
    fn current(&self) -> (Modifier, SynRole) {
        let mut modifier = Modifier::empty();
        let mut role = self.base;
        for frame in &self.stack {
            modifier |= frame.modifier;
            if let Some(r) = frame.role {
                role = r;
            }
        }
        (modifier, role)
    }

    fn push_text(&mut self, text: &str, role_override: Option<SynRole>) {
        if text.is_empty() {
            return;
        }
        let (modifier, role) = self.current();
        let role = role_override.unwrap_or(role);
        self.spans.push(Span::styled(
            text.to_string(),
            Style::default()
                .fg(theme::syn_color(role))
                .add_modifier(modifier),
        ));
    }

    /// Push prose text, turning any bare `http(s)://…` run into a Link-roled span
    /// so a raw URL the model emits reads as a (copyable) link, not flat text.
    /// Trailing sentence punctuation is left outside the URL.
    fn push_prose_text(&mut self, text: &str) {
        let mut rest = text;
        while let Some(pos) = rest.find("http") {
            let from = &rest[pos..];
            if from.starts_with("http://") || from.starts_with("https://") {
                if pos > 0 {
                    self.push_text(&rest[..pos], None);
                }
                let end = from.find(char::is_whitespace).unwrap_or(from.len());
                let raw = &from[..end];
                let url = raw.trim_end_matches(|c| {
                    matches!(
                        c,
                        '.' | ',' | ')' | ']' | '}' | '!' | '?' | ';' | ':' | '"' | '\''
                    )
                });
                let url = if url.is_empty() { raw } else { url };
                self.push_text(url, Some(SynRole::Link));
                rest = &from[url.len()..];
            } else {
                // "http" not followed by a real scheme — emit through it and go on.
                let cut = pos + 4;
                self.push_text(&rest[..cut], None);
                rest = &rest[cut..];
            }
        }
        if !rest.is_empty() {
            self.push_text(rest, None);
        }
    }

    fn take(&mut self) -> Vec<Span<'static>> {
        std::mem::take(&mut self.spans)
    }

    fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }
}

/// Ordered-list marker for `(depth, index)`: depth 0/1 → arabic, depth 2 →
/// lowercase letters, depth ≥3 → lowercase roman. `index` is 1-based. Computed
/// here so a model's own bullet/number text is never trusted for layout.
fn ordered_marker(depth: usize, index: u64) -> String {
    match depth {
        0 | 1 => format!("{index}."),
        2 => format!("{}.", alpha_marker(index)),
        _ => format!("{}.", roman_marker(index)),
    }
}

/// `1 → a, 2 → b, … 26 → z, 27 → aa` (bijective base-26).
fn alpha_marker(mut n: u64) -> String {
    if n == 0 {
        return "0".to_string();
    }
    let mut out = Vec::new();
    while n > 0 {
        n -= 1;
        out.push((b'a' + u8::try_from(n % 26).unwrap_or(0)) as char);
        n /= 26;
    }
    out.iter().rev().collect()
}

/// Lowercase roman numerals for small `n` (1..=3999); falls back to the decimal
/// for anything larger so a pathological list can't loop or panic.
fn roman_marker(n: u64) -> String {
    const TABLE: &[(u64, &str)] = &[
        (1000, "m"),
        (900, "cm"),
        (500, "d"),
        (400, "cd"),
        (100, "c"),
        (90, "xc"),
        (50, "l"),
        (40, "xl"),
        (10, "x"),
        (9, "ix"),
        (5, "v"),
        (4, "iv"),
        (1, "i"),
    ];
    if n == 0 || n > 3999 {
        return n.to_string();
    }
    let mut n = n;
    let mut out = String::new();
    for &(value, sym) in TABLE {
        while n >= value {
            out.push_str(sym);
            n -= value;
        }
    }
    out
}

/// What kind of list each open list level is (ordered + running index, or
/// unordered).
#[derive(Clone, Copy)]
enum ListKind {
    Ordered(u64),
    Unordered,
}

/// Mutable state threaded through the event walk.
struct MdState {
    lines: Vec<Line<'static>>,
    inline: InlineBuilder,
    /// Open list levels (innermost last) — used for marker style + indent.
    lists: Vec<ListKind>,
    /// `true` right after `Start(Item)` so the FIRST flushed block on that item
    /// gets the list marker prefix instead of a bare indent.
    pending_item_marker: bool,
    /// Heading level currently open (`Some(n)`), styling the inline text.
    heading: Option<u8>,
    /// Blockquote nesting depth — each level adds a `│ ` bar + italic.
    blockquote: usize,
    /// Pending table being assembled (header + rows), or `None` outside a table.
    table: Option<TableBuf>,
    /// `true` while inside a table cell so inline events route into the cell.
    in_table_cell: bool,
    /// The open link's destination URL + the inline-span index where its text
    /// began, so `End(Link)` can append a `(url)` suffix when the visible text
    /// differs from the target (a terminal can't click, so the URL must be shown).
    link: Option<(String, usize)>,
    /// `Some(checked)` after a `TaskListMarker` so the item's flushed marker
    /// renders a `☑`/`☐` checkbox instead of a bullet.
    pending_task: Option<bool>,
}

/// A table accumulated cell-by-cell as pulldown-cmark walks it.
struct TableBuf {
    /// Column alignments from `Start(Table(aligns))`.
    aligns: Vec<pulldown_cmark::Alignment>,
    /// All rows (header first). Each cell is its already-styled spans.
    rows: Vec<Vec<Vec<Span<'static>>>>,
    /// `true` while the header row is being read.
    in_header: bool,
}

impl MdState {
    /// The left indent (in spaces) for body text at the current list depth.
    /// Each list level indents by 2; the marker itself lives in this gutter.
    fn list_indent(&self) -> usize {
        self.lists.len().saturating_mul(2)
    }

    /// Emit the accumulated inline spans as one logical block line, prefixing the
    /// list marker / indent / blockquote bar as needed, then reset the builder.
    fn flush_block(&mut self) {
        if self.inline.is_empty() && !self.pending_item_marker {
            return;
        }
        let mut prefix: Vec<Span<'static>> = Vec::new();
        // Blockquote bars first (outermost gutter).
        for _ in 0..self.blockquote {
            prefix.push(role_span("│ ", SynRole::Blockquote, Modifier::empty()));
        }
        // List indent + marker.
        if self.lists.is_empty() {
            // Top-level prose gets one leading space to match the old gutter.
            prefix.push(Span::raw(" "));
        } else {
            let depth = self.lists.len() - 1;
            let indent = depth.saturating_mul(2);
            prefix.push(Span::raw(" ".repeat(indent + 1)));
            if self.pending_item_marker {
                if let Some(checked) = self.pending_task.take() {
                    // Task-list checkbox replaces the bullet (a checked box reads as
                    // "done", an empty one as "todo").
                    let (glyph, role) = if checked {
                        ("\u{2611} ", SynRole::ListMarker)
                    } else {
                        ("\u{2610} ", SynRole::Muted)
                    };
                    prefix.push(role_span(glyph.to_string(), role, Modifier::empty()));
                } else {
                    let marker = match self.lists.last() {
                        Some(ListKind::Ordered(idx)) => ordered_marker(depth, *idx),
                        _ => "•".to_string(),
                    };
                    prefix.push(role_span(
                        format!("{marker} "),
                        SynRole::ListMarker,
                        Modifier::empty(),
                    ));
                }
            } else {
                // Continuation line under a list item: align under the text.
                let marker_w = match self.lists.last() {
                    Some(ListKind::Ordered(idx)) => ordered_marker(depth, *idx).len() + 1,
                    _ => 2,
                };
                prefix.push(Span::raw(" ".repeat(marker_w)));
            }
        }
        self.pending_item_marker = false;
        let mut spans = prefix;
        spans.extend(self.inline.take());
        self.lines.push(Line::from(spans));
    }

    /// Insert a blank spacer line for vertical rhythm between blocks (collapsing
    /// repeats so we never stack two blanks).
    fn blank(&mut self) {
        if self
            .lines
            .last()
            .is_some_and(|l| l.spans.iter().all(|s| s.content.trim().is_empty()))
        {
            return;
        }
        self.lines.push(Line::from(""));
    }
}

thread_local! {
    /// The viewport content width a table must fit within, set once per transcript
    /// render by [`render_transcript`]. `0` = unbounded (tests / no caller), so a
    /// table renders at its natural width exactly as before.
    static TABLE_WIDTH_BUDGET: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Set the per-render table width budget (the content width tables must fit).
fn set_table_width_budget(w: usize) {
    TABLE_WIDTH_BUDGET.with(|c| c.set(w));
}

/// Truncate styled `spans` to at most `max` display columns, appending a muted `…`
/// when content is dropped. CJK-safe (display-width, never bytes). Used only on the
/// over-budget table path so a wide cell can't overflow the terminal.
fn truncate_spans(spans: &[Span<'static>], max: usize) -> Vec<Span<'static>> {
    if max == 0 {
        return Vec::new();
    }
    let total: usize = spans.iter().map(|s| disp_width(s.content.as_ref())).sum();
    if total <= max {
        return spans.to_vec();
    }
    let keep = max.saturating_sub(1); // leave a column for the ellipsis
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for s in spans {
        let w = disp_width(s.content.as_ref());
        if used + w <= keep {
            out.push(s.clone());
            used += w;
        } else {
            // Take as many leading chars of this span as fit.
            let mut piece = String::new();
            for ch in s.content.chars() {
                let cw = disp_width(&ch.to_string());
                if used + cw > keep {
                    break;
                }
                piece.push(ch);
                used += cw;
            }
            if !piece.is_empty() {
                out.push(Span::styled(piece, s.style));
            }
            break;
        }
    }
    out.push(role_span(
        "\u{2026}".to_string(),
        SynRole::Muted,
        Modifier::empty(),
    ));
    out
}

/// Render an assembled table into aligned, CJK-safe rows and push them onto
/// `state.lines`. Column widths are the max VISIBLE (style-stripped, `unicode-
/// width`-measured, CJK = 2) cell width — never byte length — so Chinese columns
/// line up. A muted separator row sits under the header. When the natural width
/// exceeds the per-render budget ([`TABLE_WIDTH_BUDGET`]), columns shrink
/// proportionally and over-long cells truncate with `…` so the table never
/// overflows the viewport (which previously char-folded and scrambled the grid).
fn render_table(state: &mut MdState, table: &TableBuf) {
    let cols = table
        .rows
        .iter()
        .map(Vec::len)
        .max()
        .unwrap_or(0)
        .max(table.aligns.len());
    if cols == 0 {
        return;
    }
    // Column widths from the visible width of every cell.
    let mut widths = vec![0usize; cols];
    for row in &table.rows {
        for (c, cell) in row.iter().enumerate() {
            let w = cell.iter().map(|s| disp_width(s.content.as_ref())).sum();
            if w > widths[c] {
                widths[c] = w;
            }
        }
    }
    // Shrink to the per-render width budget when the natural table overflows it, so
    // a wide table can't scramble the grid by char-folding past the viewport.
    let indent_w = state.list_indent() + 1;
    let sep_w = 5usize; // the "  │  " column separator
    let budget = TABLE_WIDTH_BUDGET.with(std::cell::Cell::get);
    let natural: usize = widths.iter().sum::<usize>() + sep_w * cols.saturating_sub(1) + indent_w;
    let shrunk = budget > 0 && natural > budget;
    if shrunk {
        let avail = budget
            .saturating_sub(indent_w + sep_w * cols.saturating_sub(1))
            .max(cols * 4);
        let nat_sum: usize = widths.iter().sum::<usize>().max(1);
        for w in &mut widths {
            *w = (*w * avail / nat_sum).max(4);
        }
        // The per-column `max(4)` floor can push the total a few cols over `avail`;
        // trim the widest column(s) back down (never below 4) so the row truly fits.
        let mut sum: usize = widths.iter().sum();
        while sum > avail {
            match widths.iter_mut().filter(|w| **w > 4).max() {
                Some(w) => {
                    *w -= 1;
                    sum -= 1;
                }
                None => break,
            }
        }
    }
    let indent = " ".repeat(state.list_indent() + 1);
    let empty: Vec<Span<'static>> = Vec::new();
    // Vertical fallback: when the table has many columns and the budget can't give
    // each a usable width (so a horizontal grid would be a wall of `…`), render each
    // data row as a stacked `header: value` block instead — far more readable on a
    // narrow terminal.
    if shrunk && cols >= 3 && budget > 0 && budget < cols * 12 && !table.rows.is_empty() {
        let header = &table.rows[0];
        let val_w = budget.saturating_sub(indent_w + 2).max(8);
        for row in table.rows.iter().skip(1) {
            for c in 0..cols {
                let key: String = header
                    .get(c)
                    .map(|cell| cell.iter().map(|s| s.content.as_ref()).collect())
                    .unwrap_or_default();
                let val = row.get(c).unwrap_or(&empty);
                let mut line: Vec<Span<'static>> = vec![Span::raw(indent.clone())];
                line.push(role_span(
                    format!("{key}: "),
                    SynRole::Heading,
                    Modifier::BOLD,
                ));
                line.extend(truncate_spans(val, val_w));
                state.lines.push(Line::from(line));
            }
            // Blank spacer between records.
            state.lines.push(Line::from(""));
        }
        return;
    }
    for (r, row) in table.rows.iter().enumerate() {
        let mut spans: Vec<Span<'static>> = vec![Span::raw(indent.clone())];
        for (c, &col_w) in widths.iter().enumerate() {
            let cell_src = row.get(c).unwrap_or(&empty);
            // On the over-budget path, clip each cell to its shrunk column width.
            let cell_owned: Vec<Span<'static>> = if shrunk {
                truncate_spans(cell_src, col_w)
            } else {
                cell_src.clone()
            };
            let cell = &cell_owned;
            let cell_w: usize = cell.iter().map(|s| disp_width(s.content.as_ref())).sum();
            let pad = col_w.saturating_sub(cell_w);
            let align = table
                .aligns
                .get(c)
                .copied()
                .unwrap_or(pulldown_cmark::Alignment::None);
            let (lpad, rpad) = match align {
                pulldown_cmark::Alignment::Right => (pad, 0),
                pulldown_cmark::Alignment::Center => (pad / 2, pad - pad / 2),
                _ => (0, pad),
            };
            if lpad > 0 {
                spans.push(Span::raw(" ".repeat(lpad)));
            }
            // Header cells render bold via the heading role for emphasis.
            if r == 0 {
                for s in cell {
                    spans.push(Span::styled(
                        s.content.to_string(),
                        s.style.add_modifier(Modifier::BOLD),
                    ));
                }
            } else {
                spans.extend(cell.iter().cloned());
            }
            if rpad > 0 {
                spans.push(Span::raw(" ".repeat(rpad)));
            }
            if c + 1 < cols {
                spans.push(role_span("  │  ", SynRole::Punctuation, Modifier::empty()));
            }
        }
        state.lines.push(Line::from(spans));
        // Separator rule under the header row.
        if r == 0 {
            let mut sep: Vec<Span<'static>> = vec![Span::raw(indent.clone())];
            for (c, w) in widths.iter().enumerate() {
                sep.push(role_span("─".repeat(*w), SynRole::Muted, Modifier::empty()));
                if c + 1 < cols {
                    sep.push(role_span("──┼──", SynRole::Muted, Modifier::empty()));
                }
            }
            state.lines.push(Line::from(sep));
        }
    }
}

/// Heading style for level `n`: all bold; H1/H2 underlined for stronger
/// hierarchy. The color is the heading role.
fn heading_modifier(level: u8) -> Modifier {
    if level <= 2 {
        Modifier::BOLD | Modifier::UNDERLINED
    } else {
        Modifier::BOLD
    }
}

/// The fail-open public entry point. Renders `text` as styled markdown Lines; if
/// the CommonMark walk panics for any reason, falls back to the plain per-line
/// renderer so the transcript NEVER loses content or crashes. `base_color` is
/// retained for signature compatibility (prose uses the `Text` role).
fn markdown_to_lines(text: &str, base_color: Color) -> Vec<Line<'static>> {
    let rendered =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| markdown_compile(text)));
    match rendered {
        Ok(lines) if !lines.is_empty() => lines,
        // Empty input → empty; any panic OR an empty parse → plain fallback.
        Ok(_) if text.trim().is_empty() => Vec::new(),
        _ => plaintext_lines(text, base_color),
    }
}

/// **P5a — stable-prefix streaming cache.** Caches the rendered `Vec<Line>` for
/// the *closed* markdown blocks of the message currently being streamed, so each
/// arriving delta only re-renders the small, still-growing tail instead of
/// re-parsing the whole (possibly 998-line) body every frame (the old O(n²)).
///
/// **Correctness — proven line-for-line identical to a whole-body render.** The
/// split point is a byte offset immediately *after* a top-level `\n\n` block
/// separator (and fence-balanced before it). At such a boundary the CommonMark
/// compiler distributes exactly:
///
/// ```text
/// markdown_to_lines(body)
///   == markdown_to_lines(&body[..s]) ++ [blank line] ++ markdown_to_lines(&body[s..])
/// ```
///
/// (the `markdown_to_lines(&body[..s])` render trims its own trailing blank, and
/// the whole-render inserts exactly ONE separator blank between the last cached
/// block and the next — so the single `[blank]` re-inserts it). This identity is
/// locked by [`tests::stream_incremental_equals_whole_render`]. Fail-open: any
/// mismatch in the monotonic-growth precondition discards the cache and renders
/// the whole body (the prior behaviour).
#[derive(Debug, Clone, Default)]
pub(crate) struct StreamMarkdownCache {
    /// Byte length of the body when [`Self::stable_offset`] / [`Self::prefix_lines`]
    /// were last computed. A new body that does NOT start with this exact prefix
    /// (shorter, or a different stable region) invalidates the cache.
    body_len: usize,
    /// Offset into the body up to which [`Self::prefix_lines`] is the render. Always
    /// at a top-level `\n\n` boundary, fence-balanced. Advances monotonically.
    stable_offset: usize,
    /// `markdown_to_lines(&body[..stable_offset])` — the closed-block render reused
    /// verbatim each frame. The separator blank is added at compose time, NOT here.
    prefix_lines: Vec<Line<'static>>,
}

/// Find the stable split point for [P5a]: the byte offset just past the
/// **second-to-last** top-level `\n\n` block separator that is fence-balanced and
/// `>= min` — so the cached prefix holds every closed block EXCEPT the last one,
/// and the re-rendered tail always carries that last full block plus any
/// still-incomplete trailing content.
///
/// **Why keep the last block in the tail.** A bare trailing construct renders
/// *differently* alone vs. after a block: e.g. `markdown_to_lines(">")` keeps a
/// `>` line, but `markdown_to_lines("after\n\n>")` drops it. If the prefix
/// absorbed `after` and the tail were just `>`, the streamed compose would gain a
/// phantom line the settled whole-render lacks. Holding the last full block in
/// the tail makes the tail render the trailing construct exactly as the whole
/// does. (Boundaries strictly inside an open code fence are never eligible.)
///
/// Returns `min` when fewer than two qualifying boundaries exist at/after `min`
/// (so the caller renders the whole small body). O(body) per call — cheap next to
/// a full markdown re-parse, and monotonic in `min`.
fn last_stable_md_boundary(body: &str, min: usize) -> usize {
    let bytes = body.as_bytes();
    // Track the LAST and SECOND-TO-LAST fence-balanced `\n\n` boundary at/after
    // `min`. The prefix ends at the second-to-last so the last full block stays
    // in the tail (see the "why" above).
    let mut last = min;
    let mut second_last = min;
    let mut i = min;
    while i + 1 < bytes.len() {
        if bytes[i] == b'\n' && bytes[i + 1] == b'\n' {
            let cand = i + 2;
            if cand > last && cand <= body.len() && !crate::app::has_open_code_fence(&body[..cand])
            {
                second_last = last;
                last = cand;
            }
        }
        i += 1;
    }
    second_last.max(min)
}

/// **P5a compose.** Render a streaming body using `cache` for its stable prefix,
/// re-rendering only the unclosed tail, and advance the cache in place. Returns
/// the full `Vec<Line>` — guaranteed identical to `markdown_to_lines(body)`.
///
/// Fail-open: if the cache's recorded prefix is no longer a true prefix of `body`
/// (the body shrank or the stable region changed — e.g. a `/clear`, a segment
/// rollover, or a non-monotonic edit), the cache is reset and the whole body is
/// rendered, exactly as before.
fn stream_markdown_lines(cache: &mut StreamMarkdownCache, body: &str) -> Vec<Line<'static>> {
    // Validate the monotonic-growth precondition: the cached stable region must
    // still be a byte-prefix of the current body. If not, discard and recompute.
    let prefix_ok = cache.stable_offset <= body.len()
        && cache.body_len <= body.len()
        && body.is_char_boundary(cache.stable_offset);
    if !prefix_ok {
        *cache = StreamMarkdownCache::default();
    }

    // Advance the stable boundary as far as the new content allows (monotonic).
    let new_offset = last_stable_md_boundary(body, cache.stable_offset);
    if new_offset > cache.stable_offset || (cache.prefix_lines.is_empty() && new_offset > 0) {
        // Re-render the (now larger) closed prefix once. This is the only place
        // the prefix is parsed; subsequent deltas reuse it untouched.
        cache.prefix_lines = markdown_to_lines(&body[..new_offset], theme::TEXT());
        cache.stable_offset = new_offset;
    }
    cache.body_len = body.len();

    if cache.stable_offset == 0 {
        // No closed block yet — nothing to reuse; render the whole (small) body.
        return markdown_to_lines(body, theme::TEXT());
    }

    // Compose: cached closed-block lines + the freshly-rendered tail, with ONE
    // separator blank between them — but ONLY when the tail actually renders a
    // following block. When the tail is empty (the stable prefix IS the whole
    // body so far, e.g. the delta just closed a block and nothing follows yet),
    // the whole-render has no trailing block and no separator blank — so we must
    // not add one either, or the streamed view would gain a spurious blank line
    // that the settled whole-render lacks. Proven equal to the whole-body render.
    let tail = markdown_to_lines(&body[cache.stable_offset..], theme::TEXT());
    if tail.is_empty() {
        return cache.prefix_lines.clone();
    }
    let mut out = Vec::with_capacity(cache.prefix_lines.len() + 1 + tail.len());
    out.extend(cache.prefix_lines.iter().cloned());
    out.push(Line::from(""));
    out.extend(tail);
    out
}

/// The plain per-line fallback (the old behavior): one styled Line per source
/// line, no parsing. Used when CommonMark parsing fails. Never panics.
fn plaintext_lines(text: &str, base_color: Color) -> Vec<Line<'static>> {
    text.lines()
        .map(|raw| {
            if raw.trim().is_empty() {
                Line::from("")
            } else {
                Line::from(Span::styled(
                    format!(" {raw}"),
                    Style::default().fg(base_color),
                ))
            }
        })
        .collect()
}

/// The real CommonMark → Lines compiler (wrapped by [`markdown_to_lines`] for
/// fail-open). Walks the pulldown-cmark event stream with tables + strikethrough
/// enabled, maintaining an inline style stack and block context.
fn markdown_compile(text: &str) -> Vec<Line<'static>> {
    use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    let parser = Parser::new_ext(text, options);

    let mut state = MdState {
        lines: Vec::new(),
        inline: InlineBuilder::new(SynRole::Text),
        lists: Vec::new(),
        pending_item_marker: false,
        heading: None,
        blockquote: 0,
        table: None,
        in_table_cell: false,
        link: None,
        pending_task: None,
    };
    // Fenced-code context: the language tag (for the tokenizer) + the buffered
    // lines, so the whole block is highlighted and boxed together.
    let mut code_lang: Option<String> = None;
    let mut code_buf = String::new();
    let mut in_code = false;

    for event in parser {
        match event {
            // ── Block starts ──
            Event::Start(Tag::Heading { level, .. }) => {
                state.flush_block();
                state.heading = Some(level as u8);
            }
            // A standalone paragraph flushes the previous block first; a
            // paragraph that opens a list item keeps the item's pending marker
            // line (so the marker and the first text share one row).
            Event::Start(Tag::Paragraph) if !state.pending_item_marker => {
                state.flush_block();
            }
            Event::Start(Tag::List(first)) => {
                state.flush_block();
                state.lists.push(match first {
                    Some(n) => ListKind::Ordered(n),
                    None => ListKind::Unordered,
                });
            }
            Event::End(TagEnd::List(_)) => {
                state.flush_block();
                state.lists.pop();
                if state.lists.is_empty() {
                    state.blank();
                }
            }
            Event::Start(Tag::Item) => {
                state.flush_block();
                state.pending_item_marker = true;
            }
            Event::End(TagEnd::Item) => {
                state.flush_block();
                // Advance the ordered-list counter for the next sibling.
                if let Some(ListKind::Ordered(idx)) = state.lists.last_mut() {
                    *idx += 1;
                }
            }
            Event::Start(Tag::BlockQuote(_)) => {
                state.flush_block();
                state.blockquote += 1;
                state.inline.stack.push(StyleFrame {
                    modifier: Modifier::ITALIC,
                    role: Some(SynRole::Blockquote),
                });
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                state.flush_block();
                state.inline.stack.pop();
                state.blockquote = state.blockquote.saturating_sub(1);
                if state.blockquote == 0 {
                    state.blank();
                }
            }
            // ── Code blocks ──
            Event::Start(Tag::CodeBlock(kind)) => {
                state.flush_block();
                in_code = true;
                code_buf.clear();
                code_lang = match kind {
                    CodeBlockKind::Fenced(lang) if !lang.is_empty() => {
                        Some(lang.split_whitespace().next().unwrap_or("").to_lowercase())
                    }
                    _ => None,
                };
            }
            Event::End(TagEnd::CodeBlock) => {
                emit_code_block(&mut state, code_lang.as_deref(), &code_buf);
                in_code = false;
                code_buf.clear();
                code_lang = None;
                state.blank();
            }
            // ── Tables ──
            Event::Start(Tag::Table(aligns)) => {
                state.flush_block();
                state.table = Some(TableBuf {
                    aligns,
                    rows: Vec::new(),
                    in_header: false,
                });
            }
            Event::End(TagEnd::Table) => {
                if let Some(table) = state.table.take() {
                    render_table(&mut state, &table);
                }
                state.blank();
            }
            Event::Start(Tag::TableHead) => {
                if let Some(t) = state.table.as_mut() {
                    t.in_header = true;
                    t.rows.push(Vec::new());
                }
            }
            Event::End(TagEnd::TableHead) => {
                if let Some(t) = state.table.as_mut() {
                    t.in_header = false;
                }
            }
            Event::Start(Tag::TableRow) => {
                if let Some(t) = state.table.as_mut() {
                    if !t.in_header {
                        t.rows.push(Vec::new());
                    }
                }
            }
            Event::Start(Tag::TableCell) => {
                state.in_table_cell = true;
            }
            Event::End(TagEnd::TableCell) => {
                let spans = state.inline.take();
                if let Some(t) = state.table.as_mut() {
                    if let Some(row) = t.rows.last_mut() {
                        row.push(spans);
                    }
                }
                state.in_table_cell = false;
            }
            // ── Inline emphasis ──
            Event::Start(Tag::Strong) => state.inline.stack.push(StyleFrame {
                modifier: Modifier::BOLD,
                role: None,
            }),
            Event::Start(Tag::Emphasis) => state.inline.stack.push(StyleFrame {
                modifier: Modifier::ITALIC,
                role: None,
            }),
            Event::Start(Tag::Strikethrough) => state.inline.stack.push(StyleFrame {
                modifier: Modifier::CROSSED_OUT,
                role: None,
            }),
            Event::Start(Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. }) => {
                // Remember the target + where the link text starts, so `End` can
                // surface the URL (a terminal can't make text clickable, so the
                // destination must be shown when it differs from the visible text).
                state.link = Some((dest_url.to_string(), state.inline.spans.len()));
                state.inline.stack.push(StyleFrame {
                    modifier: Modifier::UNDERLINED,
                    role: Some(SynRole::Link),
                });
            }
            // Closing an inline-style tag pops one frame off the style stack.
            Event::End(TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough) => {
                state.inline.stack.pop();
            }
            Event::End(TagEnd::Link | TagEnd::Image) => {
                state.inline.stack.pop();
                if let Some((url, start)) = state.link.take() {
                    if !url.is_empty() {
                        let text: String = state
                            .inline
                            .spans
                            .get(start..)
                            .map(|ss| ss.iter().map(|sp| sp.content.as_ref()).collect())
                            .unwrap_or_default();
                        let t = text.trim();
                        if t.is_empty() {
                            // No visible text → show the URL itself as the link.
                            state.inline.push_text(&url, Some(SynRole::Link));
                        } else if t != url {
                            // Visible text differs from the target → append a dim (url).
                            state.inline.spans.push(role_span(
                                format!(" ({url})"),
                                SynRole::Muted,
                                Modifier::empty(),
                            ));
                        }
                    }
                }
            }
            Event::TaskListMarker(checked) => {
                // A `- [ ]` / `- [x]` item: render a checkbox glyph in place of the
                // bullet when this item's marker is flushed.
                state.pending_task = Some(checked);
            }
            // ── Leaf inline content ──
            Event::Text(t) => {
                if in_code {
                    code_buf.push_str(&t);
                } else if let Some(level) = state.heading {
                    state.inline.spans.push(role_span(
                        t.to_string(),
                        SynRole::Heading,
                        heading_modifier(level),
                    ));
                } else if state.link.is_some() {
                    // Inside an explicit [text](url) — keep the link's own styling,
                    // don't re-autolink its visible text.
                    state.inline.push_text(&t, None);
                } else {
                    // Prose: bare http(s) URLs become Link-roled spans.
                    state.inline.push_prose_text(&t);
                }
            }
            Event::Code(t) => {
                // Inline `code` — recolor with the inline-code role, keeping any
                // surrounding emphasis modifier.
                state.inline.push_text(&t, Some(SynRole::InlineCode));
            }
            Event::SoftBreak | Event::HardBreak => {
                if in_code {
                    code_buf.push('\n');
                } else if state.in_table_cell {
                    state.inline.push_text(" ", None);
                } else {
                    // Soft/hard break inside prose ends the visual line.
                    state.flush_block();
                }
            }
            Event::End(TagEnd::Heading(_)) => {
                state.flush_block();
                state.heading = None;
                state.blank();
            }
            Event::End(TagEnd::Paragraph) => {
                state.flush_block();
                // Blank between top-level paragraphs only (inside a list the
                // tight items already control spacing).
                if state.lists.is_empty() && state.blockquote == 0 {
                    state.blank();
                }
            }
            Event::Rule => {
                state.flush_block();
                state.lines.push(Line::from(role_span(
                    "─".repeat(24),
                    SynRole::Muted,
                    Modifier::empty(),
                )));
                state.blank();
            }
            _ => {}
        }
    }
    state.flush_block();
    // Trim a trailing blank for a tidy block.
    while state
        .lines
        .last()
        .is_some_and(|l| l.spans.iter().all(|s| s.content.trim().is_empty()))
    {
        state.lines.pop();
    }
    state.lines
}

/// Emit a fenced code block: a subtle top/bottom rule + each content line run
/// through the per-language tokenizer, on a faint code background. The language
/// tag (if any) labels the opening rule.
fn emit_code_block(state: &mut MdState, lang: Option<&str>, body: &str) {
    let indent = state.list_indent();
    let gutter = " ".repeat(indent + 2);
    let label = lang.filter(|l| !l.is_empty()).unwrap_or("code");
    // Opening rule with the language label.
    state.lines.push(Line::from(vec![
        Span::raw(" ".repeat(indent + 2)),
        role_span(format!("┌── {label} "), SynRole::Muted, Modifier::empty()),
    ]));
    for raw in body.lines() {
        let mut spans: Vec<Span<'static>> = vec![Span::styled(
            gutter.clone(),
            Style::default().bg(theme::CODE_BG()),
        )];
        for mut s in highlight_code_line(raw, lang) {
            s.style = s.style.bg(theme::CODE_BG());
            spans.push(s);
        }
        state.lines.push(Line::from(spans));
    }
    state.lines.push(Line::from(vec![
        Span::raw(" ".repeat(indent + 2)),
        role_span("└──────────", SynRole::Muted, Modifier::empty()),
    ]));
}

/// Flush the pending identifier/text run from [`highlight_code_line`],
/// classifying an identifier run as keyword / type / plain text and pushing it
/// as a styled span. A no-op on an empty buffer.
fn flush_run(spans: &mut Vec<Span<'static>>, buf: &mut String, is_ident: bool, keywords: &[&str]) {
    if buf.is_empty() {
        return;
    }
    let role = if is_ident {
        classify_ident(buf, keywords)
    } else {
        SynRole::Text
    };
    spans.push(role_span(std::mem::take(buf), role, Modifier::empty()));
}

/// **P3 — syntax-highlight LRU cache.** A small per-thread cache mapping
/// `(line-content-hash, lang, theme-id)` → the highlighted spans, so scrolling
/// back over already-rendered code/diff is O(1) per line instead of re-running
/// the tokenizer every frame. Bounded (`HL_CACHE_CAP` entries, LRU eviction).
/// Fully fail-open: a borrow conflict / a full cache simply computes uncached.
const HL_CACHE_CAP: usize = 512;

struct HlCache {
    /// hash key → spans.
    map: std::collections::HashMap<u64, Vec<Span<'static>>>,
    /// Recency queue (front = oldest); the back is the most-recently used. On a
    /// hit the key is moved to the back; on insert past the cap the front evicts.
    order: std::collections::VecDeque<u64>,
}

impl HlCache {
    fn new() -> Self {
        Self {
            map: std::collections::HashMap::new(),
            order: std::collections::VecDeque::new(),
        }
    }
}

thread_local! {
    static HL_CACHE: std::cell::RefCell<HlCache> = std::cell::RefCell::new(HlCache::new());
}

/// Hash `(line, lang, theme)` into the cache key. `DefaultHasher` is fine here —
/// the cache is per-thread, advisory, and a (vanishingly unlikely) collision only
/// re-highlights a line the same way, never corrupts output.
fn hl_key(line: &str, lang: Option<&str>, theme: u8) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    line.hash(&mut h);
    lang.unwrap_or("").hash(&mut h);
    theme.hash(&mut h);
    h.finish()
}

/// Syntax-highlight `line` with the P3 LRU cache in front of the tokenizer.
/// Keyed on the content + language + active theme, so a theme flip never serves
/// stale colors. Fail-open: any cache hiccup falls through to a direct compute.
fn highlight_code_line(line: &str, lang: Option<&str>) -> Vec<Span<'static>> {
    let key = hl_key(line, lang, theme::theme_id());
    // Fast path: a cache hit clones the stored spans and promotes the key.
    let hit = HL_CACHE.with(|c| {
        c.try_borrow_mut().ok().and_then(|mut cache| {
            if let Some(spans) = cache.map.get(&key).cloned() {
                // Promote to most-recently-used.
                if let Some(pos) = cache.order.iter().position(|k| *k == key) {
                    cache.order.remove(pos);
                }
                cache.order.push_back(key);
                Some(spans)
            } else {
                None
            }
        })
    });
    if let Some(spans) = hit {
        return spans;
    }
    // Miss: compute once, then store (LRU-evicting the oldest past the cap).
    let spans = highlight_code_line_uncached(line, lang);
    HL_CACHE.with(|c| {
        if let Ok(mut cache) = c.try_borrow_mut() {
            if cache.map.insert(key, spans.clone()).is_none() {
                cache.order.push_back(key);
                while cache.order.len() > HL_CACHE_CAP {
                    if let Some(evict) = cache.order.pop_front() {
                        cache.map.remove(&evict);
                    }
                }
            }
        }
    });
    spans
}

/// Per-language lightweight tokenizer → semantic-role spans. NOT a parser: a
/// regex-free, allocation-light scan that recognises comments, string/char
/// literals, numbers, keywords (per language family), type-ish identifiers
/// (CamelCase / known primitives), and diff markers — emitting [`SynRole`] tags
/// only. An unknown language falls back to plaintext with string/number/comment
/// heuristics. Never panics (char-boundary safe). The cached entry point is
/// [`highlight_code_line`] (P3).
fn highlight_code_line_uncached(line: &str, lang: Option<&str>) -> Vec<Span<'static>> {
    // Diff hunks: color the whole line by its first column (but NOT the
    // `+++`/`---` file headers, which carry no add/del meaning).
    let lang = lang.unwrap_or("");
    let diffish =
        matches!(lang, "diff" | "patch") || line.starts_with("+++") || line.starts_with("---");
    if diffish && line.starts_with('+') && !line.starts_with("+++") {
        return vec![role_span(
            line.to_string(),
            SynRole::DiffAdd,
            Modifier::empty(),
        )];
    }
    if diffish && line.starts_with('-') && !line.starts_with("---") {
        return vec![role_span(
            line.to_string(),
            SynRole::DiffDel,
            Modifier::empty(),
        )];
    }

    let keywords = keywords_for(lang);
    let (line_comment, mut chars) = (line_comment_for(lang), line.char_indices().peekable());
    // Whole-line comment fast path.
    let trimmed = line.trim_start();
    if !line_comment.is_empty() && trimmed.starts_with(line_comment) {
        return vec![role_span(
            line.to_string(),
            SynRole::Comment,
            Modifier::empty(),
        )];
    }

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut buf_is_ident = false;

    while let Some((i, c)) = chars.next() {
        // Line comment starting mid-line.
        if !line_comment.is_empty() && line[i..].starts_with(line_comment) {
            flush_run(&mut spans, &mut buf, buf_is_ident, &keywords);
            spans.push(role_span(
                line[i..].to_string(),
                SynRole::Comment,
                Modifier::empty(),
            ));
            return spans;
        }
        match c {
            '"' | '\'' | '`' => {
                flush_run(&mut spans, &mut buf, buf_is_ident, &keywords);
                buf_is_ident = false;
                let quote = c;
                let mut lit = String::new();
                lit.push(c);
                let mut escaped = false;
                for (_, sc) in chars.by_ref() {
                    lit.push(sc);
                    if escaped {
                        escaped = false;
                    } else if sc == '\\' {
                        escaped = true;
                    } else if sc == quote {
                        break;
                    }
                }
                spans.push(role_span(lit, SynRole::StringLit, Modifier::empty()));
            }
            c if c.is_alphabetic() || c == '_' => {
                if !buf_is_ident {
                    flush_run(&mut spans, &mut buf, buf_is_ident, &keywords);
                    buf_is_ident = true;
                }
                buf.push(c);
            }
            c if c.is_ascii_digit() && !buf_is_ident => {
                flush_run(&mut spans, &mut buf, buf_is_ident, &keywords);
                let mut num = String::new();
                num.push(c);
                while let Some(&(_, nc)) = chars.peek() {
                    if nc.is_ascii_alphanumeric() || nc == '.' || nc == '_' || nc == 'x' {
                        num.push(nc);
                        chars.next();
                    } else {
                        break;
                    }
                }
                spans.push(role_span(num, SynRole::Number, Modifier::empty()));
            }
            c if c.is_ascii_digit() => {
                // Digit inside an identifier (e.g. `utf8`).
                buf.push(c);
            }
            c if "+-*/%=<>!&|^~?:.,;(){}[]".contains(c) => {
                flush_run(&mut spans, &mut buf, buf_is_ident, &keywords);
                buf_is_ident = false;
                spans.push(role_span(
                    c.to_string(),
                    SynRole::Punctuation,
                    Modifier::empty(),
                ));
            }
            _ => {
                // Whitespace or other: belongs to a text run, not an ident run.
                if buf_is_ident {
                    flush_run(&mut spans, &mut buf, buf_is_ident, &keywords);
                    buf_is_ident = false;
                }
                buf.push(c);
            }
        }
    }
    flush_run(&mut spans, &mut buf, buf_is_ident, &keywords);
    if spans.is_empty() {
        spans.push(role_span(
            line.to_string(),
            SynRole::Text,
            Modifier::empty(),
        ));
    }
    spans
}

/// Classify a bare identifier: keyword (from the language set), a type-ish name
/// (UpperCamelCase or a known primitive), else plain text.
fn classify_ident(ident: &str, keywords: &[&str]) -> SynRole {
    if keywords.contains(&ident) {
        return SynRole::Keyword;
    }
    let first = ident.chars().next();
    if first.is_some_and(char::is_uppercase) {
        return SynRole::Type;
    }
    if matches!(
        ident,
        "int"
            | "float"
            | "double"
            | "bool"
            | "char"
            | "str"
            | "void"
            | "byte"
            | "string"
            | "number"
            | "boolean"
            | "usize"
            | "isize"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "i8"
            | "i16"
            | "i32"
            | "i64"
            | "f32"
            | "f64"
    ) {
        return SynRole::Type;
    }
    SynRole::Text
}

/// The line-comment token for a language family (`//`, `#`, `--`). Empty when
/// unknown (the tokenizer then skips comment detection).
fn line_comment_for(lang: &str) -> &'static str {
    match lang {
        "rust" | "rs" | "c" | "cpp" | "c++" | "h" | "hpp" | "java" | "js" | "jsx" | "ts"
        | "tsx" | "javascript" | "typescript" | "go" | "golang" | "swift" | "kotlin" | "kt"
        | "scala" | "php" | "dart" | "json" | "json5" => "//",
        "python" | "py" | "ruby" | "rb" | "sh" | "bash" | "zsh" | "shell" | "yaml" | "yml"
        | "toml" | "ini" | "perl" | "r" | "makefile" | "dockerfile" => "#",
        "sql" | "lua" | "haskell" | "hs" => "--",
        _ => "",
    }
}

/// The keyword set for a language family. Kept compact — enough to color the
/// control-flow / declaration spine, not an exhaustive grammar.
fn keywords_for(lang: &str) -> Vec<&'static str> {
    const RUST: &[&str] = &[
        "fn", "let", "mut", "pub", "use", "mod", "struct", "enum", "trait", "impl", "for", "while",
        "loop", "if", "else", "match", "return", "self", "Self", "where", "async", "await", "move",
        "ref", "const", "static", "type", "as", "in", "break", "continue", "dyn", "crate", "super",
        "unsafe", "true", "false",
    ];
    const PY: &[&str] = &[
        "def", "class", "return", "if", "elif", "else", "for", "while", "import", "from", "as",
        "with", "try", "except", "finally", "raise", "yield", "lambda", "pass", "break",
        "continue", "in", "is", "not", "and", "or", "None", "True", "False", "async", "await",
        "global", "nonlocal", "del", "assert",
    ];
    const JS: &[&str] = &[
        "function",
        "const",
        "let",
        "var",
        "return",
        "if",
        "else",
        "for",
        "while",
        "switch",
        "case",
        "break",
        "continue",
        "class",
        "extends",
        "new",
        "this",
        "import",
        "export",
        "from",
        "default",
        "async",
        "await",
        "yield",
        "try",
        "catch",
        "finally",
        "throw",
        "typeof",
        "instanceof",
        "in",
        "of",
        "null",
        "undefined",
        "true",
        "false",
        "interface",
        "type",
        "enum",
        "public",
        "private",
        "protected",
        "readonly",
    ];
    const GO: &[&str] = &[
        "func",
        "package",
        "import",
        "var",
        "const",
        "type",
        "struct",
        "interface",
        "map",
        "chan",
        "go",
        "defer",
        "return",
        "if",
        "else",
        "for",
        "range",
        "switch",
        "case",
        "select",
        "break",
        "continue",
        "fallthrough",
        "nil",
        "true",
        "false",
    ];
    const SQL: &[&str] = &[
        "select",
        "from",
        "where",
        "insert",
        "into",
        "values",
        "update",
        "set",
        "delete",
        "create",
        "table",
        "drop",
        "alter",
        "join",
        "left",
        "right",
        "inner",
        "outer",
        "on",
        "group",
        "by",
        "order",
        "having",
        "limit",
        "and",
        "or",
        "not",
        "null",
        "as",
        "primary",
        "key",
        "foreign",
        "references",
        "index",
    ];
    match lang {
        "rust" | "rs" => RUST.to_vec(),
        "python" | "py" => PY.to_vec(),
        "js" | "jsx" | "ts" | "tsx" | "javascript" | "typescript" => JS.to_vec(),
        "go" | "golang" => GO.to_vec(),
        "sql" => SQL.to_vec(),
        // Unknown language: a broad union of common control-flow words so SOME
        // structure shows, while strings/numbers/comments still highlight.
        _ => {
            let mut v = Vec::new();
            v.extend_from_slice(&[
                "function", "fn", "def", "class", "struct", "return", "if", "else", "for", "while",
                "import", "from", "const", "let", "var", "true", "false", "null", "public",
                "private",
            ]);
            v
        }
    }
}

// ---------- Picker (first launch) -----------------------------------------

/// The honest three-state readiness mark + detail text for one base-CLI picker
/// row (gap G10). Returns `(glyph, color, detail)`:
/// - **logged in** → a filled circle (green) + the version detail.
/// - **installed · not logged in** → a half circle (amber) + `→ <login cmd>`.
/// - **not installed** → an empty circle (grey) + `→ <install cmd>`.
/// - **unknown** → a half circle (amber) + a conservative "login not verified".
///
/// Glyphs are built from codepoints so the source carries no pictographic glyph,
/// and there are no emoji.
fn picker_auth_marks(
    lang: umadev_i18n::Lang,
    item: &crate::app::PickerItem,
) -> (String, ratatui::style::Color, String) {
    use crate::app::AuthMark;
    // Geometric circles: ● filled / ◐ half / ○ empty.
    let filled = char::from_u32(0x25CF).unwrap_or('*').to_string();
    let half = char::from_u32(0x25D0).unwrap_or('o').to_string();
    let empty = char::from_u32(0x25CB).unwrap_or('.').to_string();
    match item.auth {
        AuthMark::LoggedIn => (
            filled,
            theme::SUCCESS(),
            umadev_i18n::t(lang, "picker.auth.logged_in").to_string(),
        ),
        AuthMark::NotLoggedIn => {
            let cmd = if item.login_cmd.is_empty() {
                "—".to_string()
            } else {
                item.login_cmd.clone()
            };
            (
                half,
                theme::WARNING(),
                umadev_i18n::tf(lang, "picker.auth.not_logged_in", &[&cmd]),
            )
        }
        AuthMark::NotInstalled => {
            let cmd = if item.install_cmd.is_empty() {
                "—".to_string()
            } else {
                item.install_cmd.clone()
            };
            (
                empty,
                theme::TEXT_MUTED(),
                umadev_i18n::tf(lang, "picker.auth.not_installed", &[&cmd]),
            )
        }
        // Unknown: conservative amber half-circle, "login not verified" — but the
        // row is still selectable (the base IS installed; we just couldn't probe
        // login). A legacy `ready: true` probe (no auth tag) also lands here.
        AuthMark::Unknown => {
            // If the legacy `ready` flag says installed, prefer that wording.
            let detail = if item.detail.is_empty() {
                umadev_i18n::t(lang, "picker.auth.unknown").to_string()
            } else {
                format!(
                    "{} · {}",
                    item.detail,
                    umadev_i18n::t(lang, "picker.auth.unknown")
                )
            };
            (half, theme::WARNING(), detail)
        }
    }
}

fn render_picker(frame: &mut Frame, app: &App) {
    // Tiny-terminal guard (mirror of the chat screen's): below this the fixed
    // logo / card / footer stack would overflow and shove the navigation hint —
    // or the selected row — off-screen, so a user couldn't see what they're
    // picking. Show the "make the window bigger" card and bail BEFORE laying out.
    const MIN_PICKER_WIDTH: u16 = 40;
    const MIN_PICKER_HEIGHT: u16 = 12;
    let total = frame.area();
    if total.height < MIN_PICKER_HEIGHT || total.width < MIN_PICKER_WIDTH {
        render_too_small(frame, total);
        return;
    }

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
    // Clamp the card so the fixed chrome (logo 4 + tagline 1 + 2 gaps + footer 2 =
    // 9 rows) plus at least 1 spacer top & bottom always fit. Without this clamp a
    // short terminal that cleared the guard by a hair would still let a tall card
    // push the footer past the bottom edge. The card's inner List does its own
    // windowing, so a clamped card just shows fewer rows + the `N more` hints.
    let chrome_rows: u16 = 4 + 1 + 1 + 1 + 2 + 2; // logo+tagline+2 gaps+footer+2 min spacers
    let max_card = total.height.saturating_sub(chrome_rows).max(3);
    let card_height = (list_height + 2).min(max_card); // +title +border
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
        // Base-CLI rows carry an HONEST three-state readiness mark (gap G10):
        // logged in (green ●) / installed-but-not-logged-in (amber ◐ + login cmd)
        // / not installed (grey ○ + install cmd) / unknown (amber ◐, conservative).
        // Mode/language rows carry no mark. The glyphs are geometric (no emoji).
        let (icon, icon_color, state_detail) = if item.backend_id.is_some() {
            picker_auth_marks(app.lang, item)
        } else {
            (String::new(), theme::TEXT_MUTED(), item.detail.clone())
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
            Span::styled(state_detail, Style::default().fg(theme::TEXT_MUTED())),
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
    // CLAMP it so the title(1) + at least one transcript row + the spacer(1)
    // above the prompt always fit: `area.height - 3` is the most the prompt may
    // take. Without this a tall multi-line input on a short terminal would shove
    // the spacer (and the bottom of the input) past the viewport edge.
    let prompt_h = prompt_block_height(&app.input, inner.width, mode_prefix_width(app))
        .min(inner.height.saturating_sub(3))
        .max(2);
    // Wave-1 live plan + team-review panel — a fixed region between the
    // transcript and the prompt, shown only when a plan / review is live and the
    // terminal is tall enough to spare the rows (so it NEVER crushes the
    // transcript below one row or pushes the prompt/status off-screen — the same
    // small-terminal guarantee the rest of the layout keeps). The panel is
    // capped; long plans scroll inside it conceptually but here we just clip to
    // the cap with an "N more" tail.
    let panel_lines = plan_panel_lines(app, inner.width);
    // Reserve at most this many rows for the panel, and only when there's
    // headroom: title(1) + transcript(≥3) + prompt + status must still fit.
    let panel_h = if panel_lines.is_empty() {
        0
    } else {
        // +1 for the panel's TOP border row.
        let want = u16::try_from(panel_lines.len())
            .unwrap_or(0)
            .saturating_add(1);
        let headroom = inner.height.saturating_sub(1 + 3 + 1 + prompt_h); // title + min transcript + spacer + prompt
        want.min(headroom).min(PLAN_PANEL_MAX_ROWS)
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),        // title row (borderless)
            Constraint::Min(1),           // transcript (grows; ≥1 guaranteed)
            Constraint::Length(panel_h),  // live plan / team-review panel (0 = hidden)
            Constraint::Length(1),        // spacer — breathing room above the prompt
            Constraint::Length(prompt_h), // prompt: input(N) + border(1) + meta(1, live status pinned bottom-right)
        ])
        .split(inner);

    render_title_row(frame, chunks[0], app);
    render_transcript(frame, chunks[1], app);
    if panel_h > 0 {
        render_plan_panel(frame, chunks[2], &panel_lines, app.lang);
    }
    // chunks[3] is an intentional blank spacer row — it separates the transcript
    // / live plan panel from the input box so the prompt no longer sits jammed
    // against the content above it. The live status that used to burn its own
    // footer row now rides the bottom-right of the prompt's meta row instead, so
    // this gap costs no net vertical space.
    render_prompt(frame, chunks[4], app);

    // A popover floats above the prompt: the `@`-file-mention typeahead takes
    // precedence over the slash palette so the two are mutually exclusive — only
    // one opens, chosen by what is under the cursor.
    let mention = app.mention_matches();
    if mention.is_empty() {
        // Slash-command palette popover when typing a `/`-prefixed command.
        let palette = app.palette_matches();
        if !palette.is_empty() {
            render_palette_popover(frame, chunks[4], app, &palette);
        }
    } else {
        render_mention_popover(frame, chunks[4], app, &mention);
    }
}

/// Hard cap on the live plan / team-review panel height so a 20-step plan can't
/// swallow the transcript. Beyond this the panel shows a compact "N more" tail.
const PLAN_PANEL_MAX_ROWS: u16 = 12;

/// Build the live plan checklist + team-review panel content (Wave 1
/// deliverables 2/3). Returns the pre-styled lines, or an empty vec when there's
/// nothing live (plan empty AND no verdicts) — the caller then reserves zero
/// rows. Fail-open: an unknown status string renders as a neutral pending dot.
fn plan_panel_lines(app: &App, _width: u16) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let has_plan = !app.plan_steps.is_empty();
    let has_review = !app.critic_verdicts.is_empty();
    if !has_plan && !has_review {
        return lines;
    }

    // ── Live plan checklist ──
    if has_plan {
        let done = app.plan_steps.iter().filter(|s| s.status == "done").count();
        let total = app.plan_steps.len();
        // The 1-based index of the step that's currently Active, if any. During a
        // long single-step turn the done/total counter sits still (e.g. "0/5"),
        // which reads as frozen; surfacing WHICH step is running ("步骤 2/5 ·
        // 进行中") makes the live progress legible even when `done` hasn't moved.
        let active_step = app
            .plan_steps
            .iter()
            .position(|s| s.status == "active")
            .map(|i| i + 1);
        let active_suffix = active_step
            .map(|n| {
                format!(
                    " · {}",
                    umadev_i18n::tf(
                        app.lang,
                        "plan.panel.active_step",
                        &[&n.to_string(), &total.to_string()],
                    )
                )
            })
            .unwrap_or_default();
        if app.plan_collapsed {
            lines.push(Line::from(Span::styled(
                format!(
                    "{}{active_suffix}",
                    umadev_i18n::tf(
                        app.lang,
                        "plan.panel.collapsed",
                        &[&done.to_string(), &total.to_string()],
                    )
                ),
                Style::default().fg(theme::TEXT_MUTED()),
            )));
        } else {
            lines.push(Line::from(Span::styled(
                format!(
                    " {} {done}/{total}{active_suffix}",
                    umadev_i18n::t(app.lang, "plan.panel.title")
                ),
                Style::default()
                    .fg(theme::PRIMARY())
                    .add_modifier(Modifier::BOLD),
            )));
            for step in &app.plan_steps {
                let (mark, color) = checklist_glyph(&step.status);
                lines.push(Line::from(vec![
                    Span::styled(format!("  {mark} "), Style::default().fg(color)),
                    Span::styled(
                        truncate_display(&step.title, 56),
                        if step.status == "done" {
                            Style::default().fg(theme::TEXT_MUTED())
                        } else if step.status == "active" {
                            Style::default()
                                .fg(theme::TEXT())
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(theme::TEXT())
                        },
                    ),
                ]));
            }
        }
    }

    // ── Collapsible team-review panel ──
    if has_review {
        let accepts = app.critic_verdicts.iter().filter(|c| c.accepts).count();
        let blocking: usize = app.critic_verdicts.iter().filter(|c| !c.accepts).count();
        if app.critics_collapsed {
            lines.push(Line::from(Span::styled(
                umadev_i18n::tf(
                    app.lang,
                    "plan.review.collapsed",
                    &[&accepts.to_string(), &blocking.to_string()],
                ),
                Style::default().fg(theme::TEXT_MUTED()),
            )));
        } else {
            lines.push(Line::from(Span::styled(
                format!(" {}", umadev_i18n::t(app.lang, "plan.review.title")),
                Style::default()
                    .fg(theme::SECONDARY())
                    .add_modifier(Modifier::BOLD),
            )));
            for c in &app.critic_verdicts {
                let (mark, color) = if c.accepts {
                    (review_accept_glyph(), theme::SUCCESS())
                } else {
                    (review_block_glyph(), theme::ERROR())
                };
                let verdict = if c.accepts {
                    umadev_i18n::t(app.lang, "plan.review.accept").to_string()
                } else {
                    umadev_i18n::tf(
                        app.lang,
                        "plan.review.block",
                        &[&c.blocking.len().max(1).to_string()],
                    )
                };
                // First must-fix finding inline so a blocker is actionable at a
                // glance (the full set folds into the rework directive upstream).
                let detail = c
                    .blocking
                    .first()
                    .or_else(|| c.advisory.first())
                    .map(|s| format!(": {}", truncate_display(s, 44)))
                    .unwrap_or_default();
                lines.push(Line::from(vec![
                    Span::styled(format!("  {mark} "), Style::default().fg(color)),
                    Span::styled(
                        format!("[{}] ", c.seat),
                        Style::default().fg(theme::SECONDARY()),
                    ),
                    Span::styled(
                        format!("{verdict}{detail}"),
                        Style::default().fg(theme::TEXT()),
                    ),
                ]));
            }
        }
    }
    lines
}

/// The checklist glyph + colour for a plan step status. Built from codepoints so
/// the source carries no literal pictographic glyph. Fail-open: an unknown
/// status renders as the neutral pending box.
fn checklist_glyph(status: &str) -> (String, ratatui::style::Color) {
    match status {
        // [x] filled check — done.
        "done" => (
            format!("[{}]", char::from_u32(0x2713).unwrap_or('x')),
            theme::SUCCESS(),
        ),
        // [~] in-progress.
        "active" => ("[~]".to_string(), theme::WARNING()),
        // [!] blocked.
        "blocked" => ("[!]".to_string(), theme::ERROR()),
        // [ ] pending (and any unrecognised status).
        _ => ("[ ]".to_string(), theme::TEXT_MUTED()),
    }
}

/// Accept mark for the team-review panel (a check), built from its codepoint.
fn review_accept_glyph() -> String {
    char::from_u32(0x2713).unwrap_or('+').to_string()
}

/// Blocking mark for the team-review panel (a cross), built from its codepoint.
fn review_block_glyph() -> String {
    char::from_u32(0x2717).unwrap_or('x').to_string()
}

/// Truncate a string to `max` DISPLAY characters with an ellipsis, so a long
/// step title / finding can't overflow the panel row (the panel renders without
/// `wrap`, mirroring the transcript's pre-fold model). Char-safe.
fn truncate_display(s: &str, max: usize) -> String {
    let one_line: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max {
        return one_line;
    }
    let mut t: String = one_line.chars().take(max.saturating_sub(1)).collect();
    t.push('…');
    t
}

/// Render the live plan / team-review panel into `area`. A thin top rule
/// separates it from the transcript above. Lines are clipped to the area height
/// (the caller already capped it); an overflow shows a muted "…" tail row.
fn render_plan_panel(
    frame: &mut Frame,
    area: Rect,
    lines: &[Line<'static>],
    lang: umadev_i18n::Lang,
) {
    // The TOP border eats one row, so the content fits in `area.height - 1`.
    let inner_rows = (area.height as usize).saturating_sub(1);
    if inner_rows == 0 {
        return;
    }
    let shown: Vec<Line<'static>> = if lines.len() > inner_rows {
        // Keep the head visible (title + first steps) and mark the clip with a
        // HINT — the clipped rows (usually the tail of the team-review verdicts)
        // are not lost: the full per-seat verdicts are in the transcript above,
        // and `/plan` re-prints the checklist. Telling the user HOW is the whole
        // point ("… +N" alone read as a dead end).
        let mut v: Vec<Line<'static>> = lines
            .iter()
            .take(inner_rows.saturating_sub(1))
            .cloned()
            .collect();
        v.push(Line::from(Span::styled(
            format!(
                "  … +{} · {}",
                lines.len() - inner_rows + 1,
                umadev_i18n::t(lang, "plan.panel.more_hint")
            ),
            Style::default().fg(theme::TEXT_MUTED()),
        )));
        v
    } else {
        lines.to_vec()
    };
    frame.render_widget(
        Paragraph::new(shown).block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Style::default().fg(theme::BORDER())),
        ),
        area,
    );
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
    // The base lives here in the top bar (identity + context), so the bottom row
    // doesn't have to repeat "project · base".
    let base = Span::styled(
        format!(" {} ", app.backend.as_deref().unwrap_or("offline")),
        Style::default().fg(theme::TEXT_MUTED()),
    );
    let line = Line::from(vec![
        title,
        Span::styled("·", Style::default().fg(theme::BORDER())),
        slug,
        Span::styled("·", Style::default().fg(theme::BORDER())),
        base,
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

/// The status glyph for a structured tool row, by lifecycle. Built from
/// codepoints so the source carries no literal pictographic glyph: queued = a
/// dim hollow circle, running = the live spinner frame, ok = a filled circle
/// (green), fail = a filled circle (red). Returned with its colour so the caller
/// styles it consistently.
fn tool_status_glyph(status: ToolStatus, spinner: char) -> (char, Color) {
    match status {
        // Hollow circle U+25CB, dimmed — `Queued` (not started yet) and
        // `Aborted` (settled by an interrupt: neither succeeded nor failed, the
        // run just ended first) both read as a neutral, non-spinning dot, so
        // neither is mistaken for a green success or a red error.
        ToolStatus::Queued | ToolStatus::Aborted => {
            (char::from_u32(0x25CB).unwrap_or('o'), theme::TEXT_MUTED())
        }
        ToolStatus::Running => (spinner, theme::ACCENT()),
        // Filled circle U+25CF.
        ToolStatus::Ok => (char::from_u32(0x25CF).unwrap_or('*'), theme::SUCCESS()),
        ToolStatus::Fail => (char::from_u32(0x25CF).unwrap_or('*'), theme::ERROR()),
    }
}

/// The dim continuation gutter used under a tool row's result and folded
/// summaries — a corner-bracket (`⎿  `) built from its codepoint, matching the
/// Claude-Code tool-result indent.
fn result_gutter() -> String {
    let mut s = String::with_capacity(4);
    s.push(char::from_u32(0x23BF).unwrap_or('|'));
    s.push_str("  ");
    s
}

/// Decimal digit count of `n` (`0`/`1`→1, `42`→2, …). Integer-only — no float
/// `log10` cast — so the gutter width is computed without a lossy conversion.
fn decimal_digits(mut n: u32) -> usize {
    let mut d = 1;
    while n >= 10 {
        n /= 10;
        d += 1;
    }
    d
}

/// Derive a syntax-highlighting language hint from a file path's extension. The
/// returned token feeds [`highlight_code_line`] / `keywords_for`, which key off
/// the extension directly (`rs` / `py` / `ts` / …). `None` when there's no
/// extension — the highlighter then degrades to its plaintext heuristics
/// (strings / numbers / comments), never a panic.
fn lang_hint_from_path(path: &str) -> Option<String> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let ext = name.rsplit_once('.').map(|(_, e)| e)?;
    if ext.is_empty() || ext.len() > 12 {
        return None;
    }
    Some(ext.to_ascii_lowercase())
}

/// Render a structured file diff as a Claude-Code-style diff card (P1):
///
/// - a header `path (+N −M)` (the path syntax-neutral, the metric in
///   add/del colors), framed by a dashed TOP/BOTTOM border (no left/right
///   sides — it sits in the transcript flow);
/// - one row per diff line: a FIXED-WIDTH left gutter = `digits(max_line_no)+3`
///   columns holding the `+`/`-`/` ` marker + a right-aligned line number, then
///   the line content syntax-highlighted by [`highlight_code_line`] keyed on the
///   file extension;
/// - all colors via [`SynRole`] (`DiffAdd`/`DiffDel`/`Muted`) — **no naked
///   `Color`** — so a theme swap re-skins the card.
///
/// **Folding (P6 reuse):** a big diff (over the fold threshold) renders ONLY its
/// header line with a `· Ctrl+R 展开` hint; expanding shows the hunks. Returns
/// `(Line, hang)` pairs the way the transcript expects.
///
/// **Fail-open:** pure formatting over already-built data; never panics. CJK
/// content is width-measured (`disp_width`) so a wide glyph never miscounts the
/// gutter.
fn diff_to_lines(
    d: &FileDiff,
    lang: umadev_i18n::Lang,
    width: usize,
    verbose: bool,
) -> Vec<(Line<'static>, usize)> {
    let mut out: Vec<(Line<'static>, usize)> = Vec::new();

    // ── Folded: just the header with the expand hint ──────────────────────
    // The global `verbose` toggle (Ctrl+O) force-expands EVERY diff at once, so
    // an older folded card is never stranded with no reveal gesture.
    if d.collapsed && !verbose {
        let hint = umadev_i18n::t(lang, "tui.fold.expand_hint");
        let text = umadev_i18n::tf(
            lang,
            "tui.diff.collapsed",
            &[&d.path, &d.added.to_string(), &d.removed.to_string(), hint],
        );
        out.push((
            Line::from(role_span(text, SynRole::Muted, Modifier::empty())),
            2,
        ));
        return out;
    }

    // ── Header row: ┄┄ path (+N −M) (dashed top frame, no left/right sides) ─
    let mut head: Vec<Span<'static>> = Vec::with_capacity(5);
    head.push(role_span("┄┄ ", SynRole::Muted, Modifier::empty()));
    head.push(Span::styled(
        d.path.clone(),
        Style::default()
            .fg(theme::TEXT())
            .add_modifier(Modifier::BOLD),
    ));
    head.push(role_span(" (", SynRole::Muted, Modifier::empty()));
    head.push(role_span(
        format!("+{}", d.added),
        SynRole::DiffAdd,
        Modifier::empty(),
    ));
    head.push(role_span(" ", SynRole::Muted, Modifier::empty()));
    head.push(role_span(
        format!("−{}", d.removed),
        SynRole::DiffDel,
        Modifier::empty(),
    ));
    head.push(role_span(") ", SynRole::Muted, Modifier::empty()));
    out.push((Line::from(head), 2));

    // Fixed gutter number column: enough digits for the largest line number.
    let num_w = decimal_digits(d.max_line_no().max(1));

    let lang_hint = lang_hint_from_path(&d.path);
    let last_hunk = d.hunks.len().saturating_sub(1);
    // Per-card hard cap on rendered CONTENT rows when expanded: a pathological
    // single hunk (e.g. a 2000-line rewrite that slipped under the fold
    // threshold via context grouping) still can't flood the transcript. After
    // the cap, a muted `… N more lines (open the file)` tail closes the body.
    // Counts only +/-/context rows (not the dashed frames / hunk separators).
    let mut content_rows = 0usize;
    let mut truncated_remaining = 0usize;
    'hunks: for (hi, hunk) in d.hunks.iter().enumerate() {
        // A thin separator between non-adjacent hunks (a dim "⋮" under the gutter).
        if hi > 0 {
            out.push((
                Line::from(role_span(
                    format!("{:>w$} ⋮", "", w = num_w + 1),
                    SynRole::Muted,
                    Modifier::empty(),
                )),
                2,
            ));
        }
        for (li, dl) in hunk.lines.iter().enumerate() {
            if content_rows >= DIFF_EXPANDED_ROW_CAP {
                // Everything not yet rendered, across THIS and the remaining
                // hunks, folds into the tail count.
                truncated_remaining = (hunk.lines.len() - li)
                    + d.hunks[hi + 1..]
                        .iter()
                        .map(|h| h.lines.len())
                        .sum::<usize>();
                break 'hunks;
            }
            content_rows += 1;
            // Fixed-width left gutter (no left frame char — dashed top/bottom
            // only): marker + right-aligned line number + a space, width =
            // digits(max_line_no)+3 (marker + space-after-number + trailing
            // space). All colors from semantic roles.
            let num = dl
                .line_no
                .map_or_else(|| " ".repeat(num_w), |n| format!("{n:>num_w$}"));
            // Row background tint: a low-alpha green/red fill behind a +/- line
            // (pre-mixed in the theme — never a naked Color), padded full-width
            // below; a context row stays on the terminal background.
            let row_bg = match dl.tag {
                '+' => Some(theme::DIFF_ADD_BG()),
                '-' => Some(theme::DIFF_DEL_BG()),
                _ => None,
            };
            let marker_role = match dl.tag {
                '+' => SynRole::DiffAdd,
                '-' => SynRole::DiffDel,
                _ => SynRole::Muted,
            };
            let mut spans: Vec<Span<'static>> = Vec::new();
            // Gutter span — carries the row bg too so the tint starts at column 0.
            let mut gutter = role_span(
                format!("{} {} ", dl.tag, num),
                marker_role,
                Modifier::empty(),
            );
            if let Some(bg) = row_bg {
                gutter.style = gutter.style.bg(bg);
            }
            spans.push(gutter);
            // Content: word-level when the line carries changed ranges (the
            // changed tokens pop in the brighter DiffAddWord/DiffDelWord role,
            // the rest is normally syntax-highlighted — so a deletion is no
            // longer one flat block of red). With no ranges it falls back to the
            // whole-line treatment (+ = syntax-highlight, − = uniform del color).
            spans.extend(diff_content_spans(dl, lang_hint.as_deref(), row_bg));
            // Pad the row to the full card width with a bg-filled tail so the
            // tint spans edge-to-edge (Claude-Code full-row diff background).
            if let Some(bg) = row_bg {
                let used: usize = spans.iter().map(|s| disp_width(&s.content)).sum();
                if used < width {
                    spans.push(Span::styled(
                        " ".repeat(width - used),
                        Style::default().bg(bg),
                    ));
                }
            }
            out.push((Line::from(spans), 2));
        }
        // Dashed bottom frame after the final hunk (no left/right sides).
        if hi == last_hunk {
            out.push((
                Line::from(role_span("┄┄┄┄┄┄┄┄┄┄", SynRole::Muted, Modifier::empty())),
                2,
            ));
        }
    }
    // Truncation tail: when the expanded body hit the row cap, a muted line
    // tells the user how many rows were elided (open the file to see the rest).
    if truncated_remaining > 0 {
        let tail = umadev_i18n::tf(
            lang,
            "tui.diff.truncated",
            &[&truncated_remaining.to_string()],
        );
        out.push((
            Line::from(role_span(tail, SynRole::Muted, Modifier::empty())),
            2,
        ));
        out.push((
            Line::from(role_span("┄┄┄┄┄┄┄┄┄┄", SynRole::Muted, Modifier::empty())),
            2,
        ));
    }
    // An empty diff (no hunks) still closes its frame so it never looks broken.
    if d.hunks.is_empty() {
        out.push((
            Line::from(role_span("┄┄┄┄┄┄┄┄┄┄", SynRole::Muted, Modifier::empty())),
            2,
        ));
    }
    out
}

/// Hard cap on rendered content rows for an EXPANDED diff card — past this a
/// muted `… N more lines` tail closes the body so one pathological hunk can't
/// flood the transcript (the fold threshold handles the common big diff; this
/// is the safety net for a hunk that grouped under it).
pub(crate) const DIFF_EXPANDED_ROW_CAP: usize = 200;

/// Build the styled content spans for ONE diff line, honouring its word-level
/// `changed` ranges.
///
/// - **Changed segment** (inside a `[start,end)` range): emphasised in the
///   brighter [`SynRole::DiffAddWord`] / [`SynRole::DiffDelWord`] role so the
///   actually-edited token pops.
/// - **Unchanged segment** (between ranges): on a `+`/context line it is run
///   through the normal syntax highlighter ([`highlight_code_line`]); on a `-`
///   line it stays in the muted base delete color (it's gone — no need to fully
///   re-syntax it, but it's NOT confetti-red either).
/// - **No ranges** (`changed` empty → unpaired line / rewrite-threshold
///   fallback / context row): the whole line falls back to the prior behaviour
///   (`+`/context = syntax-highlight, `−` = uniform delete color).
///
/// `row_bg`, when set, is layered behind every emitted span so the tint is
/// continuous under both changed and unchanged text. **CJK/UTF-8 safe:** the
/// ranges are byte offsets on char boundaries (built from `similar`'s word
/// slices); each segment is sliced on those boundaries, so a wide glyph is
/// never split. **Fail-open:** a malformed range (out of bounds / not on a char
/// boundary / mis-ordered) makes the whole line fall back to the no-range path.
fn diff_content_spans(
    dl: &crate::app::DiffLine,
    lang_hint: Option<&str>,
    row_bg: Option<Color>,
) -> Vec<Span<'static>> {
    let with_bg = |mut spans: Vec<Span<'static>>| -> Vec<Span<'static>> {
        if let Some(bg) = row_bg {
            for s in &mut spans {
                s.style = s.style.bg(bg);
            }
        }
        spans
    };

    // Whole-line fallback: no word ranges, OR any range is unusable.
    let ranges_ok = !dl.changed.is_empty()
        && dl.changed.iter().all(|&(s, e)| {
            s < e && e <= dl.text.len() && dl.text.is_char_boundary(s) && dl.text.is_char_boundary(e)
        })
        // Strictly increasing, non-overlapping (push_range guarantees this, but
        // we re-check so a corrupt vec can never panic the slicer below).
        && dl
            .changed
            .windows(2)
            .all(|w| w[0].1 <= w[1].0);
    if !ranges_ok {
        let base = if dl.tag == '-' {
            vec![role_span(
                dl.text.clone(),
                SynRole::DiffDel,
                Modifier::empty(),
            )]
        } else {
            highlight_code_line(&dl.text, lang_hint)
        };
        return with_bg(base);
    }

    let word_role = if dl.tag == '-' {
        SynRole::DiffDelWord
    } else {
        SynRole::DiffAddWord
    };
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut cursor = 0usize;
    for &(s, e) in &dl.changed {
        // Unchanged segment before this changed range.
        if s > cursor {
            let seg = &dl.text[cursor..s];
            if dl.tag == '-' {
                // A deletion's unchanged text stays in the base delete color
                // (muted), so only the changed token is emphasised — not the
                // whole line re-coloured.
                spans.push(role_span(
                    seg.to_string(),
                    SynRole::DiffDel,
                    Modifier::empty(),
                ));
            } else {
                spans.extend(highlight_code_line(seg, lang_hint));
            }
        }
        // The changed segment itself — emphasised, bold for extra contrast.
        spans.push(role_span(
            dl.text[s..e].to_string(),
            word_role,
            Modifier::BOLD,
        ));
        cursor = e;
    }
    // Trailing unchanged tail.
    if cursor < dl.text.len() {
        let seg = &dl.text[cursor..];
        if dl.tag == '-' {
            spans.push(role_span(
                seg.to_string(),
                SynRole::DiffDel,
                Modifier::empty(),
            ));
        } else {
            spans.extend(highlight_code_line(seg, lang_hint));
        }
    }
    with_bg(spans)
}

/// Render one structured tool call as a single status line plus (when present
/// and not collapsed) its result in a dim gutter below.
///
/// P4 — the beautified tool row: `[glyph] [name BOLD] [dim (arg)]`. A finished
/// OK call's result is folded to a head-N preview + `… N more lines` summary
/// (P6); a running / failed call always shows its result expanded. The row
/// height is stable across pending→done (the glyph swaps in place, no reflow).
fn render_tool_row(
    tool: &ToolCall,
    rendered: &mut Vec<RenderedRow>,
    lang: umadev_i18n::Lang,
    spinner: char,
    verbose: bool,
) {
    // A tool call is a Host artifact, so every row it emits carries the Host
    // spine — the vertical skeleton stays unbroken across a turn's prose +
    // tool rows + diff cards.
    let spine = theme::role_bar(ChatRole::Host);
    let (glyph, glyph_color) = tool_status_glyph(tool.status, spinner);
    let mut head: Vec<Span<'static>> = Vec::with_capacity(4);
    head.push(Span::styled(
        format!("{glyph} "),
        Style::default().fg(glyph_color),
    ));
    head.push(Span::styled(
        tool.name.clone(),
        Style::default()
            .fg(theme::TEXT())
            .add_modifier(Modifier::BOLD),
    ));
    if !tool.arg.is_empty() {
        head.push(Span::styled(
            format!(" ({})", tool.arg),
            Style::default().fg(theme::TEXT_MUTED()),
        ));
    }
    // A row settled by an interrupt carries an explicit dim `[aborted]` tag so
    // the user reads it as stopped-mid-flight (not a silent neutral dot), mirror
    // of how a failed row's red glyph signals failure — no emoji, theme colour.
    if tool.status == ToolStatus::Aborted {
        head.push(Span::styled(
            format!(" {}", umadev_i18n::t(lang, "tui.tool.aborted")),
            Style::default().fg(theme::TEXT_MUTED()),
        ));
    }
    rendered.push(RenderedRow::spined(Line::from(head), GUTTER_W, spine));

    // The result gutter. A failed / running call always shows it; a finished OK
    // call shows it only when not collapsed. Long results fold to head-N + a
    // summary line.
    let Some(result) = tool.result.as_deref().filter(|r| !r.trim().is_empty()) else {
        return;
    };
    // A failed call is force-expanded so the error is never hidden, regardless
    // of the stored `collapsed` flag. The global `verbose` toggle (Ctrl+O) also
    // force-expands EVERY tool result at once — including older rows that Ctrl+R
    // (latest-only) can't reach.
    let show_collapsed = tool.collapsed && tool.status != ToolStatus::Fail && !verbose;
    let head_n = if tool.name == "Bash" {
        crate::app::FOLD_HEAD_SHELL
    } else {
        crate::app::FOLD_HEAD_GENERAL
    };
    let gutter = result_gutter();
    let lines: Vec<&str> = result.lines().collect();
    let foldable = lines.len() > crate::app::FOLD_THRESHOLD;
    let shown: Vec<&str> = if show_collapsed && foldable {
        lines.iter().take(head_n).copied().collect()
    } else {
        lines.clone()
    };
    for (i, line) in shown.iter().enumerate() {
        let prefix = if i == 0 { gutter.clone() } else { "   ".into() };
        rendered.push(RenderedRow::spined(
            Line::from(vec![
                Span::styled(prefix, Style::default().fg(theme::TEXT_MUTED())),
                Span::styled(
                    (*line).to_string(),
                    Style::default().fg(theme::TEXT_MUTED()),
                ),
            ]),
            3,
            spine,
        ));
    }
    if show_collapsed && foldable {
        let hidden = lines.len().saturating_sub(head_n);
        rendered.push(RenderedRow::spined(
            fold_summary_line(hidden, lang),
            3,
            spine,
        ));
    }
}

/// The `… N more lines · Ctrl+R expand` summary row shown under a folded body.
fn fold_summary_line(hidden: usize, lang: umadev_i18n::Lang) -> Line<'static> {
    let hint = umadev_i18n::t(lang, "tui.fold.expand_hint");
    let text = umadev_i18n::tf(lang, "tui.fold.collapsed", &[&hidden.to_string(), hint]);
    Line::from(Span::styled(text, Style::default().fg(theme::TEXT_MUTED())))
}

/// Fold a long GENERAL (Host/UmaDev text) body to its head-N lines + a
/// `… N more lines` summary line (P6). Pure: takes the raw body, returns a
/// shorter body string that still flows through the markdown renderer. The
/// summary line is appended as plain text (it carries no markdown).
fn fold_general_text(body: &str, lang: umadev_i18n::Lang) -> String {
    let lines: Vec<&str> = body.lines().collect();
    if lines.len() <= crate::app::FOLD_THRESHOLD {
        return body.to_string();
    }
    let head_n = crate::app::FOLD_HEAD_GENERAL;
    let hidden = lines.len().saturating_sub(head_n);
    let hint = umadev_i18n::t(lang, "tui.fold.expand_hint");
    let summary = umadev_i18n::tf(lang, "tui.fold.collapsed", &[&hidden.to_string(), hint]);
    let mut head: String = lines
        .iter()
        .take(head_n)
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    head.push_str("\n\n");
    head.push_str(&summary);
    head
}

/// The first-row marker for an assistant turn, distinguishing the SEAT: the
/// UmaDev director's own voice vs the borrowed base (Host). Returns a
/// [`GUTTER_W`]-wide glyph-plus-space string and its color, so the two read as
/// different speakers, not one undifferentiated "AI."
///
/// - **Seat color** — `UmaDev` is the brand accent (cyan); `Host` is the
///   success/teammate color (green) — matching [`theme::role_bar`], so the
///   marker and the spine bar below it agree.
/// - **Glyph** — platform-aware: macOS terminals render the heavier record
///   circle `⏺` (U+23FA) crisply, elsewhere the plain filled circle `●`
///   (U+25CF). Built from the codepoint so the source carries no literal
///   pictographic glyph. Fail-open: an unmappable codepoint degrades to `*`.
///
/// Any non-assistant role passed here falls back to the Host marker (the
/// callers only ever pass `Host` / `UmaDev`).
fn assistant_marker(role: ChatRole) -> (String, Color) {
    let cp = if cfg!(target_os = "macos") {
        0x23FA // ⏺ heavy record circle — crisp on macOS terminals
    } else {
        0x25CF // ● plain filled circle — widest terminal support
    };
    let mut s = String::with_capacity(2);
    s.push(char::from_u32(cp).unwrap_or('*'));
    s.push(' ');
    let color = match role {
        ChatRole::UmaDev => theme::ACCENT(),
        _ => theme::SUCCESS(),
    };
    (s, color)
}

/// One logical transcript line plus the layout hints the per-width fold needs:
/// the hanging-indent width, an optional role-spine color (repainted down every
/// wrapped continuation row), and an optional full-row background fill (the
/// user-message bubble tint). Kept as a small struct rather than a 4-tuple so
/// the fold call stays readable and clippy's `type_complexity` lint is happy.
struct RenderedRow {
    line: Line<'static>,
    hang: usize,
    spine: Option<Color>,
    fill_bg: Option<Color>,
}

impl RenderedRow {
    /// A plain row: no spine, no fill (welcome art, blank gaps, the thinking
    /// indicator). `hang` still controls wrapped-continuation indentation.
    fn plain(line: Line<'static>, hang: usize) -> Self {
        Self {
            line,
            hang,
            spine: None,
            fill_bg: None,
        }
    }

    /// A spined row: a role bar repainted down every wrapped continuation row.
    fn spined(line: Line<'static>, hang: usize, spine: Color) -> Self {
        Self {
            line,
            hang,
            spine: Some(spine),
            fill_bg: None,
        }
    }
}

/// Compact human token count: `452` → `452`, `1500` → `1.5K`, `452000` → `452K`,
/// `1_500_000` → `1.5M`. Keeps the waiting indicator readable for a long session.
#[allow(clippy::cast_precision_loss)] // token counts never approach f64's 2^52 mantissa
fn fmt_token_count(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=9_999 => format!("{:.1}K", n as f64 / 1_000.0),
        10_000..=999_999 => format!("{}K", n / 1_000),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

fn render_transcript(frame: &mut Frame, area: Rect, app: &App) {
    // Cap the retained scrollback so a marathon session can't grow the per-frame
    // fold unbounded. Counted in **visual rows AFTER folding** (not logical lines
    // before it) so `hidden_above` — and therefore `transcript_max_scroll` — always
    // reflects the real, reachable history: PageUp / Home can scroll to the very
    // top of what's kept. Raised far above the old 500-LOGICAL-line cap (which
    // truncated long/CJK transcripts *before* the wrap, so the published
    // `max_scroll` lied and the oldest content was unreachable). 8000 rows is
    // hundreds of screens — generous for any human session, still bounded.
    const MAX_RENDER_ROWS: usize = 8000;
    let inner_height = area.height as usize;

    // Each logical line carries a `hang` (its left-gutter width). When the line
    // is pre-folded to the viewport width, continuation rows are indented by
    // `hang` so a wrapped paragraph stays aligned under its bullet/prefix instead
    // of reflowing flush-left. Welcome-banner art keeps a zero hang (it never
    // wraps at a sane width). A hang of 2 matches the two-column bullet gutter and
    // the gate bar; a hang of 1 matches the user-row leading space.
    // Each entry is a `RenderedRow`: the logical line + its hang width + an
    // optional role-spine color (repainted down every wrapped continuation row
    // so a multi-line turn reads as one vertical bar in the speaker's color) +
    // an optional full-row bg fill (the user bubble tint). Welcome art is plain
    // (it isn't a turn).
    // Tables rendered inside `markdown_to_lines` below must fit this width (minus a
    // small margin) instead of overflowing + getting char-folded into a scrambled
    // grid. Set once per render; read by `render_table`.
    set_table_width_budget((area.width as usize).saturating_sub(GUTTER_W + 2).max(20));

    let mut rendered: Vec<RenderedRow> = welcome_lines(app)
        .into_iter()
        .map(|l| RenderedRow::plain(l, 0))
        .collect();
    for (msg_idx, msg) in app.history.iter().enumerate() {
        // Top gap before each message for breathing room (Claude Code: marginTop=1).
        if msg_idx > 0 {
            rendered.push(RenderedRow::plain(Line::from(""), 0));
        }

        if msg.role == ChatRole::Gate {
            let body = msg.body();
            let bar = theme::role_bar(ChatRole::Gate);
            let mut block: Vec<Line<'static>> = Vec::new();
            render_gate_block(&body, bar, &mut block);
            // Gate keeps its own per-line `▎` prefix on the FIRST row; the spine
            // color carries that bar down any wrapped continuation row too.
            rendered.extend(
                block
                    .into_iter()
                    .map(|l| RenderedRow::spined(l, GUTTER_W, bar)),
            );
            continue;
        }

        // A structured tool row renders the same regardless of (Host) role: a
        // single status line + a folded result gutter. Handled before the
        // role-text match so its body never falls through to the prose path.
        // Tool rows belong to the Host flow, so they carry the Host spine — the
        // vertical skeleton stays unbroken across a turn's prose + tool rows.
        if let MessageBody::Tool(tool) = &msg.kind {
            render_tool_row(tool, &mut rendered, app.lang, app.spinner(), app.verbose);
            continue;
        }
        // A structured diff card (P1) — a Write/Edit rendered as a real diff.
        // Handled here for the same reason: it has its own renderer, never the
        // prose path. A diff card is a Host artifact → Host spine on every row.
        if let MessageBody::Diff(d) = &msg.kind {
            let bar = theme::role_bar(ChatRole::Host);
            rendered.extend(
                diff_to_lines(d, app.lang, area.width as usize, app.verbose)
                    .into_iter()
                    .map(|(l, hang)| RenderedRow::spined(l, hang.max(GUTTER_W), bar)),
            );
            continue;
        }

        let body = msg.body();
        let spine = theme::role_bar(msg.role);
        match msg.role {
            // **User messages** — full-width tinted background bubble (Claude
            // Code: userMessageBackground) behind a role-spine bar. The leading
            // `▎ ` spine replaces the old single leading space, so the gutter is
            // the unified `GUTTER_W` like every other speaker, and `fill_bg`
            // makes the fold right-pad each row so the tint reads as one solid
            // block instead of stopping ragged at the text.
            ChatRole::You => {
                for line in body.lines() {
                    let spans = vec![
                        role_spine_span(ChatRole::You),
                        Span::styled(
                            line.to_string(),
                            Style::default().fg(theme::TEXT()).bg(theme::USER_MSG_BG()),
                        ),
                    ];
                    rendered.push(RenderedRow {
                        line: Line::from(spans),
                        hang: GUTTER_W,
                        spine: Some(spine),
                        fill_bg: Some(theme::USER_MSG_BG()),
                    });
                }
            }
            // **Assistant/Host messages** — role spine + leading bullet + plain
            // text on the terminal background (Claude Code: AssistantTextMessage).
            // The unified `GUTTER_W` gutter is also the hang width, so a long
            // paragraph that wraps lines up under the text, not under the bullet,
            // and the spine carries down every wrapped row.
            //
            // **P6 long-output fold**: a collapsed long body is truncated to a
            // head-N preview + a `… N more lines` summary (Ctrl+R expands).
            ChatRole::Host | ChatRole::UmaDev => {
                // The global `verbose` toggle (Ctrl+O) force-expands every long
                // reply at once, so an older folded wall always has a reveal
                // gesture (Ctrl+R only reaches the most-recent row).
                let eff_collapsed =
                    msg.collapsed && !app.verbose && crate::app::message_is_collapsible(msg);
                let folded = if eff_collapsed {
                    fold_general_text(&body, app.lang)
                } else {
                    body.into_owned()
                };
                // **P5a**: the message currently being streamed (the LAST Host
                // text segment while `stream_text_active`) renders through the
                // stable-prefix cache — only its unclosed tail is re-parsed each
                // frame. Every other message renders whole, unchanged. The cached
                // compose is proven line-for-line identical to a whole render, so
                // the visible output is byte-for-byte the same either way.
                let is_live_stream = app.stream_text_active
                    && msg_idx + 1 == app.history.len()
                    && matches!(msg.role, ChatRole::Host)
                    && matches!(msg.kind, MessageBody::Text(_))
                    && !eff_collapsed;
                let body_lines = if is_live_stream {
                    // Fail-open: a borrow conflict (re-entrant render) falls back
                    // to a plain whole-body render.
                    match app.stream_md_cache.try_borrow_mut() {
                        Ok(mut cache) => stream_markdown_lines(&mut cache, &folded),
                        Err(_) => markdown_to_lines(&folded, theme::TEXT()),
                    }
                } else {
                    markdown_to_lines(&folded, theme::TEXT())
                };
                // The first row leads with the role spine; the marker (bullet)
                // distinguishes the SEAT — UmaDev's own director voice vs the
                // borrowed base (Host). Continuation rows hang under `GUTTER_W`
                // and the spine repaints there.
                for (i, bl) in body_lines.into_iter().enumerate() {
                    let mut spans: Vec<Span<'static>> = Vec::new();
                    if i == 0 {
                        let (marker, marker_color) = assistant_marker(msg.role);
                        spans.push(Span::styled(marker, Style::default().fg(marker_color)));
                    } else {
                        // Continuation: the spine glyph fills col 0; pad to gutter.
                        spans.push(role_spine_span(msg.role));
                    }
                    spans.extend(bl.spans);
                    rendered.push(RenderedRow::spined(Line::from(spans), GUTTER_W, spine));
                }
            }
            // **System messages** — dim/muted text behind a role-spine bar. The
            // old `  ` two-space prefix becomes the `▎ ` spine so System shares
            // the unified gutter and gets a vertical bar like every other turn.
            ChatRole::System => {
                for line in body.lines() {
                    let spans = vec![
                        role_spine_span(ChatRole::System),
                        Span::styled(line.to_string(), Style::default().fg(theme::TEXT_MUTED())),
                    ];
                    rendered.push(RenderedRow::spined(Line::from(spans), GUTTER_W, spine));
                }
            }
            // **Error / high-risk warnings** — a LOUD, bold line in the theme's
            // error red (the same red as a failed tool / blocked review row),
            // behind the role spine. No emoji marker — the color + bold is the
            // signal. Drives the codex `danger-full-access` startup warning.
            ChatRole::Error => {
                for line in body.lines() {
                    let spans = vec![
                        role_spine_span(ChatRole::Error),
                        Span::styled(
                            line.to_string(),
                            Style::default()
                                .fg(theme::ERROR())
                                .add_modifier(Modifier::BOLD),
                        ),
                    ];
                    rendered.push(RenderedRow::spined(Line::from(spans), GUTTER_W, spine));
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
        // First Esc ARMS the interrupt → the hint flips to "press Esc again to
        // interrupt" so a stray single Esc can't cancel a long run.
        let esc_hint = if app.interrupt_armed() {
            umadev_i18n::t(app.lang, "status.esc_confirm")
        } else {
            umadev_i18n::t(app.lang, "status.esc_cancel")
        };
        // Cumulative REAL token consumption (read from the usage ledger), not a
        // per-turn character guess — shows true total spend (e.g. `≈452K tok`).
        let tok_part = if app.session_tokens > 0 {
            format!(" · ≈{} token", fmt_token_count(app.session_tokens))
        } else {
            String::new()
        };
        let elapsed = format!("  ({secs}s{tok_part} · {esc_hint})");
        rendered.push(RenderedRow::plain(Line::from(""), 0));
        let mut think_spans = vec![Span::styled(
            format!("{} ", app.spinner()),
            Style::default().fg(theme::ACCENT()),
        )];
        // The "thinking" word shimmers — a bright band sweeps across it so a long
        // wait reads as alive. It keeps moving even on a stall (a frozen shimmer
        // reads as "crashed", the opposite of the intent); only animations-off
        // stills it.
        // The verb reflects WHAT the base is doing RIGHT NOW — thinking, or the live
        // tool's action (reading / editing / running / searching / fetching) — so the
        // indicator changes through the turn instead of sitting on a static
        // "正在思考". Reverts to thinking the moment the tool's ToolResult lands.
        let verb: String = if app.tool_in_progress {
            match &app.stream_tool_batch {
                Some((tool, _)) => format!("{}…", tool_activity_verb(tool, app.lang)),
                None => format!("{}…", umadev_i18n::t(app.lang, "status.using_tool")),
            }
        } else {
            umadev_i18n::t(app.lang, "status.thinking").to_string()
        };
        think_spans.extend(shimmer_spans(
            &verb,
            app.tick,
            theme::ACCENT(),
            theme::TEXT(),
            app.animations,
        ));
        think_spans.push(Span::styled(
            elapsed,
            Style::default().fg(theme::TEXT_MUTED()),
        ));
        rendered.push(RenderedRow::plain(Line::from(think_spans), 2));
        // A trailing blank row lifts the indicator one line up off the input box
        // (it was sitting jammed right against the prompt).
        rendered.push(RenderedRow::plain(Line::from(""), 0));
    }
    // Pre-fold every logical line to the CURRENT width into the exact visual
    // rows it occupies, then render WITHOUT `Paragraph::wrap`. This is the
    // de-scramble fix: previously we *estimated* the wrapped height with
    // `disp_width().div_ceil(w)` and let `Paragraph::wrap` fold the line a
    // *different* way (its own unicode-width pass), so the scroll offset and the
    // painted rows disagreed — long/CJK sessions scattered glyphs and smeared
    // stale cells. Now the folded `Vec<Line>` length **is** the row count, so the
    // estimate equals reality and the scroll offset lands on the right row.
    // Continuation rows are indented by each line's `hang` so wrapped paragraphs
    // stay aligned under their bullet/prefix.
    let w = usize::from(area.width).max(1);
    let mut folded: Vec<Line<'static>> = rendered
        .into_iter()
        .flat_map(|row| prefold_line_filled(&row.line, w, row.hang, row.spine, row.fill_bg))
        .collect();
    // Bound the retained scrollback by VISUAL rows (post-fold), keeping the most
    // recent `MAX_RENDER_ROWS`. Doing it here — not on logical lines up top —
    // means `total` (and the `hidden_above` derived from it) equals exactly what
    // is paintable + reachable, so Home/PageUp can always reach the top of the
    // kept history instead of clamping short of truncated-but-uncounted rows.
    if folded.len() > MAX_RENDER_ROWS {
        folded = folded.split_off(folded.len() - MAX_RENDER_ROWS);
    }
    let total = folded.len();
    // The scroll-hint title (added below when content overflows) is a `Block` title
    // row that `Block::inner` STEALS off the top — so whenever it's shown the real
    // paintable viewport is one row shorter. Account for it: decide overflow against
    // the full height, and if the title will be shown shrink the viewport by one.
    // Without this, the newest (streaming) row is pushed off the bottom and the
    // oldest row stays unreachable — a clip on the live chat surface.
    let title_shown = total > inner_height;
    let viewport = if title_shown {
        inner_height.saturating_sub(1).max(1)
    } else {
        inner_height
    };
    let hidden_above = total.saturating_sub(viewport);

    // Publish the scroll bounds for the key handlers (Home/End, Page, Ctrl-U/D,
    // Shift+↑/↓, mouse wheel) — they clamp `transcript_scroll` against these
    // width-aware numbers instead of guessing. `transcript_scroll` counts rows
    // ABOVE the bottom; clamp it here so a stale value (e.g. after the window
    // grew and content now fits) can't push the view off the end.
    app.transcript_max_scroll.set(hidden_above);
    app.transcript_viewport_rows.set(viewport);

    // **P5b — sticky-to-bottom + scroll-up anchor.** `transcript_scroll` is the
    // rows-from-bottom offset (`0` = pinned to the bottom). When the user is
    // pinned (`0`), new content keeps the view glued to the newest line (sticky,
    // follows streaming tokens). When the user has scrolled UP, fresh rows landing
    // BELOW the viewport would otherwise push the content they're reading upward
    // (the from-bottom offset stays fixed while the bottom moves). To hold the
    // anchor, bump the offset by exactly the number of rows that appeared below
    // since the last frame, so the SAME rows stay on screen — the pin is released
    // but the reading position is held. Fail-open: clamps to `hidden_above`, and a
    // first frame (`prev == 0`) makes no adjustment.
    let prev_hidden = app.transcript_prev_hidden.get();
    let cur_scroll = app.transcript_scroll.get();
    if cur_scroll > 0 && hidden_above > prev_hidden {
        let grew_below = hidden_above - prev_hidden;
        let anchored = cur_scroll.saturating_add(grew_below).min(hidden_above);
        app.transcript_scroll.set(anchored);
    }
    app.transcript_prev_hidden.set(hidden_above);

    let user_offset = app.transcript_scroll.get().min(hidden_above);

    // Effective scroll: bottom-pinned is `hidden_above`; scrolling up SUBTRACTS
    // the user's offset so older content comes into view. At offset 0 the view
    // auto-sticks to the newest line (the default).
    let scroll_rows = hidden_above.saturating_sub(user_offset);
    let scroll = u16::try_from(scroll_rows).unwrap_or(u16::MAX);

    // ── In-app text-selection layer (the Claude-Code drag-to-copy) ──
    // `folded` is the exact `Vec<Line>` about to be painted: its index IS the
    // content-row coordinate the selection uses. Publish a plain-text snapshot of
    // every row, the paintable transcript rectangle, and the index of the row
    // painted at its top — the event loop maps a mouse `(col,row)` against these.
    // The cache holds the LOGICAL row text (leading role-spine / hang-indent
    // gutter stripped, trailing bg padding trimmed) so a drag-copy is clean — no
    // `▎`, no leading indent, no trailing-space runs polluting the clipboard. The
    // stripped gutter width is published per row so a screen column still maps to
    // the right logical char index. When the scroll-hint title is shown it steals
    // the top row, so content actually begins at `area.top + 1` with one row less
    // height; publish that adjusted rect so a click lands on the right content row.
    {
        let mut rows = app.transcript_rows.borrow_mut();
        let mut gutters = app.transcript_gutters.borrow_mut();
        rows.clear();
        gutters.clear();
        rows.reserve(folded.len());
        gutters.reserve(folded.len());
        for l in &folded {
            let (logical, gutter) = logical_row_and_gutter(l);
            rows.push(logical);
            gutters.push(gutter);
        }
    }
    let content_top = if title_shown {
        area.y.saturating_add(1)
    } else {
        area.y
    };
    let content_height = if title_shown {
        area.height.saturating_sub(1)
    } else {
        area.height
    };
    app.transcript_area
        .set((area.x, content_top, area.width, content_height));
    app.transcript_first_visible.set(scroll_rows);

    // Re-style the selected span(s) with the selection background. Only the rows
    // inside the normalized selection range are rebuilt; everything else paints
    // unchanged. Fail-open: a mapping error on any row just leaves that row
    // un-highlighted (never a panic).
    if let Some(sel) = app.selection {
        if !sel.is_empty() {
            apply_selection_highlight(&mut folded, &sel, &app.transcript_gutters.borrow());
        }
    }

    let para = Paragraph::new(folded).scroll((scroll, 0));

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
///
/// Every row (title / body / hint) now leads with the `▎ ` spine in `bar`'s
/// color — the same unified [`GUTTER_W`] gutter every other speaker uses — so
/// the gate reads as one continuous bar, not a `▎` title over `  `-indented
/// body. The caller threads `bar` as the spine color so wrapped continuation
/// rows repaint the bar too.
fn render_gate_block(body: &str, bar: Color, rendered: &mut Vec<Line<'static>>) {
    let lang = umadev_i18n::current();
    // The `▎ ` spine prefix, built from the shared glyph (no literal pictograph).
    let spine = || -> Span<'static> {
        let mut s = String::with_capacity(2);
        s.push(spine_glyph());
        s.push(' ');
        Span::styled(s, Style::default().fg(bar))
    };
    let title = Line::from(vec![
        spine(),
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
            spine(),
            Span::styled(line.to_string(), Style::default().fg(theme::TEXT())),
        ]));
    }
    rendered.push(Line::from(vec![
        spine(),
        Span::styled(
            umadev_i18n::t(lang, "tui.gate_block.hint"),
            Style::default().fg(theme::TEXT_MUTED()),
        ),
    ]));
}

/// The prompt — opencode-style. A panel with the agent-tinted left bar, the
/// Display columns one char occupies in a monospace terminal, via the Unicode
/// width tables (`unicode-width`). This replaces the old hand-rolled CJK range
/// list, which was wrong in three ways that desynced the cursor: zero-width
/// combining marks (U+0300–036F), ZWJ (U+200D) and variation selectors
/// (U+FE00–0F) were counted as 1 (should be 0), and dingbat / symbol emoji that
/// render two cells (✅ U+2705, ⚠ U+26A0, ☑ U+2611) were counted as 1.
///
/// `unicode-width` returns `None` for control chars; the editor never stores
/// bare control chars (they're filtered on insert), and `\t` is the only one
/// that can reach a render path, so we map `None` → 0 (fail-open: a stray
/// control char takes no columns rather than panicking the layout).
///
/// The TUI is not bound by the "dependency-light" rule — that constraint only
/// applies to the `spec` / `governance` / `contract` crates.
pub(crate) fn char_width(c: char) -> usize {
    unicode_width::UnicodeWidthChar::width(c).unwrap_or(0)
}

/// Display columns a string occupies (uses the Unicode width table: ASCII = 1,
/// wide CJK / emoji = 2, zero-width combining marks / ZWJ / variation selectors
/// = 0).
fn disp_width(s: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(s)
}

/// Truncate `s` to at most `max` display columns (CJK = 2), char-aligned so a
/// wide glyph is never split. Returns the kept prefix; the caller decides
/// whether to add an ellipsis. Used by the status row so a long CJK phase
/// string can never overflow a narrow terminal.
fn truncate_to_width(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut col = 0usize;
    for c in s.chars() {
        let cw = char_width(c);
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
        let cw = char_width(c);
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

/// Where the caret sits in the *wrapped* layout of `text` at `width` display
/// columns, as `(row, col)` — both 0-based, matching exactly how
/// [`wrap_input_rows`] folds the same text. `cursor` is a char index into
/// `text` (`0` = before the first char).
///
/// The subtle part is the **wrap boundary**: [`wrap_input_rows`] only folds when
/// the *next* glyph would overflow, so a row can fill to exactly `width` columns
/// and the caret would land on column `width` — i.e. on the right border / next
/// cell, the "caret 越界一列" bug. When the row is exactly full **and** there is
/// another glyph after the caret, the caret really belongs at the start of the
/// next visual row, so we advance it to `(row + 1, 0)`. An explicit `\n` always
/// starts a fresh row (col resets to 0), same as the wrapper.
pub(crate) fn caret_in_wrapped(text: &str, cursor: usize, width: u16) -> (u16, u16) {
    let w = width.max(1) as usize;
    let mut row = 0usize;
    let mut col = 0usize;
    for c in text.chars().take(cursor) {
        if c == '\n' {
            row += 1;
            col = 0;
            continue;
        }
        let cw = char_width(c);
        if col + cw > w && col > 0 {
            row += 1;
            col = 0;
        }
        col += cw;
    }
    // Row is exactly full and more text follows the caret → the next glyph wraps,
    // so the caret shows at the head of the next row instead of on the border.
    if col >= w && text.chars().nth(cursor).is_some_and(|c| c != '\n') {
        row += 1;
        col = 0;
    }
    (
        u16::try_from(row).unwrap_or(u16::MAX),
        u16::try_from(col).unwrap_or(u16::MAX),
    )
}

/// Number of wrapped visual rows `text` occupies at `width` columns — the same
/// fold [`wrap_input_rows`] produces, but without allocating the row strings.
/// Always at least 1.
pub(crate) fn wrapped_row_count(text: &str, width: u16) -> u16 {
    let w = width.max(1) as usize;
    let mut rows = 1usize;
    let mut col = 0usize;
    for c in text.chars() {
        if c == '\n' {
            rows += 1;
            col = 0;
            continue;
        }
        let cw = char_width(c);
        if col + cw > w && col > 0 {
            rows += 1;
            col = 0;
        }
        col += cw;
    }
    u16::try_from(rows).unwrap_or(u16::MAX)
}

/// Inverse of [`caret_in_wrapped`]: the char index in `text` whose caret lands
/// on visual `(target_row, target_col)` at `width` columns — used by Up/Down to
/// move the caret a wrapped row while preserving the display column. Walks the
/// same fold and returns the offset of the first glyph at/after `target_col` on
/// `target_row` (clamped to the row's end, and to the end of the text). A
/// `target_col` that lands *inside* a wide glyph snaps to that glyph's start.
pub(crate) fn offset_at_wrapped(text: &str, target_row: u16, target_col: u16, width: u16) -> usize {
    let w = width.max(1) as usize;
    let target_row = target_row as usize;
    let target_col = target_col as usize;
    let mut row = 0usize;
    let mut col = 0usize;
    let mut idx = 0usize; // char index of the glyph we're about to place
    for c in text.chars() {
        if c == '\n' {
            if row == target_row {
                // Caret can sit at end-of-line before a hard newline.
                return idx;
            }
            row += 1;
            col = 0;
            idx += 1;
            continue;
        }
        let cw = char_width(c);
        if col + cw > w && col > 0 {
            if row == target_row {
                // Soft-wrap: the target row ended just before this glyph.
                return idx;
            }
            row += 1;
            col = 0;
        }
        if row == target_row && col >= target_col {
            return idx;
        }
        col += cw;
        idx += 1;
    }
    // Reached the end of the text: the caret clamps to the very end (which is
    // also the end of the final row, where any target_col past the content lands).
    idx
}

/// Strip bare control characters from `s`, keeping only printable text plus
/// `\t` (tabs the renderer can handle) — `\n` is already split out by the
/// caller before this runs, so newlines never reach here. The base streams
/// structured JSON-text (no ANSI), but a stray ESC / cursor-move byte in any
/// delta would let the model move the terminal cursor and scribble outside the
/// transcript rect. Filtering them here keeps every glyph inside its cell.
/// Fail-open: a clean string is returned unchanged (no realloc on the hot path).
fn strip_control_chars(s: &str) -> std::borrow::Cow<'_, str> {
    if s.chars().any(|c| c.is_control() && c != '\t') {
        std::borrow::Cow::Owned(
            s.chars()
                .filter(|c| !c.is_control() || *c == '\t')
                .collect(),
        )
    } else {
        std::borrow::Cow::Borrowed(s)
    }
}

/// Pre-fold one logical [`Line`] into the exact visual rows it occupies at
/// `width` display columns, splitting [`Span`]s at display-width boundaries so
/// every style is preserved. This is the heart of the de-scramble fix: instead
/// of *estimating* the wrapped height with `disp_width().div_ceil(w)` and then
/// handing the un-folded line to `Paragraph::wrap` (whose own unicode-width
/// algorithm folds it a *different* way, so the scroll offset and the painted
/// rows disagree and CJK/long sessions scatter glyphs and smear stale cells),
/// we fold here and the returned `Vec<Line>` length **is** the real row count.
///
/// `hang` indents every continuation row by that many spaces, so a wrapped
/// assistant paragraph (the bullet-prefixed body) aligns under the text rather
/// than reflowing flush to the left gutter. Wide glyphs (CJK = 2 cols, via
/// [`disp_width`]) are never split across a fold, so no half-character ever
/// lands in a cell.
///
/// `spine`, when `Some(color)`, repaints the FIRST column of each continuation
/// row's hanging indent as the role-spine glyph (`▎`) in that color (the
/// remaining `hang - 1` columns stay padding spaces), so a multi-line turn
/// reads as one unbroken vertical bar in the speaker's color — the first row's
/// spine is already in its prefix span; this carries it down the wrapped rows.
/// `None` keeps the legacy plain-space indent. Fail-open: a `hang` of 0 means
/// no spine is drawn (there's no gutter column to paint).
///
/// Always returns at least one row. Fail-open: a zero `width` is treated as 1.
///
/// This is the no-fill convenience wrapper used by the fold-invariant unit
/// tests; the render path calls [`prefold_line_filled`] directly so it can pass
/// the user-bubble `fill_bg`.
#[cfg(test)]
fn prefold_line(
    line: &Line<'static>,
    width: usize,
    hang: usize,
    spine: Option<Color>,
) -> Vec<Line<'static>> {
    prefold_line_filled(line, width, hang, spine, None)
}

/// Like [`prefold_line`], but when `fill_bg` is `Some(color)` every emitted
/// visual row is right-padded with bg-tinted spaces to exactly `width` display
/// columns — so a tinted row (a user message) reads as one solid full-width
/// bubble instead of a tint that stops at the text and leaves a ragged right
/// edge. CJK-safe: padding is measured by display columns. Fail-open: `None`
/// pads nothing and behaves exactly like the old fold.
/// Render `word` with a soft shimmer: a small bright band (in `bright`) sweeps
/// across the word over time, driven by the spinner `tick`, the rest in `base`.
/// When `animated` is false (animations off / non-TTY) the word renders flat in
/// `base` bold — no per-char strobe. The whole word is always bold.
/// Map a base tool name to the live activity verb on the waiting indicator, so the
/// status reflects WHAT the base is doing — reading / editing / running / searching
/// / fetching — instead of a static "thinking" the whole turn. Unknown tools fall
/// back to a generic "{using} {tool}" so a new tool still reads sensibly.
fn tool_activity_verb(tool: &str, lang: umadev_i18n::Lang) -> String {
    let key = match tool {
        "Read" | "NotebookRead" => "status.act.reading",
        "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => "status.act.editing",
        "Bash" | "BashOutput" | "KillBash" => "status.act.running",
        "Grep" | "Glob" | "LS" => "status.act.searching",
        "WebFetch" | "WebSearch" => "status.act.fetching",
        _ => return format!("{} {tool}", umadev_i18n::t(lang, "status.using_tool")),
    };
    umadev_i18n::t(lang, key).to_string()
}

fn shimmer_spans(
    word: &str,
    tick: u8,
    base: Color,
    bright: Color,
    animated: bool,
) -> Vec<Span<'static>> {
    let chars: Vec<char> = word.chars().collect();
    if !animated || chars.is_empty() {
        return vec![Span::styled(
            word.to_string(),
            Style::default().fg(base).add_modifier(Modifier::BOLD),
        )];
    }
    let n = chars.len();
    let period = n + 4; // a short pause after the band leaves the word
                        // Advance the band every 4th tick so the shimmer sweeps calmly (~320ms/step at
                        // the ~80ms spinner tick) instead of strobing across the word.
    let head = (tick as usize / 4) % period;
    chars
        .into_iter()
        .enumerate()
        .map(|(i, c)| {
            // A ~2-char band centred on the moving head.
            let lit = head >= i && head <= i + 1;
            let fg = if lit { bright } else { base };
            Span::styled(
                c.to_string(),
                Style::default().fg(fg).add_modifier(Modifier::BOLD),
            )
        })
        .collect()
}

/// Append a styled char run onto `cur`, coalescing equal-style chars into one
/// `Span` so the word-wrap fold doesn't emit one Span per character.
fn emit_run(cur: &mut Vec<Span<'static>>, run: &[(char, Style)]) {
    let Some(&(_, mut st)) = run.first() else {
        return;
    };
    let mut buf = String::new();
    for &(c, s) in run {
        if s != st {
            if !buf.is_empty() {
                cur.push(Span::styled(std::mem::take(&mut buf), st));
            }
            st = s;
        }
        buf.push(c);
    }
    if !buf.is_empty() {
        cur.push(Span::styled(buf, st));
    }
}

fn prefold_line_filled(
    line: &Line<'static>,
    width: usize,
    hang: usize,
    spine: Option<Color>,
    fill_bg: Option<Color>,
) -> Vec<Line<'static>> {
    let w = width.max(1);
    let hang = hang.min(w.saturating_sub(1)); // never indent past the usable width
    let mut out: Vec<Line<'static>> = Vec::new();
    // Accumulator for the current visual row: the spans built so far + the
    // display column we've filled. The first row starts at column 0; every
    // continuation row starts after the hanging indent.
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut col = 0usize;
    let mut started_continuation = false;

    // Right-pad one finished row to `w` columns with bg-tinted spaces, then push
    // it. When `fill_bg` is None this just pushes the row unchanged.
    let push_padded = |out: &mut Vec<Line<'static>>, mut row: Vec<Span<'static>>, filled: usize| {
        if let Some(bg) = fill_bg {
            if filled < w {
                row.push(Span::styled(
                    " ".repeat(w - filled),
                    Style::default().bg(bg),
                ));
            }
        }
        out.push(Line::from(row));
    };

    // Emit the accumulated row and start a fresh continuation row (with hang).
    // When a `spine` color is set the indent's first column is the role-spine
    // glyph; the rest is padding. CJK-safe: the glyph is one display column and
    // the pad is plain spaces, so the total indent is exactly `hang` columns.
    macro_rules! flush_row {
        () => {{
            push_padded(&mut out, std::mem::take(&mut cur), col);
            col = 0;
            started_continuation = true;
            if hang > 0 {
                if let Some(bar) = spine {
                    let mut g = String::with_capacity(2);
                    g.push(spine_glyph());
                    cur.push(Span::styled(g, Style::default().fg(bar)));
                    if hang > 1 {
                        cur.push(Span::styled(" ".repeat(hang - 1), Style::default()));
                    }
                } else {
                    cur.push(Span::styled(" ".repeat(hang), Style::default()));
                }
                col = hang;
            }
        }};
    }

    // Flatten to (char, style) so we can wrap at WORD boundaries (a space, or
    // either side of a wide/CJK char) instead of mid-word. A run of narrow,
    // non-space chars is one unbreakable "word"; a space or a wide char is its own
    // unit. An over-wide word (or a CJK run) still hard-breaks char-by-char, so a
    // single long token can never overflow. Per-char style is preserved.
    let mut chars: Vec<(char, Style)> = Vec::new();
    for span in &line.spans {
        let st = span.style;
        for ch in strip_control_chars(span.content.as_ref()).chars() {
            // Normalize tabs to a single space. A tab has unicode-width 0, so the
            // fold would count it as 0 columns — but a terminal expands it to a
            // tab stop when painting, making the row WIDER than the viewport and
            // triggering terminal auto-wrap (which desyncs ratatui's cursor and
            // bleeds/overlaps content — the long-line garble). Folding what we
            // actually paint keeps every row's display width ≤ the inner width.
            let ch = if ch == '\t' { ' ' } else { ch };
            chars.push((ch, st));
        }
    }
    let row_floor = |started: bool| if started { hang } else { 0 };
    let usable_word = w.saturating_sub(hang).max(1);
    let mut i = 0usize;
    while i < chars.len() {
        let (ch, st) = chars[i];
        if ch != ' ' && char_width(ch) == 1 {
            // Gather a word: a run of narrow, non-space chars.
            let start = i;
            while i < chars.len() && chars[i].0 != ' ' && char_width(chars[i].0) == 1 {
                i += 1;
            }
            let word = &chars[start..i];
            let ww = word.len(); // all narrow → 1 col each
            if col + ww <= w {
                emit_run(&mut cur, word);
                col += ww;
            } else if ww <= usable_word && col > row_floor(started_continuation) {
                // The whole word fits on a fresh row — wrap before it (no mid-word).
                flush_row!();
                emit_run(&mut cur, word);
                col += ww;
            } else {
                // Over-wide word — hard-break char by char (the only safe option).
                for &(c2, s2) in word {
                    if col + 1 > w && col > row_floor(started_continuation) {
                        flush_row!();
                    }
                    cur.push(Span::styled(c2.to_string(), s2));
                    col += 1;
                }
            }
        } else if ch == ' ' {
            // A space: drop it when it would lead a freshly-wrapped row, else place.
            if col + 1 > w && col > row_floor(started_continuation) {
                flush_row!();
            } else if !(started_continuation && col == hang) {
                cur.push(Span::styled(" ".to_string(), st));
                col += 1;
            }
            i += 1;
        } else {
            // A wide / CJK char — break before it if the row is full, then place.
            let cw = char_width(ch);
            if col + cw > w && col > row_floor(started_continuation) {
                flush_row!();
            }
            cur.push(Span::styled(ch.to_string(), st));
            col += cw;
            i += 1;
        }
    }
    push_padded(&mut out, cur, col);
    out
}

/// Reduce one painted (decorated) transcript row to the LOGICAL text a drag-copy
/// should yield, plus the leading-gutter display width that was stripped.
///
/// The painted row is `[gutter][content][trailing bg padding]`, where the gutter
/// is the role-spine glyph (`▎`) plus its hang-indent spaces (repainted down
/// every wrapped continuation row), and the trailing padding is the bg-tinted
/// spaces that fill a user bubble out to the full width. Neither is real content,
/// so copying them pollutes the clipboard (the just-shipped selection feature's
/// defect): spurious `▎` / indent prefixes and runs of trailing spaces.
///
/// This returns `(logical, gutter)`:
/// - `logical` = the content with the leading spine-glyph gutter removed and any
///   trailing whitespace trimmed (so a wrapped continuation row and a user-bubble
///   row both copy clean — no `▎`, no leading indent, no trailing-space padding).
/// - `gutter` = the display columns dropped from the front, so the caller can map
///   a screen column to the right logical char index (`screen_to_content`
///   subtracts it) and paint the highlight on the DECORATED line (whose char
///   indices are shifted right by exactly this much).
///
/// The gutter is detected by the spine glyph: a leading `▎` plus the run of
/// spaces immediately after it. A row with no spine glyph has gutter 0 and only
/// its trailing whitespace trimmed. Pure + fail-open.
fn logical_row_and_gutter(line: &Line<'static>) -> (String, usize) {
    let full: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    let spine = spine_glyph();
    let mut chars = full.chars().peekable();
    let mut gutter = 0usize;
    if chars.peek() == Some(&spine) {
        // Drop the spine glyph and the hang-indent spaces that follow it.
        chars.next();
        gutter += char_width(spine);
        while chars.peek() == Some(&' ') {
            chars.next();
            gutter += 1;
        }
    }
    let logical: String = chars.collect();
    // Trim trailing whitespace (the user-bubble bg padding, and any incidental
    // trailing spaces) — never copied.
    let logical = logical.trim_end().to_string();
    (logical, gutter)
}

/// Paint the in-app text-selection highlight onto the already-folded transcript
/// rows. `folded` is the exact `Vec<Line>` about to be rendered (its index is
/// the content-row coordinate); `sel` is the live selection. Only the rows in
/// the normalized range are rebuilt — a single-row selection highlights
/// `[anchor.col, cursor.col)`, a multi-row one highlights the first row from
/// `anchor.col` to its end, every middle row fully, and the last row up to
/// `cursor.col`. Fail-open: an out-of-range row index is skipped, never a panic.
///
/// `gutters[i]` is the leading-gutter width stripped from cached row `i` (see
/// [`logical_row_and_gutter`]); the selection columns are in LOGICAL coordinates,
/// so each is shifted right by the row's gutter to index the decorated line.
fn apply_selection_highlight(
    folded: &mut [Line<'static>],
    sel: &crate::selection::Selection,
    gutters: &[usize],
) {
    let g = |row: usize| gutters.get(row).copied().unwrap_or(0);
    let shift = |row: usize, col: usize| col.saturating_add(g(row));
    let ((sr, sc), (er, ec)) = sel.normalized();
    let sc = shift(sr, sc);
    let ec = shift(er, ec);
    apply_selection_highlight_cols(folded, sr, sc, er, ec);
}

/// The column-space-agnostic core of [`apply_selection_highlight`]: highlights
/// `folded[sr][sc..]` … `folded[er][..ec]` where the columns are already in the
/// DECORATED line's char-index space.
fn apply_selection_highlight_cols(
    folded: &mut [Line<'static>],
    sr: usize,
    sc: usize,
    er: usize,
    ec: usize,
) {
    if sr == er {
        if let Some(row) = folded.get_mut(sr) {
            *row = highlight_row(row, sc, ec);
        }
        return;
    }
    // First row: from the anchor col to end of the row.
    if let Some(row) = folded.get_mut(sr) {
        *row = highlight_row(row, sc, usize::MAX);
    }
    // Full middle rows.
    for r in (sr + 1)..er {
        if let Some(row) = folded.get_mut(r) {
            *row = highlight_row(row, 0, usize::MAX);
        }
    }
    // Last row: start of the row up to the cursor col.
    if let Some(row) = folded.get_mut(er) {
        *row = highlight_row(row, 0, ec);
    }
}

/// Rebuild one `Line`, applying the selection background to the chars whose
/// (line-wide) char index falls in `[from, to)`. Char indices are counted
/// across the concatenation of the line's span contents — the same coordinate
/// space the cached plain-text row uses — so a multi-byte / CJK glyph is split
/// on a char boundary, never a byte boundary. Each original span is sliced into
/// up to three pieces (before / selected / after); the selected piece keeps the
/// span's fg + modifiers and gains the selection bg. Fail-open: a `to <= from`
/// or out-of-range range simply highlights nothing.
fn highlight_row(line: &Line<'static>, from: usize, to: usize) -> Line<'static> {
    let sel_bg = theme::SELECTION_BG();
    let mut out: Vec<Span<'static>> = Vec::with_capacity(line.spans.len());
    let mut idx = 0usize; // running char index across the whole line
    for span in &line.spans {
        // Group this span's chars into selected / unselected runs, preserving
        // the original style and adding the selection bg to the selected run.
        let base = span.style;
        let mut buf = String::new();
        let mut buf_selected = false;
        let flush = |buf: &mut String, selected: bool, out: &mut Vec<Span<'static>>| {
            if buf.is_empty() {
                return;
            }
            let style = if selected { base.bg(sel_bg) } else { base };
            out.push(Span::styled(std::mem::take(buf), style));
        };
        for ch in span.content.chars() {
            let in_sel = idx >= from && idx < to;
            if !buf.is_empty() && in_sel != buf_selected {
                flush(&mut buf, buf_selected, &mut out);
            }
            buf.push(ch);
            buf_selected = in_sel;
            idx += 1;
        }
        flush(&mut buf, buf_selected, &mut out);
    }
    Line::from(out)
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
    // Publish the input text width so the Up/Down key handlers can move the caret
    // by one wrapped visual row inside a multi-line prompt (CC parity).
    app.input_text_cols.set(text_width);
    // Wrap the real input so the box height + underline track the content.
    let all_rows = wrap_input_rows(&app.input, text_width);
    let total_rows = u16::try_from(all_rows.len()).unwrap_or(INPUT_MAX_ROWS);
    let visible_rows = total_rows.clamp(1, INPUT_MAX_ROWS);
    // Caret's absolute (row, col) in the wrapped layout — computed BEFORE the
    // scroll so the scroll can keep it on screen.
    let (cursor_row_abs, cursor_col) = caret_in_wrapped(&app.input, app.input_cursor, text_width);
    // Scroll so the caret's row stays visible. The box only ever shows
    // `visible_rows` of the `total_rows`; when the user edits ABOVE the bottom of
    // a tall (>6-row) input, the old "always scroll to bottom" pinned the caret
    // to row 0 and pushed the text it was editing off-screen. Anchor the window
    // on the caret instead: keep it within the last visible row, clamped so we
    // never scroll past the content (top or bottom).
    let max_scroll = total_rows.saturating_sub(visible_rows);
    let scroll = cursor_row_abs
        .saturating_sub(visible_rows.saturating_sub(1))
        .min(max_scroll);
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
    } else if app.thinking || app.tool_in_progress {
        // ACTIVELY working a turn (a chat reply streaming, or a tool running) —
        // show the interruptible "running" hint. This MUST win over `aborted`
        // below: a chat turn doesn't fire `PipelineStarted` (only a build does),
        // so a stale `aborted` from a PRIOR block would otherwise persist and the
        // placeholder wrongly read "本轮已中止" while the base was replying normally
        // (user-reported). A live turn is, by definition, not aborted.
        umadev_i18n::t(app.lang, "input.running")
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
        // Empty input: the terminal cursor sits at column `prefix_w` (where the first
        // typed char will land). The placeholder used to start at that SAME column, so
        // the cursor block overlapped its first character. Shift the placeholder one
        // column right (an extra leading space) so the cursor gets its own cell and the
        // hint reads cleanly beside it.
        vec![Line::from(vec![
            Span::styled(mode_icon, Style::default().fg(mode_color)),
            Span::raw("  "),
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

    // Cursor: place it at the wrapped `(cursor_row_abs, cursor_col)` computed
    // above (same folding as the drawn rows, with the wrap-boundary push so a
    // caret at a full row's edge wraps to col 0 of the next row instead of
    // overrunning the right border). The column already counts wide glyphs (CJK
    // = 2) via the Unicode width table. The vertical position subtracts `scroll`
    // so it tracks the visible window.
    let input_area = prompt_chunks[0];
    let cursor_row_vis = cursor_row_abs.saturating_sub(scroll);
    if app.overlay.is_none() && !app.show_help {
        frame.set_cursor_position((
            input_area
                .x
                .saturating_add(u16::try_from(prefix_w).unwrap_or(3))
                .saturating_add(cursor_col),
            input_area.y.saturating_add(cursor_row_vis),
        ));
    }

    // Live state (ready / running heartbeat / aborted / complete) — pinned to the
    // bottom-RIGHT of the meta row below instead of burning its own footer line.
    // `None` mid-turn (the activity indicator above the input already proves
    // motion). Reused by both the gate-branch meta row and the normal one.
    let status = status_text_and_color(app);
    // Context line beneath the input box: model / backend / hints.
    let backend = app.backend.as_deref().unwrap_or("offline");
    let hint: String = if !app.mention_matches().is_empty() {
        umadev_i18n::t(app.lang, "tui.hint.mention").into()
    } else if app.input.starts_with('/') {
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
            status,
        );
    } else if app.input.contains('\n') {
        umadev_i18n::t(app.lang, "tui.hint.multiline").into()
    } else if !app.input.is_empty() {
        umadev_i18n::t(app.lang, "tui.hint.typed").into()
    } else if app.thinking || app.tool_in_progress {
        // ACTIVELY working (a reply streaming / a tool running) — show the
        // interruptible running hint. Must win over `finished`/`aborted`: a stale
        // terminal flag from a prior block would otherwise paint "[aborted] 本轮已停止"
        // under a build that is plainly still reading files and thinking
        // (user-reported, with a screenshot). A live turn is not aborted.
        umadev_i18n::t(app.lang, "tui.hint.running").into()
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
    meta_row(frame, prompt_chunks[1], border_color, &parts, status);
}

/// Render the meta row: `parts` left-aligned, with the optional live `status`
/// pinned to the bottom-RIGHT of the SAME row (reclaiming the footer line the
/// old standalone status row used to burn just to print one word). The left
/// meta's own trailing space provides the gap; the status is clipped by display
/// width (CJK-safe) to whatever room is left, and on a terminal too narrow to
/// fit even a sliver of it the status is dropped entirely (fail-open) so the
/// meta info always wins — it never wraps or overruns the row.
fn meta_row(
    frame: &mut Frame,
    area: Rect,
    _bar: Color,
    parts: &[(String, Color)],
    status: Option<(String, Color)>,
) {
    let mut spans: Vec<Span<'static>> = vec![Span::raw(" ")];
    for (text, color) in parts {
        spans.push(Span::styled(text.clone(), Style::default().fg(*color)));
        spans.push(Span::raw(" "));
    }
    let left_width: usize = spans.iter().map(|s| disp_width(s.content.as_ref())).sum();
    let total = usize::from(area.width);
    if let Some((text, color)) = status {
        // Room left after the meta (whose last span is already a space → the gap).
        // Need at least 2 cols to be worth showing; otherwise the meta info wins.
        let room = total.saturating_sub(left_width);
        if room >= 2 {
            let shown = if disp_width(&text) > room {
                truncate_to_width(&text, room)
            } else {
                text
            };
            let status_w = u16::try_from(disp_width(&shown)).unwrap_or(0);
            let split = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(0), Constraint::Length(status_w)])
                .split(area);
            frame.render_widget(Paragraph::new(Line::from(spans)), split[0]);
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(shown, Style::default().fg(color))))
                    .alignment(Alignment::Right),
                split[1],
            );
            return;
        }
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// A short popover above the input box that lists matching slash commands.
fn render_palette_popover(
    frame: &mut Frame,
    input_area: Rect,
    app: &App,
    matches: &[PaletteEntry],
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
        .map(|(i, entry)| {
            let idx = win_start + i;
            let arrow = if idx == selected { "›" } else { " " };
            let row_style = if idx == selected {
                Style::default()
                    .fg(theme::PRIMARY())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme::TEXT())
            };
            // `/verb [arg-hint]` then the localized description. The arg hint is
            // dim ghost text right after the verb (autocomplete convention) so a
            // command's expected argument is visible before you commit to it.
            let mut spans = vec![
                Span::styled(format!(" {arrow} "), row_style),
                Span::styled(format!("/{}", entry.verb), row_style),
            ];
            if let Some(hint) = entry.arg_hint {
                spans.push(Span::styled(
                    format!(" {hint}"),
                    Style::default()
                        .fg(theme::TEXT_MUTED())
                        .add_modifier(Modifier::DIM),
                ));
            }
            spans.push(Span::styled(
                format!("  {}", entry.desc),
                Style::default().fg(theme::TEXT_MUTED()),
            ));
            ListItem::new(Line::from(spans))
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

/// A popover above the input box listing the repo files matching the
/// `@`-mention partial under the cursor — the `@`-file typeahead, a sibling of
/// [`render_palette_popover`]. Windowed around the selection with the same
/// `N/M · ↑↓` position indicator, and it reuses the palette popover's theme so
/// the two read as one family. Each row is the candidate's repo-relative path,
/// shown with its `@` prefix (what gets inserted).
fn render_mention_popover(frame: &mut Frame, input_area: Rect, app: &App, matches: &[String]) {
    let total = matches.len();
    if total == 0 {
        return;
    }
    // Window the list around the selection so ↑↓ can reach every candidate (the
    // same upward-floating, ≤12-row treatment the slash palette uses).
    let avail_above = usize::from(input_area.y).saturating_sub(2);
    let max_rows = total.min(avail_above).clamp(1, 12);
    let selected = app.mention_selected.min(total - 1);
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
    // CLAMP to the frame (hand-built Rect): on a short/narrow terminal an
    // unclamped Rect would make `Clear` index out of bounds and panic the TUI.
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
        .map(|(i, path)| {
            let idx = win_start + i;
            let arrow = if idx == selected { "›" } else { " " };
            let row_style = if idx == selected {
                Style::default()
                    .fg(theme::PRIMARY())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme::TEXT())
            };
            let spans = vec![
                Span::styled(format!(" {arrow} "), row_style),
                Span::styled(format!("@{path}"), row_style),
            ];
            ListItem::new(Line::from(spans))
        })
        .collect();
    // Title carries the position + total (e.g. "files · 3/altogether") so the
    // user knows the list is windowed.
    let title = format!(
        " {} · {}/{} · ↑↓ ",
        umadev_i18n::t(app.lang, "tui.mention.title"),
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

/// The live state shown at the bottom-RIGHT of the prompt's meta row — `就绪`
/// (ready), the running heartbeat (`<phase> · still working (mm:ss) · ESC`),
/// `[aborted]`, or `[ok] complete` — with the colour it should carry. A stall
/// paints it red (honest "about to hang"); otherwise the normal info colour.
/// Returns `None` mid-turn (`thinking`), where the live activity indicator above
/// the input already proves motion, so the meta row stays uncluttered.
///
/// Relocated here from the old standalone status ROW, which burned a whole footer
/// line just to print one word; `meta_row` now right-aligns this onto the same
/// line as the backend / trust-mode / hint chrome.
fn status_text_and_color(app: &App) -> Option<(String, Color)> {
    if app.thinking {
        // The activity indicator above the input speaks for the live turn; the
        // bottom-right stays empty so there's no duplicate / lingering tool name.
        return None;
    }
    let text = if app.aborted {
        // Dedicated terminal branch — an aborted round reads as `[aborted]`
        // DIRECTLY, instead of leaning on `app.status` carrying the right text.
        // That coupling was fragile: a future `refresh_status` change could
        // silently make a wedged run show stale phase progress. Checked before
        // `run_started` because `mark_block_aborted` leaves `run_started` set.
        format!("[aborted] {}", umadev_i18n::t(app.lang, "status.aborted"))
    } else if app.run_started {
        // While a slow phase's heartbeat is live, show its in-place "still
        // working (mm:ss)" reassurance (overwritten each beat) instead of letting
        // it pile up in the transcript. The spinner + phase timer in `app.status`
        // still prove motion; this just makes the wait explicit and reminds the
        // user the long phase is interruptible (ESC).
        let esc_hint = if app.interrupt_armed() {
            umadev_i18n::t(app.lang, "status.esc_confirm")
        } else {
            umadev_i18n::t(app.lang, "status.esc_cancel")
        };
        match &app.transient_status {
            Some(beat) => format!("{} · {beat} · {esc_hint}", app.status),
            None if app.interrupt_armed() => format!("{} · {esc_hint}", app.status),
            None => app.status.clone(),
        }
    } else if app.finished {
        umadev_i18n::t(app.lang, "tui.status.complete").to_string()
    } else {
        umadev_i18n::t(app.lang, "status.ready").to_string()
    };
    // Stall → red (honest "about to hang"); otherwise the normal info colour.
    let color = if app.is_stalled() {
        theme::ERROR()
    } else {
        theme::INFO()
    };
    Some((text, color))
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
            // GENERATED from the one command registry (`App::COMMANDS`), grouped
            // on `CmdGroup` in declared order. The palette + dispatcher read the
            // SAME table, and a parity test locks the registry against the
            // dispatcher — so help can no longer drift (the old hand-curated rows
            // had already lost `/model`, `/goal`, a dozen others, and every
            // alias). Each row is `/verb [arg-hint]` + the shared localized desc.
            for group in CmdGroup::ALL {
                let rows: Vec<(String, &str)> = App::COMMANDS
                    .iter()
                    .filter(|c| !c.hidden && c.group == *group)
                    .map(|c| {
                        let key = match c.arg_hint {
                            Some(hint) => format!("/{} {hint}", c.name),
                            None => format!("/{}", c.name),
                        };
                        (key, umadev_i18n::t(lang, c.desc_key))
                    })
                    .collect();
                if rows.is_empty() {
                    continue;
                }
                let row_refs: Vec<(&str, &str)> =
                    rows.iter().map(|(k, v)| (k.as_str(), *v)).collect();
                push_help_group(
                    &mut items,
                    umadev_i18n::t(lang, group.title_key()),
                    &row_refs,
                );
            }
            // Keyboard shortcuts are KEYS, not slash commands, so they stay
            // hand-listed (the registry only owns `/`-verbs). Ctrl+O is the
            // global reveal-all; Ctrl+R folds just the latest row.
            push_help_group(
                &mut items,
                umadev_i18n::t(lang, "tui.help.group.editing"),
                &[
                    ("Enter", umadev_i18n::t(lang, "tui.help.pipe.enter")),
                    ("Shift+Enter", umadev_i18n::t(lang, "tui.help.edit.newline")),
                    ("↑ / ↓", umadev_i18n::t(lang, "tui.help.edit.recall")),
                    ("Tab", umadev_i18n::t(lang, "tui.help.edit.autocomplete")),
                    ("Ctrl+O", umadev_i18n::t(lang, "tui.help.key.expand_all")),
                    ("Ctrl+R", umadev_i18n::t(lang, "tui.help.edit.expand")),
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

    /// Line-for-line span+style equality, used to lock the P5a invariant.
    fn lines_eq(a: &[Line<'static>], b: &[Line<'static>]) -> bool {
        a.len() == b.len()
            && a.iter().zip(b.iter()).all(|(x, y)| {
                let xs: Vec<(&str, _)> = x
                    .spans
                    .iter()
                    .map(|s| (s.content.as_ref(), s.style))
                    .collect();
                let ys: Vec<(&str, _)> = y
                    .spans
                    .iter()
                    .map(|s| (s.content.as_ref(), s.style))
                    .collect();
                xs == ys
            })
    }

    #[test]
    fn stream_incremental_equals_whole_render() {
        // P5a HARD INVARIANT: feeding a body to the stable-prefix cache one delta
        // at a time and composing `cached-prefix ++ [blank] ++ tail` must equal a
        // one-shot whole-body `markdown_to_lines` at EVERY intermediate length —
        // otherwise the streaming view would diverge from the settled view.
        let bodies = [
            "# Heading\n\nFirst paragraph with **bold** and `code`.\n\n\
             Second paragraph.\n\n- bullet one\n- bullet two\n\n\
             ```rust\nfn main() {\n    println!(\"hi\");\n}\n```\n\n\
             Closing line of prose.",
            "Para A.\n\nPara B.\n\nPara C.\n\nPara D.",
            "intro text\n\n```\nplain code\nmore code\n```\n\nafter\n\n> a quote\n\nend",
            "## Title\n\n1. one\n2. two\n3. three\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\ndone",
            "中文段落一。\n\n中文段落二,带 **加粗**。\n\n- 列表项一\n- 列表项二\n\n结尾。",
        ];
        for body in bodies {
            // Compare against the SAME base color the production compose uses
            // (`theme::TEXT()`), so a plaintext-fallback path can't diverge purely
            // on the unused base color rather than a real structural difference.
            let whole = markdown_to_lines(body, theme::TEXT());
            let mut cache = StreamMarkdownCache::default();
            // Grow byte-by-byte at char boundaries — the worst case for the
            // boundary finder (every possible split is exercised).
            let mut prev: Vec<Line<'static>> = Vec::new();
            for end in 1..=body.len() {
                if !body.is_char_boundary(end) {
                    continue;
                }
                let partial = &body[..end];
                let inc = stream_markdown_lines(&mut cache, partial);
                let whole_partial = markdown_to_lines(partial, theme::TEXT());
                assert!(
                    lines_eq(&inc, &whole_partial),
                    "P5a divergence at len {end} of body {body:?}\n  inc={} whole={}",
                    inc.len(),
                    whole_partial.len()
                );
                prev = inc;
            }
            // Final full-body length matches the one-shot whole render.
            assert!(
                lines_eq(&prev, &whole),
                "P5a final mismatch for body {body:?}"
            );
        }
    }

    #[test]
    fn stream_cache_resets_on_shrink_failopen() {
        // Fail-open: if the body shrinks (e.g. a segment rollover starts a fresh,
        // smaller body), the cache must discard its stale prefix and still render
        // the new body correctly (== whole render), never reuse the old prefix.
        let mut cache = StreamMarkdownCache::default();
        let big = "Block one.\n\nBlock two.\n\nBlock three.\n\nBlock four.";
        let _ = stream_markdown_lines(&mut cache, big);
        assert!(
            cache.stable_offset > 0,
            "a multi-block body builds a prefix"
        );
        // Now a SHORTER, different body (rollover): must reset + render correctly.
        let small = "Totally new.\n\nDifferent body.";
        let inc = stream_markdown_lines(&mut cache, small);
        let whole = markdown_to_lines(small, theme::TEXT());
        assert!(
            lines_eq(&inc, &whole),
            "shrink must render the new body whole"
        );
    }

    #[test]
    fn stream_cache_no_split_inside_code_fence() {
        // The boundary finder must NEVER place a stable split inside an OPEN code
        // fence — that would cache the opening ``` without its close and scramble
        // the highlighting. With a body whose only `\n\n` sits inside an unclosed
        // fence, there is no eligible boundary, so the offset stays 0 (whole-body
        // render, fence intact).
        let open = "text\n\n```rust\nlet x = 1;\n\nlet y = 2;\n";
        let off = last_stable_md_boundary(open, 0);
        // The `\n\n` after `text` (offset 6) is the only fence-balanced boundary;
        // it's a SINGLE boundary, so the second-to-last rule keeps the offset at 0
        // (the last block stays in the tail) — and the inner blank, being inside
        // the open fence, is never eligible.
        assert_eq!(off, 0, "a lone fence-balanced boundary keeps offset 0");
        // Whichever offset is chosen, no split ever lands inside the open fence:
        // every candidate offset leaves a fence-balanced prefix.
        assert!(
            !crate::app::has_open_code_fence(&open[..off]),
            "the chosen prefix is never inside an open fence"
        );
        // The incremental render matches the whole render regardless.
        let mut cache = StreamMarkdownCache::default();
        let inc = stream_markdown_lines(&mut cache, open);
        let whole = markdown_to_lines(open, theme::TEXT());
        assert!(lines_eq(&inc, &whole));

        // With the fence CLOSED and prose after it, there are now two boundaries
        // (after `text`, after the closed fence) — the second-to-last (after
        // `text`) is chosen, and it is fence-balanced (the closed fence is in the
        // tail with its prose).
        let closed = "text\n\n```rust\nlet x = 1;\n```\n\nmore prose here\n\ntail";
        let off2 = last_stable_md_boundary(closed, 0);
        assert!(
            off2 > 0,
            "a closed fence + following blocks yields a boundary"
        );
        assert!(
            !crate::app::has_open_code_fence(&closed[..off2]),
            "the prefix is fence-balanced"
        );
    }

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

    // Concatenate every span's text across all lines, for content assertions.
    fn md_text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    // Does any span on any line carry the given semantic-role color?
    fn has_role(lines: &[Line<'static>], role: theme::SynRole) -> bool {
        let want = theme::syn_color(role);
        lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.style.fg == Some(want))
    }

    #[test]
    fn markdown_inline_bold_emits_bold_span() {
        // `**bold**` must produce a span carrying the BOLD modifier — not the
        // literal asterisks dumped as text.
        let lines = markdown_to_lines("plain **bold** tail", Color::White);
        let txt = md_text(&lines);
        assert!(txt.contains("bold"), "bold text preserved: {txt}");
        assert!(!txt.contains("**"), "asterisks consumed, not dumped: {txt}");
        let bold = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.content.contains("bold") && s.style.add_modifier.contains(Modifier::BOLD));
        assert!(bold, "the 'bold' run carries Modifier::BOLD");
    }

    #[test]
    fn markdown_inline_italic_and_code() {
        let lines = markdown_to_lines("an *em* and `code` here", Color::White);
        let italic = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.content.contains("em") && s.style.add_modifier.contains(Modifier::ITALIC));
        assert!(italic, "italic run carries Modifier::ITALIC");
        // Inline code recolors to the inline-code role.
        let code = lines.iter().flat_map(|l| l.spans.iter()).any(|s| {
            s.content.contains("code")
                && s.style.fg == Some(theme::syn_color(theme::SynRole::InlineCode))
        });
        assert!(code, "inline code uses the inline-code role color");
    }

    #[test]
    fn markdown_heading_is_bold_and_role_colored() {
        let lines = markdown_to_lines("# Title\n\nbody", Color::White);
        let heading = lines.iter().flat_map(|l| l.spans.iter()).any(|s| {
            s.content.contains("Title")
                && s.style.fg == Some(theme::syn_color(theme::SynRole::Heading))
                && s.style.add_modifier.contains(Modifier::BOLD)
        });
        assert!(heading, "H1 'Title' is heading-colored + bold: {lines:?}");
    }

    #[test]
    fn markdown_link_surfaces_the_destination_url() {
        // A terminal can't make text clickable, so the URL must be shown when the
        // visible text differs from the target.
        let lines = markdown_to_lines("see [the docs](https://example.com/x)", Color::White);
        let txt = md_text(&lines);
        assert!(txt.contains("the docs"), "link text kept: {txt}");
        assert!(
            txt.contains("(https://example.com/x)"),
            "destination URL surfaced: {txt}"
        );
        // A bare link (text == url) is NOT duplicated.
        let bare = md_text(&markdown_to_lines("<https://example.com>", Color::White));
        assert_eq!(
            bare.matches("https://example.com").count(),
            1,
            "bare URL shown once, not duplicated: {bare}"
        );
    }

    #[test]
    fn markdown_task_list_renders_checkboxes() {
        let md = "- [x] done\n- [ ] todo";
        let txt = md_text(&markdown_to_lines(md, Color::White));
        assert!(
            txt.contains("\u{2611}"),
            "checked box for a done item: {txt}"
        );
        assert!(txt.contains("\u{2610}"), "empty box for a todo item: {txt}");
        // The checkbox replaces the bullet — no stray '•' on a task item.
        assert!(!txt.contains('\u{2022}'), "no bullet on task items: {txt}");
    }

    #[test]
    fn markdown_image_surfaces_its_href() {
        let txt = md_text(&markdown_to_lines(
            "![logo](https://x.test/a.png)",
            Color::White,
        ));
        assert!(
            txt.contains("https://x.test/a.png"),
            "image href surfaced (not dropped): {txt}"
        );
    }

    #[test]
    fn wide_table_shrinks_to_the_width_budget() {
        // A table wider than the viewport must shrink + truncate, not overflow and
        // char-fold into a scrambled grid.
        set_table_width_budget(40);
        let md = "| Name | Description |\n|---|---|\n\
                  | alpha | a very long description that would overflow the narrow budget by far |";
        let lines = markdown_to_lines(md, Color::White);
        set_table_width_budget(0); // reset so other tests render naturally
        for l in &lines {
            let w: usize = l.spans.iter().map(|s| disp_width(s.content.as_ref())).sum();
            assert!(w <= 40, "every table row fits the 40-col budget: width={w}");
        }
        assert!(
            md_text(&lines).contains('\u{2026}'),
            "the over-long cell was truncated with an ellipsis"
        );
    }

    #[test]
    fn many_column_table_on_a_narrow_budget_goes_vertical() {
        // 4 wide columns at a tight budget can't form a usable grid → stacked
        // `header: value` records instead of a wall of `…`.
        set_table_width_budget(24);
        let md = "| Name | Status | Owner | Notes |\n|---|---|---|---|\n\
                  | alpha | active | bob | some notes here |";
        let txt = md_text(&markdown_to_lines(md, Color::White));
        set_table_width_budget(0);
        assert!(
            txt.contains("Name: alpha"),
            "vertical header:value record: {txt}"
        );
        assert!(
            txt.contains("Owner: bob"),
            "every column becomes a key:value line: {txt}"
        );
        assert!(
            !txt.contains('\u{2502}'),
            "no grid │ separators in vertical mode: {txt}"
        );
    }

    #[test]
    fn narrow_table_under_budget_is_not_truncated() {
        set_table_width_budget(80);
        let lines = markdown_to_lines("| a | b |\n|---|---|\n| 1 | 2 |", Color::White);
        set_table_width_budget(0);
        assert!(
            !md_text(&lines).contains('\u{2026}'),
            "a small table within budget keeps its full cells"
        );
    }

    #[test]
    fn markdown_nested_list_markers_by_depth() {
        // Marker style is COMPUTED from nesting depth (0/1→arabic, 2→alpha,
        // 3→roman), never taken from the source text. The input below opens four
        // ordered-list levels so every style band is exercised; each `1.` in the
        // source must re-render with the marker for its DEPTH, not its literal.
        let md = "\
1. lvl0
   1. lvl1
      1. lvl2
         1. lvl3
- bullet";
        let lines = markdown_to_lines(md, Color::White);
        let txt = md_text(&lines);
        assert!(txt.contains("1. lvl0"), "depth-0 arabic: {txt}");
        assert!(txt.contains("1. lvl1"), "depth-1 arabic: {txt}");
        // The doubly-nested ordered item uses a lowercase-alpha marker (the
        // source `1.` becomes `a.`).
        assert!(txt.contains("a. lvl2"), "depth-2 alpha marker: {txt}");
        // The triply-nested uses lowercase roman (`1.` becomes `i.`).
        assert!(txt.contains("i. lvl3"), "depth-3 roman marker: {txt}");
        // The unordered item uses a bullet.
        assert!(txt.contains("• bullet"), "unordered bullet: {txt}");
    }

    #[test]
    fn ordered_marker_depth_styles() {
        assert_eq!(ordered_marker(0, 1), "1.");
        assert_eq!(ordered_marker(1, 3), "3.");
        assert_eq!(ordered_marker(2, 1), "a.");
        assert_eq!(ordered_marker(2, 27), "aa.");
        assert_eq!(ordered_marker(3, 4), "iv.");
        assert_eq!(ordered_marker(3, 9), "ix.");
    }

    #[test]
    fn markdown_table_columns_align_cjk_safe() {
        // A table with a CJK header column and ASCII values: every body row must
        // pad to the SAME visible column width (CJK counted as 2), so the column
        // separators line up. We assert by the visible width up to the first
        // separator being equal across rows.
        let md = "\
| 名称 | Qty |
| --- | --- |
| 苹果 | 3 |
| x | 100 |";
        let lines = markdown_to_lines(md, Color::White);
        let txt = md_text(&lines);
        assert!(txt.contains("名称"), "header rendered: {txt}");
        assert!(txt.contains("苹果"), "cjk cell rendered: {txt}");
        assert!(txt.contains("100"), "value cell rendered: {txt}");
        assert!(txt.contains('│'), "column separator present: {txt}");
        // The visible width from line start to the first column separator must be
        // identical on every table row (header + 2 body rows). This is the CJK
        // alignment guarantee: byte alignment would desync 苹果 (6 bytes) vs x.
        let sep_cols: Vec<usize> = txt
            .lines()
            .filter(|l| l.contains('│'))
            .map(|l| {
                let upto: String = l.chars().take_while(|&c| c != '│').collect();
                disp_width(&upto)
            })
            .collect();
        assert!(sep_cols.len() >= 3, "header + 2 body rows have a separator");
        assert!(
            sep_cols.windows(2).all(|w| w[0] == w[1]),
            "first-column separator aligns by display width across rows: {sep_cols:?}\n{txt}"
        );
    }

    #[test]
    fn markdown_code_block_highlights_by_language() {
        // A ```rust fence: the `fn` keyword gets the keyword role, the string
        // literal gets the string role, the language label rides the open rule.
        let md = "```rust\nfn main() {\n    let s = \"hi\";\n}\n```";
        let lines = markdown_to_lines(md, Color::White);
        let txt = md_text(&lines);
        assert!(txt.contains("rust"), "language label on the rule: {txt}");
        assert!(txt.contains("fn main"), "code preserved: {txt}");
        assert!(has_role(&lines, theme::SynRole::Keyword), "keyword colored");
        assert!(
            has_role(&lines, theme::SynRole::StringLit),
            "string literal colored"
        );
        // The fenced content carries the code background tint.
        let has_bg = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.style.bg == Some(theme::CODE_BG()));
        assert!(has_bg, "code block has a distinct background tint");
    }

    #[test]
    fn markdown_unknown_language_falls_back_to_plaintext_highlight() {
        // An unknown fence language must not panic and still highlight strings.
        let md = "```wat\nthing = \"value\" 42\n```";
        let lines = markdown_to_lines(md, Color::White);
        let txt = md_text(&lines);
        assert!(txt.contains("value"), "content preserved: {txt}");
        assert!(
            has_role(&lines, theme::SynRole::StringLit),
            "string still highlighted in unknown lang"
        );
        assert!(
            has_role(&lines, theme::SynRole::Number),
            "number highlighted"
        );
    }

    #[test]
    fn markdown_blockquote_has_bar_and_italic() {
        let lines = markdown_to_lines("> quoted wisdom", Color::White);
        let txt = md_text(&lines);
        assert!(txt.contains('│'), "blockquote bar present: {txt}");
        assert!(txt.contains("quoted wisdom"), "quote content: {txt}");
        let italic = lines.iter().flat_map(|l| l.spans.iter()).any(|s| {
            s.content.contains("quoted") && s.style.add_modifier.contains(Modifier::ITALIC)
        });
        assert!(italic, "blockquote text is italic");
    }

    #[test]
    fn markdown_fail_open_never_panics_on_adversarial_input() {
        // A pile of half-open constructs must NEVER panic; fail-open returns
        // content (either parsed or plain), never empty for non-blank input.
        let nasties = [
            "```\nunterminated fence with 中文 and `inline",
            "| broken | table\n| --- |",
            "> > > deeply\n>nested **bold *mixed `code",
            "###### h6 ####### h7-ish",
            "1.\n2.\n   - \n",
            "\u{0}\u{1}\u{7} control bytes 中文 mixed",
        ];
        for n in &nasties {
            let lines = markdown_to_lines(n, Color::White);
            assert!(
                !lines.is_empty(),
                "non-empty input renders something: {n:?}"
            );
        }
        // Truly empty input → empty output (no spurious blank line).
        assert!(markdown_to_lines("", Color::White).is_empty());
        assert!(markdown_to_lines("   \n  ", Color::White).is_empty());
    }

    #[test]
    fn has_open_code_fence_detects_unclosed() {
        use crate::app::has_open_code_fence;
        assert!(has_open_code_fence("text\n```rust\nfn x"));
        assert!(!has_open_code_fence("text\n```rust\nfn x\n```\nmore"));
        assert!(!has_open_code_fence("no fence at all"));
        // Tilde fences count too.
        assert!(has_open_code_fence("~~~\nopen"));
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
        let mut app = App::new(
            "demo",
            UserConfig {
                backend: backend.map(str::to_string),
                model: None,
                ..Default::default()
            },
            std::path::PathBuf::from("/tmp/sd-test-config.toml"),
            std::path::PathBuf::from("/tmp/sd-test-workspace"),
        );
        // P5d: deterministic spinner cadence in render tests (see fresh_app).
        app.animations = true;
        app
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

    #[test]
    fn picker_shows_honest_three_state_login_marks() {
        // Gap G10: a not-logged-in base must show its login command, not a green
        // "ready". Drive a not-logged-in probe through the engine, then render.
        let mut app = app_with(None);
        let s = crate::app::PROBE_AUTH_SENTINEL;
        let packed = format!(
            "{s}auth=not_logged_in|login=claude auth login|install=npm i -g claude{s}claude 1.6.0",
        );
        app.apply_engine(umadev_agent::EngineEvent::BackendProbed {
            backend_id: "claude-code".into(),
            ready: false,
            detail: packed,
        });
        app.goto_picker_step(crate::app::PickerStep::BaseCli);
        let out = render_to_string(&app);
        // The login command is surfaced ON the picker row.
        assert!(out.contains("claude auth login"), "login cmd on row: {out}");
        // The amber half-circle (◐) marks "installed · not logged in".
        assert!(out.contains('\u{25D0}'), "half-circle mark rendered");
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
    fn newest_row_not_clipped_when_scroll_hint_steals_the_top_row() {
        let mut app = app_with(Some("offline"));
        // Plenty of rows → the transcript overflows a short viewport → the
        // scroll-hint title is shown, and that title row is stolen off the top by
        // `Block::inner`. Pinned to the bottom (the default), the NEWEST row must
        // still be on screen — the old code derived `hidden_above` from the FULL
        // height, so the title row pushed the newest (streaming) line off the
        // bottom. This locks the viewport-minus-one fix.
        for i in 0..40 {
            app.apply_engine(umadev_agent::EngineEvent::Note(format!("filler-line-{i}")));
        }
        app.apply_engine(umadev_agent::EngineEvent::Note("NEWESTMARKERROW".into()));
        let out = render_chat_to_string(&app, 80, 12);
        let suffix = umadev_i18n::t(app.lang, "tui.scroll.above")
            .rsplit("{}")
            .next()
            .unwrap()
            .trim();
        assert!(out.contains(suffix), "scroll hint should be shown: {out}");
        assert!(
            out.contains("NEWESTMARKERROW"),
            "newest row was clipped off the bottom by the stolen title row: {out}"
        );
    }

    // Render the chat at a generous size and flatten the buffer to one string.
    fn render_chat_to_string(app: &App, w: u16, h: u16) -> String {
        let backend = TestBackend::new(w, h);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| crate::ui::render(f, app)).unwrap();
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

    #[test]
    fn live_plan_panel_renders_checklist_with_ticks() {
        let mut app = app_with(Some("offline"));
        app.apply_engine(umadev_agent::EngineEvent::PlanPosted {
            steps: vec![
                "s1 · scaffold (frontend)".into(),
                "s2 · login route (backend)".into(),
            ],
            done: 0,
            total: 2,
        });
        app.apply_engine(umadev_agent::EngineEvent::PlanStepStatus {
            id: "s1".into(),
            title: "scaffold".into(),
            status: "done".into(),
        });
        let out = render_chat_to_string(&app, 100, 30);
        // The panel header + step titles + the done check render.
        assert!(out.contains("scaffold"), "step title shown: {out}");
        assert!(out.contains("login route"), "second step shown");
        // The done glyph is the check codepoint (no emoji).
        assert!(out.contains('\u{2713}'), "done tick rendered");
    }

    #[test]
    fn team_review_panel_renders_seat_verdicts() {
        let mut app = app_with(Some("offline"));
        app.apply_engine(umadev_agent::EngineEvent::CriticVerdict {
            seat: "architect".into(),
            accepts: true,
            blocking: vec![],
            advisory: vec![],
        });
        app.apply_engine(umadev_agent::EngineEvent::CriticVerdict {
            seat: "qa".into(),
            accepts: false,
            blocking: vec!["no tests".into()],
            advisory: vec![],
        });
        let out = render_chat_to_string(&app, 100, 30);
        assert!(out.contains("[architect]"), "accepting seat shown: {out}");
        assert!(out.contains("[qa]"), "blocking seat shown");
        assert!(out.contains("no tests"), "first must-fix inlined");
    }

    #[test]
    fn plan_panel_never_crushes_transcript_on_short_terminal() {
        // A many-step plan on a SHORT terminal must NOT eat the transcript / push
        // the prompt off-screen — the panel yields rows back below the headroom.
        let mut app = app_with(Some("offline"));
        let steps: Vec<String> = (0..20)
            .map(|i| format!("s{i} · step number {i} (frontend)"))
            .collect();
        app.apply_engine(umadev_agent::EngineEvent::PlanPosted {
            steps,
            done: 0,
            total: 20,
        });
        // 12 rows tall is short; the render must still succeed (no panic) and the
        // prompt mode prefix (the chevron) must still be visible at the bottom.
        let out = render_chat_to_string(&app, 80, 12);
        // The "more" tail proves the panel capped itself rather than overflowing.
        assert!(
            out.contains('…') || out.contains('+'),
            "panel capped: {out}"
        );
    }

    #[test]
    fn clipped_panel_tail_carries_a_how_to_see_more_hint() {
        // Defect 1: a clipped panel's "… +N" tail must tell the user HOW to read
        // the rest (the full verdicts are in the scrollable transcript / `/plan`),
        // not dead-end at a bare count.
        let mut app = app_with(Some("offline"));
        let steps: Vec<String> = (0..20)
            .map(|i| format!("s{i} · step number {i} (frontend)"))
            .collect();
        app.apply_engine(umadev_agent::EngineEvent::PlanPosted {
            steps,
            done: 0,
            total: 20,
        });
        let out = render_chat_to_string(&app, 80, 12);
        // The clip tail is the row carrying the ellipsis marker; it must also
        // carry the `/plan` affordance (present in the hint in every locale). A
        // line-scoped check stays robust to the locale AND to the renderer
        // spacing out wide CJK glyphs (so an exact substring match is unreliable).
        let tail_points_somewhere = out
            .lines()
            .any(|l| l.contains('\u{2026}') && l.contains("/plan"));
        assert!(
            tail_points_somewhere,
            "the clip tail surfaces a how-to-see-more affordance: {out}"
        );
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
        // groups are visible without scrolling). The registry-generated help is
        // now complete (every command + every group), so it needs more height
        // than the old hand-curated subset did.
        let backend = TestBackend::new(120, 130);
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
            app.transcript_scroll(),
            max,
            "scroll-up must clamp at hidden_above"
        );
        // Scrolling back down past 0 re-pins to the bottom.
        app.transcript_scroll_down(10_000);
        assert_eq!(app.transcript_scroll(), 0);
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
        assert_eq!(app.transcript_scroll(), 0);
        let back = render_chat_at(&app, 80, 18);
        assert!(back.contains("scroll-content-line-57"));
        assert!(!back.contains("▟▀▀▀▀▀▙"));
    }

    #[test]
    fn p5b_scrolled_up_view_holds_anchor_when_content_grows() {
        // P5b: while the user is scrolled UP reading history, content arriving at
        // the BOTTOM must NOT push what they're reading off-screen. The renderer
        // re-anchors the from-bottom offset by the rows that appeared below.
        let mut app = app_with_long_transcript(60);
        // Render once to publish the scroll bounds, then scroll up to a known spot.
        let _ = render_chat_at(&app, 80, 18);
        app.transcript_scroll_to_top();
        let before = render_chat_at(&app, 80, 18);
        assert!(
            before.contains("▟▀▀▀▀▀▙"),
            "scrolled to top shows the banner"
        );
        let off_before = app.transcript_scroll();
        // New content lands at the bottom while the user is scrolled up.
        for i in 60..75 {
            app.apply_engine(umadev_agent::EngineEvent::Note(format!(
                "scroll-content-line-{i}"
            )));
        }
        let after = render_chat_at(&app, 80, 18);
        // The anchored view still shows the very top (banner), NOT drifted down to
        // the freshly-added lines — the offset grew to absorb the new rows.
        assert!(
            after.contains("▟▀▀▀▀▀▙"),
            "anchor must hold the top in view after growth: {after}"
        );
        assert!(
            app.transcript_scroll() >= off_before,
            "offset must grow to hold the anchor (was {off_before}, now {})",
            app.transcript_scroll()
        );
        // And re-pinning to the bottom still follows the newest content (sticky):
        // the offset is 0 and a just-added recent line is on screen (the very last
        // logical row can sit on the clipped bottom edge under the input, so assert
        // on a recent-but-not-final line to prove the view jumped to the new tail).
        app.transcript_scroll_to_bottom();
        let bottom = render_chat_at(&app, 80, 18);
        assert_eq!(app.transcript_scroll(), 0, "re-pinned to the bottom");
        assert!(
            bottom.contains("scroll-content-line-72") || bottom.contains("scroll-content-line-73"),
            "bottom-pinned view follows the newest content: {bottom}"
        );
        // The OLD top (banner) is no longer on screen once pinned to the bottom.
        assert!(
            !bottom.contains("▟▀▀▀▀▀▙"),
            "bottom view scrolled past the banner"
        );
    }

    #[test]
    fn p5b_pinned_to_bottom_follows_new_content() {
        // Sticky: at offset 0 (pinned), new content keeps the view on the newest
        // line — the offset stays 0 and the latest row is visible.
        let mut app = app_with_long_transcript(40);
        let _ = render_chat_at(&app, 80, 18);
        assert_eq!(app.transcript_scroll(), 0, "starts pinned to the bottom");
        // A recent line of the initial batch is on screen while pinned (the very
        // last logical row can sit on the clipped bottom edge under the input, so
        // assert on a recent-but-not-final line).
        let initial = render_chat_at(&app, 80, 18);
        assert!(
            initial.contains("scroll-content-line-37")
                || initial.contains("scroll-content-line-38"),
            "pinned view shows recent lines: {initial}"
        );
        // New content arrives — pinned view stays at 0 (follows) and the new tail
        // comes into view, pushing the previously-newest lines toward / off the top
        // of the viewport. The offset never leaves 0 (sticky-to-bottom).
        for i in 41..50 {
            app.apply_engine(umadev_agent::EngineEvent::Note(format!(
                "scroll-content-line-{i}"
            )));
        }
        let out = render_chat_at(&app, 80, 18);
        assert_eq!(app.transcript_scroll(), 0, "stays pinned after new content");
        // The view followed the new tail: a just-added recent line is now visible
        // and the old batch's lines have scrolled off the top.
        assert!(
            out.contains("scroll-content-line-46") || out.contains("scroll-content-line-47"),
            "pinned view followed the new tail: {out}"
        );
        assert!(
            !out.contains("scroll-content-line-30"),
            "older lines scrolled off the top: {out}"
        );
    }

    #[test]
    fn p3_highlight_cache_hit_matches_uncached() {
        // P3: the cached highlighter must return EXACTLY what the uncached
        // tokenizer would — a cache hit can never alter the rendered spans.
        let line = "fn main() { let x: i32 = 42; }";
        let lang = Some("rust");
        let direct = highlight_code_line_uncached(line, lang);
        // First call populates the cache; second call hits it.
        let first = highlight_code_line(line, lang);
        let second = highlight_code_line(line, lang);
        let as_pairs = |v: &[Span<'static>]| -> Vec<(String, ratatui::style::Style)> {
            v.iter().map(|s| (s.content.to_string(), s.style)).collect()
        };
        assert_eq!(as_pairs(&direct), as_pairs(&first), "cached == uncached");
        assert_eq!(as_pairs(&first), as_pairs(&second), "hit == miss result");
    }

    #[test]
    fn p3_highlight_cache_keyed_on_theme() {
        // P3: the theme id is part of the key, so a light/dark flip can't serve
        // stale colors. (Different keys → independent entries.)
        let k_dark = hl_key("let x = 1;", Some("rust"), 0);
        let k_light = hl_key("let x = 1;", Some("rust"), 1);
        assert_ne!(k_dark, k_light, "theme id must change the cache key");
        // Same content+lang+theme → same key (a real hit).
        assert_eq!(
            hl_key("let x = 1;", Some("rust"), 0),
            hl_key("let x = 1;", Some("rust"), 0)
        );
        // Different content → different key.
        assert_ne!(
            hl_key("let x = 1;", Some("rust"), 0),
            hl_key("let y = 2;", Some("rust"), 0)
        );
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
    fn prompt_height_is_clamped_so_meta_row_stays_on_screen() {
        // A tall multi-line input on a short terminal must not push the prompt
        // past `area.height - 3`; the spacer above the prompt (and the input
        // bottom + its meta row) stay on screen. We assert the clamp arithmetic
        // directly (now reserving title + ≥1 transcript row + the spacer).
        let inner_h: u16 = 12; // a short content column
                               // A would-be very tall prompt (e.g. INPUT_MAX_ROWS + 2).
        let raw = INPUT_MAX_ROWS + 2;
        let clamped = raw.min(inner_h.saturating_sub(3)).max(2);
        assert!(
            clamped <= inner_h.saturating_sub(3),
            "prompt must leave room for title + ≥1 transcript row + spacer"
        );
        assert!(clamped >= 2, "prompt keeps at least input + meta rows");
        // And it renders without panicking even with a multi-line input on a
        // short terminal (regression guard for the clip-out-of-view bug). The live
        // status now rides the bottom-right of the meta row, so a comfortably wide
        // terminal shows it; assert the leading glyph of the localized "ready" word
        // is on screen (wide CJK glyphs carry a skip-cell, so match the first one).
        let mut app = app_with(Some("offline"));
        app.insert_str_at_cursor("a\nb\nc\nd\ne\nf\ng\nh");
        let out = render_chat_at(&app, 120, 12);
        let ready = umadev_i18n::t(app.lang, "status.ready").to_string();
        let marker = ready.chars().next().unwrap().to_string();
        assert!(
            out.contains(&marker),
            "live status clipped off the meta row: {out}"
        );
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

    // --- P2-A: live-status display-width alignment (CJK) ---

    /// Render the meta row's STATUS portion (empty left meta) at `width` cols and
    /// return its single row as a per-cell `Vec<String>` (one entry per terminal
    /// column). The live state is now pinned to the bottom-RIGHT of the meta row
    /// instead of a standalone status line; an empty left meta hands the whole row
    /// to the right-aligned status so its placement is isolated. A wide CJK glyph
    /// occupies one cell + a following skip cell, so column indices are exact.
    fn render_status_cells(app: &App, width: u16) -> Vec<String> {
        let backend = TestBackend::new(width, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                meta_row(
                    f,
                    f.area(),
                    theme::BORDER(),
                    &[],
                    status_text_and_color(app),
                );
            })
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
    fn status_renders_cjk_right_aligned_without_overflow() {
        // The live state now rides the bottom-RIGHT of the meta row (no more
        // standalone status line). A Chinese status (`就绪`, 4 display cols / 6
        // bytes) must render flush-RIGHT and never overrun the row width.
        let mut app = app_with(Some("offline"));
        app.lang = umadev_i18n::Lang::ZhCn;
        let width = 80u16;
        let cells = render_status_cells(&app, width);
        // No overflow: render_status_cells yields exactly `width` cells by build.
        assert_eq!(cells.len(), width as usize);
        // Idle → the localized "ready" status renders flush-RIGHT (an empty left
        // meta hands the whole row to the right-aligned status), past the midpoint.
        let ready = umadev_i18n::t(app.lang, "status.ready").to_string();
        let first = ready.chars().next().unwrap().to_string();
        let col = col_of(&cells, &first).expect("CJK status glyph renders");
        assert!(
            col > width as usize / 2,
            "status should be right-aligned now (col {col})"
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
    fn status_clips_overlong_cjk_on_a_narrow_terminal() {
        // On a narrow row the status (`[aborted] 本轮已中止`, 20 display cols) is
        // wider than the room left — it must be clipped (by display width, keeping
        // the HEAD) so it never wraps or overruns. A TestBackend of width W always
        // yields exactly W cells; the assertion verifies the clipped head still
        // fits with no panic.
        let mut app = app_with(Some("offline"));
        app.lang = umadev_i18n::Lang::ZhCn;
        app.aborted = true; // → "[aborted] 本轮已中止"
        let width = 16u16; // narrower than the 20-col status → forces truncation
        let cells = render_status_cells(&app, width);
        assert_eq!(cells.len(), width as usize, "row is exactly the width");
        // The `[aborted]` tag (the HEAD of the status) still renders even clipped.
        assert!(
            col_of(&cells, "a").is_some(),
            "aborted status head should still render at a narrow width"
        );
    }

    #[test]
    fn meta_row_pins_status_right_and_drops_it_when_too_narrow() {
        // The live status is pinned to the bottom-RIGHT of the meta row: flush
        // against the right edge after the left meta when there's room, and
        // DROPPED (fail-open) when the row is too narrow to fit both — so the meta
        // info always wins and nothing ever wraps or overruns.
        let render = |parts: &[(String, Color)], status: Option<(String, Color)>, w: u16| {
            let backend = TestBackend::new(w, 1);
            let mut term = Terminal::new(backend).unwrap();
            term.draw(|f| meta_row(f, f.area(), theme::BORDER(), parts, status))
                .unwrap();
            let buf = term.backend().buffer().clone();
            buf.content()
                .iter()
                .map(|c| c.symbol().to_string())
                .collect::<Vec<_>>()
        };
        let parts = vec![("UmaDev".to_string(), theme::ACCENT())];
        // Roomy: the status sits flush-RIGHT (last cell at the row edge) while the
        // left meta still shows.
        let cells = render(&parts, Some(("READY".to_string(), theme::INFO())), 40);
        assert_eq!(cells.len(), 40);
        assert!(
            cells[39].contains('Y'),
            "status flush-right at the row edge: {:?}",
            &cells[34..40]
        );
        assert!(col_of(&cells, "U").is_some(), "left meta still shown");
        // Too narrow for both (` UmaDev ` already fills 8 cols) → status dropped,
        // meta survives, exactly `w` cells, no overflow.
        let narrow = render(&parts, Some(("READY".to_string(), theme::INFO())), 8);
        assert_eq!(narrow.len(), 8);
        assert!(
            col_of(&narrow, "U").is_some(),
            "meta info wins on a narrow row"
        );
        assert!(
            !narrow.join("").contains("READY"),
            "status dropped when it can't fit: {:?}",
            narrow.join("")
        );
    }

    // --- Pre-fold de-scramble fix ---

    /// Total display width of all spans on a line — the honest per-row width.
    fn line_width(line: &Line<'_>) -> usize {
        line.spans
            .iter()
            .map(|s| disp_width(s.content.as_ref()))
            .sum()
    }

    #[test]
    fn prefold_row_count_equals_real_visual_rows() {
        // The core invariant the scroll math depends on: the number of folded
        // rows is EXACTLY ceil(content_width / w), and no row exceeds `w` cols.
        // This is what the old div_ceil estimate got wrong relative to ratatui's
        // own wrap. ASCII case: 25 cols of text at width 10 → 3 rows.
        let line = Line::from(Span::raw("a".repeat(25)));
        let rows = prefold_line(&line, 10, 0, None);
        assert_eq!(rows.len(), 3, "25 cols / 10 = 3 rows");
        for r in &rows {
            assert!(line_width(r) <= 10, "no folded row exceeds the width");
        }
        // A short line is one row, unchanged.
        let short = Line::from(Span::raw("hi"));
        assert_eq!(prefold_line(&short, 10, 0, None).len(), 1);
    }

    // ── In-app selection highlight (span-splitting by char index) ──
    /// Reconstruct, char by char, whether each char of a line carries the
    /// selection background — so a test can assert exactly which chars got
    /// highlighted without depending on how the spans were chunked.
    fn selected_mask(line: &Line<'static>) -> Vec<bool> {
        let sel_bg = theme::SELECTION_BG();
        let mut mask = Vec::new();
        for span in &line.spans {
            let on = span.style.bg == Some(sel_bg);
            for _ in span.content.chars() {
                mask.push(on);
            }
        }
        mask
    }

    fn plain_text(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn highlight_row_marks_exactly_the_selected_char_range() {
        // A line built from several spans (mixed styles) — the highlight must
        // cut across span boundaries by char index, not span index.
        let line = Line::from(vec![
            Span::styled("foo", Style::default().fg(Color::Red)),
            Span::raw("bar"),
            Span::styled("baz", Style::default().fg(Color::Blue)),
        ]); // "foobarbaz", indices 0..9
        let hl = highlight_row(&line, 2, 7); // chars 2..7 = "obarb"
        assert_eq!(plain_text(&hl), "foobarbaz", "text is preserved exactly");
        assert_eq!(
            selected_mask(&hl),
            vec![false, false, true, true, true, true, true, false, false],
            "exactly chars [2,7) carry the selection bg"
        );
        // The unselected pieces keep their original fg.
        let first = hl.spans.first().expect("at least one span");
        assert_eq!(first.style.fg, Some(Color::Red), "fg preserved on the head");
    }

    #[test]
    fn highlight_row_cjk_splits_on_char_boundaries() {
        // CJK chars are multi-byte; the range is char-indexed so a glyph is never
        // split mid-byte. "你好世界" → highlight chars [1,3) = "好世".
        let line = Line::from(Span::raw("你好世界"));
        let hl = highlight_row(&line, 1, 3);
        assert_eq!(plain_text(&hl), "你好世界");
        assert_eq!(
            selected_mask(&hl),
            vec![false, true, true, false],
            "only the middle two CJK glyphs are highlighted"
        );
    }

    #[test]
    fn highlight_row_fail_open_on_empty_or_reversed_range() {
        let line = Line::from(Span::raw("hello"));
        // to <= from → nothing highlighted, text intact.
        let hl = highlight_row(&line, 3, 3);
        assert_eq!(plain_text(&hl), "hello");
        assert!(
            selected_mask(&hl).iter().all(|&b| !b),
            "an empty range highlights nothing"
        );
        // A range past the end clamps — only the in-range chars light up.
        let tail = highlight_row(&line, 2, 999);
        assert_eq!(
            selected_mask(&tail),
            vec![false, false, true, true, true],
            "chars [2,end) highlighted, no panic on the over-long bound"
        );
    }

    #[test]
    fn apply_selection_highlight_spans_multiple_rows() {
        let mut folded = vec![
            Line::from(Span::raw("first line")),
            Line::from(Span::raw("middle")),
            Line::from(Span::raw("last line")),
        ];
        // From col 6 of row 0 to col 4 of row 2.
        let sel = crate::selection::Selection {
            anchor: (0, 6),
            cursor: (2, 4),
        };
        apply_selection_highlight(&mut folded, &sel, &[]);
        // Row 0: chars [6,end) highlighted ("line").
        assert_eq!(
            selected_mask(&folded[0]),
            vec![false, false, false, false, false, false, true, true, true, true]
        );
        // Row 1 (a full middle row): all highlighted.
        assert!(selected_mask(&folded[1]).iter().all(|&b| b));
        // Row 2: chars [0,4) highlighted ("last").
        assert_eq!(
            selected_mask(&folded[2]),
            vec![true, true, true, true, false, false, false, false, false]
        );
    }

    #[test]
    fn apply_selection_highlight_shifts_logical_cols_by_the_gutter() {
        // The painted line carries a 2-col gutter ("xy") before "hello"; the
        // selection cols are in LOGICAL space ([0,3) = "hel"), so the highlight
        // must land on the decorated chars 2..5, leaving the gutter unselected.
        let mut folded = vec![Line::from(Span::raw("xyhello"))];
        let sel = crate::selection::Selection {
            anchor: (0, 0),
            cursor: (0, 3),
        };
        apply_selection_highlight(&mut folded, &sel, &[2]);
        assert_eq!(
            selected_mask(&folded[0]),
            vec![false, false, true, true, true, false, false],
            "logical [0,3) maps to decorated [2,5) — gutter stays unselected"
        );
    }

    #[test]
    fn logical_row_and_gutter_strips_spine_and_trailing_padding() {
        // A wrapped continuation row: spine glyph + hang space, then content. The
        // logical text drops the `▎`-gutter and the gutter width is 2.
        let spine = spine_glyph();
        let cont = Line::from(vec![
            Span::raw(format!("{spine} ")),
            Span::raw("wrapped tail"),
        ]);
        let (logical, gutter) = logical_row_and_gutter(&cont);
        assert_eq!(logical, "wrapped tail");
        assert_eq!(gutter, 2);
        assert!(
            !logical.contains(spine),
            "no spine glyph leaks into the copy"
        );
        assert!(
            !logical.starts_with(' '),
            "no leading hang-indent in the copy"
        );

        // A user-bubble row: `▎ ` spine prefix + content + trailing bg padding.
        let bubble = Line::from(vec![
            Span::raw(format!("{spine} ")),
            Span::raw("user said hi"),
            Span::raw("      "), // the fill_bg padding out to full width
        ]);
        let (logical, gutter) = logical_row_and_gutter(&bubble);
        assert_eq!(logical, "user said hi");
        assert_eq!(gutter, 2);
        assert!(
            !logical.ends_with(' '),
            "no trailing-space padding in the copy"
        );

        // A plain row with no spine: gutter 0, content kept, trailing trimmed.
        let plain = Line::from(Span::raw("plain content   "));
        let (logical, gutter) = logical_row_and_gutter(&plain);
        assert_eq!(logical, "plain content");
        assert_eq!(gutter, 0);
    }

    #[test]
    fn prefold_cjk_width_never_splits_a_wide_glyph() {
        // 6 CJK glyphs = 12 cols. At width 5, a glyph is 2 cols, so each row fits
        // 2 glyphs (4 cols; a 3rd would need 6 > 5) → 3 rows. Critically, no row
        // is wider than 5 and no glyph is split across the fold.
        let line = Line::from(Span::raw("正在思考问题".to_string())); // 6 wide glyphs
        let rows = prefold_line(&line, 5, 0, None);
        assert_eq!(rows.len(), 3, "6 wide glyphs at width 5 → 3 rows of 2");
        for r in &rows {
            assert!(line_width(r) <= 5, "a folded CJK row never exceeds width");
            // Every span content is whole CJK chars — reassembling preserves them.
            let joined: String = r.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(
                joined
                    .chars()
                    .all(|c| c == ' ' || "正在思考问题".contains(c)),
                "no half-glyph leaked into a row: {joined:?}"
            );
        }
        // Round-trip: concatenating all rows (minus the hang spaces) is the input.
        let back: String = rows
            .iter()
            .flat_map(|r| r.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(back, "正在思考问题");
    }

    #[test]
    fn prefold_wraps_at_word_boundaries_not_mid_word() {
        // At width 12, "hello world foobar" must break between words, never inside
        // one — each visual row is a sequence of whole words.
        let line = Line::from(Span::raw("hello world foobar".to_string()));
        let rows = prefold_line(&line, 12, 0, None);
        let words = ["hello", "world", "foobar"];
        for r in &rows {
            assert!(line_width(r) <= 12, "row fits width 12");
            let joined: String = r.spans.iter().map(|s| s.content.as_ref()).collect();
            for tok in joined.split_whitespace() {
                assert!(
                    words.contains(&tok),
                    "every token on a row is a WHOLE word (no mid-word split): {tok:?} in {joined:?}"
                );
            }
        }
        // Round-trips to the original words in order.
        let back: String = rows
            .iter()
            .flat_map(|r| r.spans.iter())
            .map(|s| s.content.as_ref())
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(back, "hello world foobar");
    }

    #[test]
    fn shimmer_animates_a_bright_band_else_flat() {
        // Animated → per-char spans with at least one bright (band) glyph.
        let lit = shimmer_spans("thinking", 3, Color::Blue, Color::White, true);
        assert!(lit.len() > 1, "animated shimmer splits per char");
        assert!(
            lit.iter().any(|s| s.style.fg == Some(Color::White)),
            "a bright band glyph is present"
        );
        // Not animated → one flat bold span in the base color (no strobe).
        let flat = shimmer_spans("thinking", 3, Color::Blue, Color::White, false);
        assert_eq!(flat.len(), 1, "flat shimmer is a single span");
        assert_eq!(
            flat[0].style.fg,
            Some(Color::Blue),
            "flat uses the base color"
        );
    }

    #[test]
    fn prefold_hard_breaks_a_word_longer_than_the_width() {
        // A single token wider than the row still has to break (char-by-char) so it
        // can never overflow.
        let line = Line::from(Span::raw("supercalifragilistic".to_string())); // 20 chars
        let rows = prefold_line(&line, 8, 0, None);
        assert!(
            rows.len() >= 3,
            "a 20-char word at width 8 spans multiple rows"
        );
        for r in &rows {
            assert!(line_width(r) <= 8, "no row overflows even mid-word");
        }
    }

    #[test]
    fn prefold_normalizes_tabs_so_painted_width_matches_the_fold() {
        // A tab has unicode-width 0, but a terminal expands it to a tab stop —
        // so an un-normalized tab makes the painted row WIDER than the fold
        // measured, the long-line desync. The fold converts tabs, so NO `\t`
        // survives into the painted rows and every row stays within the width.
        let line = Line::from(Span::raw("a\tb\tc\td\te\tf\tg\th".to_string()));
        let rows = prefold_line(&line, 6, 0, None);
        for r in &rows {
            let joined: String = r.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(
                !joined.contains('\t'),
                "no tab survives the fold: {joined:?}"
            );
            assert!(line_width(r) <= 6, "no folded row exceeds the width");
        }
    }

    #[test]
    fn prefold_hard_breaks_a_no_space_path_with_cjk_to_the_width() {
        // The worst overflow case: a long unbroken path (no spaces, the way a
        // full Windows path arrives) intermixed with a wide CJK glyph. Every
        // produced row must stay within the inner width — hard-broken char by
        // char, never splitting the wide glyph — so the terminal never auto-wraps
        // and bleeds content.
        let path = "C:\\Users\\weiyou\\项目\\long\\path\\to\\file.rs";
        let line = Line::from(Span::raw(path.to_string()));
        let w = 10;
        let rows = prefold_line(&line, w, 0, None);
        for r in &rows {
            assert!(
                line_width(r) <= w,
                "no row overflows the inner width: got {}",
                line_width(r)
            );
        }
        // Lossless: nothing dropped while hard-breaking (no spaces to fold away).
        let back: String = rows
            .iter()
            .flat_map(|r| r.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(back, path, "the path round-trips through the fold");
    }

    #[test]
    fn prefold_hang_indents_continuation_rows() {
        // A 2-col hang means every continuation row starts with 2 spaces, so a
        // wrapped assistant paragraph aligns under the bullet's text column.
        let line = Line::from(Span::raw("a".repeat(20)));
        let rows = prefold_line(&line, 10, 2, None);
        assert!(rows.len() >= 2, "20 cols at width 10 wraps");
        // Row 0 has no leading hang; every later row starts with the 2-space hang.
        let first: String = rows[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            !first.starts_with("  "),
            "first row is not indented: {first:?}"
        );
        for r in &rows[1..] {
            let s: String = r.spans.iter().map(|sp| sp.content.as_ref()).collect();
            assert!(s.starts_with("  "), "continuation row not hung: {s:?}");
            assert!(line_width(r) <= 10, "hang row still respects width");
        }
    }

    #[test]
    fn prefold_spine_repaints_the_gutter_glyph_on_every_continuation_row() {
        // With a spine color set, each wrapped continuation row's hanging indent
        // leads with the role-spine glyph (`▎`) — so a multi-line turn shows one
        // unbroken vertical bar — and the indent is still exactly `hang` cols.
        let glyph = spine_glyph();
        let bar = theme::role_bar(ChatRole::Host);
        let line = Line::from(Span::raw("a".repeat(20)));
        let rows = prefold_line(&line, 10, GUTTER_W, Some(bar));
        assert!(rows.len() >= 2, "20 cols at width 10 wraps");
        // Row 0 keeps the caller's own prefix (here: none) — no injected spine.
        let first: String = rows[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            !first.starts_with(glyph),
            "row 0 spine comes from the caller"
        );
        for r in &rows[1..] {
            let joined: String = r.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(
                joined.starts_with(glyph),
                "continuation row leads with the spine glyph: {joined:?}"
            );
            // The spine glyph span carries the bar color; the pad does not.
            assert_eq!(
                r.spans[0].style.fg,
                Some(bar),
                "spine glyph is painted in the role color"
            );
            // Indent is still exactly GUTTER_W columns (glyph=1 + pad=1).
            assert!(line_width(r) <= 10, "spine row still respects width");
            let indent_cols: usize = joined
                .chars()
                .take_while(|&c| c == glyph || c == ' ')
                .map(char_width)
                .sum();
            assert_eq!(
                indent_cols, GUTTER_W,
                "indent stays the unified gutter width"
            );
        }
        // A `None` spine keeps the legacy plain-space indent (no glyph).
        let plain = prefold_line(&line, 10, GUTTER_W, None);
        let cont: String = plain[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            !cont.starts_with(glyph),
            "no spine glyph when color is None"
        );
    }

    // ── Transcript visual skeleton (role spine / full-width bubble / seat
    //    markers / unified gutter) ────────────────────────────────────────

    /// One rendered transcript row, captured at the cell level so a test can
    /// assert the spine glyph, the per-cell background (the user bubble tint),
    /// and the row text without re-deriving the wrap.
    struct CellRow {
        /// Full row text (one entry per buffer cell, row-major).
        text: String,
        /// Per-cell background color, index-aligned with `text`'s cells.
        bgs: Vec<Option<Color>>,
    }

    /// Render the chat to a cell grid and return its non-blank rows.
    fn transcript_cell_rows(app: &App, w: u16, h: u16) -> Vec<CellRow> {
        let backend = TestBackend::new(w, h);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| render(f, app)).unwrap();
        let buf = term.backend().buffer();
        let mut rows = Vec::new();
        for y in 0..buf.area().height {
            let mut text = String::new();
            let mut bgs: Vec<Option<Color>> = Vec::new();
            for x in 0..buf.area().width {
                text.push_str(buf[(x, y)].symbol());
                bgs.push(Some(buf[(x, y)].bg));
            }
            if text.trim().is_empty() {
                continue;
            }
            rows.push(CellRow { text, bgs });
        }
        rows
    }

    fn push_msg(app: &mut App, role: ChatRole, text: &str) {
        app.history.push_back(crate::app::ChatMessage {
            role,
            kind: MessageBody::Text(text.to_string()),
            collapsed: false,
        });
    }

    #[test]
    fn every_turn_row_carries_a_role_spine_including_continuation() {
        // HARD REQ 1: every visual row of a turn (first AND wrapped continuation)
        // carries the role spine glyph `▎`. A long assistant line wraps and every
        // wrapped row must still show the spine. The transcript is inset from the
        // buffer edge, so we scan each row's text for the glyph rather than
        // assuming column 0.
        let glyph = spine_glyph();
        let mut app = app_with(Some("offline"));
        app.history.clear(); // drop the auto-resumed greeting so only ours render
        push_msg(&mut app, ChatRole::You, "hi there");
        push_msg(
            &mut app,
            ChatRole::UmaDev,
            "this is a fairly long director reply that absolutely has to wrap \
             across several rows because it keeps going well past any \
             reasonable single line width at this terminal size",
        );
        push_msg(&mut app, ChatRole::System, "config saved to disk");
        let rows = transcript_cell_rows(&app, 50, 30);
        // The wrapped director reply contributes several rows; its continuation
        // rows must each carry the spine glyph. Match by content fragments.
        let cont_with_spine = rows
            .iter()
            .filter(|r| {
                (r.text.contains("absolutely")
                    || r.text.contains("several")
                    || r.text.contains("reasonable")
                    || r.text.contains("terminal"))
                    && r.text.contains(glyph)
            })
            .count();
        assert!(
            cont_with_spine >= 2,
            "wrapped reply continuation rows carry the spine"
        );
        // The user row carries the spine glyph (no longer a bare leading space).
        let user_row = rows
            .iter()
            .find(|r| r.text.contains("hi there"))
            .expect("user row present");
        assert!(
            user_row.text.contains(glyph),
            "user row carries the spine: {:?}",
            user_row.text
        );
        // The system row carries the spine glyph too.
        let sys_row = rows
            .iter()
            .find(|r| r.text.contains("config saved"))
            .expect("system row present");
        assert!(
            sys_row.text.contains(glyph),
            "system row carries the spine: {:?}",
            sys_row.text
        );
    }

    #[test]
    fn user_message_is_a_full_width_background_bubble() {
        // HARD REQ 2: the user-message tint reads as one solid block to the FULL
        // transcript width, not just the text width (the old ragged-right bug).
        let mut app = app_with(Some("offline"));
        app.history.clear();
        push_msg(&mut app, ChatRole::You, "short");
        let w = 50u16;
        let rows = transcript_cell_rows(&app, w, 20);
        let user_row = rows
            .iter()
            .find(|r| r.text.contains("short"))
            .expect("user row present");
        let bg = theme::USER_MSG_BG();
        // The tint must extend WELL past the 5-char word — to (near) the right
        // edge of the transcript. We count the user-bg cells and require them to
        // span most of the row, proving it's a block bubble.
        let tinted = user_row.bgs.iter().filter(|c| **c == Some(bg)).count();
        assert!(
            tinted >= usize::from(w) / 2,
            "user tint fills the row, not just the word: {tinted} of {w}"
        );
        // And specifically: cells well to the RIGHT of the word still carry the
        // tint (the ragged-right regression would leave them on the default bg).
        let tinted_far_right = user_row
            .bgs
            .iter()
            .rev()
            .take(8)
            .filter(|c| **c == Some(bg))
            .count();
        assert!(
            tinted_far_right >= 4,
            "the tint reaches the right side of the bubble: {:?}",
            user_row.text
        );
    }

    #[test]
    fn host_and_umadev_seats_use_distinct_markers() {
        // HARD REQ 3: the borrowed base (Host) and the UmaDev director read as
        // different seats — same glyph family, different seat COLOR.
        let (host_marker, host_color) = assistant_marker(ChatRole::Host);
        let (uma_marker, uma_color) = assistant_marker(ChatRole::UmaDev);
        assert_eq!(
            host_marker, uma_marker,
            "same glyph family, both filled circles"
        );
        assert_ne!(
            host_color, uma_color,
            "Host vs UmaDev are different seat colors"
        );
        assert_eq!(uma_color, theme::ACCENT(), "UmaDev director = brand accent");
        assert_eq!(host_color, theme::SUCCESS(), "Host base = teammate success");
        // And the spine bar colors also differ (You/Host/UmaDev/System/Gate).
        assert_ne!(
            theme::role_bar(ChatRole::Host),
            theme::role_bar(ChatRole::UmaDev),
            "Host and UmaDev spines are different colors"
        );
    }

    #[test]
    fn unified_gutter_aligns_user_assistant_system_continuation() {
        // HARD REQ 4: every speaker hangs its body under the SAME unified gutter,
        // so a wrapped paragraph's continuation rows all align — i.e. the spine
        // glyph sits at one identical screen column across every spine-led row of
        // EVERY speaker (Host prose, System line). That single column is the
        // unified-gutter invariant; the marker on row 0 also occupies that column.
        let glyph = spine_glyph();
        let mut app = app_with(Some("offline"));
        app.history.clear();
        push_msg(
            &mut app,
            ChatRole::Host,
            "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu \
             nu xi omicron pi rho sigma tau upsilon phi chi psi omega extra words",
        );
        push_msg(
            &mut app,
            ChatRole::System,
            "system status one two three four five six seven eight nine ten \
             eleven twelve thirteen fourteen fifteen sixteen seventeen eighteen",
        );
        let rows = transcript_cell_rows(&app, 50, 40);
        // The screen column of the spine glyph (char index == column for the
        // ASCII content used here) must be identical on every spine-led row.
        let mut cols: Vec<usize> = Vec::new();
        for r in &rows {
            if let Some(idx) = r.text.chars().position(|c| c == glyph) {
                cols.push(idx);
            }
        }
        assert!(
            cols.len() >= 4,
            "both wrapped messages contribute several spine-led rows: {cols:?}"
        );
        let first = cols[0];
        assert!(
            cols.iter().all(|&c| c == first),
            "every spine-led row aligns its bar at the same gutter column: {cols:?}"
        );
        // And the content begins exactly GUTTER_W columns past the bar's column:
        // the gutter is the bar glyph (1 col) + one pad space.
        for r in &rows {
            let chars: Vec<char> = r.text.chars().collect();
            if chars.get(first) != Some(&glyph) {
                continue;
            }
            // The cell right after the bar is the single pad space of the gutter.
            assert_eq!(
                chars.get(first + 1),
                Some(&' '),
                "the bar is followed by exactly one pad space (GUTTER_W=2): {:?}",
                r.text
            );
        }
    }

    #[test]
    fn strip_control_chars_drops_escapes_keeps_tab() {
        // The control bytes themselves are removed so the model can't move the
        // cursor or clear the screen; printable text and tabs survive untouched.
        // (Only the C0/C1 control *characters* are dropped — the printable
        // residue of an escape sequence, e.g. `[2J`, is harmless text and stays;
        // what matters is that the ESC byte that would activate it is gone.)
        assert_eq!(strip_control_chars("clean text").as_ref(), "clean text");
        assert_eq!(strip_control_chars("a\x1b[2Jb").as_ref(), "a[2Jb");
        assert_eq!(strip_control_chars("a\x00\x07\x1bb").as_ref(), "ab");
        assert!(!strip_control_chars("x\x1by").contains('\x1b'));
        assert_eq!(strip_control_chars("col1\tcol2").as_ref(), "col1\tcol2");
        // Borrowed (no realloc) when already clean.
        assert!(matches!(
            strip_control_chars("plain"),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    #[test]
    fn long_cjk_transcript_renders_without_scrambling() {
        // End-to-end regression for the floating-glyph bug: a long CJK assistant
        // message in a narrow terminal must render cleanly — every painted cell is
        // a real glyph or space, the scroll lands on a row boundary, and the most
        // recent content is on screen (bottom-pinned). No control chars, no smear.
        let mut app = app_with(Some("offline"));
        app.lang = umadev_i18n::Lang::ZhCn;
        for i in 0..40 {
            app.apply_engine(umadev_agent::EngineEvent::WorkerStream {
                event: umadev_runtime::StreamEvent::Text {
                    delta: format!("这是第{i}行很长的中文回复内容用来触发换行和滚动"),
                },
            });
            // Force each into its own bubble so the transcript has many CJK rows.
            app.stream_text_active = false;
        }
        // Render at a narrow width where CJK lines must wrap — the old code
        // scrambled here. The proof is simply that it renders to a full buffer
        // without panicking and the bottom-pinned recent content is visible.
        let out = render_chat_at(&app, 40, 20);
        assert!(out.contains('行'), "CJK content should be on screen: {out}");
        // No ESC/control bytes ever reach the buffer.
        assert!(
            !out.chars().any(|c| c.is_control() && c != '\n'),
            "no control chars in the rendered transcript"
        );
    }

    // ── P1 diff-card rendering ────────────────────────────────────────────

    #[test]
    fn diff_card_renders_gutter_markers_and_add_del_colors() {
        use crate::app::FileDiff;
        let d = FileDiff::from_tool_edit(&umadev_runtime::ToolEdit {
            path: "src/app.rs".into(),
            before: "let x = 1;\nlet y = 2;\n".into(),
            after: "let x = 1;\nlet y = 3;\n".into(),
        });
        let lines = diff_to_lines(&d, umadev_i18n::Lang::En, 80, false);
        // Header carries the path + the +N −M metric in add/del colors.
        let header: String = lines[0]
            .0
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(header.contains("src/app.rs"), "header path: {header}");
        assert!(header.contains("+1"), "added metric: {header}");
        assert!(header.contains("−1"), "removed metric: {header}");

        // Find the added line's row: its FIRST span (the gutter) is the '+'
        // marker, colored with the DiffAdd role.
        let add_color = theme::syn_color(SynRole::DiffAdd);
        let del_color = theme::syn_color(SynRole::DiffDel);
        let mut saw_add_gutter = false;
        let mut saw_del_gutter = false;
        for (line, _) in &lines {
            let Some(first) = line.spans.first() else {
                continue;
            };
            let g = first.content.as_ref();
            if g.starts_with('+') {
                saw_add_gutter = true;
                assert_eq!(first.style.fg, Some(add_color), "+ gutter uses DiffAdd");
                // The gutter holds a right-aligned line number, not naked text.
                assert!(
                    g.chars().any(|c| c.is_ascii_digit()),
                    "gutter has a line no: {g:?}"
                );
            } else if g.starts_with('-') {
                saw_del_gutter = true;
                assert_eq!(first.style.fg, Some(del_color), "- gutter uses DiffDel");
            }
        }
        assert!(saw_add_gutter, "an added row with a + gutter renders");
        assert!(saw_del_gutter, "a removed row with a - gutter renders");
    }

    #[test]
    fn word_diff_renders_only_the_changed_token_in_the_word_role() {
        use crate::app::FileDiff;
        // `const oldName = compute(input);` → `const newName = compute(input);`
        // The rename is a small fraction (under the 0.4 fallback), so only the
        // `oldName`/`newName` token should paint in the bright DiffAddWord /
        // DiffDelWord role; the surrounding code keeps its NORMAL syntax colors
        // (on the + line) / the muted delete color (on the − line), NOT the
        // word-emphasis color.
        let d = FileDiff::from_tool_edit(&umadev_runtime::ToolEdit {
            path: "x.ts".into(),
            before: "const oldName = compute(input);\n".into(),
            after: "const newName = compute(input);\n".into(),
        });
        let lines = diff_to_lines(&d, umadev_i18n::Lang::En, 80, false);
        let add_word = theme::syn_color(SynRole::DiffAddWord);
        let del_word = theme::syn_color(SynRole::DiffDelWord);

        // Collect, per +/− row, the text painted in the WORD-emphasis role.
        let mut add_emph = String::new();
        let mut del_emph = String::new();
        for (line, _) in &lines {
            let Some(first) = line.spans.first() else {
                continue;
            };
            let g = first.content.as_ref();
            let is_add = g.starts_with('+');
            let is_del = g.starts_with('-');
            if !is_add && !is_del {
                continue;
            }
            for s in line.spans.iter().skip(1) {
                if is_add && s.style.fg == Some(add_word) {
                    add_emph.push_str(&s.content);
                } else if is_del && s.style.fg == Some(del_word) {
                    del_emph.push_str(&s.content);
                }
            }
        }
        assert_eq!(
            add_emph, "newName",
            "only the renamed token is emphasised (+)"
        );
        assert_eq!(
            del_emph, "oldName",
            "only the renamed token is emphasised (−)"
        );
    }

    #[test]
    fn deletion_line_unchanged_tokens_are_not_all_one_red_block() {
        use crate::app::FileDiff;
        // A small change inside a longer line: the − line's UNCHANGED tokens must
        // NOT all be painted in the bright word-del color — only the changed
        // token is. (Pre-fix the whole − line was one flat block.)
        let d = FileDiff::from_tool_edit(&umadev_runtime::ToolEdit {
            path: "x.rs".into(),
            before: "let total = sum_all(items, 1);\n".into(),
            after: "let total = sum_all(items, 2);\n".into(),
        });
        let lines = diff_to_lines(&d, umadev_i18n::Lang::En, 80, false);
        let del_word = theme::syn_color(SynRole::DiffDelWord);
        // Find the − row and count how many of its content chars are in the
        // word-emphasis color — it must be a small minority (just the `1`).
        for (line, _) in &lines {
            let Some(first) = line.spans.first() else {
                continue;
            };
            if !first.content.as_ref().starts_with('-') {
                continue;
            }
            let mut emph = 0usize;
            let mut total = 0usize;
            for s in line.spans.iter().skip(1) {
                let n = s.content.chars().filter(|c| !c.is_whitespace()).count();
                total += n;
                if s.style.fg == Some(del_word) {
                    emph += n;
                }
            }
            // The emphasised portion is a small minority — the bulk of the line
            // (the unchanged `let total = sum_all(items, …)`) is NOT word-red.
            assert!(
                total > emph * 3,
                "the changed token is a small minority of the − line; the rest \
                 is not one red block (emph={emph}, total={total})"
            );
        }
    }

    #[test]
    fn diff_rows_carry_a_full_width_background_tint() {
        use crate::app::FileDiff;
        let d = FileDiff::from_tool_edit(&umadev_runtime::ToolEdit {
            path: "x.rs".into(),
            before: "let y = 2;\n".into(),
            after: "let y = 3;\n".into(),
        });
        let width = 40usize;
        let lines = diff_to_lines(&d, umadev_i18n::Lang::En, width, false);
        let add_bg = theme::DIFF_ADD_BG();
        let del_bg = theme::DIFF_DEL_BG();
        let mut saw_full_add = false;
        let mut saw_full_del = false;
        for (line, _) in &lines {
            let Some(first) = line.spans.first() else {
                continue;
            };
            let g = first.content.as_ref();
            let (is_add, is_del) = (g.starts_with('+'), g.starts_with('-'));
            if !is_add && !is_del {
                continue;
            }
            // Every span on the row carries the row bg, and the painted width
            // (sum of display widths) reaches the full card width.
            let want = if is_add { add_bg } else { del_bg };
            let painted: usize = line.spans.iter().map(|s| disp_width(&s.content)).sum();
            let all_bg = line.spans.iter().all(|s| s.style.bg == Some(want));
            assert!(all_bg, "every span on a +/- row carries the row bg");
            assert_eq!(painted, width, "the row is padded to the full card width");
            if is_add {
                saw_full_add = true;
            } else {
                saw_full_del = true;
            }
        }
        assert!(
            saw_full_add && saw_full_del,
            "both +/- rows got a full-width tint"
        );
    }

    #[test]
    fn expanded_diff_truncates_with_a_muted_tail() {
        use crate::app::{DiffHunk, DiffLine, FileDiff};
        // Build (by hand) an EXPANDED card whose single hunk exceeds the row cap,
        // so the renderer must stop and emit a `… N more lines` tail. (Building
        // it directly avoids relying on the fold heuristics.)
        let n = super::DIFF_EXPANDED_ROW_CAP + 25;
        let lines: Vec<DiffLine> = (0..n)
            .map(|i| DiffLine {
                tag: '+',
                line_no: Some(u32::try_from(i).unwrap_or(0) + 1),
                text: format!("row {i}"),
                changed: Vec::new(),
            })
            .collect();
        let d = FileDiff {
            path: "big.rs".into(),
            added: u32::try_from(n).unwrap_or(0),
            removed: 0,
            hunks: vec![DiffHunk { lines }],
            collapsed: false, // explicitly expanded
        };
        let out = diff_to_lines(&d, umadev_i18n::Lang::En, 80, false);
        let joined: String = out
            .iter()
            .flat_map(|(l, _)| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        // Exactly the cap's worth of `+` content rows render, then the tail.
        let plus_rows = out
            .iter()
            .filter(|(l, _)| {
                l.spans
                    .first()
                    .is_some_and(|s| s.content.as_ref().starts_with('+'))
            })
            .count();
        assert_eq!(
            plus_rows,
            super::DIFF_EXPANDED_ROW_CAP,
            "renders up to the cap"
        );
        // The muted tail names the elided remainder (25 rows).
        assert!(
            joined.contains("25"),
            "tail names the remaining rows: {joined:?}"
        );
        assert!(
            joined.contains("more lines") || joined.contains('行'),
            "tail is the truncation message: {joined:?}"
        );
    }

    #[test]
    fn collapsed_diff_card_renders_only_the_header_with_expand_hint() {
        use crate::app::FileDiff;
        // A big diff defaults collapsed → just the header + the Ctrl+R hint.
        use std::fmt::Write as _;
        let mut after = String::new();
        for i in 0..40 {
            let _ = writeln!(after, "row{i}");
        }
        let d = FileDiff::from_tool_edit(&umadev_runtime::ToolEdit {
            path: "big.rs".into(),
            before: String::new(),
            after,
        });
        assert!(d.collapsed);
        let lines = diff_to_lines(&d, umadev_i18n::Lang::En, 80, false);
        assert_eq!(lines.len(), 1, "folded card is a single header row");
        let text: String = lines[0]
            .0
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("big.rs"));
        assert!(text.contains("expand"), "shows the expand hint: {text}");
    }

    // ---- Fix B: ONE global "expand everything" toggle (Ctrl+O / verbose) ----

    #[test]
    fn global_verbose_force_expands_a_collapsed_diff() {
        use crate::app::FileDiff;
        use std::fmt::Write as _;
        let mut after = String::new();
        for i in 0..40 {
            let _ = writeln!(after, "row{i}");
        }
        let d = FileDiff::from_tool_edit(&umadev_runtime::ToolEdit {
            path: "big.rs".into(),
            before: String::new(),
            after,
        });
        assert!(d.collapsed, "a big diff defaults collapsed");
        // verbose=false → its per-row state: folded to a single header row.
        assert_eq!(diff_to_lines(&d, umadev_i18n::Lang::En, 80, false).len(), 1);
        // verbose=true (Ctrl+O) → force-expanded regardless of the collapsed flag.
        let expanded = diff_to_lines(&d, umadev_i18n::Lang::En, 80, true);
        assert!(
            expanded.len() > 1,
            "global verbose reveals the folded diff body"
        );
    }

    #[test]
    fn global_verbose_force_expands_a_collapsed_tool() {
        let result: String = (0..30)
            .map(|i| format!("tool-line-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let tool = ToolCall {
            name: "Read".into(),
            arg: "big.txt".into(),
            status: ToolStatus::Ok,
            result: Some(result),
            merged: false,
            count: 1,
            collapsed: true,
        };
        let joined = |verbose: bool| -> String {
            let mut rows = Vec::new();
            render_tool_row(&tool, &mut rows, umadev_i18n::Lang::En, ' ', verbose);
            rows.iter()
                .flat_map(|r| r.line.spans.iter())
                .map(|s| s.content.as_ref().to_string())
                .collect()
        };
        let folded = joined(false);
        assert!(
            !folded.contains("tool-line-29"),
            "a collapsed OK tool hides its tail by default: {folded}"
        );
        let revealed = joined(true);
        assert!(
            revealed.contains("tool-line-29"),
            "global verbose reveals the full tool result: {revealed}"
        );
    }

    #[test]
    fn global_verbose_reveals_a_non_latest_collapsed_tool() {
        // The reported bug: only the MOST-RECENT collapsed row had a reveal
        // gesture (Ctrl+R). A single Ctrl+O / `verbose` flip must reveal an OLDER
        // collapsed row too. Push an older tool (alpha) then a newer one (beta);
        // alpha is the non-latest row.
        use crate::app::{ChatMessage, MessageBody};
        let tool_msg = |marker: &str| ChatMessage {
            role: ChatRole::Host,
            kind: MessageBody::Tool(ToolCall {
                name: "Read".into(),
                arg: format!("{marker}.txt"),
                status: ToolStatus::Ok,
                result: Some(
                    (0..30)
                        .map(|i| format!("{marker}-line-{i}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                merged: false,
                count: 1,
                collapsed: true,
            }),
            collapsed: false,
        };
        let mut app = app_with(Some("offline"));
        app.history.push_back(tool_msg("alpha")); // older / non-latest
        app.history.push_back(tool_msg("beta")); // latest

        let out_collapsed = render_chat_at(&app, 100, 80);
        assert!(
            !out_collapsed.contains("alpha-line-29"),
            "older tool stays folded by default: {out_collapsed}"
        );

        app.verbose = true;
        let out_verbose = render_chat_at(&app, 100, 80);
        assert!(
            out_verbose.contains("alpha-line-29"),
            "Ctrl+O reveals the OLDER (non-latest) collapsed tool: {out_verbose}"
        );
    }

    #[test]
    fn diff_card_renders_end_to_end_without_panic_or_control_bytes() {
        // A Write/Edit diff card paints to a real buffer cleanly (CJK-safe).
        let mut app = app_with(Some("offline"));
        app.apply_engine(umadev_agent::EngineEvent::WorkerStream {
            event: umadev_runtime::StreamEvent::ToolUse {
                name: "Edit".into(),
                detail: "面板.rs".into(),
                edit: Some(umadev_runtime::ToolEdit {
                    path: "面板.rs".into(),
                    before: "第一行\n".into(),
                    after: "第一行改\n".into(),
                }),
            },
        });
        let out = render_chat_at(&app, 60, 24);
        // The TestBackend pads each wide CJK glyph with a trailing space cell, so
        // assert on the card's ASCII landmarks (the dashed frame, the +N −M
        // metric, the +/- gutter markers) + the CJK content char, not a literal
        // contiguous "面板.rs".
        assert!(
            out.contains('面') && out.contains('板'),
            "CJK header path on screen: {out}"
        );
        assert!(out.contains("┄┄"), "dashed top frame on screen: {out}");
        assert!(
            out.contains("+1") && out.contains("−1"),
            "metric on screen: {out}"
        );
        assert!(
            out.contains("第") && out.contains("改"),
            "diff content on screen: {out}"
        );
        assert!(
            !out.chars().any(|c| c.is_control() && c != '\n'),
            "no control chars in the diff card"
        );
    }
}
