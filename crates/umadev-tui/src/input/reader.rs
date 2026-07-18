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
//! - **a PASTE-STATE-AWARE flush timer** (Wave 2 P1) — armed only while the
//!   tokenizer holds a buffered incomplete escape OR the decoder has an open
//!   bracketed paste, and only when no more bytes are already queued (the
//!   pre-gate). The window is ~50 ms normally (a real Esc resolves fast) but
//!   ~500 ms while in paste — a buffered partial mid-paste is almost certainly
//!   a split `\x1b[201~` end marker whose continuation is in flight, and
//!   force-flushing it at 50 ms was the old paste-wedge. On fire, queued bytes
//!   are ingested instead (the re-arm trick); only a genuinely idle FD flushes
//!   — resolving a lone `\x1b` to Esc, or force-closing a paste whose end
//!   marker is not coming;
//! - **SIGWINCH** — owning fd 0 means crossterm's `Event::Resize` (which it
//!   derived from SIGWINCH) is gone, so we install our own safe `tokio::signal`
//!   handler and synthesize `Event::Resize` from [`crossterm::terminal::size`].
//!
//! The legacy `crossterm::EventStream` path is retained behind
//! [`legacy_input_from_env`] (`UMADEV_LEGACY_INPUT=1`) so a tokenizer bug in the
//! field is one env var away from reverting. It is also the default on Windows:
//! console keys such as Esc and arrows can arrive as Windows input records rather
//! than stdin bytes, and crossterm's native backend is the correct reader there.

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

/// Default flush timeout while a bracketed paste is OPEN (Wave 2 P1). Mid-paste
/// there is no lone-Esc-vs-arrow verdict to hurry: a buffered partial escape is
/// almost certainly a split `\x1b[201~` end marker whose continuation is in
/// flight (slow SSH, terminal chunking), so the window is an order of magnitude
/// more patient than the 50 ms Esc window — the value a mature reference
/// terminal uses. Only a paste whose fd is genuinely idle this long is declared
/// dead and force-closed.
const DEFAULT_PASTE_FLUSH_MS: u64 = 500;

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

