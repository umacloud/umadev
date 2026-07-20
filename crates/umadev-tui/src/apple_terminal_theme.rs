//! Fully-automatic light/dark detection for Apple Terminal.app.
//!
//! Apple Terminal.app answers NEITHER `$COLORFGBG` NOR the OSC 11 background
//! query and exposes no profile in the environment, so [`crate::detect_light_bg`]
//! can only fall back to a static "default light" heuristic
//! ([`crate::theme_from_apple_terminal`]) for it. The only in-band way to read
//! Terminal.app's REAL background colour on macOS is an Apple Event, which this
//! module obtains out-of-process via the `osascript` subprocess — the crate is
//! `#![forbid(unsafe_code)]`, so there is no native ObjC / ScriptingBridge path.
//!
//! The verdict slots in AHEAD of the static heuristic, giving the precedence
//! (for Apple Terminal.app): `UMADEV_THEME` > an OSC 11 reply (if one ever
//! arrives) > this AppleScript real-background probe > the static default-light
//! heuristic.
//!
//! Everything here is fail-open: the pure parser ([`background_is_light`])
//! returns `None` for any malformed reply, the gate ([`should_probe`]) refuses to
//! spawn outside macOS + Apple Terminal.app + an unpinned palette, and the
//! subprocess wrapper ([`run_probe`]) yields `None` on spawn failure, timeout,
//! non-zero exit, permission denial, or unparseable output. A `None` anywhere
//! means "no verdict" and the caller keeps whatever the static heuristic already
//! chose — the probe can never crash, stall, or block the TUI.

use std::time::Duration;

/// The AppleScript sent to `osascript`: read the frontmost Terminal.app window's
/// background colour. `window 1` is the frontmost window — correct both at
/// startup (freshly launched) and at focus-in (UmaDev's window just regained
/// focus). It prints the three 16-bit RGB channel values Terminal.app stores,
/// e.g. `65535, 65535, 65535` for a white background.
const BACKGROUND_QUERY: &str = "tell application \"Terminal\" to get background color of window 1";

/// Maximum value of one Terminal.app colour channel (16-bit, `0`–`65535`). Any
/// parsed channel above this is treated as malformed (fail-open to `None`).
const CHANNEL_MAX: u32 = 65_535;

/// How long the `osascript` probe may run before it is abandoned (fail-open).
/// Generous enough that a first-time macOS "allow controlling Terminal" (TCC)
/// prompt the user is answering does not always lose the race, yet bounded so a
/// denied or ignored prompt cannot keep the subprocess (or, more importantly, the
/// spawned task) alive indefinitely. The probe runs OFF the event loop, so this
/// timeout never blocks the render path regardless of its length.
const PROBE_TIMEOUT: Duration = Duration::from_secs(4);

/// Decide light/dark from a Terminal.app background RGB triple via Rec. 601
/// perceived luminance (`0.299·R + 0.587·G + 0.114·B`) over the `0`–`65535`
/// range. Light when the luminance is above the mid-point.
#[must_use]
fn channel_triple_is_light(r: u32, g: u32, b: u32) -> bool {
    let luminance = 0.299 * f64::from(r) + 0.587 * f64::from(g) + 0.114 * f64::from(b);
    luminance > f64::from(CHANNEL_MAX) / 2.0
}

/// Parse the RGB triple `osascript` prints for a Terminal.app window's
/// background colour into a light/dark verdict: `Some(true)` = light,
/// `Some(false)` = dark.
///
/// Accepts the three 16-bit channel values (`0`–`65535`) separated by commas
/// and/or whitespace — Terminal.app prints comma-space separated
/// (`"65535, 65535, 65535"`), and whitespace-only separators are tolerated too.
///
/// Returns `None` (fail-open) for anything that is not EXACTLY three in-range
/// integers: empty, partial (`"0, 0"`), non-numeric, an extra field, or a channel
/// above [`CHANNEL_MAX`]. Every caller treats `None` as "no verdict" and keeps the
/// static default-light heuristic, so a malformed reply never changes the palette.
#[must_use]
pub(crate) fn background_is_light(raw: &str) -> Option<bool> {
    let mut channels = raw
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|field| !field.is_empty());
    let r: u32 = channels.next()?.parse().ok()?;
    let g: u32 = channels.next()?.parse().ok()?;
    let b: u32 = channels.next()?.parse().ok()?;
    // More than three fields is an unexpected shape → fail-open.
    if channels.next().is_some() {
        return None;
    }
    if r > CHANNEL_MAX || g > CHANNEL_MAX || b > CHANNEL_MAX {
        return None;
    }
    Some(channel_triple_is_light(r, g, b))
}

