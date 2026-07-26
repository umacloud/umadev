//! Shared, bounded STDERR-tail capture for all five continuous-session bases:
//! three native drivers plus the Grok Build ACP driver.
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
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::task::JoinHandle;

use crate::redaction::redact_text;

/// Keep at most this many trailing stderr lines.
const MAX_LINES: usize = 20;

/// ...and at most this many bytes across those lines (whichever bound trips
/// first evicts the oldest line). ~4 KB is plenty for a base's error banner
/// while staying a hard cap on memory.
const MAX_BYTES: usize = 4 * 1024;
const OVERSIZE_LINE_MARKER: &str = "[stderr line omitted: exceeded 4096-byte safety limit]";

const DRAIN_SHUTDOWN_BUDGET: Duration = Duration::from_millis(250);

/// The largest index `<= max` that lands on a UTF-8 char boundary of `s`, so a
/// `String::truncate` at that index never splits a multibyte char (CJK / emoji)
/// and panics. (`str::floor_char_boundary` is still unstable, so we walk back by
/// hand.) Fail-open: keeps the "capture never panics" contract of this module.
fn floor_char_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() {
        return s.len();
    }
    let mut idx = max;
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

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

/// Owns one stderr drain task for exactly as long as its base session.
pub(crate) struct StderrDrain {
    task: Option<JoinHandle<()>>,
}

impl StderrDrain {
    pub(crate) fn spawn<R>(stderr: R, tail: StderrTail) -> Self
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
    {
        Self {
            task: Some(tokio::spawn(drain_stderr_into(stderr, tail))),
        }
    }

    pub(crate) fn empty() -> Self {
        Self { task: None }
    }

    pub(crate) async fn shutdown(&mut self) {
        self.shutdown_with_budget(DRAIN_SHUTDOWN_BUDGET).await;
    }

    #[cfg(all(test, unix))]
    pub(crate) fn is_active(&self) -> bool {
        self.task.is_some()
    }

    async fn shutdown_with_budget(&mut self, budget: Duration) {
        let Some(mut task) = self.task.take() else {
            return;
        };
        if tokio::time::timeout(budget, &mut task).await.is_ok() {
            return;
        }
        task.abort();
        let _ = tokio::time::timeout(budget, &mut task).await;
    }
}

