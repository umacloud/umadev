//! Byte-level terminal-input tokenizer — the **root fix** for the leaked-mouse
//! / phantom-Esc bug class (UX maturity roadmap §2, P1).
//!
//! ## Why this exists
//!
//! crossterm's own parser eagerly emits [`crossterm::event::KeyCode::Esc`] when
//! a read ends on a lone `\x1b` with no more bytes immediately available, then
//! discards that byte. Interactive stdin rarely fills its read buffer, so a read
//! that happens to end exactly on the `\x1b` of an SGR mouse report
//! (`\x1b[<b;x;yM`) makes crossterm emit a **phantom Esc** and the continuation
//! `[<b;x;yM` arrives anchorless — leaking as raw text in the prompt AND firing
//! a false "press Esc again to interrupt" during a wheel-scroll.
//!
//! The mature fix is to own the byte stream and tokenize it ourselves: an
//! incomplete escape sequence at end-of-input is **buffered** (never discarded),
//! and a lone `\x1b` is **buffered** rather than eagerly classified — the
//! lone-Esc verdict is deferred to a timed flush (driven by the reader, see
//! `super::reader`).
//!
//! ## Model (mirrors a mature reference terminal's `tokenize` state machine)
//!
//! A pure state machine — `Ground / Escape / EscapeIntermediate / Csi / Ss3 /
//! Osc / Dcs / Apc` — with a **persistent `buffer` carried across `feed()`
//! calls**. [`Tokenizer::feed`] returns [`Token`]s: text runs and complete
//! escape **sequences**. The whole point is the invariant:
//!
//! - an SGR mouse report is **one** [`Token::Sequence`] whether it arrives whole
//!   or split across N reads (the CSI param bytes `0x30..=0x3F` include `<`; the
//!   CSI final bytes `0x40..=0x7E` include `M`/`m`);
//! - an incomplete sequence at end-of-input is **buffered** (no token emitted)
//!   and never discarded;
//! - a lone `\x1b` is **buffered**, never eagerly a key.
//!
//! When the tokenizer is mid-ground with a partial trailing UTF-8 code point,
//! that partial tail is also buffered so a multibyte char split across reads is
//! never mojibake'd. The two buffered cases are distinguished by [`Tokenizer`]'s
//! `state`: `state == Ground` with a non-empty buffer means a partial UTF-8
//! tail; any other state means a partial escape sequence.

/// ESC — the escape introducer.
const ESC: u8 = 0x1b;
/// BEL — one of the string-terminator forms for OSC/DCS/APC.
const BEL: u8 = 0x07;
/// `\` — the final byte of the `ESC \` (ST) string terminator.
const ST: u8 = 0x5c;

/// Whether `b` is a CSI parameter byte (`0..9 : ; < = > ?`).
#[inline]
fn is_csi_param(b: u8) -> bool {
    (0x30..=0x3f).contains(&b)
}

/// Whether `b` is a CSI intermediate byte (space through `/`).
#[inline]
fn is_csi_intermediate(b: u8) -> bool {
    (0x20..=0x2f).contains(&b)
}

/// Whether `b` is a CSI final byte (`@` through `~`).
#[inline]
fn is_csi_final(b: u8) -> bool {
    (0x40..=0x7e).contains(&b)
}

/// Whether `b` is an ESC-sequence final byte. ESC sequences have a wider final
/// range (`0`..`~`) than CSI.
#[inline]
fn is_esc_final(b: u8) -> bool {
    (0x30..=0x7e).contains(&b)
}

