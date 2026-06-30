//! Opt-in **process-log visibility** for the base's long-running commands.
//!
//! # The swallow this addresses
//!
//! A base (`claude` / `codex` / `opencode`) runs a long shell command — a Maven
//! / Gradle / `npm`/`cargo` build, a `spring-boot:run`, a dependency install —
//! inside its OWN agentic tool loop and its OWN sandbox. The base CAPTURES that
//! command's stdout/stderr internally and only hands UmaDev a single, structured
//! `tool_result` (codex: a `commandExecution` item's `aggregatedOutput`) when the
//! command FINISHES — and each driver then clips that preview to a tight
//! [`DEFAULT_TOOL_OUTPUT_CAP`] chars. So during a multi-minute build the user sees
//! a silent "thinking" with no log lines, and even at the end only a 200-char
//! clip — the logs feel "swallowed by the sandbox redirect."
//!
//! There is no raw byte stream of the running command available from the base's
//! wire protocol (the base owns the pipe), so UmaDev cannot tail the file the
//! sandbox redirects to. What IT can do, when the user opts in, is (1) surface the
//! FULL captured output instead of a 200-char clip, and (2) for the codex
//! app-server — whose item lifecycle DOES emit `item/started` + `item/updated`
//! frames as the command runs — surface the running command immediately and stream
//! its growing output, so the multi-minute void becomes a live, progressing log.
//!
//! # The toggle
//!
//! [`SHOW_PROCESS_LOGS_ENV`] (`UMADEV_SHOW_PROCESS_LOGS`) — truthy turns it on at
//! launch. The live flag, however, is process-wide **thread-safe shared state**
//! (an [`AtomicBool`]), NOT the process env: the TUI seeds it from the saved
//! preference / env at startup and flips it live via the `/logs` command through
//! [`set_show_process_logs`], while the three base drivers read it per streaming
//! event via [`show_process_logs`]. **OFF by default**: every driver behaves
//! exactly as before.
//!
//! ## Why not the env
//!
//! The drivers read this flag from background tokio tasks that live for the whole
//! session, while a `/logs` toggle runs on a different task. `std::env::set_var` /
//! `std::env::var` (POSIX `setenv`/`getenv`) are **not** thread-safe — a runtime
//! `setenv` racing a concurrent `getenv` is a data race (UB: can segfault / read a
//! freed `environ` slot). So the live value lives in an `AtomicBool`; the env is
//! read **once** at startup only, to seed it (an external launch override still
//! works), and is never mutated at runtime.
//!
//! Fail-open by contract: every function here is total and never panics — an unset
//! / unparsable env seeds "off", and the cap always returns a usable bound.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

/// Env toggle, read **once at startup** to seed [`show_process_logs`]. Truthy
/// (`1` / `true` / `yes` / `on`, case-insensitive) makes the base drivers surface
/// the FULL long-running command output (and, for codex, stream it as it runs)
/// instead of a tight clip. Anything else (incl. unset) is off — the historical
/// behaviour. The LIVE toggle is the [`AtomicBool`] behind [`set_show_process_logs`],
/// never this env at runtime.
pub const SHOW_PROCESS_LOGS_ENV: &str = "UMADEV_SHOW_PROCESS_LOGS";

/// Process-wide, thread-safe live process-log visibility flag — the single source
/// of truth the drivers read and the TUI writes. Lazily seeded from
/// [`SHOW_PROCESS_LOGS_ENV`] on first access (one-time startup read, so an
/// external launch override is honoured), then driven only by
/// [`set_show_process_logs`]. Replaces a per-call `std::env::var` read whose
/// matching runtime `set_var` raced the drivers' getenv (a `setenv`/`getenv` data
/// race → UB).
static SHOW_PROCESS_LOGS: OnceLock<AtomicBool> = OnceLock::new();

/// The lazily-initialised flag cell, seeded from the env exactly once. The seed is
/// the ONLY env read; after it, the value is pure shared state.
fn show_process_logs_cell() -> &'static AtomicBool {
    SHOW_PROCESS_LOGS.get_or_init(|| {
        AtomicBool::new(is_truthy(
            std::env::var(SHOW_PROCESS_LOGS_ENV).ok().as_deref(),
        ))
    })
}

/// Set the live process-log visibility flag (the `/logs` toggle + the startup
/// seed-from-preference). Thread-safe: the drivers observe it on their next
/// per-event [`show_process_logs`] read WITHOUT any process-global env mutation,
/// so a live toggle can never data-race a streaming getenv.
pub fn set_show_process_logs(on: bool) {
    show_process_logs_cell().store(on, Ordering::Relaxed);
}

/// Per-command output preview cap (chars) when process logs are OFF — the
/// historical tight clip that keeps a chatty tool result from flooding the
/// transcript.
const DEFAULT_TOOL_OUTPUT_CAP: usize = 200;

