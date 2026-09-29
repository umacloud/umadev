//! Owned terminal-input pipeline (UX maturity roadmap §2, P1) — the **root fix**
//! for the leaked-mouse / phantom-Esc / Esc-latency bug class.
//!
//! Instead of letting crossterm's `EventStream` parse stdin (where a read that
//! ends on the lone `\x1b` of an SGR mouse report eagerly emits a phantom Esc
//! and discards the byte, leaking the continuation as text), UmaDev **owns fd 0**
//! and tokenizes the byte stream itself:
//!
//! - [`tokenize`] — a pure state machine that carries a persistent buffer across
//!   reads, so an SGR mouse report is one atomic [`tokenize::Token::Sequence`]
//!   however the reads chunk it, an incomplete sequence is buffered (never
//!   discarded), and a lone `\x1b` is buffered (never eagerly a key);
//! - [`decode`] — maps tokens to [`decode::InputEvent`]s using crossterm's own
//!   key/mouse semantics, so downstream handlers consume the result unchanged;
//! - [`keymap`] — the ONE shared key-mapping layer (Wave 2 P0): the char→key
//!   table the owned decoder consumes, plus the [`keymap::normalize_key`] /
//!   [`keymap::normalize_event`] fold that routes the legacy crossterm path
//!   through the SAME table, so both paths emit identical events (locked by
//!   the cross-path contract tests below);
//! - [`reader`] — owns the stdin reader thread + a paste-state-aware flush
//!   timer (~50 ms for a lone ESC, ~500 ms while a bracketed paste is open,
//!   with a queued-bytes pre-gate — Wave 2 P1) + a SIGWINCH→resize handler,
//!   and exposes [`reader::InputSource`] (the owned path by default, the
//!   legacy `EventStream` behind `UMADEV_LEGACY_INPUT=1` / on Windows);
//! - [`legacy`] — that legacy path, which on Windows delivers a burst of
//!   already-queued key events (how the console delivers a paste) as one paste.

pub mod decode;
pub mod keymap;
pub mod legacy;
pub mod reader;
pub mod tokenize;

pub use reader::{legacy_input_from_env, InputSource};