/// Length of the longest prefix of `bytes` that is **complete** valid UTF-8.
///
/// Used at end-of-input in the ground state to hold back a partial trailing
/// code point (so a multibyte char split across reads survives). Genuinely
/// invalid bytes are NOT held back (we emit them and let
/// [`String::from_utf8_lossy`] substitute U+FFFD) — only an *incomplete*
/// trailing sequence is buffered, so a garbage byte can never wedge the buffer.
fn complete_utf8_len(bytes: &[u8]) -> usize {
    match std::str::from_utf8(bytes) {
        Ok(_) => bytes.len(),
        Err(e) => {
            if e.error_len().is_none() {
                // Incomplete trailing code point — keep only the valid prefix.
                e.valid_up_to()
            } else {
                // A genuinely invalid byte somewhere — don't buffer; emit all
                // (lossy) so it can never wedge the buffer forever.
                bytes.len()
            }
        }
    }
}

/// A boundary-detected input token.
///
/// Unlike a semantic parser, the tokenizer only identifies **boundaries**: a run
/// of printable/text bytes, or one complete (or force-flushed) escape sequence.
/// Interpreting a [`Token::Sequence`] into a key / mouse / paste event is
/// [`super::decode`]'s job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    /// A run of non-escape bytes, decoded lossily to text. May contain C0
    /// control bytes (`\r`, `\t`, Ctrl-letters, …) which the decoder maps to
    /// the matching keys.
    Text(String),
    /// One complete escape sequence (CSI / SS3 / OSC / DCS / APC / two-byte
    /// ESC), captured atomically regardless of how the reads were chunked.
    Sequence(Vec<u8>),
}

/// The tokenizer state. A persistent field of [`Tokenizer`], advanced one byte
/// at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum State {
    /// Outside any escape sequence — bytes are text (or the start of one).
    #[default]
    Ground,
    /// Saw `\x1b` — awaiting the sequence-type byte (or a lone-Esc verdict).
    Escape,
    /// Saw `\x1b` then an intermediate byte (e.g. `ESC (` charset) — awaiting
    /// the final byte.
    EscapeIntermediate,
    /// Inside a CSI sequence (`\x1b[` …) — consuming params/intermediates until
    /// a final byte.
    Csi,
    /// Inside an SS3 sequence (`\x1bO` …) — awaiting a single final byte.
    Ss3,
    /// Inside an OSC string (`\x1b]` …) — consuming until BEL or `ESC \`.
    Osc,
    /// Inside a DCS string (`\x1bP` …) — consuming until BEL or `ESC \`.
    Dcs,
    /// Inside an APC string (`\x1b_` …) — consuming until BEL or `ESC \`.
    Apc,
}

/// A streaming byte tokenizer for terminal input.
///
/// Feed raw bytes (any chunking) with [`Tokenizer::feed`]; force any buffered
/// incomplete sequence out as a final [`Token::Sequence`] with
/// [`Tokenizer::flush`] (used by the reader's lone-Esc / FD-idle timeout).
#[derive(Debug)]
pub struct Tokenizer {
    /// Current state, carried across `feed()` calls.
    state: State,
    /// Bytes carried across `feed()` calls: a partial escape sequence (when
    /// `state != Ground`) or a partial trailing UTF-8 code point (when
    /// `state == Ground`).
    buffer: Vec<u8>,
    /// Whether to recognise the legacy X10 mouse form (`\x1b[M` + 3 raw payload
    /// bytes) as one sequence. Enabled for stdin (terminals that honour DECSET
    /// 1000/1002 but not 1006 emit this); see [`Tokenizer::for_stdin`].
    x10_mouse: bool,
}

impl Default for Tokenizer {
    fn default() -> Self {
        Self {
            state: State::Ground,
            buffer: Vec::new(),
            x10_mouse: false,
        }
    }
}

impl Tokenizer {
    /// A tokenizer for **stdin** input — recognises the legacy X10 mouse form in
    /// addition to SGR. `\x1b[M` is also CSI DL (Delete Lines) in *output*
    /// streams, so X10 recognition is only ever enabled here, never for parsing
    /// terminal output.
    #[must_use]
    pub fn for_stdin() -> Self {
        Self {
            x10_mouse: true,
            ..Self::default()
        }
    }

