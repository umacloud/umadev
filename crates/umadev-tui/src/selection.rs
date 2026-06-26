//! In-app text-selection layer for the transcript (the Claude-Code approach).
//!
//! UmaDev runs full-screen on the **alternate screen**, which has no native
//! terminal scrollback. With mouse capture ON the wheel can page the transcript
//! — but capture also swallows the terminal's *own* click-drag selection, so
//! native copy stops working. To get BOTH wheel-scroll AND drag-to-copy we run
//! our own selection layer over the alt screen: the renderer caches each
//! rendered transcript row as plain text + the transcript [`Rect`] + the index
//! of the first visible row, the event loop turns mouse down/drag/up into a
//! [`Selection`] over those cached rows, the renderer paints the selected span
//! with a selection background, and on mouse-up we extract the selected text and
//! copy it to the system clipboard via OSC 52 (which works from inside the alt
//! screen, where a normal clipboard write would not reach the host).
//!
//! Everything in this module is **pure + fail-open**: every coordinate is
//! clamped, every out-of-range index degrades to an empty string or a no-op
//! selection, and nothing here can panic on adversarial input. The actual mouse
//! wiring (in `lib.rs`) and the highlight rendering (in `ui.rs`) stay thin; this
//! module is where the testable logic lives.

/// A point in transcript-content coordinates: `(content_row, col)` where
/// `content_row` indexes the renderer's cached `transcript_rows` (one entry per
/// wrapped visual row) and `col` is a **char index** into that row's plain text
/// (`0` = before the first char).
pub type Point = (usize, usize);

/// An in-app text selection over the cached transcript rows. `anchor` is where
/// the drag started (mouse-down), `cursor` is where it currently is (the latest
/// drag / up point). Either order is valid on the wire — readers normalize via
/// [`Selection::normalized`] so anchor ≤ cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    /// Where the drag began (mouse-down point), in content coordinates.
    pub anchor: Point,
    /// The current end of the drag (latest drag / mouse-up point).
    pub cursor: Point,
}

impl Selection {
    /// A fresh single-point selection (anchor == cursor): a click with no drag
    /// yet. Extracts to the empty string until the cursor moves.
    #[must_use]
    pub fn at(p: Point) -> Self {
        Self {
            anchor: p,
            cursor: p,
        }
    }

    /// `(start, end)` with `start <= end` in reading order (row first, then
    /// col), regardless of drag direction. This is the canonical form every
    /// reader (text extraction, highlight) uses.
    #[must_use]
    pub fn normalized(&self) -> (Point, Point) {
        if self.anchor <= self.cursor {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }

    /// `true` when the selection covers no characters (a click without a drag,
    /// i.e. `start == end`). An empty selection extracts to `""` and paints no
    /// highlight.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        let (s, e) = self.normalized();
        s == e
    }
}

/// Map a screen point `(screen_col, screen_row)` (absolute terminal cells, as
/// crossterm reports them) to a transcript-content [`Point`].
///
/// `area` is the transcript rectangle as `(left, top, width, height)`;
/// `first_visible_row` is the index into the cached rows of the row currently
/// painted at `area.top` (the renderer's `hidden_above - user_offset`);
/// `rows` is the cached plain-text rows (used only to clamp the column to the
/// resolved row's char length).
///
/// Returns `None` when the point is OUTSIDE the transcript area (so the caller
/// can clear the selection), or when there are no cached rows. The resolved
/// `content_row` is clamped to the last cached row, and `col` is clamped to that
/// row's char count — so a click in the blank area below the last line still
/// resolves to a valid in-range point rather than dangling.
#[must_use]
pub fn screen_to_content(
    screen_col: u16,
    screen_row: u16,
    area: (u16, u16, u16, u16),
    first_visible_row: usize,
    rows: &[String],
) -> Option<Point> {
    let (left, top, width, height) = area;
    if width == 0 || height == 0 || rows.is_empty() {
        return None;
    }
    // Outside the rectangle → no content point (caller clears the selection).
    if screen_col < left
        || screen_row < top
        || screen_col >= left.saturating_add(width)
        || screen_row >= top.saturating_add(height)
    {
        return None;
    }
    let row_off = usize::from(screen_row - top);
    // Clamp to the last cached row so a click in the empty area under the last
    // line still lands on a real row instead of past the end.
    let content_row = first_visible_row
        .saturating_add(row_off)
        .min(rows.len() - 1);
    let col_off = usize::from(screen_col - left);
    let row_len = rows[content_row].chars().count();
    let content_col = col_off.min(row_len);
    Some((content_row, content_col))
}

