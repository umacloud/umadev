//! Own fd 0 and turn raw bytes into [`crossterm::event::Event`]s (UX maturity
//! roadmap §2, P1) — the runtime half of the byte-tokenizer root fix.
//!
//! A blocking thread reads raw bytes from stdin into a [`tokio::sync::mpsc`]
//! channel (honouring `#![forbid(unsafe_code)]` — `std::io::stdin().lock()` on a
//! thread, never a `libc::read`). [`OwnedInput`] feeds those bytes through the
//! [`Tokenizer`] + [`Decoder`] and exposes a single async [`OwnedInput::next`]
//! that yields one `Event` at a time, so the existing event loop's
//! key/mouse/paste/resize arm consumes it unchanged.
//!
//! Three things the owned source arms internally:
//! - **bytes** — `mpsc` chunk → `tokenizer.feed` → `decoder` → event queue;
//! - **a 50 ms ESC-flush timer** — armed only while the tokenizer holds a
//!   buffered incomplete escape (a lone `\x1b`); on fire, if more bytes are
//!   already queued the continuation is ingested instead (the re-arm trick), and
//!   only a genuinely idle FD flushes the buffered `\x1b` as a real Esc;
//! - **SIGWINCH** — owning fd 0 means crossterm's `Event::Resize` (which it
//!   derived from SIGWINCH) is gone, so we install our own safe `tokio::signal`
//!   handler and synthesize `Event::Resize` from [`crossterm::terminal::size`].
//!
//! The legacy `crossterm::EventStream` path is retained behind
//! [`legacy_input_from_env`] (`UMADEV_LEGACY_INPUT=1`) so a tokenizer bug in the
//! field is one env var away from reverting.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crossterm::event::{Event, EventStream};
use futures::StreamExt;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use super::decode::{Decoder, InputEvent};
use super::tokenize::Tokenizer;

/// Default lone-ESC flush timeout. Deferred-verdict window: a real Esc resolves
/// within this long; a split arrow's continuation arrives far sooner (within the
/// same input burst), so it completes the sequence before the timer fires.
const DEFAULT_ESC_FLUSH_MS: u64 = 50;

/// Read-chunk size for the stdin reader thread. One `read()` returns whatever is
/// available up to this; a large paste arrives in a few chunks, not byte-by-byte.
const READ_CHUNK: usize = 4096;

/// Whether to use the legacy `crossterm::EventStream` input path instead of the
/// owned byte-tokenizer (`UMADEV_LEGACY_INPUT=1`). The de-risk escape hatch: the
/// owned tokenizer is the DEFAULT, but a field bug reverts with one env var.
#[must_use]
pub fn legacy_input_from_env() -> bool {
    std::env::var("UMADEV_LEGACY_INPUT").is_ok_and(|v| {
        let v = v.trim();
        v == "1" || v.eq_ignore_ascii_case("true")
    })
}