impl Drop for StderrDrain {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
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
    pub(crate) fn push(&self, line: impl AsRef<str>) {
        let mut buf = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        // A single oversize line is still truncated to the byte budget so it
        // can't blow the cap on its own. Truncate on a UTF-8 char boundary:
        // `String::truncate(MAX_BYTES)` PANICS when the byte index splits a
        // multibyte char (a CJK / emoji stderr banner straddling the cut) —
        // which would violate this module's "capture never panics" contract.
        let mut line = redact_text(line.as_ref());
        if line.len() > MAX_BYTES {
            line.truncate(floor_char_boundary(&line, MAX_BYTES));
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
    let mut reader = stderr;
    let mut line_tail = umadev_process::BoundedTail::new(MAX_BYTES);
    let mut chunk = [0_u8; 8192];
    // Fixed-size reads + a bounded current-line tail preserve lossy decoding
    // without `read_until`'s hidden unbounded Vec growth. We always keep reading
    // after the tail fills so stderr can never backpressure the base.
    loop {
        let read = match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        let mut start = 0;
        for (index, byte) in chunk[..read].iter().enumerate() {
            if *byte == b'\n' {
                line_tail.push(&chunk[start..index]);
                if line_tail.truncated() {
                    // Redaction needs the line's identifying prefix. Keeping
                    // only a bounded suffix first could discard
                    // `Authorization: Bearer` and then persist the secret tail
                    // verbatim, so suppress an oversize line as a unit.
                    tail.push(OVERSIZE_LINE_MARKER);
                } else {
                    let bytes = line_tail.clone().into_bytes();
                    let line = String::from_utf8_lossy(&bytes);
                    tail.push(line.trim_end_matches('\r'));
                }
                line_tail.clear();
                start = index + 1;
            }
        }
        line_tail.push(&chunk[start..read]);
    }
    // Match `read_until` semantics for a final unterminated line.
    if line_tail.total_seen() > 0 {
        if line_tail.truncated() {
            tail.push(OVERSIZE_LINE_MARKER);
        } else {
            let bytes = line_tail.into_bytes();
            let line = String::from_utf8_lossy(&bytes);
            tail.push(line.trim_end_matches('\r'));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[test]
    fn empty_tail_is_none() {
        assert!(StderrTail::new().snapshot().is_none());
    }

    #[test]
    fn snapshot_joins_lines_in_order() {
        let t = StderrTail::new();
        t.push("first");
        t.push("second");
        assert_eq!(t.snapshot().as_deref(), Some("first\nsecond"));
    }

    #[test]
    fn snapshot_never_stores_synthetic_secrets() {
        const SECRET: &str = "SYNTH_STDERR_SECRET_DO_NOT_LEAK_92";
        let t = StderrTail::new();
        t.push(format!("Authorization: Bearer {SECRET}"));
        let snapshot = t.snapshot().expect("redacted diagnostic");
        assert!(!snapshot.contains(SECRET));
        assert!(snapshot.contains("[redacted]"));
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

    #[test]
    fn oversize_multibyte_line_truncates_without_panic() {
        // A CJK stderr banner far larger than MAX_BYTES. `中` is 3 bytes, so
        // MAX_BYTES (4096) is NOT a char boundary (4096 % 3 == 1) — a naive
        // `truncate(MAX_BYTES)` would panic mid-char. The boundary-floored
        // truncation must not panic and must stay within the byte budget while
        // keeping only whole chars.
        let t = StderrTail::new();
        t.push("中".repeat(MAX_BYTES)); // 3 * 4096 bytes
        let snap = t.snapshot().unwrap();
        assert!(
            snap.len() <= MAX_BYTES,
            "multibyte line stayed within budget"
        );
        assert_eq!(snap.len() % 3, 0, "truncation kept whole 3-byte chars");
        assert!(snap.chars().all(|c| c == '中'), "no split/replacement char");
    }

    #[test]
    fn oversize_emoji_line_truncates_on_boundary() {
        // Emoji are 4 bytes; MAX_BYTES (4096) IS divisible by 4, so also test an
        // offset run so the floor must walk back. No panic, valid UTF-8, bounded.
        let t = StderrTail::new();
        let mut line = String::from("x"); // 1-byte lead shifts every emoji boundary
        line.push_str(&"😀".repeat(MAX_BYTES));
        t.push(line);
        let snap = t.snapshot().unwrap();
        assert!(snap.len() <= MAX_BYTES, "emoji line stayed within budget");
        // Valid UTF-8 by construction (it's a String); assert no replacement char
        // crept in from a bad split.
        assert!(!snap.contains('\u{FFFD}'), "no U+FFFD from a split char");
    }

    #[test]
    fn floor_char_boundary_walks_back_to_a_boundary() {
        let s = "中".repeat(3); // 9 bytes, boundaries at 0/3/6/9
        assert_eq!(floor_char_boundary(s.as_str(), 9), 9);
        assert_eq!(floor_char_boundary(s.as_str(), 100), 9);
        assert_eq!(floor_char_boundary(s.as_str(), 4), 3);
        assert_eq!(floor_char_boundary(s.as_str(), 5), 3);
        assert_eq!(floor_char_boundary(s.as_str(), 0), 0);
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

    #[tokio::test]
    async fn newline_free_oversize_stderr_is_safely_suppressed() {
        let mut data = vec![b'x'; MAX_BYTES * 64];
        data.extend_from_slice(b"TAIL-SENTINEL");
        let tail = StderrTail::new();
        drain_stderr_into(data.as_slice(), tail.clone()).await;
        let snapshot = tail.snapshot().unwrap();
        assert_eq!(snapshot, OVERSIZE_LINE_MARKER);
        assert!(!snapshot.contains("TAIL-SENTINEL"));
    }

    #[tokio::test]
    async fn oversize_bearer_line_cannot_leak_after_prefix_eviction() {
        const SECRET_SUFFIX: &str = "SYNTH_LONG_BEARER_SECRET_SUFFIX_7F92";
        let mut data = b"Authorization: Bearer ".to_vec();
        data.extend(std::iter::repeat_n(b's', MAX_BYTES * 4));
        data.extend_from_slice(SECRET_SUFFIX.as_bytes());
        data.push(b'\n');
        let tail = StderrTail::new();
        drain_stderr_into(data.as_slice(), tail.clone()).await;
        let snapshot = tail.snapshot().expect("oversize marker");
        assert_eq!(snapshot, OVERSIZE_LINE_MARKER);
        assert!(!snapshot.contains(SECRET_SUFFIX));
    }

    #[tokio::test]
    async fn owned_drain_finishes_normally_at_eof() {
        let (reader, mut writer) = tokio::io::duplex(64);
        let tail = StderrTail::new();
        let mut drain = StderrDrain::spawn(reader, tail.clone());
        writer.write_all(b"normal eof\n").await.unwrap();
        drop(writer);
        drain.shutdown_with_budget(Duration::from_secs(1)).await;
        assert_eq!(tail.snapshot().as_deref(), Some("normal eof"));
    }

    #[tokio::test]
    async fn owned_drain_aborts_when_a_writer_keeps_the_pipe_open() {
        let (reader, mut inherited_writer) = tokio::io::duplex(64);
        let tail = StderrTail::new();
        let mut drain = StderrDrain::spawn(reader, tail);
        inherited_writer.write_all(b"held open\n").await.unwrap();
        drain.shutdown_with_budget(Duration::from_millis(20)).await;
        assert!(
            inherited_writer.write_all(b"after shutdown").await.is_err(),
            "aborted drain must drop the pipe reader"
        );
    }
}