/// Extract the selected text from the cached `rows` for `sel`.
///
/// Single row → the substring `[start.col, end.col)` of that row. Multi-row →
/// the first row from `start.col` to its end, every full middle row, and the
/// last row from its start to `end.col`, joined by `'\n'`.
///
/// Fail-open by contract: any out-of-range row index or column contributes the
/// empty string for that row instead of panicking, and char-indexing is used
/// throughout so a multi-byte / CJK boundary can never split a code point.
#[must_use]
pub fn extract(rows: &[String], sel: &Selection) -> String {
    let ((sr, sc), (er, ec)) = sel.normalized();
    // A selection whose start row is past the end selects nothing at all
    // (fail-open: don't emit a run of separator newlines for ghost rows).
    if sr >= rows.len() {
        return String::new();
    }
    // Clamp the END row to the last real row so a too-large cursor row can't
    // append `\n`s for rows that don't exist.
    let er = er.min(rows.len() - 1);
    // A row slice by CHAR index `[from, to)`, clamped + fail-open: an
    // out-of-range row yields "", and the columns are clamped to the row's char
    // length so `to < from` or a past-the-end index can never panic.
    let slice = |row: usize, from: usize, to: usize| -> String {
        let Some(s) = rows.get(row) else {
            return String::new();
        };
        let len = s.chars().count();
        let from = from.min(len);
        let to = to.min(len);
        if to <= from {
            return String::new();
        }
        s.chars().skip(from).take(to - from).collect()
    };
    if sr >= er {
        // Single effective row (start row == clamped end row): one substring.
        // When the clamp collapsed a multi-row selection onto the last line, the
        // whole line from `sc` to its end is the intent, so widen the end col.
        let end_col = if sr == er { ec } else { usize::MAX };
        return slice(sr, sc, end_col);
    }
    let mut out = String::new();
    // First (partial) row: from the anchor col to end of line.
    out.push_str(&slice(sr, sc, usize::MAX));
    // Full middle rows.
    for r in (sr + 1)..er {
        out.push('\n');
        if let Some(s) = rows.get(r) {
            out.push_str(s);
        }
    }
    // Last (partial) row: start of line up to the cursor col.
    out.push('\n');
    out.push_str(&slice(er, 0, ec));
    out
}

