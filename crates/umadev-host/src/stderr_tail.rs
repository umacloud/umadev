//! Shared, bounded STDERR-tail capture for the three continuous-session base
//! drivers (`claude_session` / `codex_session` / `opencode_session`).
//!
//! Each driver drains its base child's STDERR on its own task so a chatty /
//! stuck base can never backpressure the stdout reader. Historically that drain
//! threw every line away — so a broken base config (a bad model id, "not logged
//! in", a config parse error the base prints to stderr *then falls silent*) was
//! invisible: the user only ever saw "base session idle." with no cause.
//!
//! [`StderrTail`] keeps a small ring of the most recent lines (the cause is
//! almost always in the *last* thing the base said), bounded by BOTH a line
//! count and a byte budget so it can never grow without limit. The driver hands
//! a clone to [`drain_stderr_into`] (the new drain task) and exposes the tail to
//! the [`umadev_runtime::BaseSession::stderr_tail`] diagnostic the TUI reads.
//!
//! **Fail-open by contract:** capture must NEVER block or stall the stdout
//! reader. The buffer is behind a [`std::sync::Mutex`] held only for the
//! micro-moment of a push/read; a poisoned lock is recovered (the diagnostic is
//! best-effort, never a reason to crash or block the host), and a full buffer
//! just drops its oldest line.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, BufReader};

/// Keep at most this many trailing stderr lines.
const MAX_LINES: usize = 20;

/// ...and at most this many bytes across those lines (whichever bound trips
/// first evicts the oldest line). ~4 KB is plenty for a base's error banner
/// while staying a hard cap on memory.
const MAX_BYTES: usize = 4 * 1024;

/// A shared, bounded ring of the most-recent stderr lines from a base child.
///
/// Cheap to [`Clone`] (an `Arc`): the driver keeps one handle for
/// [`BaseSession::stderr_tail`](umadev_runtime::BaseSession::stderr_tail) and
/// moves another into the drain task.
#[derive(Clone, Default)]
pub struct StderrTail {
    inner: Arc<Mutex<TailBuf>>,
}

/// The bounded ring behind the shared handle.
#[derive(Default)]
struct TailBuf {
    lines: VecDeque<String>,
    bytes: usize,
}

impl StderrTail {
    /// A fresh, empty tail buffer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Push one captured stderr line, evicting the oldest line(s) until both the
    /// line-count and byte bounds hold. Fail-open: a poisoned lock is recovered
    /// (a prior panic while holding it must not wedge the drain task).
    fn push(&self, line: String) {
        let mut buf = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        // A single oversize line is still truncated to the byte budget so it
        // can't blow the cap on its own.
        let mut line = line;
        if line.len() > MAX_BYTES {
            line.truncate(MAX_BYTES);
        }
        buf.bytes += line.len();
        buf.lines.push_back(line);
        while buf.lines.len() > MAX_LINES || buf.bytes > MAX_BYTES {
            if let Some(old) = buf.lines.pop_front() {
                buf.bytes = buf.bytes.saturating_sub(old.len());
            } else {
                break;
            }
        }
    }

    /// The captured tail as a single newline-joined string, or `None` when
    /// nothing has been captured. Fail-open: a poisoned lock is recovered.
    #[must_use]
    pub fn snapshot(&self) -> Option<String> {
        let buf = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if buf.lines.is_empty() {
            return None;
        }
        Some(buf.lines.iter().cloned().collect::<Vec<_>>().join("\n"))
    }
}

/// Drain a base child's STDERR line-by-line into `tail` until EOF. This is the
/// drop-in replacement for each driver's old drain-to-nowhere task: it keeps the
/// pipe drained (so a noisy base can never backpressure the stdout reader) AND
/// captures the bounded tail for diagnosis.
///
/// Fail-open: a read error simply ends the loop (the pipe is gone); capture
/// never blocks the stdout reader and never panics.
pub async fn drain_stderr_into<R>(stderr: R, tail: StderrTail)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        tail.push(line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_tail_is_none() {
        assert!(StderrTail::new().snapshot().is_none());
    }

    #[test]
    fn snapshot_joins_lines_in_order() {
        let t = StderrTail::new();
        t.push("first".to_string());
        t.push("second".to_string());
        assert_eq!(t.snapshot().as_deref(), Some("first\nsecond"));
    }

    #[test]
    fn bounded_by_line_count_drops_oldest() {
        let t = StderrTail::new();
        for i in 0..(MAX_LINES + 5) {
            t.push(format!("line{i}"));
        }
        let snap = t.snapshot().unwrap();
        let n = snap.lines().count();
        assert_eq!(n, MAX_LINES, "kept at most MAX_LINES lines");
        // The newest line survives; the oldest are evicted.
        assert!(snap.contains(&format!("line{}", MAX_LINES + 4)));
        assert!(!snap.contains("line0\n") && !snap.starts_with("line0"));
    }

    #[test]
    fn bounded_by_bytes_drops_oldest() {
        let t = StderrTail::new();
        // Each line ~1KB; pushing well past MAX_BYTES must keep total under cap.
        let chunk = "x".repeat(1000);
        for _ in 0..20 {
            t.push(chunk.clone());
        }
        let snap = t.snapshot().unwrap();
        assert!(
            snap.len() <= MAX_BYTES + 1, // +1 slack for join newline accounting
            "tail stayed within the byte budget: {}",
            snap.len()
        );
    }

    #[test]
    fn oversize_single_line_is_truncated() {
        let t = StderrTail::new();
        t.push("y".repeat(MAX_BYTES * 2));
        let snap = t.snapshot().unwrap();
        assert!(snap.len() <= MAX_BYTES, "single oversize line truncated");
    }

    #[tokio::test]
    async fn drain_captures_tail_from_a_reader() {
        let data = b"err line one\nerr line two\n" as &[u8];
        let tail = StderrTail::new();
        drain_stderr_into(data, tail.clone()).await;
        let snap = tail.snapshot().unwrap();
        assert!(snap.contains("err line one"));
        assert!(snap.contains("err line two"));
    }
}
