//! The ONE shared key-mapping layer both input paths route through
//! (structural-debt Wave 2, P0 — input-path convergence).
//!
//! UmaDev has two input paths: the owned byte tokenizer (unix default —
//! [`super::tokenize`] + [`super::decode`]) and the legacy
//! `crossterm::EventStream` (Windows default / `UMADEV_LEGACY_INPUT=1`).
//! Historically every key/paste/focus behaviour was implemented twice — the
//! owned decoder mapped `0x08`/`0x7f` to Backspace while the crossterm path
//! relied on per-arm `KeyCode::Char('\u{8}' | '\u{7f}')` catches inside the
//! app — so a fix landing on one path only was the signature of the whole
//! Windows bug class.
//!
//! This module is the convergence point:
//!
//! - the internal `char_to_key` function is the single char→key table. The owned decoder consumes
//!   it directly (its text bytes ARE chars); the legacy path reaches the same
//!   table through [`normalize_key`].
//! - [`normalize_key`] folds any *literal control-char* key form a backend may
//!   surface (`Char('\u{8}')` for Backspace from Windows/ConPTY,
//!   `Char('\u{3}')` for Ctrl+C, …) through that same table, preserving the
//!   incoming modifiers. It is idempotent, so applying it to an
//!   already-normalized event is a no-op.
//! - [`normalize_event`] lifts [`normalize_key`] to whole
//!   [`crossterm::event::Event`]s; [`super::reader::InputSource::next`] routes
//!   BOTH paths through it, so the app can never observe a divergent event
//!   stream. `App::apply_key_with_mods` applies [`normalize_key`] once more as
//!   a delegating catch for direct callers (tests, future surfaces) — same
//!   table, still one mapping.
//!
//! The cross-path contract tests in [`super`] (`mod contract`) assert that the
//! owned pipeline and the legacy crossterm delivery produce IDENTICAL events
//! for the same logical input; any future divergence fails a test instead of
//! shipping to Windows users.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// Build a `Press` key event (the kind both paths emit for a fresh keypress).
pub(crate) fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
    KeyEvent::new_with_kind(code, mods, KeyEventKind::Press)
}