    /// Feed a chunk of bytes and get the tokens that completed. An incomplete
    /// sequence (or a partial trailing UTF-8 code point) at the end is buffered
    /// for the next call, never emitted.
    pub fn feed(&mut self, input: &[u8]) -> Vec<Token> {
        self.run(input, false)
    }

    /// Force any buffered incomplete sequence out as a final [`Token::Sequence`]
    /// and reset to the ground state. Called by the reader on a lone-Esc /
    /// FD-idle timeout so a genuinely lone `\x1b` finally resolves to an Esc key
    /// (and a never-completed partial sequence can't wedge the buffer).
    pub fn flush(&mut self) -> Vec<Token> {
        self.run(&[], true)
    }

    /// Whether the tokenizer is holding an **incomplete escape sequence** (a
    /// lone `\x1b` or a partial CSI/OSC/…). The reader arms its flush timer on
    /// this; a partial UTF-8 tail (ground state) does NOT count.
    #[must_use]
    pub fn has_pending_escape(&self) -> bool {
        self.state != State::Ground && !self.buffer.is_empty()
    }

    /// The bytes currently buffered (incomplete sequence or partial UTF-8).
    /// Exposed for tests / diagnostics.
    #[must_use]
    pub fn pending(&self) -> &[u8] {
        &self.buffer
    }

