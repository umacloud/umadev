//! First-pass acceptance rate — UmaDev's own measured engineering doctrine.
//!
//! UmaDev's cheap path PROPOSES (the router's intent decision, a plan step's
//! doer output) and the deterministic floor + acceptance VERIFIES. This module
//! measures, per **kind**, the fraction of cheap-path proposals that PASS
//! verification on the FIRST attempt — with ZERO rework rounds. That
//! "first-pass acceptance rate" is a principled, *measured* north-star:
//!
//! - a kind with consistently **low** first-pass acceptance is where the cheap
//!   path is unreliable → the director should spend more brain consult, or
//!   default to a **lower autonomy** there;
//! - **high** acceptance → the cheap path is trustworthy → lean on it.
//!
//! The aggregate is a tiny per-kind counter (`attempts`, `first_pass`) persisted
//! to [`STATS_FILE`], merged + atomically written. A "kind" is a namespaced key
//! so two orthogonal dimensions accumulate side by side without colliding:
//! [`seat_kind`] (`seat:<role-id>` — the doer-seat / step-kind dimension) and
//! [`class_kind`] (`class:<route-class>` — the route-class dimension).
//!
//! **Fail-open + advisory by contract.** Every path tolerates a missing or
//! corrupt file by yielding *no signal* and never panics; recording an outcome
//! never affects the step's pass/fail result, loop termination, or any gate. The
//! signal only *informs a default* — it is consulted as a NUDGE (see
//! [`low_confidence_nudge`] / [`autonomy_default`]), never as a control input to
//! the deterministic floor.

use std::collections::BTreeMap;
use std::path::Path;

use fs2::FileExt as _;
use serde::{Deserialize, Serialize};

use crate::trust::TrustMode;

/// Where the per-project first-pass aggregate lives (under the gitignored
/// `.umadev/` user-data dir).
pub const STATS_FILE: &str = ".umadev/acceptance-stats.json";
const MAX_STATS_BYTES: usize = 2 * 1024 * 1024;
const STATS_FILENAME: &str = "acceptance-stats.json";
const LOCK_FILENAME: &str = "acceptance-stats.lock";

/// Minimum number of recorded attempts for a kind before its rate is TRUSTED.
/// Below this the rate is statistically meaningless, so [`FirstPassStats::rate`]
/// returns `None` and every consult treats the kind as "no signal" (behaviour
/// unchanged). Small — a handful of samples is enough to notice a chronically
/// unreliable cheap path without waiting for a long history.
pub const MIN_SAMPLES: u32 = 5;

/// At or below this first-pass rate (over [`MIN_SAMPLES`]+ attempts) a kind's
/// cheap path is judged UNRELIABLE — the trigger for an advisory nudge toward
/// more consult / lower autonomy. A rate above it is "healthy" → no nudge.
pub const LOW_RATE_THRESHOLD: f64 = 0.5;

/// Hard cap on the number of distinct kinds tracked, so a long-lived repo's
/// stats file can never grow unbounded. Generous — the kind space is small
/// (eight seats + five route classes), so this is only a runaway guard: once
/// the cap is reached, EXISTING kinds keep accumulating but no NEW kind is added.
const MAX_KINDS: usize = 64;

/// Namespace a doer-seat role id into a first-pass kind key (the step-kind
/// dimension): `frontend-engineer` → `seat:frontend-engineer`.
#[must_use]
pub fn seat_kind(role_id: &str) -> String {
    format!("seat:{role_id}")
}

/// Namespace a route-class id into a first-pass kind key (the route-class
/// dimension): `build` → `class:build`.
#[must_use]
pub fn class_kind(class: &str) -> String {
    format!("class:{class}")
}

/// The per-kind counter: how many cheap-path proposals were verified
/// (`attempts`) and how many of those PASSED on the first attempt (`first_pass`,
/// always `<= attempts`).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct KindStat {
    /// Total verified proposals for this kind.
    #[serde(default)]
    pub attempts: u32,
    /// Proposals that passed verification on the FIRST attempt (no rework).
    #[serde(default)]
    pub first_pass: u32,
}