/// The single char→key table: map one char of terminal input to a key event,
/// mirroring crossterm's own single-byte mapping so the owned path is
/// identical to the legacy path for ordinary keys.
///
/// Consumed directly by the owned decoder ([`super::decode`]) for text tokens
/// and indirectly by the legacy path via [`normalize_key`] — the ONE place
/// control bytes acquire meaning.
pub(crate) fn char_to_key(c: char) -> KeyEvent {
    match c {
        '\r' => key(KeyCode::Enter, KeyModifiers::NONE),
        '\t' => key(KeyCode::Tab, KeyModifiers::NONE),
        '\u{8}' | '\u{7f}' => key(KeyCode::Backspace, KeyModifiers::NONE),
        // A literal ESC char never reaches here from the tokenizer (ESC always
        // opens a sequence), but the table is total so `normalize_key` can fold
        // a backend-surfaced `Char('\u{1b}')` to the Esc key.
        '\u{1b}' => key(KeyCode::Esc, KeyModifiers::NONE),
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

/// Normalize one `(code, modifiers)` pair so both input paths agree.
///
/// A backend that surfaces a C0 control byte / DEL as a literal
/// `KeyCode::Char` (Windows/ConPTY Backspace as `Char('\u{8}')` or
/// `Char('\u{7f}')`, a raw Ctrl-C as `Char('\u{3}')`, …) is folded through
/// the internal `char_to_key` function — the SAME table the owned decoder uses — with the incoming
/// modifiers preserved (so an Alt-prefixed BS/DEL becomes Alt+Backspace on
/// both paths). Everything else passes through unchanged; a pair that is
/// already normalized stays fixed (idempotent).
///
/// The CONTROL guard mirrors the historical per-arm catches: a control char
/// that arrives WITH an explicit CONTROL modifier is an intentional
/// already-decoded combo from the backend and is left alone (fail-open).
///
/// On Windows an AltGr character is then folded to plain text (the internal
/// `fold_altgr_text` function).
#[must_use]
pub fn normalize_key(code: KeyCode, mods: KeyModifiers) -> (KeyCode, KeyModifiers) {
    let (code, mods) = match code {
        KeyCode::Char(c)
            if (c <= '\u{1f}' || c == '\u{7f}') && !mods.contains(KeyModifiers::CONTROL) =>
        {
            let k = char_to_key(c);
            (k.code, k.modifiers | mods)
        }
        _ => (code, mods),
    };
    if cfg!(windows) {
        fold_altgr_text(code, mods)
    } else {
        (code, mods)
    }
}

/// Fold a Windows AltGr character to plain text.
///
/// The Windows console reports AltGr as LEFT_CTRL + RIGHT_ALT, so a character
/// typed with it (`@ { } [ ] \ | ~ €` on German / French / Nordic layouts,
/// `ą ę ł ó ś ż` on Polish Programmer) arrives as `Char(c)` with CONTROL | ALT
/// and every text arm (`!ctrl && !alt`) dropped it. A printable char other than
/// an ASCII letter is text: CONTROL | ALT are stripped (SHIFT is kept). A real
/// Ctrl+Alt+letter arrives as the base ASCII letter and keeps both modifiers,
/// so the Ctrl+Alt+U/B/D/F bindings still fire. Applied only on Windows: unix
/// terminals send AltGr text unmodified, and a Ctrl+Alt+Space or Ctrl+Alt+\
/// there is a real chord.
#[must_use]
pub(crate) fn fold_altgr_text(code: KeyCode, mods: KeyModifiers) -> (KeyCode, KeyModifiers) {
    let altgr = KeyModifiers::CONTROL | KeyModifiers::ALT;
    match code {
        KeyCode::Char(c)
            if mods.contains(altgr) && !c.is_ascii_alphabetic() && !c.is_control() =>
        {
            (code, mods - altgr)
        }
        _ => (code, mods),
    }
}

/// Lift [`normalize_key`] to a whole terminal [`Event`]: key events are
/// normalized (kind and state preserved), every other event passes through
/// unchanged. Idempotent — safe to apply on both input paths.
#[must_use]
pub fn normalize_event(ev: Event) -> Event {
    match ev {
        Event::Key(k) => {
            let (code, modifiers) = normalize_key(k.code, k.modifiers);
            Event::Key(KeyEvent {
                code,
                modifiers,
                ..k
            })
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_bs_and_del_fold_to_backspace() {
        for c in ['\u{8}', '\u{7f}'] {
            assert_eq!(
                normalize_key(KeyCode::Char(c), KeyModifiers::NONE),
                (KeyCode::Backspace, KeyModifiers::NONE),
                "literal {c:?} must fold to Backspace"
            );
        }
    }

    #[test]
    fn alt_prefixed_bs_and_del_fold_to_alt_backspace() {
        for c in ['\u{8}', '\u{7f}'] {
            assert_eq!(
                normalize_key(KeyCode::Char(c), KeyModifiers::ALT),
                (KeyCode::Backspace, KeyModifiers::ALT),
                "Alt + literal {c:?} must fold to Alt+Backspace (delete word)"
            );
        }
    }

    #[test]
    fn altgr_text_folds_to_plain_chars_while_ctrl_alt_letters_stay_chords() {
        let altgr = KeyModifiers::CONTROL | KeyModifiers::ALT;
        // German / French / Nordic / Polish Programmer AltGr characters.
        for c in ['@', '{', '}', '[', ']', '\\', '|', '~', '€', 'ą', 'ł', 'µ', '²'] {
            assert_eq!(
                fold_altgr_text(KeyCode::Char(c), altgr),
                (KeyCode::Char(c), KeyModifiers::NONE),
                "AltGr {c:?} is text"
            );
        }
        // AltGr+Shift keeps SHIFT (an uppercase AltGr letter).
        assert_eq!(
            fold_altgr_text(KeyCode::Char('Ł'), altgr | KeyModifiers::SHIFT),
            (KeyCode::Char('Ł'), KeyModifiers::SHIFT)
        );
        // A real Ctrl+Alt+letter arrives as the base ASCII letter: still a chord.
        for c in ['u', 'b', 'd', 'f', 'U'] {
            assert_eq!(
                fold_altgr_text(KeyCode::Char(c), altgr),
                (KeyCode::Char(c), altgr),
                "Ctrl+Alt+{c} must stay a chord"
            );
        }
        // Only the CONTROL|ALT pair means AltGr; other keys pass through.
        assert_eq!(
            fold_altgr_text(KeyCode::Char('@'), KeyModifiers::CONTROL),
            (KeyCode::Char('@'), KeyModifiers::CONTROL)
        );
        assert_eq!(
            fold_altgr_text(KeyCode::Char('@'), KeyModifiers::ALT),
            (KeyCode::Char('@'), KeyModifiers::ALT)
        );
        assert_eq!(fold_altgr_text(KeyCode::Enter, altgr), (KeyCode::Enter, altgr));
        // Idempotent: a folded pair folds to itself.
        let once = fold_altgr_text(KeyCode::Char('€'), altgr);
        assert_eq!(fold_altgr_text(once.0, once.1), once);
    }

    #[test]
    fn normalize_key_folds_altgr_only_on_windows() {
        let altgr = KeyModifiers::CONTROL | KeyModifiers::ALT;
        let folded = normalize_key(KeyCode::Char('@'), altgr);
        if cfg!(windows) {
            assert_eq!(folded, (KeyCode::Char('@'), KeyModifiers::NONE));
        } else {
            // Unix terminals send AltGr text unmodified; CONTROL|ALT there is a
            // real chord (e.g. `ESC NUL` = Ctrl+Alt+Space) and stays one.
            assert_eq!(folded, (KeyCode::Char('@'), altgr));
        }
        // Ctrl+Alt+D stays a chord everywhere.
        assert_eq!(
            normalize_key(KeyCode::Char('d'), altgr),
            (KeyCode::Char('d'), altgr)
        );
    }

    #[test]
    fn ctrl_modified_literal_is_left_alone() {
        // An explicit CONTROL modifier means the backend already decoded the
        // combo — fail-open, pass through untouched.
        assert_eq!(
            normalize_key(KeyCode::Char('\u{8}'), KeyModifiers::CONTROL),
            (KeyCode::Char('\u{8}'), KeyModifiers::CONTROL)
        );
    }

    #[test]
    fn literal_control_chars_fold_like_the_owned_decoder() {
        // Raw Ctrl-C byte surfaced as a literal char → Ctrl+C.
        assert_eq!(
            normalize_key(KeyCode::Char('\u{3}'), KeyModifiers::NONE),
            (KeyCode::Char('c'), KeyModifiers::CONTROL)
        );
        // Raw ESC char → Esc key.
        assert_eq!(
            normalize_key(KeyCode::Char('\u{1b}'), KeyModifiers::NONE),
            (KeyCode::Esc, KeyModifiers::NONE)
        );
        // CR / TAB literals → Enter / Tab.
        assert_eq!(
            normalize_key(KeyCode::Char('\r'), KeyModifiers::NONE),
            (KeyCode::Enter, KeyModifiers::NONE)
        );
        assert_eq!(
            normalize_key(KeyCode::Char('\t'), KeyModifiers::NONE),
            (KeyCode::Tab, KeyModifiers::NONE)
        );
    }

    #[test]
    fn printable_and_special_keys_pass_through() {
        assert_eq!(
            normalize_key(KeyCode::Char('a'), KeyModifiers::NONE),
            (KeyCode::Char('a'), KeyModifiers::NONE)
        );
        assert_eq!(
            normalize_key(KeyCode::Char('A'), KeyModifiers::SHIFT),
            (KeyCode::Char('A'), KeyModifiers::SHIFT)
        );
        assert_eq!(
            normalize_key(KeyCode::Backspace, KeyModifiers::NONE),
            (KeyCode::Backspace, KeyModifiers::NONE)
        );
        assert_eq!(
            normalize_key(KeyCode::F(5), KeyModifiers::NONE),
            (KeyCode::F(5), KeyModifiers::NONE)
        );
    }

    #[test]
    fn normalize_key_is_idempotent() {
        let samples = [
            (KeyCode::Char('\u{8}'), KeyModifiers::NONE),
            (KeyCode::Char('\u{7f}'), KeyModifiers::ALT),
            (KeyCode::Char('\u{3}'), KeyModifiers::NONE),
            (KeyCode::Char('x'), KeyModifiers::NONE),
            (KeyCode::Backspace, KeyModifiers::NONE),
            (KeyCode::Enter, KeyModifiers::SHIFT),
            (
                KeyCode::Char('@'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
            (
                KeyCode::Char('\u{4}'),
                KeyModifiers::ALT,
            ),
        ];
        for (code, mods) in samples {
            let once = normalize_key(code, mods);
            let twice = normalize_key(once.0, once.1);
            assert_eq!(once, twice, "normalize must be a fixpoint for {code:?}");
        }
    }

    #[test]
    fn normalize_event_maps_keys_and_passes_everything_else() {
        // A key event is normalized, kind preserved.
        let raw = KeyEvent::new_with_kind(
            KeyCode::Char('\u{7f}'),
            KeyModifiers::NONE,
            KeyEventKind::Repeat,
        );
        match normalize_event(Event::Key(raw)) {
            Event::Key(k) => {
                assert_eq!(k.code, KeyCode::Backspace);
                assert_eq!(k.kind, KeyEventKind::Repeat, "kind must be preserved");
            }
            other => panic!("expected a key event, got {other:?}"),
        }
        // Non-key events pass through untouched.
        assert_eq!(
            normalize_event(Event::Paste("hi".into())),
            Event::Paste("hi".into())
        );
        assert_eq!(normalize_event(Event::FocusGained), Event::FocusGained);
        assert_eq!(
            normalize_event(Event::Resize(80, 24)),
            Event::Resize(80, 24)
        );
    }
}