/// Whether the AppleScript background probe should run. Pure so the gate is
/// unit-testable without spawning `osascript` or touching a real terminal: pass
/// the compile-target OS ([`std::env::consts::OS`]), the trimmed `$TERM_PROGRAM`,
/// and whether the palette is pinned.
///
/// True ONLY on macOS, under Apple Terminal.app, and when the palette is NOT
/// pinned (neither an explicit `UMADEV_THEME` nor a manual `/theme light|dark` —
/// see [`crate::theme_pinned`]). Every other terminal and OS is excluded, so
/// `osascript` never runs for them.
#[must_use]
pub(crate) fn should_probe(target_os: &str, is_apple_terminal: bool, pinned: bool) -> bool {
    target_os == "macos" && is_apple_terminal && !pinned
}

/// Runtime gate: wire the real environment into [`should_probe`]. Reads the
/// compile-target OS, the trimmed `$TERM_PROGRAM`, and the live pin state
/// ([`crate::theme_pinned`]). Kept separate from the pure predicate so the
/// predicate stays hermetically testable.
#[must_use]
fn probe_allowed() -> bool {
    should_probe(
        std::env::consts::OS,
        crate::is_apple_terminal(),
        crate::theme_pinned(),
    )
}

/// Thin, deliberately non-unit-tested wrapper around the tested pure parser
/// ([`background_is_light`]): run `osascript` to read Terminal.app's frontmost
/// window background colour under a bounded [`PROBE_TIMEOUT`], then classify its
/// stdout. The subprocess is the ONE piece that cannot be exercised hermetically
/// (it needs macOS + a live Terminal.app), so it stays as small as possible.
///
/// Fully fail-open — returns `None` on:
/// - **timeout** ([`PROBE_TIMEOUT`] elapsed; the dropped future kills the child
///   via `kill_on_drop`, so a slow / unanswered TCC prompt cannot leak a process),
/// - **spawn / wait failure** (`osascript` missing, etc.),
/// - **non-zero exit** (e.g. the TCC permission was denied, or there is no window),
/// - **non-UTF-8 or unparseable output**.
async fn run_probe() -> Option<bool> {
    let mut command = tokio::process::Command::new("osascript");
    command
        .arg("-e")
        .arg(BACKGROUND_QUERY)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    // On timeout the outer future is dropped, dropping the Child; `kill_on_drop`
    // then reaps the abandoned `osascript` (e.g. one parked on an unanswered TCC
    // prompt) so nothing lingers.
    let output = tokio::time::timeout(PROBE_TIMEOUT, command.output())
        .await
        .ok()? // Elapsed → fail-open
        .ok()?; // io::Error (spawn / wait) → fail-open
    if !output.status.success() {
        return None;
    }
    background_is_light(std::str::from_utf8(&output.stdout).ok()?)
}