    /// The core state machine. `input` is appended to the carried `buffer`; the
    /// whole run is tokenized; an incomplete tail is re-buffered (or, when
    /// `flush`, force-emitted).
    fn run(&mut self, input: &[u8], flush: bool) -> Vec<Token> {
        let mut data = std::mem::take(&mut self.buffer);
        data.extend_from_slice(input);
        let len = data.len();

        let mut tokens = Vec::new();
        let mut state = self.state;
        let mut i = 0usize;
        let mut text_start = 0usize;
        // Start of the in-progress sequence. When resuming mid-sequence the
        // carried buffer was prepended, so the sequence starts at index 0.
        let mut seq_start = 0usize;

        // Emit the buffered text run [text_start, i) if non-empty.
        macro_rules! flush_text {
            () => {
                if i > text_start {
                    push_text(&mut tokens, &data[text_start..i]);
                }
            };
        }
        // Emit the in-progress sequence [seq_start, i) and return to ground.
        macro_rules! emit_seq {
            () => {{
                tokens.push(Token::Sequence(data[seq_start..i].to_vec()));
                state = State::Ground;
                text_start = i;
            }};
        }

        while i < len {
            let code = data[i];
            match state {
                State::Ground => {
                    if code == ESC {
                        flush_text!();
                        seq_start = i;
                        state = State::Escape;
                        i += 1;
                    } else {
                        i += 1;
                    }
                }
                State::Escape => {
                    if code == b'[' {
                        state = State::Csi;
                        i += 1;
                    } else if code == b']' {
                        state = State::Osc;
                        i += 1;
                    } else if code == b'P' {
                        state = State::Dcs;
                        i += 1;
                    } else if code == b'_' {
                        state = State::Apc;
                        i += 1;
                    } else if code == b'O' {
                        state = State::Ss3;
                        i += 1;
                    } else if code == ESC {
                        // Double escape — emit the first ESC as its own sequence,
                        // start a fresh one at this byte. The `i > seq_start`
                        // guard is load-bearing for RESUME: when a buffered lone
                        // ESC is re-fed (state carried as Escape, the ESC is
                        // data[0]), this branch re-reads it at i == seq_start, so
                        // we must NOT emit an empty sequence — just advance and
                        // stay in Escape, re-deriving the rest from the next byte.
                        if i > seq_start {
                            tokens.push(Token::Sequence(data[seq_start..i].to_vec()));
                        }
                        seq_start = i;
                        state = State::Escape;
                        i += 1;
                    } else if is_csi_intermediate(code) {
                        state = State::EscapeIntermediate;
                        i += 1;
                    } else if is_esc_final(code) {
                        // Two-byte escape (e.g. Alt+letter `ESC c`).
                        i += 1;
                        emit_seq!();
                    } else {
                        // Invalid after ESC — rewind to ground at seq_start and
                        // reprocess this byte as text. (This branch is ALSO the
                        // resume mechanism: a buffered partial CSI/SS3/Escape is
                        // re-fed with state set and its leading ESC trips this,
                        // re-deriving the sequence from scratch. Do NOT advance
                        // i — the byte is reprocessed in Ground.)
                        state = State::Ground;
                        text_start = seq_start;
                    }
                }
                State::EscapeIntermediate => {
                    if is_csi_intermediate(code) {
                        i += 1;
                    } else if is_esc_final(code) {
                        i += 1;
                        emit_seq!();
                    } else {
                        state = State::Ground;
                        text_start = seq_start;
                    }
                }
                State::Csi => {
                    // Legacy X10 mouse: `\x1b[M` + 3 raw payload bytes (each
                    // >= 0x20). `M` right after `[` (offset 2) and no `<` param
                    // means X10, not SGR. A control byte in any payload slot
                    // means this is actually CSI DL adjacent to another
                    // sequence — fall through to the normal CSI-final handling.
                    let x10 = self.x10_mouse && code == b'M' && i - seq_start == 2;
                    let payload_ok = x10
                        && (i + 1 >= len || data[i + 1] >= 0x20)
                        && (i + 2 >= len || data[i + 2] >= 0x20)
                        && (i + 3 >= len || data[i + 3] >= 0x20);
                    if payload_ok {
                        if i + 4 <= len {
                            i += 4;
                            emit_seq!();
                        } else {
                            // Incomplete X10 payload — buffer from seq_start.
                            i = len;
                        }
                    } else if is_csi_final(code) {
                        i += 1;
                        emit_seq!();
                    } else if is_csi_param(code) || is_csi_intermediate(code) {
                        i += 1;
                    } else {
                        // Invalid CSI — rewind (resume mechanism / fail open).
                        state = State::Ground;
                        text_start = seq_start;
                    }
                }
                State::Ss3 => {
                    if (0x40..=0x7e).contains(&code) {
                        i += 1;
                        emit_seq!();
                    } else {
                        state = State::Ground;
                        text_start = seq_start;
                    }
                }
                State::Osc | State::Dcs | State::Apc => {
                    if code == BEL {
                        i += 1;
                        emit_seq!();
                    } else if code == ESC && i + 1 < len && data[i + 1] == ST {
                        i += 2;
                        emit_seq!();
                    } else {
                        i += 1;
                    }
                }
            }
        }

        // End of input.
        self.state = state;
        if state == State::Ground {
            // Emit complete text; hold back any partial trailing UTF-8 byte(s).
            let tail = &data[text_start..];
            let complete = complete_utf8_len(tail);
            if complete > 0 {
                push_text(&mut tokens, &tail[..complete]);
            }
            self.buffer = tail[complete..].to_vec();
        } else if flush {
            // Force the incomplete sequence out as a final token.
            let remaining = &data[seq_start..];
            if !remaining.is_empty() {
                tokens.push(Token::Sequence(remaining.to_vec()));
            }
            self.state = State::Ground;
            self.buffer.clear();
        } else {
            // Buffer the incomplete sequence for the next feed.
            self.buffer = data[seq_start..].to_vec();
        }

        tokens
    }
}