/// Per-command output cap (chars) when process logs are ON — generous enough to
/// carry a real build log's signal (the tail of an `mvn` / `gradle` run) while
/// still being a hard bound, so even verbose mode can't surface an unbounded blob.
const VERBOSE_TOOL_OUTPUT_CAP: usize = 16 * 1024;

/// `true` when the user opted in to seeing the base's long-running process logs.
/// Reads the thread-safe live flag (seeded once from the env, then driven by
/// [`set_show_process_logs`]) so a live `/logs` toggle takes effect on the next
/// streamed event without a process-global env read. Fail-open: an unset /
/// unparsable seed → `false`.
#[must_use]
pub fn show_process_logs() -> bool {
    show_process_logs_cell().load(Ordering::Relaxed)
}

/// Pure, testable core of [`show_process_logs`]: a lenient truthy check.
fn is_truthy(raw: Option<&str>) -> bool {
    matches!(
        raw.map(|s| s.trim().to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

/// Pure mapping from the on/off flag to the per-command output preview cap: the
/// generous [`VERBOSE_TOOL_OUTPUT_CAP`] when ON, else the tight
/// [`DEFAULT_TOOL_OUTPUT_CAP`]. Takes the flag explicitly so a caller that already
/// resolved [`show_process_logs`] once (e.g. a per-line dispatch) doesn't re-read
/// the env — and so it is unit-testable without mutating process env.
#[must_use]
pub fn cap_for(on: bool) -> usize {
    if on {
        VERBOSE_TOOL_OUTPUT_CAP
    } else {
        DEFAULT_TOOL_OUTPUT_CAP
    }
}

/// The per-command output preview cap the drivers truncate to, resolved from the
/// live [`show_process_logs`] toggle. Single source of truth so all three drivers
/// stay in lockstep.
#[must_use]
pub fn tool_output_cap() -> usize {
    cap_for(show_process_logs())
}

/// Marker prefixed to a TAIL-truncated verbose log so the reader can tell the
/// head was dropped — the frame is showing the LATEST output, not the start.
/// Plain ASCII (no emoji): the governance emoji rule never trips on it.
const LOG_TAIL_MARKER: &str = "[... log tail ...]\n";

/// Truncate a tool-result preview to at most `max` chars, **char-boundary-safe**,
/// with a direction that depends on which path is active:
///
/// - `verbose = true` (process logs ON, the generous [`VERBOSE_TOOL_OUTPUT_CAP`]):
///   keep the **TAIL** — the LAST `max` chars. Each `item/updated` frame carries
///   the CUMULATIVE growing output, so head-truncation would pin every frame past
///   the cap to the same first 16 KiB and the live stream would FREEZE; and the
///   failure verdict of an `mvn`/`gradle` run (`BUILD FAILURE`, the stack trace)
///   lives at the END, which head-truncation clips off. Keeping the tail makes the
///   stream advance as the build runs and preserves the error tail. The kept tail
///   is trimmed to start at a clean line boundary and prefixed with
///   [`LOG_TAIL_MARKER`].
/// - `verbose = false` (process logs OFF, the tight [`DEFAULT_TOOL_OUTPUT_CAP`]):
///   keep the **HEAD** — it is a short summary/preview, where the first chars are
///   the signal; the historical behaviour, unchanged.
///
/// Pure + fail-open: operates on `char`s (never slices mid-codepoint), never
/// panics, always returns a usable `String`.
#[must_use]
pub fn truncate_preview(s: &str, max: usize, verbose: bool) -> String {
    // Fast path: already within the cap. Count chars, not bytes (multibyte-safe).
    if s.chars().count() <= max {
        return s.to_string();
    }
    if verbose {
        keep_tail(s, max)
    } else {
        keep_head(s, max)
    }
}

/// Keep the FIRST `max` chars (the summary/preview path). Char-boundary-safe.
fn keep_head(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Keep the LAST `max` chars (the verbose live-log path), trimmed to a clean
/// leading line boundary and prefixed with [`LOG_TAIL_MARKER`]. Char-boundary-safe
/// (char iteration for the cut; the line-boundary slice cuts at a `'\n'` byte,
/// which is always a valid boundary).
fn keep_tail(s: &str, max: usize) -> String {
    let total = s.chars().count();
    let skip = total.saturating_sub(max);
    let tail: String = s.chars().skip(skip).collect();
    // We dropped the head mid-line; advance past the first partial line so the
    // preview starts cleanly. Only when a later newline leaves real content after
    // it (never trim away the whole tail).
    let body = match tail.find('\n') {
        Some(nl) if nl + 1 < tail.len() => &tail[nl + 1..],
        _ => tail.as_str(),
    };
    format!("{LOG_TAIL_MARKER}{body}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truthy_accepts_common_on_spellings_only() {
        for on in ["1", "true", "TRUE", " yes ", "On", "ON"] {
            assert!(is_truthy(Some(on)), "{on:?} should be truthy");
        }
        for off in [None, Some(""), Some("0"), Some("false"), Some("nope")] {
            assert!(!is_truthy(off), "{off:?} should be falsy");
        }
    }

    #[test]
    fn setter_is_observed_by_getter_with_no_env() {
        // The live flag is thread-safe shared state, NOT the process env: a
        // `/logs`-style toggle via the setter is observed by the drivers' getter
        // without any `set_var`/`var` round-trip (which would be a setenv/getenv
        // data race → UB). Save/restore the global so parallel tests stay clean.
        let prev = show_process_logs();
        set_show_process_logs(true);
        assert!(show_process_logs(), "setter ON is observed by the getter");
        set_show_process_logs(false);
        assert!(!show_process_logs(), "setter OFF is observed by the getter");
        set_show_process_logs(prev);
    }

    #[test]
    fn cap_is_tight_off_and_generous_on() {
        // Pure mapping from the boolean, independent of process env (which a
        // sibling test could be mutating in parallel).
        let off = cap_for(false);
        let on = cap_for(true);
        assert_eq!(off, DEFAULT_TOOL_OUTPUT_CAP);
        assert!(on >= off * 10, "verbose cap {on} >> tight cap {off}");
    }

    #[test]
    fn verbose_keeps_the_tail_so_the_stream_advances_and_the_error_survives() {
        // The user's scenario: a Maven build whose cumulative output blows past the
        // cap, with the failure verdict at the very END. Verbose mode must keep the
        // TAIL (so the live stream advances + the error is visible), not the head.
        let head = "[INFO] compiling module\n".repeat(2000); // far past any small cap
        let log = format!("{head}[INFO] BUILD FAILURE\n[ERROR] boom at Foo.java:42\n");
        let out = truncate_preview(&log, 64, true);
        assert!(
            out.contains("BUILD FAILURE"),
            "the error tail must survive: {out:?}"
        );
        assert!(
            out.contains("boom at Foo.java:42"),
            "the failure line must survive"
        );
        assert!(
            !out.contains("compiling module"),
            "the head must be dropped: {out:?}"
        );
        assert!(
            out.starts_with(LOG_TAIL_MARKER),
            "a tail marker flags the dropped head"
        );
        // Bounded: the marker plus at most `max` tail chars.
        assert!(out.chars().count() <= 64 + LOG_TAIL_MARKER.chars().count());
    }

    #[test]
    fn verbose_tail_advances_as_cumulative_output_grows() {
        // Two successive `item/updated` frames carry growing cumulative output; with
        // tail-truncation the SECOND frame differs from the first (the stream is not
        // frozen on the same first 16 K, which was the bug).
        let cap = 32;
        let frame1 = "x".repeat(100) + "first-window-end";
        let frame2 = frame1.clone() + &"y".repeat(100) + "second-window-end";
        let out1 = truncate_preview(&frame1, cap, true);
        let out2 = truncate_preview(&frame2, cap, true);
        assert_ne!(out1, out2, "the live tail must advance, not freeze");
        assert!(
            out2.contains("second-window-end"),
            "the newest output is shown"
        );
    }

    #[test]
    fn summary_keeps_the_head_as_before() {
        // Process logs OFF: the 200-char summary keeps the HEAD (a preview), no marker.
        let log = format!("first line is the summary{}", "x".repeat(500));
        let out = truncate_preview(&log, 32, false);
        assert!(out.starts_with("first line is the summary"));
        assert!(
            !out.starts_with(LOG_TAIL_MARKER),
            "summary path has no tail marker"
        );
        assert_eq!(out.chars().count(), 32, "exactly the head cap, no marker");
    }

    #[test]
    fn under_cap_is_returned_verbatim_both_directions() {
        let s = "short build output";
        assert_eq!(truncate_preview(s, 100, true), s);
        assert_eq!(truncate_preview(s, 100, false), s);
    }

    #[test]
    fn char_boundary_safe_on_multibyte_both_directions() {
        // Each multibyte char is several bytes; truncation must cut on CHAR
        // boundaries, never mid-codepoint (a byte slice there would panic).
        let s = "héllo wörld 日本語 ".repeat(50);
        let head = truncate_preview(&s, 10, false);
        let tail = truncate_preview(&s, 10, true);
        assert_eq!(head.chars().count(), 10, "head cap counts chars, not bytes");
        assert!(tail.chars().count() <= 10 + LOG_TAIL_MARKER.chars().count());
        // Valid UTF-8 round-trips (a mid-codepoint cut could not).
        assert!(std::str::from_utf8(tail.as_bytes()).is_ok());
        assert!(std::str::from_utf8(head.as_bytes()).is_ok());
    }
}
