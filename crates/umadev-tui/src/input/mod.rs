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
//! - [`reader`] — owns the stdin reader thread + a 50 ms lone-ESC flush timer +
//!   a SIGWINCH→resize handler, and exposes [`reader::InputSource`] (the owned
//!   path by default, the legacy `EventStream` behind `UMADEV_LEGACY_INPUT=1`).

pub mod decode;
pub mod reader;
pub mod tokenize;

pub use reader::{legacy_input_from_env, InputSource};
