//! Decode tokenizer [`Token`]s into [`InputEvent`]s (UX maturity roadmap §2, P1).
//!
//! [`super::tokenize`] gives boundaries; this gives meaning. A [`Token::Text`]
//! becomes one [`crossterm::event::KeyEvent`] per char; a [`Token::Sequence`] is
//! interpreted as an SGR/X10 mouse event, bracketed-paste enter/exit (with body
//! accumulation), focus in/out, a CSI/SS3 cursor / arrow / Home/End / Page / Fn
//! key, a kitty `CSI u` / `modifyOtherKeys` key (so Shift+Enter / Ctrl+Enter are
//! expressible), or a terminal **response** (device attributes / cursor report)
//! that is dropped.
//!
//! The decoder produces crossterm's own [`KeyEvent`] / [`MouseEvent`] types so
//! the existing downstream handlers (`apply_key_with_mods`, the mouse-wheel /
//! selection layer, paste insertion) consume the result unchanged. The char→key
//! table lives in [`super::keymap`] — the ONE mapping shared with the legacy
//! `EventStream` path (Wave 2 P0) — and the `Cb`→mouse mapping mirrors
//! crossterm's own parser, so the owned-input path is behaviourally identical
//! to the legacy path for ordinary keys (locked by the cross-path contract
//! tests in `super`).
//!
//! Fail-open: an unrecognised or malformed sequence yields an empty result (it
//! is dropped, never leaked as text, never a panic).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use super::keymap::{char_to_key, key};
use super::tokenize::Token;

/// ESC introducer.
const ESC: u8 = 0x1b;
/// Bracketed-paste start marker (`CSI 200 ~`).
const PASTE_START: &[u8] = b"\x1b[200~";
/// Bracketed-paste end marker (`CSI 201 ~`).
const PASTE_END: &[u8] = b"\x1b[201~";
/// Upper bound on an in-flight bracketed-paste body before it is force-closed.
/// A pure MEMORY ceiling: a stream that keeps sending body bytes forever
/// without a terminator (a misbehaving terminal) never goes fd-idle, so the
/// reader's paste-window flush never fires. Kept equal to the application paste
/// envelope so the overflow sentinel is rejected before it can be duplicated.
const PASTE_BUF_CAP: usize = 1024 * 1024;

/// One decoded input event. Mirrors the surface the TUI event loop already
/// switches on; [`InputEvent::Response`] is a terminal reply to a query we sent
/// (or that the terminal volunteered) and is dropped by the loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    /// A keypress (crossterm `KeyEvent`, always `KeyEventKind::Press`).
    Key(KeyEvent),
    /// A mouse click / drag / release / wheel event.
    Mouse(MouseEvent),
    /// A completed bracketed paste (the accumulated body).
    Paste(String),
    /// Terminal focus change — `true` = gained, `false` = lost.
    Focus(bool),
    /// A terminal resize to `(cols, rows)`. Never produced by the decoder
    /// (resize comes from SIGWINCH in the reader); present so the reader can use
    /// one event type.
    Resize(u16, u16),
    /// A terminal response to a query (DA1 / DECRPM / cursor position / kitty
    /// flags). Carries the raw bytes; the event loop drops it.
    Response(Vec<u8>),
}

/// Stateful token → event decoder. Holds the bracketed-paste accumulation state
/// across tokens (paste start, body fragments, paste end may all arrive in
/// separate reads).
#[derive(Debug, Default)]
pub struct Decoder {
    /// Whether we are between a paste-start and paste-end marker.
    in_paste: bool,
    /// The accumulated paste body.
    paste_buf: String,
    /// An oversized paste emitted one bounded rejection sentinel; ignore the
    /// remaining body until its real end marker instead of treating it as keys.
    discarding_paste: bool,
}

impl Decoder {
    /// A fresh decoder in the normal (non-paste) state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode one token into zero or more [`InputEvent`]s.
    pub fn feed_token(&mut self, token: Token) -> Vec<InputEvent> {
        match token {
            Token::Sequence(bytes) => self.feed_sequence(&bytes),
            Token::Text(text) => {
                if self.in_paste {
                    self.append_paste(&text)
                } else if self.discarding_paste {
                    Vec::new()
                } else {
                    decode_text(&text)
                }
            }
        }
    }

    /// Decode a sequence token, handling bracketed-paste framing.
    fn feed_sequence(&mut self, bytes: &[u8]) -> Vec<InputEvent> {
        if bytes == PASTE_START {
            self.in_paste = true;
            self.discarding_paste = false;
            self.paste_buf.clear();
            return Vec::new();
        }
        if bytes == PASTE_END {
            if self.discarding_paste {
                self.discarding_paste = false;
                return Vec::new();
            }
            let body = std::mem::take(&mut self.paste_buf);
            self.in_paste = false;
            return vec![InputEvent::Paste(body)];
        }
        if self.discarding_paste {
            return Vec::new();
        }
        if self.in_paste {
            // A sequence inside a paste is literal body text (bracketed paste
            // guarantees the terminal strips real markers from the body). The
            // reader's paste-state-aware flush window guarantees an end marker
            // is never force-split into here mid-paste (Wave 2 P1), so no
            // marker-reassembly backstop is needed.
            return self.append_paste(&String::from_utf8_lossy(bytes));
        }
        decode_sequence(bytes)
    }