/// Cross-path CONTRACT TESTS (Wave 2 P0 — input-path convergence).
///
/// UmaDev has two input paths: the owned byte tokenizer (unix default) and the
/// legacy crossterm `EventStream` (Windows default / `UMADEV_LEGACY_INPUT=1`).
/// Both must emit an IDENTICAL event stream for the same logical input, or a
/// fix lands on one path only and ships broken to Windows users. Each case
/// below pairs (a) the raw bytes the owned pipeline parses with (b) the
/// crossterm [`Event`]s the legacy path delivers for the same logical input
/// (as crossterm's parser / the Windows console produce them — including the
/// ConPTY literal control-char forms), and asserts both sides normalize to the
/// same stream through [`keymap::normalize_event`] — the exact fold
/// [`reader::InputSource::next`] applies. Any future divergence fails here
/// instead of shipping.
#[cfg(test)]
mod contract {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};

    use super::decode::Decoder;
    use super::keymap::normalize_event;
    use super::tokenize::Tokenizer;

    /// Drive the FULL owned pipeline (tokenizer → decoder → event mapping →
    /// the shared normalize fold) over `bytes`, exactly as `OwnedInput` does.
    fn owned(bytes: &[u8]) -> Vec<Event> {
        owned_chunked(&[bytes])
    }

    /// [`owned`] with explicit read-chunk boundaries (for split-marker cases).
    fn owned_chunked(chunks: &[&[u8]]) -> Vec<Event> {
        let mut tk = Tokenizer::for_stdin();
        let mut dec = Decoder::new();
        let mut out = Vec::new();
        for chunk in chunks {
            for token in tk.feed(chunk) {
                for ev in dec.feed_token(token) {
                    if let Some(e) = ev.into_event() {
                        out.push(normalize_event(e));
                    }
                }
            }
        }
        out
    }

    /// Run the events the LEGACY path delivers through the same normalize fold
    /// `InputSource::next` applies to that path.
    fn legacy(events: &[Event]) -> Vec<Event> {
        events.iter().cloned().map(normalize_event).collect()
    }

    /// A crossterm key-press event (the form both paths compare as).
    fn press(code: KeyCode, mods: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, mods))
    }

    /// Assert every legacy delivery form for one logical input normalizes to
    /// the owned pipeline's stream for the same input.
    fn assert_converges(name: &str, bytes: &[u8], legacy_forms: &[Vec<Event>]) {
        let owned_stream = owned(bytes);
        for (i, form) in legacy_forms.iter().enumerate() {
            assert_eq!(
                owned_stream,
                legacy(form),
                "{name}: owned bytes {bytes:?} vs legacy form #{i} must converge"
            );
        }
    }

    #[test]
    fn backspace_converges_for_bs_del_and_alt_forms() {
        let none = KeyModifiers::NONE;
        let alt = KeyModifiers::ALT;
        // 0x7f (DEL — the common POSIX Backspace byte). Legacy forms: the
        // decoded `Backspace` key (crossterm's unix parser / a Windows native
        // key record) and the ConPTY literal-char form.
        assert_converges(
            "backspace 0x7f",
            b"\x7f",
            &[
                vec![press(KeyCode::Backspace, none)],
                vec![press(KeyCode::Char('\u{7f}'), none)],
            ],
        );
        // 0x08 (BS — the common Windows Terminal / ConPTY Backspace byte).
        assert_converges(
            "backspace 0x08",
            b"\x08",
            &[
                vec![press(KeyCode::Backspace, none)],
                vec![press(KeyCode::Char('\u{8}'), none)],
            ],
        );
        // Alt+Backspace (delete word) — ESC-prefixed on the wire.
        assert_converges(
            "alt+backspace 0x7f",
            b"\x1b\x7f",
            &[
                vec![press(KeyCode::Backspace, alt)],
                vec![press(KeyCode::Char('\u{7f}'), alt)],
            ],
        );
        assert_converges(
            "alt+backspace 0x08",
            b"\x1b\x08",
            &[
                vec![press(KeyCode::Backspace, alt)],
                vec![press(KeyCode::Char('\u{8}'), alt)],
            ],
        );
    }

    #[test]
    fn enter_tab_and_backtab_converge() {
        let none = KeyModifiers::NONE;
        assert_converges("enter", b"\r", &[vec![press(KeyCode::Enter, none)]]);
        assert_converges("tab", b"\t", &[vec![press(KeyCode::Tab, none)]]);
        // Shift+Tab arrives as CSI Z; crossterm delivers BackTab with SHIFT.
        assert_converges(
            "backtab",
            b"\x1b[Z",
            &[vec![press(KeyCode::BackTab, KeyModifiers::SHIFT)]],
        );
    }

    #[test]
    fn esc_prefixed_ctrl_alt_letters_and_alt_enter_converge() {
        // ESC-prefix Meta terminals (tmux, Apple Terminal "Use Option as Meta",
        // xterm without the kitty protocol) send Alt+<control key> as ESC plus
        // the C0 byte. Crossterm's parser decodes the byte after ESC and adds
        // ALT; the owned path must too — never a bare Esc followed by the
        // unmodified control key (Esc + Ctrl+D quit an idle session).
        let alt = KeyModifiers::ALT;
        let ctrl_alt = KeyModifiers::CONTROL | KeyModifiers::ALT;
        for (name, bytes, letter, literal) in [
            ("ctrl+alt+d", b"\x1b\x04" as &[u8], 'd', '\u{4}'),
            ("ctrl+alt+u", b"\x1b\x15", 'u', '\u{15}'),
            ("ctrl+alt+b", b"\x1b\x02", 'b', '\u{2}'),
            ("ctrl+alt+f", b"\x1b\x06", 'f', '\u{6}'),
        ] {
            assert_converges(
                name,
                bytes,
                &[
                    // The decoded combo (crossterm unix parser / Windows native).
                    vec![press(KeyCode::Char(letter), ctrl_alt)],
                    // The literal control-char form carrying ALT.
                    vec![press(KeyCode::Char(literal), alt)],
                ],
            );
        }
        assert_converges(
            "alt+enter",
            b"\x1b\r",
            &[
                vec![press(KeyCode::Enter, alt)],
                vec![press(KeyCode::Char('\r'), alt)],
            ],
        );
        assert_converges("alt+tab", b"\x1b\t", &[vec![press(KeyCode::Tab, alt)]]);
        assert_converges(
            "ctrl+alt+space",
            b"\x1b\x00",
            &[vec![press(KeyCode::Char(' '), ctrl_alt)]],
        );
    }

    #[test]
    fn ctrl_j_universal_newline_converges() {
        // Ctrl+J is a literal LF (0x0A) on every terminal — the terminal-agnostic
        // newline. The owned path tokenizes it as text and folds it to
        // `Char('j')` + CONTROL; the legacy path delivers either the decoded
        // combo or the ConPTY literal-char form. All must converge, so the app's
        // single `Char('j') if ctrl` newline arm fires identically on both.
        assert_converges(
            "ctrl+j (newline)",
            b"\x0a",
            &[
                vec![press(KeyCode::Char('j'), KeyModifiers::CONTROL)],
                vec![press(KeyCode::Char('\u{a}'), KeyModifiers::NONE)],
            ],
        );
    }

    #[test]
    fn shift_enter_via_kitty_csi_u_converges() {
        // With the kitty keyboard protocol enabled (see `setup_terminal`),
        // Shift+Enter is reported as `CSI 13 ; 2 u`. The owned decoder parses it;
        // crossterm's native parser delivers the same Enter+SHIFT — so the
        // app's Shift+Enter newline path is reachable on both input paths.
        assert_converges(
            "shift+enter (CSI-u)",
            b"\x1b[13;2u",
            &[vec![press(KeyCode::Enter, KeyModifiers::SHIFT)]],
        );
    }

    #[test]
    fn modify_other_keys_form_converges() {
        // tmux `extended-keys always` and xterm `modifyOtherKeys` send the
        // `CSI 27 ; mods ; key ~` form. It must reach the app as the same key
        // the Windows console / a kitty terminal delivers natively.
        assert_converges(
            "shift+enter (modifyOtherKeys)",
            b"\x1b[27;2;13~",
            &[vec![press(KeyCode::Enter, KeyModifiers::SHIFT)]],
        );
        assert_converges(
            "ctrl+j (modifyOtherKeys)",
            b"\x1b[27;5;106~",
            &[vec![press(KeyCode::Char('j'), KeyModifiers::CONTROL)]],
        );
    }

    #[test]
    fn arrows_converge_in_csi_and_ss3_forms() {
        let none = KeyModifiers::NONE;
        for (name, bytes, code) in [
            ("up", b"\x1b[A" as &[u8], KeyCode::Up),
            ("down", b"\x1b[B", KeyCode::Down),
            ("right", b"\x1b[C", KeyCode::Right),
            ("left", b"\x1b[D", KeyCode::Left),
            // Application cursor mode (SS3).
            ("up (SS3)", b"\x1bOA", KeyCode::Up),
            ("left (SS3)", b"\x1bOD", KeyCode::Left),
        ] {
            assert_converges(name, bytes, &[vec![press(code, none)]]);
        }
    }

    #[test]
    fn home_end_pgup_pgdn_converge() {
        let none = KeyModifiers::NONE;
        for (name, bytes, code) in [
            ("home (CSI H)", b"\x1b[H" as &[u8], KeyCode::Home),
            ("home (tilde)", b"\x1b[1~", KeyCode::Home),
            ("end (CSI F)", b"\x1b[F", KeyCode::End),
            ("end (tilde)", b"\x1b[4~", KeyCode::End),
            ("pgup", b"\x1b[5~", KeyCode::PageUp),
            ("pgdn", b"\x1b[6~", KeyCode::PageDown),
        ] {
            assert_converges(name, bytes, &[vec![press(code, none)]]);
        }
    }

    #[test]
    fn ctrl_letters_converge_including_the_literal_char_form() {
        let none = KeyModifiers::NONE;
        let ctrl = KeyModifiers::CONTROL;
        for (name, bytes, letter, literal) in [
            ("ctrl+a", b"\x01" as &[u8], 'a', '\u{1}'),
            ("ctrl+c", b"\x03", 'c', '\u{3}'),
            ("ctrl+z", b"\x1a", 'z', '\u{1a}'),
        ] {
            assert_converges(
                name,
                bytes,
                &[
                    // The decoded combo (crossterm unix parser / Windows native).
                    vec![press(KeyCode::Char(letter), ctrl)],
                    // The ConPTY literal control-char form.
                    vec![press(KeyCode::Char(literal), none)],
                ],
            );
        }
    }

    #[test]
    fn f_keys_converge_in_ss3_and_tilde_forms() {
        let none = KeyModifiers::NONE;
        for (name, bytes, n) in [
            ("F1", b"\x1bOP" as &[u8], 1),
            ("F2", b"\x1bOQ", 2),
            ("F3", b"\x1bOR", 3),
            ("F4", b"\x1bOS", 4),
            ("F5", b"\x1b[15~", 5),
            ("F8", b"\x1b[19~", 8),
            ("F12", b"\x1b[24~", 12),
        ] {
            assert_converges(name, bytes, &[vec![press(KeyCode::F(n), none)]]);
        }
    }

    #[test]
    fn focus_in_and_out_converge() {
        // Owned: the decoded `\x1b[I` / `\x1b[O` reports. Legacy: crossterm's
        // native `FocusGained` / `FocusLost` events. The event loop's repaint
        // arm consumes ONE event type either way.
        assert_converges("focus in", b"\x1b[I", &[vec![Event::FocusGained]]);
        assert_converges("focus out", b"\x1b[O", &[vec![Event::FocusLost]]);
    }

    #[test]
    fn mouse_wheel_report_converges() {
        // SGR wheel-up at 1-indexed (10,5) → 0-indexed (9,4) on both paths.
        assert_converges(
            "wheel up",
            b"\x1b[<64;10;5M",
            &[vec![Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 9,
                row: 4,
                modifiers: KeyModifiers::NONE,
            })]],
        );
        assert_converges(
            "wheel down",
            b"\x1b[<65;10;5M",
            &[vec![Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 9,
                row: 4,
                modifiers: KeyModifiers::NONE,
            })]],
        );
    }

    #[test]
    fn bracketed_paste_converges() {
        // A multi-line paste arrives as ONE atomic Paste on both paths.
        assert_converges(
            "bracketed paste",
            b"\x1b[200~hello\nworld\x1b[201~",
            &[vec![Event::Paste("hello\nworld".into())]],
        );
    }

    #[test]
    fn split_paste_end_marker_converges_at_every_byte_position() {
        // The owned path must deliver the SAME single Paste as the legacy path
        // no matter where a read boundary splits the input — including mid
        // `\x1b[201~` end marker (the old paste-wedge shape).
        let input = b"\x1b[200~requirement text\x1b[201~";
        let want = legacy(&[Event::Paste("requirement text".into())]);
        for split in 0..=input.len() {
            assert_eq!(
                owned_chunked(&[&input[..split], &input[split..]]),
                want,
                "split at byte {split} must still converge with the legacy path"
            );
        }
    }
}