/// The in-paste flush timeout, env-overridable via `UMADEV_PASTE_FLUSH_MS` and
/// clamped to `1..=5000` ms (a `0` would force-close every paste with a split
/// end marker instantly — the very wedge this window exists to prevent; a huge
/// value would leave a dead paste swallowing input for too long).
fn paste_flush_interval() -> Duration {
    let ms = std::env::var("UMADEV_PASTE_FLUSH_MS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&v| (1..=5000).contains(&v))
        .unwrap_or(DEFAULT_PASTE_FLUSH_MS);
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

/// Parse an OSC 11 background-color reply and classify its luminance.
/// Components may contain one to four hex digits, as allowed by xterm.
fn osc11_light_theme(bytes: &[u8]) -> Option<bool> {
    let body = bytes.strip_prefix(b"\x1b]11;rgb:")?;
    let body = body
        .strip_suffix(b"\x07")
        .or_else(|| body.strip_suffix(b"\x1b\\"))?;
    let text = std::str::from_utf8(body).ok()?;
    let mut parts = text.split('/');
    let scale = |component: &str| -> Option<u64> {
        if component.is_empty() || component.len() > 4 {
            return None;
        }
        let value = u64::from_str_radix(component, 16).ok()?;
        let max = (1_u64 << (component.len() * 4)) - 1;
        Some(value.saturating_mul(65_535) / max)
    };
    let r = scale(parts.next()?)?;
    let g = scale(parts.next()?)?;
    let b = scale(parts.next()?)?;
    if parts.next().is_some() {
        return None;
    }
    let luminance = 2_126 * r + 7_152 * g + 722 * b;
    Some(luminance >= 5_000 * 65_535)
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
/// event queue + the paste-state-aware flush deadline + the SIGWINCH listener.
pub struct OwnedInput {
    /// Raw byte chunks from the reader thread.
    rx: UnboundedReceiver<Vec<u8>>,
    /// Boundary tokenizer (persistent buffer across reads).
    tokenizer: Tokenizer,
    /// Token → event decoder (persistent paste state).
    decoder: Decoder,
    /// Decoded events ready to hand out, one per [`OwnedInput::next`] call.
    queue: VecDeque<Event>,
    /// When `Some`, the instant at which the pending input (a buffered lone
    /// ESC, or an open paste on an idle fd) should force-flush — unless its
    /// continuation arrives first.
    flush_deadline: Option<Instant>,
    /// The configured ESC-flush window (used while NOT in a paste).
    esc_interval: Duration,
    /// The configured in-paste flush window (Wave 2 P1 — patient enough that a
    /// split `\x1b[201~` end marker's continuation always wins the race).
    paste_interval: Duration,
    /// SIGWINCH listener (resize) — `None` on non-unix / registration failure.
    winch: Option<WinchSignal>,
    /// Whether the reader channel has closed (thread ended). Disables the recv
    /// arm so the source parks instead of busy-looping on `None`.
    closed: bool,
    /// Captured OSC 11 background classification. Terminal replies flow through
    /// the same reader as keys, so the probe cannot race or corrupt stdin.
    background_reply: Option<bool>,
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
            flush_deadline: None,
            esc_interval: esc_flush_interval(),
            paste_interval: paste_flush_interval(),
            winch: register_winch_signal(),
            closed: false,
            background_reply: None,
        }
    }

    /// Decode-side event sink shared by ingest and flush. OSC 11 is captured;
    /// every terminal response is kept out of the user-input queue.
    fn enqueue(&mut self, ev: InputEvent) {
        if let InputEvent::Response(bytes) = &ev {
            if let Some(is_light) = osc11_light_theme(bytes) {
                self.background_reply = Some(is_light);
            }
        }
        if let Some(event) = ev.into_event() {
            self.queue.push_back(event);
        }
    }

    /// Take the captured OSC 11 background classification, if one arrived.
    #[must_use]
    pub fn take_background_reply(&mut self) -> Option<bool> {
        self.background_reply.take()
    }

    /// Feed a byte chunk through the tokenizer + decoder, enqueueing events, then
    /// (re)arm or disarm the flush deadline.
    fn ingest(&mut self, bytes: &[u8]) {
        for token in self.tokenizer.feed(bytes) {
            for ev in self.decoder.feed_token(token) {
                self.enqueue(ev);
            }
        }
        self.update_flush_deadline();
    }

    /// Force-flush the pending input on a genuinely idle fd: a buffered
    /// incomplete escape resolves (the lone-ESC → Esc verdict), and an open
    /// paste whose end marker is not coming is force-closed so `in_paste` can
    /// never wedge input (Wave 2 P1 — this replaces the decoder's deleted
    /// split-marker backstop with a decision made where BOTH states are
    /// visible).
    fn flush_pending(&mut self) {
        for token in self.tokenizer.flush() {
            for ev in self.decoder.feed_token(token) {
                self.enqueue(ev);
            }
        }
        // Still in a paste after the tokenizer flushed? The fd has been idle
        // past the paste window — the terminator is not coming. Deliver the
        // buffered body and free the input path.
        if let Some(ev) = self.decoder.force_close_paste() {
            self.enqueue(ev);
        }
        self.flush_deadline = None;
    }

    /// (Re)arm the flush deadline iff an incomplete escape is buffered or a
    /// bracketed paste is open, using the PASTE-STATE-AWARE window (Wave 2 P1):
    /// ~50 ms normally, ~500 ms while in paste. Reset on every ingest so
    /// still-incomplete input extends the window (a mature reference terminal's
    /// re-arm-on-each-input behaviour).
    ///
    /// Pre-gate: when more bytes are ALREADY queued on the reader channel, the
    /// deadline is not armed at all — the continuation will be ingested before
    /// any flush verdict could be correct, so arming would only race it. The
    /// `try_recv` re-check on fire remains the second gate for bytes that
    /// arrive while the timer is in flight.
    fn update_flush_deadline(&mut self) {
        let pending = self.tokenizer.has_pending_escape() || self.decoder.in_paste();
        if !pending || !self.rx.is_empty() {
            self.flush_deadline = None;
            return;
        }
        let window = if self.decoder.in_paste() {
            self.paste_interval
        } else {
            self.esc_interval
        };
        self.flush_deadline = Some(Instant::now() + window);
    }

    /// Yield the next input event, awaiting bytes / the paste-state-aware flush
    /// timer / SIGWINCH as needed. Returns `None` only on a hard end (never for
    /// an idle terminal); a closed reader channel parks instead so the rest of
    /// the loop keeps running.
    pub async fn next(&mut self) -> Option<std::io::Result<Event>> {
        loop {
            if let Some(event) = self.queue.pop_front() {
                return Some(Ok(event));
            }
            let deadline = self.flush_deadline;
            tokio::select! {
                chunk = self.rx.recv(), if !self.closed => {
                    match chunk {
                        Some(bytes) => self.ingest(&bytes),
                        None => self.closed = true,
                    }
                }
                () = sleep_until_opt(deadline) => {
                    // The flush timer fired. Re-arm trick (the second gate,
                    // after the arm-time pre-gate): if a continuation is
                    // already queued (a heavy render blocked the loop past the
                    // timeout), ingest it instead of flushing — so a split
                    // arrow / paste end-marker completes and no phantom Esc or
                    // truncated paste surfaces. Only a genuinely idle FD
                    // flushes: a buffered lone ESC resolves to a real Esc, a
                    // dead paste is force-closed.
                    self.flush_deadline = None;
                    match self.rx.try_recv() {
                        Ok(bytes) => self.ingest(&bytes),
                        Err(_) => self.flush_pending(),
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

    /// Pump stdin for up to `budget`, just long enough to capture a fast OSC 11
    /// background-color reply, WITHOUT dequeuing any input.
    ///
    /// Bytes are fed through the normal tokenizer/decoder path, so a reply is
    /// captured into `background_reply` (see `enqueue`) while any real
    /// keystrokes that arrive in the same window are queued for the ordinary
    /// [`Self::next`] loop — nothing is dropped. Returns as soon as a reply
    /// lands (the common case, a few milliseconds) or when `budget` elapses (a
    /// terminal that never answers, e.g. Apple Terminal.app). Fail-open: a
    /// closed reader channel returns early and any parse miss simply leaves the
    /// startup default in place. Called once, before the first frame, so a
    /// terminal that DOES support OSC 11 gets the correct palette on the FIRST
    /// paint instead of flashing the default until the asynchronous reply lands.
    pub async fn prime_background_theme(&mut self, budget: Duration) {
        if self.background_reply.is_some() {
            return;
        }
        let deadline = Instant::now() + budget;
        while self.background_reply.is_none() {
            tokio::select! {
                chunk = self.rx.recv(), if !self.closed => {
                    let Some(bytes) = chunk else {
                        self.closed = true;
                        return;
                    };
                    self.ingest(&bytes);
                }
                () = sleep_until_opt(Some(deadline)) => return,
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
    /// it is dropped, never surfaced as input. Crate-visible so the cross-path
    /// contract tests (`super::contract`) can drive the full owned pipeline.
    pub(crate) fn into_event(self) -> Option<Event> {
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
        if cfg!(windows) || legacy_input_from_env() {
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
    ///
    /// BOTH arms emerge through the ONE shared mapping
    /// ([`super::keymap::normalize_event`], Wave 2 P0): the owned decoder is
    /// already normalized by construction (it consumes the same
    /// `keymap::char_to_key` table — the call is an idempotent no-op), and the
    /// legacy crossterm stream has its literal control-char key forms
    /// (Windows/ConPTY Backspace as `Char('\u{8}')`/`Char('\u{7f}')`, …)
    /// folded here — so the event loop can never observe a divergent stream.
    pub async fn next(&mut self) -> Option<std::io::Result<Event>> {
        let ev = match self {
            InputSource::Owned(o) => o.next().await,
            InputSource::Legacy(s) => s.next().await,
        };
        ev.map(|r| r.map(super::keymap::normalize_event))
    }

    /// Take an OSC 11 background reply. The legacy crossterm path has no response
    /// lane, so capability queries are only sent for the owned reader.
    #[must_use]
    pub fn take_background_reply(&mut self) -> Option<bool> {
        match self {
            InputSource::Owned(o) => o.take_background_reply(),
            InputSource::Legacy(_) => None,
        }
    }

    /// Pre-paint OSC 11 drain — see [`OwnedInput::prime_background_theme`]. A
    /// no-op on the legacy path (it has no response lane). Bounded and
    /// fail-open, so it never blocks startup.
    pub async fn prime_background_theme(&mut self, budget: Duration) {
        if let InputSource::Owned(o) = self {
            o.prime_background_theme(budget).await;
        }
    }
}

/// Test-only: a hermetic owned source — no stdin reader thread, no SIGWINCH, a
/// pre-closed channel — carrying a pre-captured OSC 11 background classification
/// (`background_reply`). Lets the reply-apply path be exercised without a live
/// terminal (e.g. `None` for the "terminal never answered" fail-open case).
#[cfg(test)]
#[must_use]
pub(crate) fn owned_test_source(background_reply: Option<bool>) -> InputSource {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
    InputSource::Owned(Box::new(OwnedInput {
        rx,
        tokenizer: Tokenizer::for_stdin(),
        decoder: Decoder::new(),
        queue: VecDeque::new(),
        flush_deadline: None,
        esc_interval: esc_flush_interval(),
        paste_interval: paste_flush_interval(),
        winch: None,
        closed: true,
        background_reply,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct EnvRestore {
        key: &'static str,
        prev: Option<String>,
    }

    impl EnvRestore {
        fn capture(key: &'static str) -> Self {
            Self {
                key,
                prev: std::env::var(key).ok(),
            }
        }

        fn set(&self, value: &str) {
            std::env::set_var(self.key, value);
        }

        fn remove(&self) {
            std::env::remove_var(self.key);
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            match self.prev.as_ref() {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[test]
    fn legacy_input_env_gate() {
        let _guard = ENV_LOCK.lock().unwrap();
        let env = EnvRestore::capture("UMADEV_LEGACY_INPUT");
        env.remove();
        assert!(!legacy_input_from_env(), "default is the owned tokenizer");
        env.set("1");
        assert!(legacy_input_from_env(), "=1 selects the legacy path");
        env.set("true");
        assert!(legacy_input_from_env(), "=true also selects legacy");
        env.set("0");
        assert!(!legacy_input_from_env(), "=0 stays on the owned path");
    }

    #[tokio::test]
    #[cfg(not(windows))]
    async fn input_source_defaults_to_owned_off_windows() {
        let _guard = ENV_LOCK.lock().unwrap();
        let env = EnvRestore::capture("UMADEV_LEGACY_INPUT");
        env.remove();
        assert!(InputSource::from_env().is_owned());
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn input_source_defaults_to_legacy_on_windows() {
        let _guard = ENV_LOCK.lock().unwrap();
        let env = EnvRestore::capture("UMADEV_LEGACY_INPUT");
        env.remove();
        assert!(
            !InputSource::from_env().is_owned(),
            "Windows console Esc/arrows are native input records, not stdin bytes"
        );
    }

    #[test]
    fn esc_flush_interval_clamps() {
        let _guard = ENV_LOCK.lock().unwrap();
        let env = EnvRestore::capture("UMADEV_ESC_FLUSH_MS");
        env.remove();
        assert_eq!(
            esc_flush_interval(),
            Duration::from_millis(DEFAULT_ESC_FLUSH_MS)
        );
        env.set("0");
        assert_eq!(
            esc_flush_interval(),
            Duration::from_millis(DEFAULT_ESC_FLUSH_MS),
            "0 is rejected (clamped to default)"
        );
        env.set("120");
        assert_eq!(esc_flush_interval(), Duration::from_millis(120));
        env.set("999999");
        assert_eq!(
            esc_flush_interval(),
            Duration::from_millis(DEFAULT_ESC_FLUSH_MS),
            "out-of-range is rejected"
        );
    }

    /// A bare `OwnedInput` around a hand-made byte channel — no stdin reader
    /// thread, no SIGWINCH — so the decode/capture path is testable hermetically.
    fn owned_for_test() -> (OwnedInput, UnboundedSender<Vec<u8>>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (
            OwnedInput {
                rx,
                tokenizer: Tokenizer::for_stdin(),
                decoder: Decoder::new(),
                queue: VecDeque::new(),
                flush_deadline: None,
                esc_interval: Duration::from_millis(DEFAULT_ESC_FLUSH_MS),
                paste_interval: Duration::from_millis(DEFAULT_PASTE_FLUSH_MS),
                winch: None,
                closed: false,
                background_reply: None,
            },
            tx,
        )
    }

    #[test]
    fn paste_flush_interval_clamps() {
        let _guard = ENV_LOCK.lock().unwrap();
        let env = EnvRestore::capture("UMADEV_PASTE_FLUSH_MS");
        env.remove();
        assert_eq!(
            paste_flush_interval(),
            Duration::from_millis(DEFAULT_PASTE_FLUSH_MS)
        );
        env.set("0");
        assert_eq!(
            paste_flush_interval(),
            Duration::from_millis(DEFAULT_PASTE_FLUSH_MS),
            "0 is rejected (would resurrect the paste-wedge)"
        );
        env.set("800");
        assert_eq!(paste_flush_interval(), Duration::from_millis(800));
        env.set("999999");
        assert_eq!(
            paste_flush_interval(),
            Duration::from_millis(DEFAULT_PASTE_FLUSH_MS),
            "out-of-range is rejected"
        );
    }

    #[test]
    fn flush_deadline_is_paste_state_aware() {
        // Wave 2 P1 — the flush decision must SEE paste state: a buffered
        // partial escape gets the ~50 ms Esc window normally, but the ~500 ms
        // paste window while a bracketed paste is open (a partial mid-paste is
        // almost certainly a split end marker whose continuation is in flight).
        let (mut oi, _tx) = owned_for_test();
        let esc = oi.esc_interval;
        let paste = oi.paste_interval;
        assert!(paste > esc, "the paste window must be the patient one");

        // A lone ESC outside a paste → the short Esc window.
        let before = Instant::now();
        oi.ingest(b"\x1b");
        let d = oi.flush_deadline.expect("a pending escape arms the timer");
        assert!(
            d <= before + esc + Duration::from_millis(20),
            "outside a paste the SHORT Esc window applies"
        );

        // The same partial escape INSIDE an open paste → the long paste window.
        let (mut oi, _tx) = owned_for_test();
        let before = Instant::now();
        oi.ingest(b"\x1b[200~hello\x1b[20");
        let d = oi
            .flush_deadline
            .expect("a split end-marker arms the timer");
        assert!(
            d > before + esc,
            "in paste the deadline must OUTLIVE the Esc window"
        );
        assert!(
            d <= before + paste + Duration::from_millis(20),
            "in paste the ~500 ms paste window applies"
        );

        // An open paste with NO partial escape (end marker never sent) still
        // arms the paste window, so a dead paste cannot wedge input forever.
        let (mut oi, _tx) = owned_for_test();
        oi.ingest(b"\x1b[200~hello");
        assert!(!oi.tokenizer.has_pending_escape());
        assert!(
            oi.flush_deadline.is_some(),
            "an open paste alone must arm the flush timer"
        );
    }

    #[test]
    fn flush_deadline_pre_gate_skips_arming_while_bytes_are_queued() {
        // Wave 2 P1 pre-gate: when the continuation is ALREADY on the reader
        // channel, arming a flush deadline could only race it — don't arm.
        let (mut oi, tx) = owned_for_test();
        tx.send(b"[A".to_vec()).unwrap();
        oi.ingest(b"\x1b");
        assert!(oi.tokenizer.has_pending_escape());
        assert!(
            oi.flush_deadline.is_none(),
            "queued bytes must suppress the flush deadline"
        );
        // Ingest the queued continuation: the arrow completes, nothing pends.
        let chunk = oi.rx.try_recv().unwrap();
        oi.ingest(&chunk);
        assert!(oi.flush_deadline.is_none());
        assert!(matches!(
            oi.queue.front(),
            Some(Event::Key(k)) if k.code == crossterm::event::KeyCode::Up
        ));
    }

    #[test]
    fn split_paste_end_marker_closes_cleanly_at_every_byte_position() {
        // The normal path (the paste-aware window means the timer never fires
        // during a real paste): a bracketed paste whose reads split ANYWHERE —
        // including mid `\x1b[201~` end marker — decodes to exactly ONE clean
        // Paste, with no decoder-side backstop needed.
        let input = b"\x1b[200~build me a dashboard\nwith login\x1b[201~";
        let want = "build me a dashboard\nwith login";
        for split in 0..=input.len() {
            let (mut oi, _tx) = owned_for_test();
            oi.ingest(&input[..split]);
            oi.ingest(&input[split..]);
            let events: Vec<Event> = std::mem::take(&mut oi.queue).into();
            match events.as_slice() {
                [Event::Paste(body)] => {
                    assert_eq!(body, want, "split at {split}: body must be intact");
                }
                other => panic!("split at {split}: expected one Paste, got {other:?}"),
            }
            assert!(!oi.decoder.in_paste(), "split at {split}: paste must close");
        }
    }

    #[test]
    fn flush_fired_mid_paste_never_wedges_input() {
        // The pathological path: even if the fd goes genuinely idle past the
        // paste window at ANY split point (so the flush force-closes the paste
        // mid-marker), input must never wedge — later keystrokes still decode.
        let input = b"\x1b[200~requirement\x1b[201~";
        for split in 0..=input.len() {
            let (mut oi, _tx) = owned_for_test();
            oi.ingest(&input[..split]);
            oi.flush_pending(); // the worst case: the timer fired right here
            oi.ingest(&input[split..]);
            oi.queue.clear();
            oi.ingest(b"x");
            assert!(
                matches!(
                    oi.queue.back(),
                    Some(Event::Key(k)) if k.code == crossterm::event::KeyCode::Char('x')
                ),
                "split at {split}: input must stay alive after a mid-paste flush"
            );
            assert!(
                !oi.decoder.in_paste(),
                "split at {split}: the decoder must not stay wedged in paste"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn split_end_marker_with_a_slow_gap_still_one_clean_paste() {
        // End-to-end through `next()` with virtual time: the end marker's
        // continuation arrives 200 ms after the first chunk — far past the old
        // 50 ms ESC window (which force-flushed mid-marker: the paste-wedge),
        // comfortably inside the 500 ms paste window. The result must be ONE
        // clean Paste with the marker fully stripped.
        let (mut oi, tx) = owned_for_test();
        tx.send(b"\x1b[200~hello\x1b[20".to_vec()).unwrap();
        let sender = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            tx.send(b"1~".to_vec()).unwrap();
            tx // keep the channel open so `next` never sees a close
        });
        let ev = oi.next().await.expect("an event").expect("no io error");
        assert_eq!(
            ev,
            Event::Paste("hello".into()),
            "the split end-marker must close the paste cleanly"
        );
        let _tx = sender.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn dead_paste_force_closes_at_the_paste_window_and_frees_input() {
        // A paste whose end marker NEVER arrives: after ~500 ms of genuine fd
        // idleness (not the 50 ms Esc window) the reader force-closes it,
        // delivering the buffered body — and input stays alive afterwards.
        let (mut oi, tx) = owned_for_test();
        tx.send(b"\x1b[200~hello".to_vec()).unwrap();
        let t0 = tokio::time::Instant::now();
        let ev = oi.next().await.expect("an event").expect("no io error");
        assert_eq!(ev, Event::Paste("hello".into()));
        let waited = t0.elapsed();
        assert!(
            waited >= Duration::from_millis(DEFAULT_PASTE_FLUSH_MS),
            "the force-close must wait the PASTE window, waited {waited:?}"
        );
        assert!(
            waited < Duration::from_millis(DEFAULT_PASTE_FLUSH_MS * 2),
            "but not longer than one window, waited {waited:?}"
        );
        // Input is not wedged: a later keystroke still arrives.
        tx.send(b"x".to_vec()).unwrap();
        let ev = oi.next().await.expect("an event").expect("no io error");
        assert!(matches!(ev, Event::Key(k) if k.code == crossterm::event::KeyCode::Char('x')));
    }

    #[tokio::test(start_paused = true)]
    async fn lone_esc_still_resolves_at_the_short_esc_window() {
        // The paste-aware window must NOT slow down the lone-Esc verdict: an
        // idle fd holding a bare `\x1b` still resolves at ~50 ms, not 500 ms.
        let (mut oi, tx) = owned_for_test();
        tx.send(b"\x1b".to_vec()).unwrap();
        let t0 = tokio::time::Instant::now();
        let ev = oi.next().await.expect("an event").expect("no io error");
        assert!(
            matches!(ev, Event::Key(k) if k.code == crossterm::event::KeyCode::Esc),
            "a genuinely lone ESC resolves to a real Esc key"
        );
        let waited = t0.elapsed();
        assert!(
            waited < Duration::from_millis(DEFAULT_PASTE_FLUSH_MS),
            "Esc latency must stay on the SHORT window, waited {waited:?}"
        );
        drop(tx);
    }

    #[test]
    fn osc11_reply_classifies_light_dark_and_component_widths() {
        assert_eq!(
            osc11_light_theme(b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\"),
            Some(true)
        );
        assert_eq!(osc11_light_theme(b"\x1b]11;rgb:00/00/00\x07"), Some(false));
        assert_eq!(osc11_light_theme(b"\x1b]11;rgb:f/f/f\x07"), Some(true));
        assert_eq!(osc11_light_theme(b"\x1b]10;rgb:ffff/ffff/ffff\x07"), None);
        assert_eq!(osc11_light_theme(b"not a reply"), None);
    }

    #[test]
    fn background_reply_is_captured_and_never_leaks_as_input() {
        let (mut oi, _tx) = owned_for_test();
        oi.ingest(b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\");
        assert!(
            oi.queue.is_empty(),
            "the OSC reply must be consumed, never surfaced as keystrokes"
        );
        assert_eq!(oi.take_background_reply(), Some(true));
        assert_eq!(oi.take_background_reply(), None, "the take is one-shot");
    }

    #[test]
    fn background_reply_split_across_reads_does_not_consume_typing() {
        // The reply can straddle a read boundary and be followed by real input
        // in the same chunk — the tokenizer reassembles the sequence, the
        // verdict is captured, and ONLY the real keys surface.
        let (mut oi, _tx) = owned_for_test();
        oi.ingest(b"\x1b]11;rgb:0000/");
        assert!(oi.queue.is_empty(), "an incomplete reply emits nothing");
        oi.ingest(b"0000/0000\x1b\\hi");
        assert_eq!(oi.take_background_reply(), Some(false));
        let keys: Vec<Event> = std::mem::take(&mut oi.queue).into();
        assert_eq!(
            keys.len(),
            2,
            "exactly the two real keystrokes surface, none of the reply: {keys:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn prime_background_theme_captures_a_fast_reply_before_paint() {
        // The pre-paint drain must catch an OSC 11 reply that is already on the
        // wire, and any keystrokes riding in the same chunk stay queued for the
        // ordinary input loop — none are lost or surfaced early.
        let (mut oi, tx) = owned_for_test();
        tx.send(b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\hi".to_vec())
            .unwrap();
        oi.prime_background_theme(Duration::from_millis(120)).await;
        assert_eq!(
            oi.take_background_reply(),
            Some(true),
            "a light reply available before the first frame must be captured"
        );
        let keys: Vec<Event> = std::mem::take(&mut oi.queue).into();
        assert_eq!(keys.len(), 2, "the two typed-ahead keys are preserved");
        let _tx = tx;
    }

    #[tokio::test(start_paused = true)]
    async fn prime_background_theme_is_bounded_when_no_reply_arrives() {
        // Apple Terminal.app answers neither OSC 11 nor COLORFGBG: the drain
        // must return within its budget (fail-open) rather than block startup,
        // leaving the caller's default in place.
        let (mut oi, tx) = owned_for_test();
        let t0 = tokio::time::Instant::now();
        oi.prime_background_theme(Duration::from_millis(120)).await;
        assert!(
            t0.elapsed() >= Duration::from_millis(120),
            "the drain must wait its full budget when nothing answers"
        );
        assert_eq!(
            oi.take_background_reply(),
            None,
            "no reply means no classification — the startup default stands"
        );
        let _tx = tx;
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