    /// Append body text to the open paste, enforcing the [`PASTE_BUF_CAP`]
    /// memory ceiling: a body that grows past the cap with no terminator in
    /// sight (a terminal streaming garbage forever) is force-closed, delivering
    /// what was buffered as a `Paste` so no input is lost and memory stays
    /// bounded. Returns the forced paste, or empty while the paste stays open.
    fn append_paste(&mut self, s: &str) -> Vec<InputEvent> {
        let room = PASTE_BUF_CAP.saturating_sub(self.paste_buf.len());
        if s.len() <= room {
            self.paste_buf.push_str(s);
            return Vec::new();
        }
        // Keep only enough of the next scalar to make an over-cap sentinel.
        // The application rejects it and the decoder discards the rest of this
        // paste until PASTE_END, so neither memory nor the input can be flooded.
        let mut take = room.saturating_add(1).min(s.len());
        while take < s.len() && !s.is_char_boundary(take) {
            take += 1;
        }
        self.paste_buf.push_str(&s[..take]);
        let body = std::mem::take(&mut self.paste_buf);
        self.in_paste = false;
        self.discarding_paste = true;
        vec![InputEvent::Paste(body)]
    }

    /// Whether the decoder is between a paste-start and paste-end marker.
    ///
    /// Exposed so the reader's flush timer can make a PASTE-STATE-AWARE
    /// decision (Wave 2 P1): a buffered partial escape mid-paste is almost
    /// certainly a split end marker whose continuation is in flight, so the
    /// reader waits the longer paste window instead of force-flushing at the
    /// lone-ESC timeout (the old paste-wedge).
    #[must_use]
    pub fn in_paste(&self) -> bool {
        self.in_paste || self.discarding_paste
    }

    /// Force-close an open paste, delivering the accumulated body and
    /// returning the decoder to the normal state so keys flow again.
    ///
    /// Called by the reader when the fd has been genuinely idle past the paste
    /// flush window while a paste is open — the end marker is not coming (a
    /// disconnect or a misbehaving terminal), and waiting longer would wedge
    /// `in_paste = true` forever, silently swallowing every later keystroke.
    /// `None` when no paste is open.
    pub fn force_close_paste(&mut self) -> Option<InputEvent> {
        if !self.in_paste && !self.discarding_paste {
            return None;
        }
        self.in_paste = false;
        if self.discarding_paste {
            self.discarding_paste = false;
            return None;
        }
        Some(InputEvent::Paste(std::mem::take(&mut self.paste_buf)))
    }
}

/// Decode a text token, coalescing each maximal run of ordinary (non-control)
/// characters into ONE [`InputEvent::Paste`] instead of one
/// [`InputEvent::Key`] per char.
///
/// A single keystroke arrives as a one-char `Token::Text`; a MULTI-char text
/// run reaches the UNFRAMED path only from bulk input — a large paste on a
/// terminal that ignores bracketed paste, or a large IME commit. Exploding that
/// into N key events made the app insert char-by-char, and
/// `App::insert_at_cursor` is O(n) with a per-char undo snapshot → O(n²): a
/// visible hang on a big paste. Coalescing a run into one `Paste` routes it
/// through the same single `insert_str_at_cursor` (one snapshot) that the framed
/// `Event::Paste` path already uses.
///
/// Control characters (Enter `\r`, Tab `\t`, Backspace, Ctrl-combos) are NOT
/// coalesced: each stays its own key event so a newline still submits, Tab still
/// completes, etc. A LONE ordinary char also stays a key event, so ordinary
/// typing (kill/yank windows, per-key semantics) is unchanged — only a run of
/// two or more ordinary chars collapses to a paste.
fn decode_text(text: &str) -> Vec<InputEvent> {
    let mut events = Vec::new();
    let mut run = String::new();
    for c in text.chars() {
        if c.is_control() {
            flush_text_run(&mut run, &mut events);
            events.push(InputEvent::Key(char_to_key(c)));
        } else {
            run.push(c);
        }
    }
    flush_text_run(&mut run, &mut events);
    events
}

/// Drain a pending ordinary-char run into `events`: a two-or-more-char run
/// becomes ONE coalesced [`InputEvent::Paste`]; a lone char stays a key press.
/// Clears `run` either way.
fn flush_text_run(run: &mut String, events: &mut Vec<InputEvent>) {
    let mut chars = run.chars();
    match (chars.next(), chars.next()) {
        (None, _) => {}
        (Some(only), None) => {
            events.push(InputEvent::Key(char_to_key(only)));
            run.clear();
        }
        (Some(_), Some(_)) => events.push(InputEvent::Paste(std::mem::take(run))),
    }
}