/// Push a [`Token::Text`] for `bytes` (lossy UTF-8), skipping an empty run.
fn push_text(tokens: &mut Vec<Token>, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    let text = String::from_utf8_lossy(bytes).into_owned();
    if !text.is_empty() {
        tokens.push(Token::Text(text));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole SGR mouse report, as bytes.
    const SGR: &[u8] = b"\x1b[<64;10;5M";

    /// Merge consecutive `Text` tokens (text may legitimately arrive in
    /// fragments depending on chunking; sequences are atomic). The merged stream
    /// is the chunking-invariant identity we assert on.
    fn normalize(tokens: &[Token]) -> Vec<Token> {
        let mut out: Vec<Token> = Vec::new();
        for t in tokens {
            match (out.last_mut(), t) {
                (Some(Token::Text(prev)), Token::Text(next)) => prev.push_str(next),
                _ => out.push(t.clone()),
            }
        }
        out
    }

    /// Feed `input` split at every byte boundary `split` and collect all tokens.
    fn feed_split(input: &[u8], split: usize) -> Vec<Token> {
        let mut tk = Tokenizer::for_stdin();
        let mut out = Vec::new();
        out.extend(tk.feed(&input[..split]));
        out.extend(tk.feed(&input[split..]));
        normalize(&out)
    }

    #[test]
    fn whole_sgr_is_one_sequence() {
        let mut tk = Tokenizer::for_stdin();
        let toks = tk.feed(SGR);
        assert_eq!(toks, vec![Token::Sequence(SGR.to_vec())]);
        assert!(!tk.has_pending_escape());
    }

    #[test]
    fn sgr_split_at_every_boundary_is_invariant() {
        // The heart of the deliverable: an SGR mouse report is ONE Sequence
        // token no matter where the reads split it.
        let whole = {
            let mut tk = Tokenizer::for_stdin();
            normalize(&tk.feed(SGR))
        };
        assert_eq!(whole, vec![Token::Sequence(SGR.to_vec())]);
        for split in 0..=SGR.len() {
            assert_eq!(
                feed_split(SGR, split),
                whole,
                "split at byte {split} must yield an identical token stream"
            );
        }
    }

    #[test]
    fn sgr_fed_one_byte_at_a_time_is_one_sequence() {
        let mut tk = Tokenizer::for_stdin();
        let mut out = Vec::new();
        for (idx, b) in SGR.iter().enumerate() {
            let toks = tk.feed(&[*b]);
            if idx + 1 < SGR.len() {
                // Mid-sequence: nothing emitted, sequence buffered.
                assert!(toks.is_empty(), "no token until the final byte");
                assert!(tk.has_pending_escape());
            }
            out.extend(toks);
        }
        assert_eq!(normalize(&out), vec![Token::Sequence(SGR.to_vec())]);
    }

    #[test]
    fn sgr_interleaved_with_text_is_invariant() {
        let input = b"ab\x1b[<64;10;5Mcd";
        let whole = {
            let mut tk = Tokenizer::for_stdin();
            normalize(&tk.feed(input))
        };
        assert_eq!(
            whole,
            vec![
                Token::Text("ab".into()),
                Token::Sequence(SGR.to_vec()),
                Token::Text("cd".into()),
            ]
        );
        for split in 0..=input.len() {
            assert_eq!(feed_split(input, split), whole, "split at {split}");
        }
    }

    #[test]
    fn lone_esc_is_buffered_not_emitted() {
        let mut tk = Tokenizer::for_stdin();
        let toks = tk.feed(b"\x1b");
        assert!(toks.is_empty(), "a lone ESC is buffered, never eager");
        assert!(tk.has_pending_escape());
        assert_eq!(tk.pending(), b"\x1b");
        // A flush (FD-idle timeout) finally resolves it to a sequence.
        let flushed = tk.flush();
        assert_eq!(flushed, vec![Token::Sequence(b"\x1b".to_vec())]);
        assert!(!tk.has_pending_escape());
    }

    #[test]
    fn lone_esc_then_arrow_is_one_sequence_no_phantom_esc() {
        // ESC arrives in one read, the `[A` continuation in the next: the result
        // is a single arrow sequence — NO phantom Esc token in between.
        let mut tk = Tokenizer::for_stdin();
        assert!(tk.feed(b"\x1b").is_empty());
        let toks = tk.feed(b"[A");
        assert_eq!(toks, vec![Token::Sequence(b"\x1b[A".to_vec())]);
    }

    #[test]
    fn bracketed_paste_markers_split_are_atomic_sequences() {
        let input = b"\x1b[200~hello\x1b[201~";
        let whole = {
            let mut tk = Tokenizer::for_stdin();
            normalize(&tk.feed(input))
        };
        assert_eq!(
            whole,
            vec![
                Token::Sequence(b"\x1b[200~".to_vec()),
                Token::Text("hello".into()),
                Token::Sequence(b"\x1b[201~".to_vec()),
            ]
        );
        for split in 0..=input.len() {
            assert_eq!(feed_split(input, split), whole, "split at {split}");
        }
    }

    #[test]
    fn split_utf8_text_survives() {
        // A 3-byte CJK char ("好" = E5 A5 BD) split mid-char must not mojibake.
        let cjk = "好".as_bytes();
        assert_eq!(cjk.len(), 3);
        let mut tk = Tokenizer::default();
        assert!(tk.feed(&cjk[..1]).is_empty(), "partial UTF-8 held back");
        assert!(tk.feed(&cjk[1..2]).is_empty(), "still partial");
        let toks = tk.feed(&cjk[2..]);
        assert_eq!(toks, vec![Token::Text("好".into())]);
    }

    #[test]
    fn osc_terminated_by_st_is_one_sequence() {
        let input = b"\x1b]0;title\x1b\\rest";
        let mut tk = Tokenizer::for_stdin();
        let toks = normalize(&tk.feed(input));
        assert_eq!(
            toks,
            vec![
                Token::Sequence(b"\x1b]0;title\x1b\\".to_vec()),
                Token::Text("rest".into()),
            ]
        );
    }

    #[test]
    fn x10_mouse_report_is_one_sequence() {
        // `\x1b[M` + 3 payload bytes (Cb, Cx, Cy each >= 0x20).
        let input = b"\x1b[M\x20\x21\x22";
        let mut tk = Tokenizer::for_stdin();
        let toks = tk.feed(input);
        assert_eq!(toks, vec![Token::Sequence(input.to_vec())]);
        // Split anywhere → still one sequence.
        for split in 0..=input.len() {
            assert_eq!(
                feed_split(input, split),
                vec![Token::Sequence(input.to_vec())]
            );
        }
    }

    #[test]
    fn alt_letter_is_one_two_byte_sequence() {
        let mut tk = Tokenizer::for_stdin();
        assert_eq!(tk.feed(b"\x1bc"), vec![Token::Sequence(b"\x1bc".to_vec())]);
    }

    #[test]
    fn plain_text_passes_through() {
        let mut tk = Tokenizer::for_stdin();
        assert_eq!(tk.feed(b"hello"), vec![Token::Text("hello".into())]);
        assert!(!tk.has_pending_escape());
    }

    #[test]
    fn flush_of_partial_csi_does_not_wedge() {
        // A partial CSI that never completes is force-emitted on flush so it
        // can't wedge the buffer forever.
        let mut tk = Tokenizer::for_stdin();
        assert!(tk.feed(b"\x1b[<64;").is_empty());
        assert!(tk.has_pending_escape());
        let flushed = tk.flush();
        assert_eq!(flushed, vec![Token::Sequence(b"\x1b[<64;".to_vec())]);
        assert!(!tk.has_pending_escape());
    }

    #[test]
    fn double_esc_emits_two_sequences() {
        let mut tk = Tokenizer::for_stdin();
        let toks = tk.feed(b"\x1b\x1b");
        assert_eq!(
            toks,
            vec![
                Token::Sequence(b"\x1b".to_vec()),
                // second ESC is still buffered (could be Alt-combo lead)
            ]
        );
        assert!(tk.has_pending_escape());
    }
}
