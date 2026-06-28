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
//! selection layer, paste insertion) consume the result unchanged. Byte→key and
//! `Cb`→mouse mappings mirror crossterm's own parser so the owned-input path is
//! behaviourally identical to the legacy `EventStream` path for ordinary keys.
//!
//! Fail-open: an unrecognised or malformed sequence yields an empty result (it
//! is dropped, never leaked as text, never a panic).

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use super::tokenize::Token;

/// ESC introducer.
const ESC: u8 = 0x1b;
/// Bracketed-paste start marker (`CSI 200 ~`).
const PASTE_START: &[u8] = b"\x1b[200~";
/// Bracketed-paste end marker (`CSI 201 ~`).
const PASTE_END: &[u8] = b"\x1b[201~";

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
                    self.paste_buf.push_str(&text);
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
            self.paste_buf.clear();
            return Vec::new();
        }
        if bytes == PASTE_END {
            let body = std::mem::take(&mut self.paste_buf);
            self.in_paste = false;
            return vec![InputEvent::Paste(body)];
        }
        if self.in_paste {
            // A sequence inside a paste is treated as literal body text.
            self.paste_buf.push_str(&String::from_utf8_lossy(bytes));
            return Vec::new();
        }
        decode_sequence(bytes)
    }
}

/// Build a `Press` key event.
fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
    KeyEvent::new_with_kind(code, mods, KeyEventKind::Press)
}

/// Map one char of a text token to a key event, mirroring crossterm's own
/// single-byte mapping (so the owned path is identical for ordinary keys).
fn char_to_key(c: char) -> KeyEvent {
    match c {
        '\r' => key(KeyCode::Enter, KeyModifiers::NONE),
        '\t' => key(KeyCode::Tab, KeyModifiers::NONE),
        '\u{7f}' => key(KeyCode::Backspace, KeyModifiers::NONE),
        '\0' => key(KeyCode::Char(' '), KeyModifiers::CONTROL),
        c if ('\u{1}'..='\u{1a}').contains(&c) => {
            // Ctrl-A..Ctrl-Z (incl. \n=Ctrl-J, \b=Ctrl-H in raw mode).
            let letter = (c as u8 - 1 + b'a') as char;
            key(KeyCode::Char(letter), KeyModifiers::CONTROL)
        }
        c if ('\u{1c}'..='\u{1f}').contains(&c) => {
            let d = (c as u8 - 0x1c + b'4') as char;
            key(KeyCode::Char(d), KeyModifiers::CONTROL)
        }
        c if c.is_uppercase() => key(KeyCode::Char(c), KeyModifiers::SHIFT),
        c => key(KeyCode::Char(c), KeyModifiers::NONE),
    }
}

/// Decode a text token into one key event per char.
fn decode_text(text: &str) -> Vec<InputEvent> {
    text.chars()
        .map(|c| InputEvent::Key(char_to_key(c)))
        .collect()
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

    let final_byte = *bytes.last().expect("len checked >= 3");
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
            Some('\u{7f}') => KeyCode::Backspace,
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
    let last = *bytes.last().expect("non-empty");
    if last != b'M' && last != b'm' {
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
    fn alt_letter_decodes() {
        let k = one_key(&seq(b"\x1bc"));
        assert_eq!(k.code, KeyCode::Char('c'));
        assert!(k.modifiers.contains(KeyModifiers::ALT));
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
    fn text_yields_one_key_per_char() {
        let out = text("ab");
        assert_eq!(out.len(), 2);
        assert_eq!(one_key(&out[..1]).code, KeyCode::Char('a'));
    }

    #[test]
    fn control_bytes_in_text_map_to_keys() {
        assert_eq!(one_key(&text("\r")).code, KeyCode::Enter);
        assert_eq!(one_key(&text("\t")).code, KeyCode::Tab);
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