impl KindStat {
    /// The first-pass acceptance rate in `0.0..=1.0`, or `None` until at least
    /// [`MIN_SAMPLES`] attempts are recorded (the rate is not yet trusted).
    /// Guards against a hand-corrupted row where `first_pass > attempts` by
    /// clamping the ratio to `1.0`.
    #[must_use]
    pub fn rate(&self) -> Option<f64> {
        if self.attempts < MIN_SAMPLES {
            return None;
        }
        let r = f64::from(self.first_pass) / f64::from(self.attempts);
        Some(r.clamp(0.0, 1.0))
    }
}

/// The persisted aggregate: a map from kind key → its counter.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FirstPassStats {
    /// Per-kind counters, keyed by a namespaced kind (see [`seat_kind`] /
    /// [`class_kind`]). A `BTreeMap` keeps the on-disk JSON stable + diffable.
    #[serde(default)]
    pub kinds: BTreeMap<String, KindStat>,
}

impl FirstPassStats {
    /// The first-pass rate for a kind, or `None` when the kind is unseen or has
    /// fewer than [`MIN_SAMPLES`] attempts (no trusted signal yet).
    #[must_use]
    pub fn rate(&self, kind: &str) -> Option<f64> {
        self.kinds.get(kind).and_then(KindStat::rate)
    }

    /// Record ONE verified proposal for `kind`: always `attempts += 1`, plus
    /// `first_pass += 1` when it passed on the first attempt. Pure mutation;
    /// persisting is the caller's job. A brand-new kind is only inserted while
    /// under the bounded kind cap — an already-tracked kind always updates, so
    /// the cap never silently freezes an existing signal.
    pub fn observe(&mut self, kind: &str, first_pass: bool) {
        if !self.kinds.contains_key(kind) && self.kinds.len() >= MAX_KINDS {
            return; // runaway guard — never grow past the cap with new kinds
        }
        let e = self.kinds.entry(kind.to_string()).or_default();
        e.attempts = e.attempts.saturating_add(1);
        if first_pass {
            e.first_pass = e.first_pass.saturating_add(1);
        }
    }
}

/// Load the persisted stats for a project. Fail-open: a missing or unparseable
/// file yields an empty (default) aggregate — never an error, never a panic.
#[must_use]
pub fn load(project_root: &Path) -> FirstPassStats {
    let path = project_root.join(STATS_FILE);
    let Ok(text) = crate::bounded_fs::read_utf8_beneath(project_root, &path, MAX_STATS_BYTES)
    else {
        return FirstPassStats::default();
    };
    serde_json::from_str::<FirstPassStats>(&text).unwrap_or_default()
}

/// Record one verified cheap-path proposal for `kind`, merging into the
/// persisted aggregate (read-modify-write) and atomically rewriting the file.
///
/// Serialised by a process-wide lock so two concurrent recorders (e.g. parallel
/// steps) can't clobber each other's read-modify-write. FAIL-OPEN throughout: a
/// missing dir, an unreadable file, or a write error is swallowed — recording is
/// pure telemetry and must NEVER block or fail the build.
pub fn record(project_root: &Path, kind: &str, first_pass: bool) {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let Ok(root) = std::fs::canonicalize(project_root) else {
        return;
    };
    if !umadev_state::fs::real_dir(&root) {
        return;
    }
    let Ok(dir) = umadev_state::fs::ensure_real_child_dir(&root, ".umadev") else {
        return;
    };
    // The in-process mutex above covers parallel steps, while this advisory OS lock also
    // serializes independent UmaDev processes. Telemetry is strictly best-effort: if another
    // process owns the lock, skip this sample instead of ever delaying the user's run.
    let Ok(lock) = umadev_state::fs::open_private_lock(&dir.join(LOCK_FILENAME)) else {
        return;
    };
    if lock.try_lock_exclusive().is_err() {
        return;
    }

    let mut stats = load(&root);
    stats.observe(kind, first_pass);

    let Ok(body) = serde_json::to_string_pretty(&stats) else {
        return;
    };
    if body.len() > MAX_STATS_BYTES {
        return;
    }
    let _ = umadev_state::fs::atomic_write(&dir.join(STATS_FILENAME), body.as_bytes());
}