/// Decode a complete escape sequence (always starts with ESC). Fail-open: an
/// unrecognised sequence is dropped (empty result).
fn decode_sequence(bytes: &[u8]) -> Vec<InputEvent> {
    if bytes.first() != Some(&ESC) {
        return Vec::new();
    }
    if bytes.len() == 1 {
        return vec![InputEvent::Key(key(KeyCode::Esc, KeyModifiers::NONE))];
    }
    match bytes[1] {
        b'[' => decode_csi(bytes),
        b'O' => decode_ss3(bytes),
        // String-protocol replies are terminal metadata, never Alt-key input.
        // Keeping them as Response lets the single owned reader consume a
        // capability query without racing stdin or leaking bytes into the app.
        b']' | b'P' | b'_' => vec![InputEvent::Response(bytes.to_vec())],
        ESC => vec![InputEvent::Key(key(KeyCode::Esc, KeyModifiers::NONE))],
        _ => decode_meta(&bytes[1..]),
    }
}

/// Decode `ESC <rest>` as Alt + whatever `rest` is (single char). Used for
/// Alt+letter combos and as the fail-open path for a flushed partial.
fn decode_meta(rest: &[u8]) -> Vec<InputEvent> {
    if rest.is_empty() || rest[0] == ESC {
        return vec![InputEvent::Key(key(KeyCode::Esc, KeyModifiers::NONE))];
    }
    if let Ok(s) = std::str::from_utf8(rest) {
        if let Some(c) = s.chars().next() {
            let mut k = char_to_key(c);
            k.modifiers |= KeyModifiers::ALT;
            return vec![InputEvent::Key(k)];
        }
    }
    Vec::new()
}

/// Decode an SS3 sequence (`ESC O <final>`).
fn decode_ss3(bytes: &[u8]) -> Vec<InputEvent> {
    if bytes.len() < 3 {
        return Vec::new();
    }
    let code = match bytes[2] {
        b'A' => KeyCode::Up,
        b'B' => KeyCode::Down,
        b'C' => KeyCode::Right,
        b'D' => KeyCode::Left,
        b'H' => KeyCode::Home,
        b'F' => KeyCode::End,
        b'P' => KeyCode::F(1),
        b'Q' => KeyCode::F(2),
        b'R' => KeyCode::F(3),
        b'S' => KeyCode::F(4),
        b'M' => KeyCode::Enter,
        _ => return Vec::new(),
    };
    vec![InputEvent::Key(key(code, KeyModifiers::NONE))]
}

/// Decode a CSI sequence (`ESC [` …). The tokenizer guarantees the final byte is
/// present.
fn decode_csi(bytes: &[u8]) -> Vec<InputEvent> {
    if bytes.len() < 3 {
        return Vec::new();
    }
    match bytes[2] {
        b'<' => return decode_sgr_mouse(bytes),
        b'M' => return decode_x10_mouse(bytes),
        b'I' => return vec![InputEvent::Focus(true)],
        b'O' => return vec![InputEvent::Focus(false)],
        // Private-marker replies: device attributes / DEC mode report / kitty
        // flags / cursor position. Never a keypress — drop via Response.
        b'?' => return vec![InputEvent::Response(bytes.to_vec())],
        _ => {}
    }

    let Some(&final_byte) = bytes.last() else {
        return Vec::new();
    };
    let params = &bytes[2..bytes.len() - 1];

    match final_byte {
        b'~' => decode_tilde(params),
        b'u' => decode_csi_u(params),
        b'A' | b'B' | b'C' | b'D' | b'H' | b'F' | b'P' | b'Q' | b'S' => {
            let mods = parse_csi_mods(params);
            let code = match final_byte {
                b'A' => KeyCode::Up,
                b'B' => KeyCode::Down,
                b'C' => KeyCode::Right,
                b'D' => KeyCode::Left,
                b'H' => KeyCode::Home,
                b'F' => KeyCode::End,
                b'P' => KeyCode::F(1),
                b'Q' => KeyCode::F(2),
                b'S' => KeyCode::F(4),
                _ => unreachable!(),
            };
            vec![InputEvent::Key(key(code, mods))]
        }
        b'Z' => {
            let mut mods = parse_csi_mods(params);
            mods |= KeyModifiers::SHIFT;
            vec![InputEvent::Key(key(KeyCode::BackTab, mods))]
        }
        // `R` is a cursor-position report (`CSI row;col R`, syntactically
        // ambiguous with a modified F3); anything else is an unrecognised CSI.
        // Both are routed to a Response and dropped — never leaked as text.
        _ => vec![InputEvent::Response(bytes.to_vec())],
    }
}