/// Spawn the Apple Terminal.app background probe on a background task and send its
/// light/dark verdict — when it produces one — to `tx`. Fully OFF the render
/// path: `osascript` runs under a bounded [`PROBE_TIMEOUT`] via `tokio::process`
/// inside a spawned task, and the event loop only ever RECEIVES a verdict. The
/// spawn itself returns immediately, so a slow or denied macOS "allow controlling
/// Terminal" (TCC) prompt cannot stall startup, focus-in, or any turn.
///
/// Gated by [`probe_allowed`] / [`should_probe`]: a no-op — NO process spawned —
/// on any non-macOS target, any terminal other than Apple Terminal.app, and any
/// pinned palette. Fail-open throughout: a spawn failure, non-zero exit, timeout,
/// permission denial, or unparseable output produces no verdict and the caller
/// keeps the static default-light heuristic. On a `None` verdict nothing is sent,
/// so a transient probe failure never disturbs an already-detected palette.
pub(crate) fn spawn_probe(tx: &tokio::sync::mpsc::UnboundedSender<bool>) {
    if !probe_allowed() {
        return;
    }
    let tx = tx.clone();
    tokio::spawn(async move {
        if let Some(is_light) = run_probe().await {
            // Receiver gone (loop tearing down) → drop the verdict; fail-open.
            let _ = tx.send(is_light);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{background_is_light, channel_triple_is_light, should_probe, CHANNEL_MAX};

    #[test]
    fn white_background_is_light() {
        assert_eq!(background_is_light("65535, 65535, 65535"), Some(true));
    }

    #[test]
    fn black_background_is_dark() {
        assert_eq!(background_is_light("0, 0, 0"), Some(false));
    }

    #[test]
    fn whitespace_only_separators_parse() {
        // AppleScript may print space-separated triples; tolerate them too.
        assert_eq!(background_is_light("65535 65535 65535"), Some(true));
        assert_eq!(background_is_light("0 0 0"), Some(false));
    }

    #[test]
    fn mid_gray_decided_by_luminance() {
        // A light gray above the mid-point → light; a dark gray below it → dark.
        assert_eq!(background_is_light("48000, 48000, 48000"), Some(true));
        assert_eq!(background_is_light("16000, 16000, 16000"), Some(false));
    }

    #[test]
    fn luminance_threshold_is_the_midpoint() {
        // Uniform gray of value v has luminance v (weights sum to 1.0), so the
        // decision flips exactly at CHANNEL_MAX / 2 = 32767.5.
        assert!(channel_triple_is_light(32_768, 32_768, 32_768));
        assert!(!channel_triple_is_light(32_767, 32_767, 32_767));
    }

    #[test]
    fn luminance_uses_rec_601_weighting() {
        // Pure green (heaviest weight, 0.587) reads light; pure red (0.299) and
        // pure blue (0.114) read dark at full channel — proving the weighting,
        // not a naive average, drives the verdict.
        assert!(channel_triple_is_light(0, CHANNEL_MAX, 0));
        assert!(!channel_triple_is_light(CHANNEL_MAX, 0, 0));
        assert!(!channel_triple_is_light(0, 0, CHANNEL_MAX));
    }

    #[test]
    fn malformed_output_fails_open_to_none() {
        assert_eq!(background_is_light(""), None);
        assert_eq!(background_is_light("   "), None);
        assert_eq!(background_is_light("65535, 65535"), None); // partial
        assert_eq!(background_is_light("0"), None); // partial
        assert_eq!(background_is_light("foo, bar, baz"), None); // non-numeric
        assert_eq!(background_is_light("65535, 65535, 65535, 65535"), None); // extra field
        assert_eq!(background_is_light("70000, 0, 0"), None); // channel out of range
        assert_eq!(background_is_light("-1, 0, 0"), None); // negative (not u32)
    }

    #[test]
    fn gate_true_only_for_macos_apple_terminal_unpinned() {
        assert!(should_probe("macos", true, false));
    }

    #[test]
    fn gate_skips_when_pinned() {
        // A `/theme` or `UMADEV_THEME` pin blocks the probe entirely.
        assert!(!should_probe("macos", true, true));
    }

    #[test]
    fn gate_skips_other_os() {
        assert!(!should_probe("linux", true, false));
        assert!(!should_probe("windows", true, false));
    }

    #[test]
    fn gate_skips_when_not_apple_terminal() {
        // Neither `$TERM_PROGRAM` nor `$__CFBundleIdentifier` identified Terminal.app
        // (iTerm/WezTerm/…): the osascript "tell Terminal" probe would target the wrong
        // app, so it must not run.
        assert!(!should_probe("macos", false, false));
    }

    #[test]
    fn fail_open_falls_back_to_static_default_when_probe_yields_none() {
        // `theme_from_apple_terminal` (the static heuristic) yields `true`
        // (default light) under Apple Terminal.app. When the probe is
        // unparseable, the runtime keeps that value — modelled here by the same
        // `.unwrap_or(default)` fallback the wiring performs (no send on `None`,
        // so the pre-set default stands).
        let static_default_light = true;
        assert!(background_is_light("garbage").unwrap_or(static_default_light));
        assert!(background_is_light("").unwrap_or(static_default_light));
        // A real dark probe still overrides the default-light fallback.
        assert!(!background_is_light("0, 0, 0").unwrap_or(static_default_light));
    }
}