/// The trusted first-pass rate for a kind, loaded fresh — `None` until at least
/// [`MIN_SAMPLES`] attempts (or on any read/parse failure). The single advisory
/// read the router / trust / director consult. Pure + fail-open.
#[must_use]
pub fn first_pass_rate(project_root: &Path, kind: &str) -> Option<f64> {
    load(project_root).rate(kind)
}

/// An advisory NUDGE for a kind: `Some(human-readable reason)` when the kind has
/// a TRUSTWORTHY-LOW first-pass rate (≥ [`MIN_SAMPLES`] attempts AND rate ≤
/// [`LOW_RATE_THRESHOLD`]) — a hint that this kind's cheap path is unreliable, so
/// a caller may spend more brain consult / default to a lower autonomy there.
/// `None` = no signal (too few samples, a healthy rate, or a missing file) → the
/// caller's behaviour is unchanged.
///
/// ADVISORY ONLY: the returned string is for surfacing + informing a default; it
/// never changes the deterministic floor, loop termination, or any gate.
#[must_use]
pub fn low_confidence_nudge(project_root: &Path, kind: &str) -> Option<String> {
    let rate = first_pass_rate(project_root, kind)?;
    if rate > LOW_RATE_THRESHOLD {
        return None;
    }
    Some(format!(
        "signal · 这一类需求的一次过验收率偏低({:.0}%)— 轻量路径历史上不太可靠,\
         建议多校验 / 降低自动化档位(仅供参考,确定性底线不变)",
        rate * 100.0
    ))
}

/// The advisory autonomy DEFAULT for a kind, given the caller's `current` mode.
///
/// When a kind's cheap path has a trustworthy-low first-pass rate, the safe
/// default leans toward LESS autonomy (more human / critic checkpoints), so this
/// returns the more conservative of `current` and [`TrustMode::Guarded`]. It can
/// only ever LOWER autonomy toward the existing guarded default — it never grants
/// MORE autonomy than the caller already chose, and (being a mere default hint)
/// it never bypasses the always-on irreversible-action floor. Fail-open: no
/// signal (too few samples / a healthy rate / a missing file) returns `current`
/// unchanged.
#[must_use]
pub fn autonomy_default(project_root: &Path, kind: &str, current: TrustMode) -> TrustMode {
    // Only a trustworthy-low rate nudges; otherwise the caller's mode stands.
    if low_confidence_nudge(project_root, kind).is_none() {
        return current;
    }
    // Lower autonomy toward Guarded, but never below what the user already chose
    // (Plan stays Plan — already the most conservative; Auto is damped to
    // Guarded; Guarded stays Guarded). Never raises autonomy.
    match current {
        TrustMode::Auto => TrustMode::Guarded,
        other => other,
    }
}