/// Decode `CSI <num>[;<mods>] ~` special keys.
fn decode_tilde(params: &[u8]) -> Vec<InputEvent> {
    let Ok(s) = std::str::from_utf8(params) else {
        return Vec::new();
    };
    let mut it = s.split(';');
    let Some(Ok(first)) = it.next().map(str::parse::<u8>) else {
        return Vec::new();
    };
    let mods = it
        .next()
        .and_then(|m| m.split(':').next())
        .and_then(|m| m.parse::<u8>().ok())
        .map_or(KeyModifiers::NONE, parse_modifiers);
    let code = match first {
        1 | 7 => KeyCode::Home,
        2 => KeyCode::Insert,
        3 => KeyCode::Delete,
        4 | 8 => KeyCode::End,
        5 => KeyCode::PageUp,
        6 => KeyCode::PageDown,
        v @ 11..=15 => KeyCode::F(v - 10),
        v @ 17..=21 => KeyCode::F(v - 11),
        v @ 23..=26 => KeyCode::F(v - 12),
        v @ 28..=29 => KeyCode::F(v - 15),
        v @ 31..=34 => KeyCode::F(v - 17),
        _ => return Vec::new(),
    };
    vec![InputEvent::Key(key(code, mods))]
}

/// Decode a kitty / `CSI u` key: `CSI codepoint[;mods] u`.
fn decode_csi_u(params: &[u8]) -> Vec<InputEvent> {
    let Ok(s) = std::str::from_utf8(params) else {
        return Vec::new();
    };
    let mut it = s.split(';');
    let Some(cp_field) = it.next() else {
        return Vec::new();
    };
    let Some(Ok(codepoint)) = cp_field.split(':').next().map(str::parse::<u32>) else {
        return Vec::new();
    };
    let mods = it
        .next()
        .and_then(|m| m.split(':').next())
        .and_then(|m| m.parse::<u8>().ok())
        .map_or(KeyModifiers::NONE, parse_modifiers);

    let code = match functional_key(codepoint) {
        Some(c) => c,
        None => match char::from_u32(codepoint) {
            Some('\u{1b}') => KeyCode::Esc,
            Some('\r') => KeyCode::Enter,
            Some('\t') => {
                if mods.contains(KeyModifiers::SHIFT) {
                    KeyCode::BackTab
                } else {
                    KeyCode::Tab
                }
            }
            Some('\u{8}' | '\u{7f}') => KeyCode::Backspace,
            Some(c) => KeyCode::Char(c),
            None => return Vec::new(),
        },
    };
    vec![InputEvent::Key(key(code, mods))]
}

/// A small subset of kitty functional key codepoints (numpad + nav) we care
/// about; everything else falls back to the char mapping.
fn functional_key(codepoint: u32) -> Option<KeyCode> {
    Some(match codepoint {
        57414 => KeyCode::Enter, // KP_ENTER
        57417 => KeyCode::Left,
        57418 => KeyCode::Right,
        57419 => KeyCode::Up,
        57420 => KeyCode::Down,
        57421 => KeyCode::PageUp,
        57422 => KeyCode::PageDown,
        57423 => KeyCode::Home,
        57424 => KeyCode::End,
        57425 => KeyCode::Insert,
        57426 => KeyCode::Delete,
        _ => return None,
    })
}

/// Parse a CSI modifier param string of the form `1;<mask>` (the leading `1` is
/// the conventional first param; the modifier mask is the second). Empty / no
/// second param → no modifiers.
fn parse_csi_mods(params: &[u8]) -> KeyModifiers {
    let Ok(s) = std::str::from_utf8(params) else {
        return KeyModifiers::NONE;
    };
    let mut it = s.split(';');
    let _first = it.next();
    it.next()
        .and_then(|m| m.split(':').next())
        .and_then(|m| m.parse::<u8>().ok())
        .map_or(KeyModifiers::NONE, parse_modifiers)
}

/// Decode an xterm modifier mask (`1 + shift|2*alt|4*ctrl|8*super|…`) into
/// [`KeyModifiers`]. Mirrors crossterm's `parse_modifiers`.
fn parse_modifiers(mask: u8) -> KeyModifiers {
    let m = mask.saturating_sub(1);
    let mut mods = KeyModifiers::empty();
    if m & 1 != 0 {
        mods |= KeyModifiers::SHIFT;
    }
    if m & 2 != 0 {
        mods |= KeyModifiers::ALT;
    }
    if m & 4 != 0 {
        mods |= KeyModifiers::CONTROL;
    }
    if m & 8 != 0 {
        mods |= KeyModifiers::SUPER;
    }
    if m & 16 != 0 {
        mods |= KeyModifiers::HYPER;
    }
    if m & 32 != 0 {
        mods |= KeyModifiers::META;
    }
    mods
}