/// The ESC-flush timeout, env-overridable via `UMADEV_ESC_FLUSH_MS` and clamped
/// to a sane `1..=1000` ms range (a `0` would flush every lone ESC instantly and
/// resurrect the phantom-Esc race; a huge value would make Esc feel laggy).
fn esc_flush_interval() -> Duration {
    let ms = std::env::var("UMADEV_ESC_FLUSH_MS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&v| (1..=1000).contains(&v))
        .unwrap_or(DEFAULT_ESC_FLUSH_MS);
    Duration::from_millis(ms)
}

/// Spawn the blocking stdin reader thread and return the byte channel.
///
/// `std::io::stdin().lock()` on a dedicated thread (NOT `libc::read` — no
/// `unsafe`). Each `read()` returns the bytes currently available; we forward
/// them to the async side. Fail-open: EOF, a send error (receiver dropped), or
/// a non-interrupt read error all end the thread cleanly; the receiver then sees
/// the channel close and the input source degrades gracefully (the rest of the
/// app keeps running). On process exit a thread still blocked in `read()` is
/// reaped by the OS — it is intentionally detached, never joined.
fn spawn_stdin_reader() -> UnboundedReceiver<Vec<u8>> {
    let (tx, rx): (UnboundedSender<Vec<u8>>, _) = tokio::sync::mpsc::unbounded_channel();
    // If the thread can't even be spawned, `spawn` consumes + drops the closure
    // (and its captured `tx`), so the channel closes immediately and the receiver
    // degrades gracefully (fail-open: input is dead, the app still runs).
    let _ = std::thread::Builder::new()
        .name("umadev-stdin".into())
        .spawn(move || {
            use std::io::Read as _;
            let stdin = std::io::stdin();
            let mut lock = stdin.lock();
            let mut buf = [0u8; READ_CHUNK];
            loop {
                match lock.read(&mut buf) {
                    Ok(0) => break, // EOF
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break; // receiver gone
                        }
                    }
                    // A signal interrupted the read — just loop and try again.
                    Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        });
    rx
}

/// The SIGWINCH (terminal-resize) signal stream the owned source selects on. On
/// non-unix there is no such signal; the field is `None` and the arm stays inert
/// (mirrors the SIGCONT R5 handling in the event loop).
#[cfg(unix)]
type WinchSignal = tokio::signal::unix::Signal;
/// See [`WinchSignal`] — the non-unix placeholder.
#[cfg(not(unix))]
type WinchSignal = ();

/// Register the SIGWINCH listener (terminal resize). tokio installs the handler
/// safely (no `unsafe`, no work in signal context). `None` on non-unix / if
/// registration fails (fail-open: resize self-heal just won't fire).
fn register_winch_signal() -> Option<WinchSignal> {
    #[cfg(unix)]
    {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change()).ok()
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Await the next SIGWINCH. Inert (never resolves) on non-unix / if registration
/// failed, so the select! arm stays dormant rather than busy-looping.
async fn next_winch(sig: &mut Option<WinchSignal>) {
    #[cfg(unix)]
    {
        match sig.as_mut() {
            Some(s) => {
                let _ = s.recv().await;
            }
            None => std::future::pending::<()>().await,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = sig;
        std::future::pending::<()>().await;
    }
}

/// Sleep until `deadline`, or never (when `None`) — so the ESC-flush arm is a
/// plain always-enabled select! branch (no precondition) that simply parks when
/// no flush is pending.
async fn sleep_until_opt(deadline: Option<Instant>) {
    match deadline {
        Some(d) => {
            // Saturating: a past deadline fires immediately.
            tokio::time::sleep_until(tokio::time::Instant::from_std(d)).await;
        }
        None => std::future::pending::<()>().await,
    }
}

/// The owned stdin input source: byte channel + tokenizer + decoder + a ready
/// event queue + the ESC-flush deadline + the SIGWINCH listener.
pub struct OwnedInput {
    /// Raw byte chunks from the reader thread.
    rx: UnboundedReceiver<Vec<u8>>,
    /// Boundary tokenizer (persistent buffer across reads).
    tokenizer: Tokenizer,
    /// Token → event decoder (persistent paste state).
    decoder: Decoder,
    /// Decoded events ready to hand out, one per [`OwnedInput::next`] call.
    queue: VecDeque<Event>,
    /// When `Some`, the instant at which a buffered lone-ESC should flush to a
    /// real Esc (unless its continuation arrives first).
    esc_deadline: Option<Instant>,
    /// The configured ESC-flush window.
    esc_interval: Duration,
    /// SIGWINCH listener (resize) — `None` on non-unix / registration failure.
    winch: Option<WinchSignal>,
    /// Whether the reader channel has closed (thread ended). Disables the recv
    /// arm so the source parks instead of busy-looping on `None`.
    closed: bool,
}

impl OwnedInput {
    /// Create the owned source: spawn the reader thread, register SIGWINCH.
    #[must_use]
    pub fn new() -> Self {
        Self {
            rx: spawn_stdin_reader(),
            tokenizer: Tokenizer::for_stdin(),
            decoder: Decoder::new(),
            queue: VecDeque::new(),
            esc_deadline: None,
            esc_interval: esc_flush_interval(),
            winch: register_winch_signal(),
            closed: false,
        }
    }

    /// Feed a byte chunk through the tokenizer + decoder, enqueueing events, then
    /// (re)arm or disarm the ESC-flush deadline.
    fn ingest(&mut self, bytes: &[u8]) {
        for token in self.tokenizer.feed(bytes) {
            for ev in self.decoder.feed_token(token) {
                if let Some(event) = ev.into_event() {
                    self.queue.push_back(event);
                }
            }
        }
        self.update_esc_deadline();
    }

    /// Force-flush a buffered incomplete escape (the lone-ESC → Esc verdict).
    fn flush_escape(&mut self) {
        for token in self.tokenizer.flush() {
            for ev in self.decoder.feed_token(token) {
                if let Some(event) = ev.into_event() {
                    self.queue.push_back(event);
                }
            }
        }
        self.esc_deadline = None;
    }

    /// (Re)arm the flush deadline iff an incomplete escape is buffered. Reset on
    /// every ingest so a still-incomplete sequence extends the window (mirrors a
    /// mature reference terminal's re-arm-on-each-input behaviour).
    fn update_esc_deadline(&mut self) {
        if self.tokenizer.has_pending_escape() {
            self.esc_deadline = Some(Instant::now() + self.esc_interval);
        } else {
            self.esc_deadline = None;
        }
    }

    /// Yield the next input event, awaiting bytes / the ESC-flush timer /
    /// SIGWINCH as needed. Returns `None` only on a hard end (never for an idle
    /// terminal); a closed reader channel parks instead so the rest of the loop
    /// keeps running.
    pub async fn next(&mut self) -> Option<std::io::Result<Event>> {
        loop {
            if let Some(event) = self.queue.pop_front() {
                return Some(Ok(event));
            }
            let deadline = self.esc_deadline;
            tokio::select! {
                chunk = self.rx.recv(), if !self.closed => {
                    match chunk {
                        Some(bytes) => self.ingest(&bytes),
                        None => self.closed = true,
                    }
                }
                () = sleep_until_opt(deadline) => {
                    // The flush timer fired. Re-arm trick: if a continuation is
                    // already queued (a heavy render blocked the loop past the
                    // timeout), ingest it instead of flushing — so a split arrow
                    // completes and no phantom Esc surfaces. Only a genuinely
                    // idle FD flushes the buffered lone ESC as a real Esc.
                    self.esc_deadline = None;
                    match self.rx.try_recv() {
                        Ok(bytes) => self.ingest(&bytes),
                        Err(_) => self.flush_escape(),
                    }
                }
                () = next_winch(&mut self.winch) => {
                    if let Ok((cols, rows)) = crossterm::terminal::size() {
                        self.queue.push_back(Event::Resize(cols, rows));
                    }
                }
            }
        }
    }
}

impl Default for OwnedInput {
    fn default() -> Self {
        Self::new()
    }
}

impl InputEvent {
    /// Convert a decoded event to a crossterm [`Event`] for the unified event
    /// loop. A [`InputEvent::Response`] (terminal query reply) maps to `None` —
    /// it is dropped, never surfaced as input.
    fn into_event(self) -> Option<Event> {
        match self {
            InputEvent::Key(k) => Some(Event::Key(k)),
            InputEvent::Mouse(m) => Some(Event::Mouse(m)),
            InputEvent::Paste(p) => Some(Event::Paste(p)),
            InputEvent::Focus(true) => Some(Event::FocusGained),
            InputEvent::Focus(false) => Some(Event::FocusLost),
            InputEvent::Resize(c, r) => Some(Event::Resize(c, r)),
            InputEvent::Response(_) => None,
        }
    }
}

/// The event-loop input source: the owned byte tokenizer (default) or the
/// legacy `crossterm::EventStream` (escape hatch). Both expose one async
/// [`InputSource::next`] returning `Option<io::Result<Event>>`, so the event
/// loop's `select!` arm is identical for either — the gate is a clean branch at
/// setup, never a per-event check.
pub enum InputSource {
    /// The owned byte-tokenizer source (default).
    Owned(Box<OwnedInput>),
    /// The legacy crossterm stream (`UMADEV_LEGACY_INPUT=1`).
    Legacy(Box<EventStream>),
}

impl InputSource {
    /// Construct the source per the escape-hatch env gate.
    #[must_use]
    pub fn from_env() -> Self {
        if legacy_input_from_env() {
            InputSource::Legacy(Box::new(EventStream::new()))
        } else {
            InputSource::Owned(Box::<OwnedInput>::default())
        }
    }

    /// Whether this is the owned tokenizer path. Used to bypass the legacy
    /// `MouseSeqFilter` backstop (the tokenizer subsumes it, and re-buffering a
    /// resolved Esc through the filter would re-introduce the very Esc latency
    /// the root fix removes).
    #[must_use]
    pub fn is_owned(&self) -> bool {
        matches!(self, InputSource::Owned(_))
    }

    /// Await the next terminal event.
    pub async fn next(&mut self) -> Option<std::io::Result<Event>> {
        match self {
            InputSource::Owned(o) => o.next().await,
            InputSource::Legacy(s) => s.next().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_input_env_gate() {
        // Process-global env: snapshot, force, restore.
        let prev = std::env::var("UMADEV_LEGACY_INPUT").ok();
        std::env::remove_var("UMADEV_LEGACY_INPUT");
        assert!(!legacy_input_from_env(), "default is the owned tokenizer");
        std::env::set_var("UMADEV_LEGACY_INPUT", "1");
        assert!(legacy_input_from_env(), "=1 selects the legacy path");
        std::env::set_var("UMADEV_LEGACY_INPUT", "true");
        assert!(legacy_input_from_env(), "=true also selects legacy");
        std::env::set_var("UMADEV_LEGACY_INPUT", "0");
        assert!(!legacy_input_from_env(), "=0 stays on the owned path");
        match prev {
            Some(v) => std::env::set_var("UMADEV_LEGACY_INPUT", v),
            None => std::env::remove_var("UMADEV_LEGACY_INPUT"),
        }
    }

    #[test]
    fn esc_flush_interval_clamps() {
        let prev = std::env::var("UMADEV_ESC_FLUSH_MS").ok();
        std::env::remove_var("UMADEV_ESC_FLUSH_MS");
        assert_eq!(
            esc_flush_interval(),
            Duration::from_millis(DEFAULT_ESC_FLUSH_MS)
        );
        std::env::set_var("UMADEV_ESC_FLUSH_MS", "0");
        assert_eq!(
            esc_flush_interval(),
            Duration::from_millis(DEFAULT_ESC_FLUSH_MS),
            "0 is rejected (clamped to default)"
        );
        std::env::set_var("UMADEV_ESC_FLUSH_MS", "120");
        assert_eq!(esc_flush_interval(), Duration::from_millis(120));
        std::env::set_var("UMADEV_ESC_FLUSH_MS", "999999");
        assert_eq!(
            esc_flush_interval(),
            Duration::from_millis(DEFAULT_ESC_FLUSH_MS),
            "out-of-range is rejected"
        );
        match prev {
            Some(v) => std::env::set_var("UMADEV_ESC_FLUSH_MS", v),
            None => std::env::remove_var("UMADEV_ESC_FLUSH_MS"),
        }
    }

    #[test]
    fn input_event_into_event_maps_surface() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        assert!(matches!(
            InputEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)).into_event(),
            Some(Event::Key(_))
        ));
        assert!(matches!(
            InputEvent::Paste("x".into()).into_event(),
            Some(Event::Paste(_))
        ));
        assert!(matches!(
            InputEvent::Focus(true).into_event(),
            Some(Event::FocusGained)
        ));
        assert!(matches!(
            InputEvent::Focus(false).into_event(),
            Some(Event::FocusLost)
        ));
        assert!(matches!(
            InputEvent::Resize(80, 24).into_event(),
            Some(Event::Resize(80, 24))
        ));
        // A terminal response is dropped (not surfaced as input).
        assert!(InputEvent::Response(b"\x1b[?1c".to_vec())
            .into_event()
            .is_none());
    }
}
