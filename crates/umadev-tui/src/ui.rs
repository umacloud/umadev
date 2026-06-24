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
    let title_full = format!("{}{progress}", ov.title);

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
                let marker = match self.lists.last() {
                    Some(ListKind::Ordered(idx)) => ordered_marker(depth, *idx),
                    _ => "•".to_string(),
                };
                prefix.push(role_span(
                    format!("{marker} "),
                    SynRole::ListMarker,
                    Modifier::empty(),
                ));
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

/// Render an assembled table into aligned, CJK-safe rows and push them onto
/// `state.lines`. Column widths are the max VISIBLE (style-stripped, `unicode-
/// width`-measured, CJK = 2) cell width — never byte length — so Chinese columns
/// line up. A muted separator row sits under the header.
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
    let indent = " ".repeat(state.list_indent() + 1);
    let empty: Vec<Span<'static>> = Vec::new();
    for (r, row) in table.rows.iter().enumerate() {
        let mut spans: Vec<Span<'static>> = vec![Span::raw(indent.clone())];
        for (c, &col_w) in widths.iter().enumerate() {
            let cell = row.get(c).unwrap_or(&empty);
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
            Event::Start(Tag::Link { .. }) => state.inline.stack.push(StyleFrame {
                modifier: Modifier::UNDERLINED,
                role: Some(SynRole::Link),
            }),
            // Closing any inline-style tag pops one frame off the style stack.
            Event::End(
                TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough | TagEnd::Link,
            ) => {
                state.inline.stack.pop();
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
                } else {
                    state.inline.push_text(&t, None);
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

/// Per-language lightweight tokenizer → semantic-role spans. NOT a parser: a
/// regex-free, allocation-light scan that recognises comments, string/char
/// literals, numbers, keywords (per language family), type-ish identifiers
/// (CamelCase / known primitives), and diff markers — emitting [`SynRole`] tags
/// only. An unknown language falls back to plaintext with string/number/comment
/// heuristics. Never panics (char-boundary safe).
fn highlight_code_line(line: &str, lang: Option<&str>) -> Vec<Span<'static>> {
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
    // CLAMP it so the title(1) + at least one transcript row + status(1) always
    // fit: `area.height - 3` is the most the prompt may take. Without this a
    // tall multi-line input on a short terminal would shove the status row (and
    // the bottom of the input) past the viewport edge.
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
        let headroom = inner.height.saturating_sub(1 + 3 + prompt_h + 1); // title + min transcript + prompt + status
        want.min(headroom).min(PLAN_PANEL_MAX_ROWS)
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),        // title row (borderless)
            Constraint::Min(1),           // transcript (grows; ≥1 guaranteed)
            Constraint::Length(panel_h),  // live plan / team-review panel (0 = hidden)
            Constraint::Length(prompt_h), // prompt: input(N) + border(1) + meta(1)
            Constraint::Length(1),        // status row
        ])
        .split(inner);

    render_title_row(frame, chunks[0], app);
    render_transcript(frame, chunks[1], app);
    if panel_h > 0 {
        render_plan_panel(frame, chunks[2], &panel_lines);
    }
    render_prompt(frame, chunks[3], app);
    render_status_row(frame, chunks[4], app);

    // Slash-command palette popover floats above the prompt when the user is
    // typing a `/`-prefixed command with at least one match.
    let palette = app.palette_matches();
    if !palette.is_empty() {
        render_palette_popover(frame, chunks[3], app, &palette);
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
        if app.plan_collapsed {
            lines.push(Line::from(Span::styled(
                umadev_i18n::tf(
                    app.lang,
                    "plan.panel.collapsed",
                    &[&done.to_string(), &total.to_string()],
                ),
                Style::default().fg(theme::TEXT_MUTED()),
            )));
        } else {
            lines.push(Line::from(Span::styled(
                format!(
                    " {} {done}/{total}",
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
fn render_plan_panel(frame: &mut Frame, area: Rect, lines: &[Line<'static>]) {
    // The TOP border eats one row, so the content fits in `area.height - 1`.
    let inner_rows = (area.height as usize).saturating_sub(1);
    if inner_rows == 0 {
        return;
    }
    let shown: Vec<Line<'static>> = if lines.len() > inner_rows {
        // Keep the head visible (title + first steps) and mark the clip so the
        // user knows there's more behind /plan.
        let mut v: Vec<Line<'static>> = lines
            .iter()
            .take(inner_rows.saturating_sub(1))
            .cloned()
            .collect();
        v.push(Line::from(Span::styled(
            format!("  … +{}", lines.len() - inner_rows + 1),
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

/// The assistant bullet glyph (a filled circle, the Claude-Code
/// `AssistantTextMessage` marker), built from its codepoint so the source
/// carries no literal pictographic glyph. Followed by one space, it forms the
/// two-column left gutter under which a wrapped body aligns.
fn assistant_bullet() -> String {
    let mut s = String::with_capacity(2);
    s.push(char::from_u32(0x25CF).unwrap_or('*'));
    s.push(' ');
    s
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
    let mut rendered: Vec<(Line<'static>, usize)> = welcome_lines(app)
        .into_iter()
        .map(|l| (l, 0usize))
        .collect();
    for (msg_idx, msg) in app.history.iter().enumerate() {
        // Top gap before each message for breathing room (Claude Code: marginTop=1).
        if msg_idx > 0 {
            rendered.push((Line::from(""), 0));
        }

        if msg.role == ChatRole::Gate {
            let mut block: Vec<Line<'static>> = Vec::new();
            render_gate_block(&msg.body, theme::WARNING(), &mut block);
            rendered.extend(block.into_iter().map(|l| (l, 2usize)));
            continue;
        }

        match msg.role {
            // **User messages** — full-width tinted background bar (Claude Code:
            // userMessageBackground = rgb(55,55,55)), no leading dot.
            ChatRole::You => {
                for line in msg.body.lines() {
                    rendered.push((
                        Line::from(Span::styled(
                            format!(" {line}"),
                            Style::default().fg(theme::TEXT()).bg(theme::USER_MSG_BG()),
                        )),
                        1,
                    ));
                }
            }
            // **Assistant/Host messages** — leading bullet + plain text on the
            // terminal background (Claude Code: AssistantTextMessage). The
            // two-column bullet gutter is also the hang width, so a long paragraph
            // that wraps lines up under the text, not under the bullet.
            ChatRole::Host | ChatRole::UmaDev => {
                let body_lines = markdown_to_lines(&msg.body, theme::TEXT());
                for (i, bl) in body_lines.into_iter().enumerate() {
                    let mut spans: Vec<Span<'static>> = Vec::new();
                    if i == 0 {
                        spans.push(Span::styled(
                            assistant_bullet(),
                            Style::default().fg(theme::ACCENT()),
                        ));
                    } else {
                        spans.push(Span::raw("  "));
                    }
                    spans.extend(bl.spans);
                    rendered.push((Line::from(spans), 2));
                }
            }
            // **System messages** — dim/muted, no bullet.
            ChatRole::System => {
                for line in msg.body.lines() {
                    rendered.push((
                        Line::from(Span::styled(
                            format!("  {line}"),
                            Style::default().fg(theme::TEXT_MUTED()),
                        )),
                        2,
                    ));
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
        rendered.push((Line::from(""), 0));
        rendered.push((
            Line::from(vec![
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
            ]),
            2,
        ));
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
        .flat_map(|(line, hang)| prefold_line(&line, w, hang))
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
    let para = Paragraph::new(folded);
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
/// Always returns at least one row. Fail-open: a zero `width` is treated as 1.
fn prefold_line(line: &Line<'static>, width: usize, hang: usize) -> Vec<Line<'static>> {
    let w = width.max(1);
    let hang = hang.min(w.saturating_sub(1)); // never indent past the usable width
    let mut out: Vec<Line<'static>> = Vec::new();
    // Accumulator for the current visual row: the spans built so far + the
    // display column we've filled. The first row starts at column 0; every
    // continuation row starts after the hanging indent.
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut col = 0usize;
    let mut started_continuation = false;

    // Emit the accumulated row and start a fresh continuation row (with hang).
    macro_rules! flush_row {
        () => {{
            out.push(Line::from(std::mem::take(&mut cur)));
            col = 0;
            started_continuation = true;
            if hang > 0 {
                cur.push(Span::styled(" ".repeat(hang), Style::default()));
                col = hang;
            }
        }};
    }

    for span in &line.spans {
        let style = span.style;
        let clean = strip_control_chars(span.content.as_ref());
        // Build the current span's text char-by-char so a wide glyph never
        // straddles the fold. Accumulate into a buffer flushed whenever we wrap.
        let mut buf = String::new();
        for ch in clean.chars() {
            let cw = char_width(ch);
            if col + cw > w && col > (if started_continuation { hang } else { 0 }) {
                // This row is full — commit the buffered text for THIS span,
                // then start a new visual row.
                if !buf.is_empty() {
                    cur.push(Span::styled(std::mem::take(&mut buf), style));
                }
                flush_row!();
            }
            buf.push(ch);
            col += cw;
        }
        if !buf.is_empty() {
            cur.push(Span::styled(buf, style));
        }
    }
    out.push(Line::from(cur));
    out
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
    // Clamp the dir name to a display-width budget so a long workspace name can
    // never push the `· backend · /help` chrome (or the right-aligned phase) off
    // the row. The fixed chrome around the dir is `" " + " · " + " {backend} " +
    // " · " + " /help "` = 1 + (1+2) + disp(backend)+2 + (1+2) + 7. We reserve at
    // least 12 cols for the phase on the right, then give the dir whatever's left
    // (floor 6 so it stays legible). Truncated names get a `…` so the cut is
    // visible rather than silently swallowing characters.
    let fixed_chrome = 1 + 1 + disp_width(backend_label) + 2 + 1 + 7;
    let dir_budget = usize::from(area.width)
        .saturating_sub(fixed_chrome)
        .saturating_sub(12)
        .max(6);
    let dir_shown = if disp_width(dir_name) > dir_budget {
        // Keep room for the ellipsis inside the budget.
        format!(
            "{}…",
            truncate_to_width(dir_name, dir_budget.saturating_sub(1))
        )
    } else {
        dir_name.to_string()
    };
    let left = Line::from(vec![
        Span::styled(
            format!(" {dir_shown} "),
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
    // Uses the (possibly clamped) `dir_shown`, so the pad math matches what's drawn.
    let left_width = disp_width(&dir_shown) + 2 + 1 + disp_width(backend_label) + 2 + 1 + 7;
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
                    (
                        "/sessions",
                        umadev_i18n::t(lang, "tui.help.inspect.sessions"),
                    ),
                    (
                        "/resume <id>",
                        umadev_i18n::t(lang, "tui.help.inspect.resume"),
                    ),
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
                    ("/compact", umadev_i18n::t(lang, "tui.help.edit.compact")),
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
        let rows = prefold_line(&line, 10, 0);
        assert_eq!(rows.len(), 3, "25 cols / 10 = 3 rows");
        for r in &rows {
            assert!(line_width(r) <= 10, "no folded row exceeds the width");
        }
        // A short line is one row, unchanged.
        let short = Line::from(Span::raw("hi"));
        assert_eq!(prefold_line(&short, 10, 0).len(), 1);
    }

    #[test]
    fn prefold_cjk_width_never_splits_a_wide_glyph() {
        // 6 CJK glyphs = 12 cols. At width 5, a glyph is 2 cols, so each row fits
        // 2 glyphs (4 cols; a 3rd would need 6 > 5) → 3 rows. Critically, no row
        // is wider than 5 and no glyph is split across the fold.
        let line = Line::from(Span::raw("正在思考问题".to_string())); // 6 wide glyphs
        let rows = prefold_line(&line, 5, 0);
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
    fn prefold_hang_indents_continuation_rows() {
        // A 2-col hang means every continuation row starts with 2 spaces, so a
        // wrapped assistant paragraph aligns under the bullet's text column.
        let line = Line::from(Span::raw("a".repeat(20)));
        let rows = prefold_line(&line, 10, 2);
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
}