/// Decode an SGR mouse report (`CSI < Cb ; Cx ; Cy (M|m)`).
fn decode_sgr_mouse(bytes: &[u8]) -> Vec<InputEvent> {
    let Some(&last) = bytes.last() else {
        return Vec::new();
    };
    if last != b'M' && last != b'm' {
        return Vec::new();
    }
    if bytes.len() < 4 {
        return Vec::new();
    }
    // body = between `ESC [ <` and the final M/m
    let Ok(s) = std::str::from_utf8(&bytes[3..bytes.len() - 1]) else {
        return Vec::new();
    };
    let mut it = s.split(';');
    let Some(Ok(cb)) = it.next().map(str::parse::<u16>) else {
        return Vec::new();
    };
    let Ok(cb) = u8::try_from(cb) else {
        return Vec::new();
    };
    let Some((kind, modifiers)) = parse_cb(cb) else {
        return Vec::new();
    };
    let Some(Ok(cx)) = it.next().map(str::parse::<u16>) else {
        return Vec::new();
    };
    let Some(Ok(cy)) = it.next().map(str::parse::<u16>) else {
        return Vec::new();
    };
    // `m` terminator means a release; SGR can't say which button, so a Down
    // becomes an Up (matches crossterm).
    let kind = if last == b'm' {
        match kind {
            MouseEventKind::Down(button) => MouseEventKind::Up(button),
            other => other,
        }
    } else {
        kind
    };
    vec![InputEvent::Mouse(MouseEvent {
        kind,
        column: cx.saturating_sub(1),
        row: cy.saturating_sub(1),
        modifiers,
    })]
}

/// Decode a legacy X10 mouse report (`CSI M Cb Cx Cy`, 6 bytes).
fn decode_x10_mouse(bytes: &[u8]) -> Vec<InputEvent> {
    if bytes.len() < 6 {
        return Vec::new();
    }
    let cb = bytes[3].wrapping_sub(32);
    let Some((kind, modifiers)) = parse_cb(cb) else {
        return Vec::new();
    };
    let cx = u16::from(bytes[4].saturating_sub(32)).saturating_sub(1);
    let cy = u16::from(bytes[5].saturating_sub(32)).saturating_sub(1);
    vec![InputEvent::Mouse(MouseEvent {
        kind,
        column: cx,
        row: cy,
        modifiers,
    })]
}