/// Standard-alphabet (RFC 4648) base64 encoder, no padding omission — emits the
/// canonical `=` padding. Implemented inline so the TUI gains the OSC 52
/// clipboard path without a new crate dependency.
#[must_use]
pub fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        // Pack up to three bytes into a 24-bit group (missing bytes = 0).
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).copied().map_or(0, u32::from);
        let b2 = chunk.get(2).copied().map_or(0, u32::from);
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        // The 3rd/4th symbols are padding when the input chunk is short.
        if chunk.len() > 1 {
            out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(n & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Build the OSC 52 clipboard-set escape sequence for `text`:
/// `ESC ] 52 ; c ; <base64(text)> BEL`. Writing this to the terminal copies
/// `text` to the system clipboard — the one clipboard path that works from
/// inside the alternate screen, where the terminal's own selection is suppressed
/// by mouse capture. `c` targets the primary (system) clipboard.
#[must_use]
pub fn osc52_sequence(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64_encode(text.as_bytes()))
}

/// Pure decision: which clipboard path to PREFER for the current environment.
///
/// `"native"` — shell out to the OS clipboard command (pbcopy / wl-copy / xclip
/// / xsel), the most compatible path on a LOCAL session (works in macOS
/// Terminal.app, which has no OSC 52 support). `"osc52"` — write the OSC 52
/// escape, the only path that reaches the *user's* clipboard across an SSH /
/// remote session (a native command would target the far host, not the terminal
/// the user is sitting at).
///
/// Factored out so the local-vs-remote routing is unit-testable without
/// spawning a process. The caller still falls back from `"native"` to OSC 52 at
/// runtime when no native binary is found / succeeds — that runtime fallback is
/// best-effort and not represented here. `os` is the target-OS string
/// (`std::env::consts::OS`), reserved for future per-OS routing; the
/// local/remote split is what decides the preference today.
#[must_use]
pub fn clipboard_path(is_remote: bool, _os: &str) -> &'static str {
    if is_remote {
        "osc52"
    } else {
        "native"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Coordinate mapping ────────────────────────────────────────────────
    #[test]
    fn screen_to_content_maps_inside_the_area() {
        let rows = vec!["hello".to_string(), "world wide".to_string()];
        // area: left=2, top=5, width=20, height=10. first visible content row 0.
        let area = (2u16, 5u16, 20u16, 10u16);
        // Click at screen (col=4, row=6): row_off=1 → content_row 1; col_off=2.
        assert_eq!(
            screen_to_content(4, 6, area, 0, &rows),
            Some((1, 2)),
            "screen (4,6) maps to content row 1, col 2"
        );
        // Top-left corner of the area maps to (first_visible_row, 0).
        assert_eq!(screen_to_content(2, 5, area, 0, &rows), Some((0, 0)));
    }

    #[test]
    fn screen_to_content_honors_the_scroll_offset() {
        let rows: Vec<String> = (0..10).map(|i| format!("line{i}")).collect();
        let area = (0u16, 0u16, 10u16, 4u16);
        // first_visible_row = 6 (scrolled): screen row 0 → content row 6.
        assert_eq!(screen_to_content(0, 0, area, 6, &rows), Some((6, 0)));
        assert_eq!(screen_to_content(3, 2, area, 6, &rows), Some((8, 3)));
    }

    #[test]
    fn screen_to_content_clamps_col_to_row_length() {
        let rows = vec!["hi".to_string()];
        let area = (0u16, 0u16, 40u16, 4u16);
        // col 30 is far past "hi" (len 2) → clamps to 2 (end of the row).
        assert_eq!(screen_to_content(30, 0, area, 0, &rows), Some((0, 2)));
    }

    #[test]
    fn screen_to_content_clamps_row_below_last_line_to_last_row() {
        let rows = vec!["a".to_string(), "b".to_string()];
        let area = (0u16, 0u16, 10u16, 10u16);
        // screen row 7 is past the last content row (1) → clamps to row 1.
        assert_eq!(screen_to_content(0, 7, area, 0, &rows), Some((1, 0)));
    }

    #[test]
    fn screen_to_content_outside_area_is_none() {
        let rows = vec!["x".to_string()];
        let area = (5u16, 5u16, 10u16, 10u16);
        assert_eq!(
            screen_to_content(4, 6, area, 0, &rows),
            None,
            "left of area"
        );
        assert_eq!(screen_to_content(6, 4, area, 0, &rows), None, "above area");
        assert_eq!(
            screen_to_content(15, 6, area, 0, &rows),
            None,
            "right of area (col == left+width)"
        );
        assert_eq!(
            screen_to_content(6, 15, area, 0, &rows),
            None,
            "below area (row == top+height)"
        );
    }

    #[test]
    fn screen_to_content_empty_rows_is_none() {
        assert_eq!(
            screen_to_content(0, 0, (0, 0, 10, 10), 0, &[]),
            None,
            "no cached rows → nothing to select"
        );
    }

    // ── Normalization ─────────────────────────────────────────────────────
    #[test]
    fn normalized_orders_anchor_before_cursor() {
        // Forward drag: already ordered.
        let fwd = Selection {
            anchor: (1, 2),
            cursor: (3, 4),
        };
        assert_eq!(fwd.normalized(), ((1, 2), (3, 4)));
        // Reversed drag (cursor above anchor): swapped on read.
        let rev = Selection {
            anchor: (3, 4),
            cursor: (1, 2),
        };
        assert_eq!(rev.normalized(), ((1, 2), (3, 4)));
        // Same row, reversed column: swapped too.
        let rev_col = Selection {
            anchor: (2, 7),
            cursor: (2, 1),
        };
        assert_eq!(rev_col.normalized(), ((2, 1), (2, 7)));
    }

    #[test]
    fn is_empty_only_for_a_zero_width_point() {
        assert!(Selection::at((4, 9)).is_empty(), "a click is empty");
        assert!(
            !Selection {
                anchor: (1, 0),
                cursor: (1, 1),
            }
            .is_empty(),
            "one-char selection is non-empty"
        );
    }

    // ── Text extraction ───────────────────────────────────────────────────
    #[test]
    fn extract_single_row() {
        let rows = vec!["hello world".to_string()];
        let sel = Selection {
            anchor: (0, 0),
            cursor: (0, 5),
        };
        assert_eq!(extract(&rows, &sel), "hello");
        let mid = Selection {
            anchor: (0, 6),
            cursor: (0, 11),
        };
        assert_eq!(extract(&rows, &mid), "world");
    }

    #[test]
    fn extract_multi_row() {
        let rows = vec![
            "first line".to_string(),
            "second".to_string(),
            "third line".to_string(),
        ];
        // From col 6 of row 0 ("line"), all of row 1, through col 5 of row 2 ("third").
        let sel = Selection {
            anchor: (0, 6),
            cursor: (2, 5),
        };
        assert_eq!(extract(&rows, &sel), "line\nsecond\nthird");
    }

    #[test]
    fn extract_reversed_drag_is_normalized() {
        let rows = vec!["alpha".to_string(), "beta".to_string()];
        // Dragged UP: anchor below cursor — extraction must read top-to-bottom.
        let sel = Selection {
            anchor: (1, 4),
            cursor: (0, 0),
        };
        assert_eq!(extract(&rows, &sel), "alpha\nbeta");
    }

    #[test]
    fn extract_empty_selection_is_empty_string() {
        let rows = vec!["anything".to_string()];
        assert_eq!(extract(&rows, &Selection::at((0, 3))), "");
    }

    #[test]
    fn extract_clamps_overlong_end_row_without_ghost_newlines() {
        // A multi-row selection whose cursor row is past the end clamps to the
        // last real row — no trailing run of `\n` for rows that don't exist.
        let rows = vec!["one".to_string(), "two".to_string()];
        let sel = Selection {
            anchor: (0, 1),
            cursor: (9, 3),
        };
        // Row 0 from col 1 ("ne") + all of the (clamped) last row "two".
        assert_eq!(extract(&rows, &sel), "ne\ntwo");
    }

    #[test]
    fn extract_is_char_aware_for_cjk() {
        // Columns are CHAR indices, so a multi-byte CJK run never splits.
        let rows = vec!["你好世界".to_string()];
        let sel = Selection {
            anchor: (0, 1),
            cursor: (0, 3),
        };
        assert_eq!(extract(&rows, &sel), "好世");
    }

    #[test]
    fn extract_out_of_range_indices_fail_open_to_empty() {
        let rows = vec!["short".to_string()];
        // Row index past the end → "".
        let bad_row = Selection {
            anchor: (5, 0),
            cursor: (9, 2),
        };
        assert_eq!(extract(&rows, &bad_row), "");
        // Columns past the row length on a single row → clamped, no panic.
        let bad_col = Selection {
            anchor: (0, 100),
            cursor: (0, 200),
        };
        assert_eq!(extract(&rows, &bad_col), "");
        // A valid start col but a past-the-end end col → up to end of row.
        let partial = Selection {
            anchor: (0, 2),
            cursor: (0, 999),
        };
        assert_eq!(extract(&rows, &partial), "ort");
    }

    // ── base64 + OSC 52 ───────────────────────────────────────────────────
    #[test]
    fn base64_matches_known_vectors() {
        // The classic RFC 4648 progression that exercises both padding cases.
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_encodes_utf8_bytes() {
        // "你好" is 6 UTF-8 bytes — base64 operates on the bytes, not chars.
        assert_eq!(base64_encode("你好".as_bytes()), "5L2g5aW9");
    }

    #[test]
    fn osc52_wraps_base64_in_the_exact_escape_bytes() {
        // ESC ] 52 ; c ; <b64> BEL — assert the exact byte sequence.
        let seq = osc52_sequence("foobar");
        assert_eq!(seq, "\u{1b}]52;c;Zm9vYmFy\u{07}");
        assert_eq!(seq.as_bytes()[0], 0x1b, "starts with ESC");
        assert_eq!(*seq.as_bytes().last().unwrap(), 0x07, "ends with BEL");
    }

    // ── clipboard path routing ────────────────────────────────────────────
    #[test]
    fn clipboard_path_prefers_native_when_local() {
        // A LOCAL macOS session prefers the native command (pbcopy) — OSC 52 is
        // unsupported by macOS Terminal.app, so native is the compatible path.
        assert_eq!(clipboard_path(false, "macos"), "native");
        // Local Linux likewise prefers native (wl-copy / xclip / xsel).
        assert_eq!(clipboard_path(false, "linux"), "native");
    }

    #[test]
    fn clipboard_path_uses_osc52_when_remote() {
        // Over SSH the native command would target the far host's clipboard, not
        // the terminal the user sits at — only OSC 52 reaches their clipboard.
        assert_eq!(clipboard_path(true, "macos"), "osc52");
        assert_eq!(clipboard_path(true, "linux"), "osc52");
    }
}