/// A compact human summary of every tracked kind's first-pass rate (only kinds
/// that have crossed [`MIN_SAMPLES`]), for `/status`-style reporting. Empty when
/// nothing is trusted yet. Pure + fail-open.
#[must_use]
pub fn summary(project_root: &Path) -> Vec<String> {
    load(project_root)
        .kinds
        .iter()
        .filter_map(|(k, s)| {
            s.rate().map(|r| {
                format!(
                    "{k}: {:.0}% first-pass ({}/{} attempts)",
                    r * 100.0,
                    s.first_pass,
                    s.attempts
                )
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn first_pass_step_increments_first_pass_and_attempts() {
        let tmp = TempDir::new().unwrap();
        let kind = seat_kind("frontend-engineer");
        record(tmp.path(), &kind, true);
        let stats = load(tmp.path());
        let s = stats.kinds.get(&kind).copied().unwrap();
        assert_eq!(s.attempts, 1);
        assert_eq!(s.first_pass, 1, "a first-pass step bumps BOTH counters");
    }

    #[test]
    fn reworked_step_increments_attempts_only() {
        let tmp = TempDir::new().unwrap();
        let kind = seat_kind("backend-engineer");
        record(tmp.path(), &kind, false);
        let stats = load(tmp.path());
        let s = stats.kinds.get(&kind).copied().unwrap();
        assert_eq!(s.attempts, 1);
        assert_eq!(s.first_pass, 0, "a reworked step bumps attempts only");
    }

    #[test]
    fn rate_is_none_below_min_samples_and_correct_above() {
        let tmp = TempDir::new().unwrap();
        let kind = class_kind("build");
        // 4 attempts (< MIN_SAMPLES = 5): not enough → None.
        for _ in 0..4 {
            record(tmp.path(), &kind, true);
        }
        assert_eq!(
            first_pass_rate(tmp.path(), &kind),
            None,
            "below the min sample the rate is untrusted"
        );
        // 6 attempts, 3 first-pass → 0.5 once over the threshold of samples.
        let tmp2 = TempDir::new().unwrap();
        for i in 0..6 {
            record(tmp2.path(), &kind, i < 3);
        }
        let r = first_pass_rate(tmp2.path(), &kind).expect("trusted over min samples");
        assert!((r - 0.5).abs() < 1e-9, "3/6 == 0.5, got {r}");
    }

    #[test]
    fn aggregate_persists_and_reloads() {
        let tmp = TempDir::new().unwrap();
        let kind = seat_kind("frontend-engineer");
        // Two separate record calls must MERGE on disk (read-modify-write), not
        // overwrite — proving persistence + reload across calls.
        record(tmp.path(), &kind, true);
        record(tmp.path(), &kind, false);
        let stats = load(tmp.path());
        let s = stats.kinds.get(&kind).copied().unwrap();
        assert_eq!((s.attempts, s.first_pass), (2, 1));
        // The file really exists where documented.
        assert!(tmp.path().join(STATS_FILE).exists());
    }

    #[test]
    fn fail_open_on_a_corrupt_file() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".umadev")).unwrap();
        std::fs::write(tmp.path().join(STATS_FILE), "{ not valid json ]").unwrap();
        // A corrupt file → no signal, never a panic.
        assert_eq!(load(tmp.path()), FirstPassStats::default());
        assert_eq!(first_pass_rate(tmp.path(), &seat_kind("qa-engineer")), None);
        // A subsequent record still works (overwrites the garbage cleanly).
        let kind = seat_kind("qa-engineer");
        record(tmp.path(), &kind, true);
        assert_eq!(load(tmp.path()).kinds.get(&kind).unwrap().attempts, 1);
    }

    #[test]
    fn oversized_stats_fail_open_without_becoming_a_signal() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".umadev")).unwrap();
        std::fs::File::create(tmp.path().join(STATS_FILE))
            .unwrap()
            .set_len(u64::try_from(MAX_STATS_BYTES + 1).unwrap())
            .unwrap();
        assert_eq!(load(tmp.path()), FirstPassStats::default());
        assert_eq!(first_pass_rate(tmp.path(), &seat_kind("qa-engineer")), None);
    }

    #[cfg(unix)]
    #[test]
    fn record_never_follows_a_linked_managed_directory_or_stats_leaf() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let outside_stats = outside.path().join(STATS_FILENAME);
        std::fs::write(&outside_stats, b"outside").unwrap();
        symlink(outside.path(), root.path().join(".umadev")).unwrap();
        record(root.path(), "class:build", true);
        assert_eq!(std::fs::read(&outside_stats).unwrap(), b"outside");

        std::fs::remove_file(root.path().join(".umadev")).unwrap();
        std::fs::create_dir(root.path().join(".umadev")).unwrap();
        symlink(&outside_stats, root.path().join(STATS_FILE)).unwrap();
        record(root.path(), "class:build", true);
        assert_eq!(std::fs::read(&outside_stats).unwrap(), b"outside");
    }

    #[test]
    fn missing_file_yields_no_signal() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(load(tmp.path()), FirstPassStats::default());
        assert_eq!(first_pass_rate(tmp.path(), &class_kind("debug")), None);
        assert_eq!(low_confidence_nudge(tmp.path(), &class_kind("debug")), None);
    }

    #[test]
    fn a_busy_cross_process_lock_never_blocks_the_run() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".umadev");
        std::fs::create_dir(&dir).unwrap();
        let held = umadev_state::fs::open_private_lock(&dir.join(LOCK_FILENAME)).unwrap();
        held.lock_exclusive().unwrap();

        let started = std::time::Instant::now();
        record(tmp.path(), "class:build", true);
        assert!(
            started.elapsed() < std::time::Duration::from_millis(500),
            "best-effort telemetry must skip a contended sample"
        );
        assert_eq!(load(tmp.path()), FirstPassStats::default());
    }

    #[test]
    fn low_confidence_nudge_fires_only_on_a_trustworthy_low_rate() {
        let tmp = TempDir::new().unwrap();
        let kind = class_kind("build");
        // 6 attempts, 1 first-pass = 16% ≤ 50% over the min sample → nudge fires.
        for i in 0..6 {
            record(tmp.path(), &kind, i == 0);
        }
        assert!(
            low_confidence_nudge(tmp.path(), &kind).is_some(),
            "a low rate over the min sample nudges"
        );

        // A healthy rate (5/5 = 100%) → no nudge.
        let tmp2 = TempDir::new().unwrap();
        for _ in 0..5 {
            record(tmp2.path(), &kind, true);
        }
        assert!(
            low_confidence_nudge(tmp2.path(), &kind).is_none(),
            "a healthy rate never nudges"
        );

        // Low rate but too few samples (3 attempts) → no nudge yet.
        let tmp3 = TempDir::new().unwrap();
        for _ in 0..3 {
            record(tmp3.path(), &kind, false);
        }
        assert!(
            low_confidence_nudge(tmp3.path(), &kind).is_none(),
            "below the min sample, even a 0% rate stays silent"
        );
    }

    #[test]
    fn autonomy_default_only_lowers_never_raises() {
        let tmp = TempDir::new().unwrap();
        let kind = class_kind("build");
        // Drive a trustworthy-low rate.
        for _ in 0..6 {
            record(tmp.path(), &kind, false);
        }
        // Auto is damped to Guarded on a low-confidence kind …
        assert_eq!(
            autonomy_default(tmp.path(), &kind, TrustMode::Auto),
            TrustMode::Guarded
        );
        // … but a more conservative mode is never raised.
        assert_eq!(
            autonomy_default(tmp.path(), &kind, TrustMode::Plan),
            TrustMode::Plan
        );
        assert_eq!(
            autonomy_default(tmp.path(), &kind, TrustMode::Guarded),
            TrustMode::Guarded
        );

        // With NO signal, the caller's mode always stands (even Auto).
        let tmp2 = TempDir::new().unwrap();
        assert_eq!(
            autonomy_default(tmp2.path(), &kind, TrustMode::Auto),
            TrustMode::Auto,
            "no signal → mode unchanged (advisory, never changes behaviour)"
        );
    }

    #[test]
    fn the_kind_cap_bounds_growth_but_updates_existing() {
        let mut stats = FirstPassStats::default();
        for i in 0..(MAX_KINDS + 10) {
            stats.observe(&format!("seat:role-{i}"), true);
        }
        assert_eq!(
            stats.kinds.len(),
            MAX_KINDS,
            "new kinds capped at MAX_KINDS"
        );
        // An already-tracked kind still updates past the cap.
        let existing = "seat:role-0".to_string();
        let before = stats.kinds.get(&existing).copied().unwrap();
        stats.observe(&existing, true);
        let after = stats.kinds.get(&existing).copied().unwrap();
        assert_eq!(after.attempts, before.attempts + 1);
    }

    #[test]
    fn summary_lists_only_trusted_kinds() {
        let tmp = TempDir::new().unwrap();
        let trusted = seat_kind("frontend-engineer");
        let untrusted = seat_kind("backend-engineer");
        for _ in 0..5 {
            record(tmp.path(), &trusted, true);
        }
        record(tmp.path(), &untrusted, true); // 1 attempt < MIN_SAMPLES
        let lines = summary(tmp.path());
        assert_eq!(
            lines.len(),
            1,
            "only the trusted kind is summarised: {lines:?}"
        );
        assert!(lines[0].contains("frontend-engineer"));
    }
}