/// Decode a mouse `Cb` byte into a kind + modifiers. Mirrors crossterm's
/// `parse_cb`: low 2 bits + bits 6-7 are the button number, bit 5 is drag, bits
/// 2-4 are shift/alt/ctrl. Wheel (button 4/5) maps to `ScrollUp`/`ScrollDown`.
fn parse_cb(cb: u8) -> Option<(MouseEventKind, KeyModifiers)> {
    let button_number = (cb & 0b0000_0011) | ((cb & 0b1100_0000) >> 4);
    let dragging = cb & 0b0010_0000 == 0b0010_0000;
    let kind = match (button_number, dragging) {
        (0, false) => MouseEventKind::Down(MouseButton::Left),
        (1, false) => MouseEventKind::Down(MouseButton::Middle),
        (2, false) => MouseEventKind::Down(MouseButton::Right),
        (0, true) => MouseEventKind::Drag(MouseButton::Left),
        (1, true) => MouseEventKind::Drag(MouseButton::Middle),
        (2, true) => MouseEventKind::Drag(MouseButton::Right),
        (3, false) => MouseEventKind::Up(MouseButton::Left),
        (3..=5, true) => MouseEventKind::Moved,
        (4, false) => MouseEventKind::ScrollUp,
        (5, false) => MouseEventKind::ScrollDown,
        (6, false) => MouseEventKind::ScrollLeft,
        (7, false) => MouseEventKind::ScrollRight,
        _ => return None,
    };
    let mut modifiers = KeyModifiers::empty();
    if cb & 0b0000_0100 != 0 {
        modifiers |= KeyModifiers::SHIFT;
    }
    if cb & 0b0000_1000 != 0 {
        modifiers |= KeyModifiers::ALT;
    }
    if cb & 0b0001_0000 != 0 {
        modifiers |= KeyModifiers::CONTROL;
    }
    Some((kind, modifiers))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seq(bytes: &[u8]) -> Vec<InputEvent> {
        Decoder::new().feed_token(Token::Sequence(bytes.to_vec()))
    }

    fn text(s: &str) -> Vec<InputEvent> {
        Decoder::new().feed_token(Token::Text(s.into()))
    }

    fn one_key(events: &[InputEvent]) -> KeyEvent {
        match events {
            [InputEvent::Key(k)] => *k,
            other => panic!("expected one key, got {other:?}"),
        }
    }

    #[test]
    fn lone_esc_decodes_to_esc_key() {
        assert_eq!(
            seq(b"\x1b"),
            vec![InputEvent::Key(key(KeyCode::Esc, KeyModifiers::NONE))]
        );
    }

    #[test]
    fn arrow_keys_decode() {
        assert_eq!(one_key(&seq(b"\x1b[A")).code, KeyCode::Up);
        assert_eq!(one_key(&seq(b"\x1b[B")).code, KeyCode::Down);
        assert_eq!(one_key(&seq(b"\x1b[C")).code, KeyCode::Right);
        assert_eq!(one_key(&seq(b"\x1b[D")).code, KeyCode::Left);
        // SS3 form (application cursor keys).
        assert_eq!(one_key(&seq(b"\x1bOA")).code, KeyCode::Up);
    }

    #[test]
    fn ctrl_left_arrow_decodes_with_modifier() {
        let k = one_key(&seq(b"\x1b[1;5D"));
        assert_eq!(k.code, KeyCode::Left);
        assert!(k.modifiers.contains(KeyModifiers::CONTROL));
    }

    #[test]
    fn special_keys_decode() {
        assert_eq!(one_key(&seq(b"\x1b[5~")).code, KeyCode::PageUp);
        assert_eq!(one_key(&seq(b"\x1b[6~")).code, KeyCode::PageDown);
        assert_eq!(one_key(&seq(b"\x1b[3~")).code, KeyCode::Delete);
        assert_eq!(one_key(&seq(b"\x1b[2~")).code, KeyCode::Insert);
        assert_eq!(one_key(&seq(b"\x1b[15~")).code, KeyCode::F(5));
    }

    #[test]
    fn shift_enter_via_csi_u_decodes() {
        // CSI 13 ; 2 u = Shift+Enter.
        let k = one_key(&seq(b"\x1b[13;2u"));
        assert_eq!(k.code, KeyCode::Enter);
        assert!(k.modifiers.contains(KeyModifiers::SHIFT));
        // CSI 13 ; 5 u = Ctrl+Enter.
        let k = one_key(&seq(b"\x1b[13;5u"));
        assert_eq!(k.code, KeyCode::Enter);
        assert!(k.modifiers.contains(KeyModifiers::CONTROL));
    }

    #[test]
    fn backspace_via_csi_u_decodes() {
        // CSI 8 u = BS (common Windows/ConPTY Backspace representation).
        assert_eq!(one_key(&seq(b"\x1b[8u")).code, KeyCode::Backspace);
        // CSI 127 u = DEL (common POSIX Backspace representation).
        assert_eq!(one_key(&seq(b"\x1b[127u")).code, KeyCode::Backspace);
    }

    #[test]
    fn alt_letter_decodes() {
        let k = one_key(&seq(b"\x1bc"));
        assert_eq!(k.code, KeyCode::Char('c'));
        assert!(k.modifiers.contains(KeyModifiers::ALT));
    }

    #[test]
    fn alt_backspace_decodes_for_both_backspace_bytes() {
        // ESC DEL and ESC BS both mean Alt+Backspace (delete word back) — the
        // same fold the legacy path reaches via `keymap::normalize_key`.
        for bytes in [b"\x1b\x7f".as_slice(), b"\x1b\x08".as_slice()] {
            let k = one_key(&seq(bytes));
            assert_eq!(k.code, KeyCode::Backspace, "{bytes:?}");
            assert!(k.modifiers.contains(KeyModifiers::ALT), "{bytes:?}");
        }
    }

    #[test]
    fn focus_events_decode() {
        assert_eq!(seq(b"\x1b[I"), vec![InputEvent::Focus(true)]);
        assert_eq!(seq(b"\x1b[O"), vec![InputEvent::Focus(false)]);
    }

    #[test]
    fn device_attributes_reply_is_a_dropped_response() {
        match seq(b"\x1b[?62;1c").as_slice() {
            [InputEvent::Response(_)] => {}
            other => panic!("expected a response, got {other:?}"),
        }
    }

    #[test]
    fn string_protocol_replies_are_never_alt_keys() {
        for bytes in [
            b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\".as_slice(),
            b"\x1bP1+r544e=787465726d\x1b\\".as_slice(),
            b"\x1b_Gi=1;OK\x1b\\".as_slice(),
        ] {
            assert!(
                matches!(seq(bytes).as_slice(), [InputEvent::Response(_)]),
                "terminal metadata must stay out of the key stream: {bytes:?}"
            );
        }
    }

    #[test]
    fn sgr_wheel_decodes_to_scroll() {
        // Cb 64 = wheel up, 65 = wheel down.
        match decode_sgr_mouse(b"\x1b[<64;10;5M").as_slice() {
            [InputEvent::Mouse(m)] => {
                assert_eq!(m.kind, MouseEventKind::ScrollUp);
                assert_eq!((m.column, m.row), (9, 4)); // 1-indexed → 0-indexed
            }
            other => panic!("got {other:?}"),
        }
        match decode_sgr_mouse(b"\x1b[<65;10;5M").as_slice() {
            [InputEvent::Mouse(m)] => assert_eq!(m.kind, MouseEventKind::ScrollDown),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn sgr_click_drag_release_decode() {
        // Cb 0 press = left down; `m` terminator = release.
        match seq(b"\x1b[<0;3;4M").as_slice() {
            [InputEvent::Mouse(m)] => assert_eq!(m.kind, MouseEventKind::Down(MouseButton::Left)),
            other => panic!("got {other:?}"),
        }
        match seq(b"\x1b[<0;3;4m").as_slice() {
            [InputEvent::Mouse(m)] => assert_eq!(m.kind, MouseEventKind::Up(MouseButton::Left)),
            other => panic!("got {other:?}"),
        }
        // Cb 32 = left drag (motion bit 0x20 + button 0).
        match seq(b"\x1b[<32;3;4M").as_slice() {
            [InputEvent::Mouse(m)] => assert_eq!(m.kind, MouseEventKind::Drag(MouseButton::Left)),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn bracketed_paste_accumulates_across_tokens() {
        let mut d = Decoder::new();
        assert!(d
            .feed_token(Token::Sequence(PASTE_START.to_vec()))
            .is_empty());
        assert!(d.feed_token(Token::Text("hel".into())).is_empty());
        assert!(d.feed_token(Token::Text("lo".into())).is_empty());
        // A bare sequence inside paste is literal body text.
        assert!(d.feed_token(Token::Sequence(b"\x1b[A".to_vec())).is_empty());
        let out = d.feed_token(Token::Sequence(PASTE_END.to_vec()));
        match out.as_slice() {
            [InputEvent::Paste(body)] => assert_eq!(body, "hello\u{1b}[A"),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn owned_path_bracketed_paste_is_one_paste_at_every_split() {
        // End-to-end through the OWNED input pipeline (tokenizer → decoder): a
        // bracketed paste `\x1b[200~<body>\x1b[201~` must decode to exactly ONE
        // `Paste(body)` no matter where the stdin reads chunk it — the property a
        // user pasting a multi-line requirement relies on.
        use super::super::tokenize::Tokenizer;
        let input = b"\x1b[200~build me a dashboard\nwith login\x1b[201~";
        let want = "build me a dashboard\nwith login";
        for split in 0..=input.len() {
            let mut tk = Tokenizer::for_stdin();
            let mut dec = Decoder::new();
            let mut events = Vec::new();
            for part in [&input[..split], &input[split..]] {
                for token in tk.feed(part) {
                    events.extend(dec.feed_token(token));
                }
            }
            match events.as_slice() {
                [InputEvent::Paste(body)] => {
                    assert_eq!(body, want, "split at {split}: body must be intact");
                }
                other => panic!("split at {split}: expected one Paste, got {other:?}"),
            }
        }
    }

    #[test]
    fn paste_state_is_visible_and_force_close_frees_input() {
        // Wave 2 P1 — the reader's flush timer needs to SEE paste state
        // (`in_paste`) to pick the paste window, and needs `force_close_paste`
        // to resolve a genuinely dead paste (end marker never coming on an
        // idle fd) without wedging input.
        let mut d = Decoder::new();
        assert!(!d.in_paste(), "fresh decoder starts outside a paste");
        assert!(d
            .feed_token(Token::Sequence(PASTE_START.to_vec()))
            .is_empty());
        assert!(d.in_paste(), "paste start opens the paste");
        assert!(d.feed_token(Token::Text("requirement".into())).is_empty());
        // The reader decides the paste is dead → force-close delivers the body.
        assert_eq!(
            d.force_close_paste(),
            Some(InputEvent::Paste("requirement".into()))
        );
        assert!(!d.in_paste(), "force-close returns to the normal state");
        assert_eq!(d.force_close_paste(), None, "no paste open → None");
        // And the decoder is not wedged: ordinary text decodes to keys again.
        assert_eq!(
            one_key(&d.feed_token(Token::Text("x".into()))).code,
            KeyCode::Char('x')
        );
    }

    #[test]
    fn unterminated_paste_force_closes_past_the_cap_and_frees_input() {
        // A terminal that streams paste body forever without a terminator never
        // goes fd-idle (so the reader's paste-window flush never fires) — the
        // memory ceiling must force-close the paste (delivering the buffered
        // body) so the buffer stays bounded and input can never wedge.
        let mut d = Decoder::new();
        assert!(d
            .feed_token(Token::Sequence(PASTE_START.to_vec()))
            .is_empty());
        // A chunk safely UNDER the cap keeps the paste open (no premature close).
        assert!(d.feed_token(Token::Text("x".repeat(1024))).is_empty());
        // Push the buffer past the cap with NO end marker in sight → force-close.
        let out = d.feed_token(Token::Text("a".repeat(PASTE_BUF_CAP + 1)));
        match out.as_slice() {
            [InputEvent::Paste(body)] => assert!(
                body.len() > PASTE_BUF_CAP,
                "the force-closed paste delivers the buffered body, not nothing"
            ),
            other => panic!("expected a force-closed paste, got {} events", other.len()),
        }
        assert!(d.in_paste(), "overflow discards until the real end marker");
        assert!(d.feed_token(Token::Text("must-not-leak".into())).is_empty());
        assert!(d.feed_token(Token::Sequence(PASTE_END.to_vec())).is_empty());
        assert!(!d.in_paste());
        assert_eq!(
            one_key(&d.feed_token(Token::Text("x".into()))).code,
            KeyCode::Char('x')
        );
    }

    #[test]
    fn clean_end_marker_emits_exactly_one_paste() {
        // The clean path (one PASTE_END token) emits exactly one paste.
        let mut d = Decoder::new();
        assert!(d
            .feed_token(Token::Sequence(PASTE_START.to_vec()))
            .is_empty());
        assert!(d.feed_token(Token::Text("hi".into())).is_empty());
        let out = d.feed_token(Token::Sequence(PASTE_END.to_vec()));
        assert_eq!(out, vec![InputEvent::Paste("hi".into())]);
    }

    #[test]
    fn lone_ordinary_char_stays_one_key() {
        // A single keystroke is a one-char text token — it must stay an ordinary
        // key press (per-key semantics: kill/yank windows, etc.), never a paste.
        let out = text("a");
        assert_eq!(out, vec![InputEvent::Key(char_to_key('a'))]);
    }

    #[test]
    fn multi_char_run_coalesces_into_one_paste() {
        // A multi-char ordinary run reaches the UNFRAMED path only from bulk input
        // (a paste on a bracketed-paste-blind terminal, or a large IME commit). It
        // must collapse into ONE Paste so the app inserts it via a single
        // `insert_str_at_cursor` (one undo snapshot) — not N O(n) per-char inserts
        // (the O(n²) hang). CJK included, since a large IME commit is the worst case.
        assert_eq!(text("abc"), vec![InputEvent::Paste("abc".into())]);
        assert_eq!(text("你好世界"), vec![InputEvent::Paste("你好世界".into())]);
    }

    #[test]
    fn control_chars_split_the_coalesced_run() {
        // A control char (here Enter) is NOT coalesced: the ordinary runs on each
        // side collapse to pastes, but the `\r` stays its own key so a newline
        // still submits / inserts per the existing handlers.
        match text("ab\rcd").as_slice() {
            [InputEvent::Paste(a), InputEvent::Key(k), InputEvent::Paste(b)] => {
                assert_eq!(a, "ab");
                assert_eq!(k.code, KeyCode::Enter);
                assert_eq!(b, "cd");
            }
            other => panic!("expected paste/key/paste, got {other:?}"),
        }
        // A lone ordinary char between two controls still stays a single key.
        match text("\ra\r").as_slice() {
            [InputEvent::Key(k1), InputEvent::Key(k2), InputEvent::Key(k3)] => {
                assert_eq!(k1.code, KeyCode::Enter);
                assert_eq!(k2.code, KeyCode::Char('a'));
                assert_eq!(k3.code, KeyCode::Enter);
            }
            other => panic!("expected three keys, got {other:?}"),
        }
    }

    #[test]
    fn control_bytes_in_text_map_to_keys() {
        assert_eq!(one_key(&text("\r")).code, KeyCode::Enter);
        assert_eq!(one_key(&text("\t")).code, KeyCode::Tab);
        // Windows Terminal / ConPTY commonly sends BS (0x08) for Backspace.
        assert_eq!(one_key(&text("\u{8}")).code, KeyCode::Backspace);
        // POSIX terminals commonly send DEL (0x7f) for Backspace.
        assert_eq!(one_key(&text("\u{7f}")).code, KeyCode::Backspace);
        // Ctrl-C = 0x03.
        let k = one_key(&text("\u{3}"));
        assert_eq!(k.code, KeyCode::Char('c'));
        assert!(k.modifiers.contains(KeyModifiers::CONTROL));
        // Uppercase carries SHIFT.
        let k = one_key(&text("A"));
        assert_eq!(k.code, KeyCode::Char('A'));
        assert!(k.modifiers.contains(KeyModifiers::SHIFT));
    }

    #[test]
    fn x10_mouse_decodes() {
        // `\x1b[M` + Cb=0x20(wheel? no: 0x20-32=0 → left down), Cx=0x21(col1),
        // Cy=0x22(row2). 0-indexed → (0,1).
        match decode_x10_mouse(b"\x1b[M\x20\x21\x22").as_slice() {
            [InputEvent::Mouse(m)] => {
                assert_eq!(m.kind, MouseEventKind::Down(MouseButton::Left));
                assert_eq!((m.column, m.row), (0, 1));
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn malformed_sequence_is_dropped_not_panicked() {
        // Garbage params → empty (fail-open).
        assert!(decode_sgr_mouse(b"\x1b[<zz;1;1M").is_empty());
        assert!(seq(b"\x1b[999999999~").is_empty());
    }
}
