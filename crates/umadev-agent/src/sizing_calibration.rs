//! Prediction-sizing calibration — UmaDev's own measured self-evolution doctrine.
//!
//! Every turn the router / planner make TWO predictions: the intent **class** and the
//! complexity **sizing** (the kind / depth / team they pick). [`crate::first_pass`]
//! already measures whether the cheap path's proposals SURVIVE verification — that is a
//! *quality* signal. This module measures the ORTHOGONAL thing the router is never
//! scored on: was the **SIZING** right?
//!
//! - a route sized **Light / small** that actually needed a real multi-step build
//!   UNDER-sized the turn (the cheap path escalated);
//! - a route sized **Greenfield / Deep** that finished in one trivial step OVER-sized it
//!   (the heavy machinery produced a trivial result).
//!
//! Nothing learns from a *systematic* sizing miss today, so the router keeps making it.
//! Per route-class this records (predicted size vs. the run's actual size) and exposes
//! ONE advisory adjustment ([`sizing_calibration`]): a class that consistently
//! UNDER-sizes nudges its DEFAULT heavier, one that consistently OVER-sizes nudges it
//! lighter.
//!
//! **Fail-open + advisory by contract** (identical to [`crate::first_pass`]). A missing
//! or corrupt file yields *no signal* and never panics; recording a run's outcome never
//! affects that run's route, plan, deterministic floor, gates, or termination — the
//! route is already decided by the time the outcome is known. The adjustment only ever
//! informs a DEFAULT / PRIOR ([`calibrated_default`]); it never overrides the brain's
//! per-turn decision and never touches the deterministic floor or any gate.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::router::{RouteClass, RoutePlan};

/// Where the per-project sizing-calibration aggregate lives (under the gitignored
/// `.umadev/` user-data dir). Distinct from [`crate::first_pass::STATS_FILE`] so the two
/// orthogonal signals never collide on disk.
pub const STATS_FILE: &str = ".umadev/sizing-calibration.json";
const MAX_STATS_BYTES: usize = 2 * 1024 * 1024;
const STATS_FILENAME: &str = "sizing-calibration.json";
const LOCK_FILENAME: &str = "sizing-calibration.lock";
const LOCK_RETRY_BUDGET: Duration = Duration::from_millis(100);

/// Minimum recorded runs for a route-class before its calibration is TRUSTED. Below
/// this the over/under-size fractions are statistically meaningless, so
/// [`ClassSizing::adjustment`] returns `None` and every consult treats the class as
/// "no signal" (behaviour unchanged). Small — a handful of runs is enough to notice a
/// chronic mis-sizing without waiting for a long history.
pub const MIN_SAMPLES: u32 = 5;

/// The fraction of runs (over [`MIN_SAMPLES`]+) that must miss in ONE direction before
/// the calibration nudges that way. At/above this the class is judged to *systematically*
/// over- or under-size; below it the misses are noise and no nudge fires.
pub const MISS_THRESHOLD: f64 = 0.5;

/// Hard cap on distinct route-classes tracked, so the file can never grow unbounded.
/// Generous — the class space is tiny (five [`RouteClass`] variants) — so this is only a
/// runaway guard: at the cap EXISTING classes keep accumulating but no NEW key is added.
const MAX_CLASSES: usize = 32;

/// A coarse, ORDERED "how big was this turn" rank — the single comparable axis the
/// calibration scores on. Derived from a route's (class, depth) for the PREDICTION
/// ([`predicted_size`]) and from a run's observed work for the ACTUAL. The ordering
/// `Trivial < Light < Heavy` is what makes "actual heavier than predicted = under-sized"
/// a plain comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SizeRank {
    /// No real work — a chat / explain turn, or a "build" that produced no code change.
    Trivial,
    /// A small, single-surface change — one fast turn, code changed, no rework.
    Light,
    /// A real, multi-step / reworked build — a plan walked over several steps, or the
    /// cheap single turn needed bounded QC fix rounds to settle.
    Heavy,
}

impl SizeRank {
    /// Stable lowercase id for logs / summaries.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trivial => "trivial",
            Self::Light => "light",
            Self::Heavy => "heavy",
        }
    }

    /// One step heavier, saturating at [`Self::Heavy`] — the nudge applied to a DEFAULT
    /// when a class systematically UNDER-sizes.
    #[must_use]
    pub const fn heavier(self) -> Self {
        match self {
            Self::Trivial => Self::Light,
            Self::Light | Self::Heavy => Self::Heavy,
        }
    }

    /// One step lighter, saturating at [`Self::Trivial`] — the nudge applied to a DEFAULT
    /// when a class systematically OVER-sizes.
    #[must_use]
    pub const fn lighter(self) -> Self {
        match self {
            Self::Heavy => Self::Light,
            Self::Light | Self::Trivial => Self::Trivial,
        }
    }
}

/// The router's PREDICTED size for a route — derived deterministically from its
/// (class, depth). A read-only class (`Chat` / `Explain`) is `Trivial`; a mutating class
/// is `Heavy` when the router sized it deliberate (`Standard` / `Deep` — the same signal
/// that drives seat-by-seat building) else `Light`. No model call.
#[must_use]
pub fn predicted_size(route: &RoutePlan) -> SizeRank {
    match route.class {
        RouteClass::Chat | RouteClass::Explain => SizeRank::Trivial,
        RouteClass::QuickEdit | RouteClass::Debug | RouteClass::Build => {
            if route.depth.is_deliberate() {
                SizeRank::Heavy
            } else {
                SizeRank::Light
            }
        }
    }
}

/// The advisory direction a class's calibration suggests for its DEFAULT sizing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizingAdjustment {
    /// The class consistently UNDER-sizes — nudge its default heavier.
    Heavier,
    /// The class consistently OVER-sizes — nudge its default lighter.
    Lighter,
}

impl SizingAdjustment {
    /// Stable lowercase id for logs / events.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Heavier => "heavier",
            Self::Lighter => "lighter",
        }
    }
}

/// The per-class counter: how many runs were scored (`samples`), how many predicted
/// LIGHTER than reality (`under` — the turn escalated) and how many predicted HEAVIER
/// than reality (`over` — the heavy path produced a trivial result). A run whose actual
/// matched its prediction bumps only `samples`.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClassSizing {
    /// Total scored runs for this route-class.
    #[serde(default)]
    pub samples: u32,
    /// Runs the router UNDER-sized (actual heavier than predicted).
    #[serde(default)]
    pub under: u32,
    /// Runs the router OVER-sized (actual lighter than predicted).
    #[serde(default)]
    pub over: u32,
}

impl ClassSizing {
    /// Record ONE scored run: always `samples += 1`, plus `under += 1` when the actual
    /// size exceeded the prediction or `over += 1` when it fell short. Pure mutation.
    pub fn observe(&mut self, predicted: SizeRank, actual: SizeRank) {
        self.samples = self.samples.saturating_add(1);
        if actual > predicted {
            self.under = self.under.saturating_add(1);
        } else if actual < predicted {
            self.over = self.over.saturating_add(1);
        }
    }

    /// The advisory adjustment for this class, or `None` until at least [`MIN_SAMPLES`]
    /// runs AND a systematic miss (one direction's fraction `>= `[`MISS_THRESHOLD`] and
    /// strictly dominant). A class that misses both ways (or rarely) yields `None`.
    /// Guards a hand-corrupted row where `under + over > samples` by clamping fractions.
    #[must_use]
    pub fn adjustment(&self) -> Option<SizingAdjustment> {
        if self.samples < MIN_SAMPLES {
            return None;
        }
        let samples = f64::from(self.samples);
        let under_frac = (f64::from(self.under) / samples).clamp(0.0, 1.0);
        let over_frac = (f64::from(self.over) / samples).clamp(0.0, 1.0);
        if self.under > self.over && under_frac >= MISS_THRESHOLD {
            Some(SizingAdjustment::Heavier)
        } else if self.over > self.under && over_frac >= MISS_THRESHOLD {
            Some(SizingAdjustment::Lighter)
        } else {
            None
        }
    }
}

/// The persisted aggregate: a map from route-class id (`chat` / `explain` / `quick_edit`
/// / `debug` / `build` — see [`RouteClass::as_str`]) to its counter. A `BTreeMap` keeps
/// the on-disk JSON stable + diffable.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SizingStats {
    /// Per-route-class sizing counters.
    #[serde(default)]
    pub classes: BTreeMap<String, ClassSizing>,
}

impl SizingStats {
    /// The advisory adjustment for `class`, or `None` when the class is unseen or below
    /// [`MIN_SAMPLES`] (no trusted signal yet).
    #[must_use]
    pub fn adjustment(&self, class: &str) -> Option<SizingAdjustment> {
        self.classes.get(class).and_then(ClassSizing::adjustment)
    }

    /// Record ONE scored run for `class`. A brand-new class is only inserted while under
    /// the internal class cap (a runaway guard); an already-tracked class always updates.
    pub fn observe(&mut self, class: &str, predicted: SizeRank, actual: SizeRank) {
        if !self.classes.contains_key(class) && self.classes.len() >= MAX_CLASSES {
            return;
        }
        self.classes
            .entry(class.to_string())
            .or_default()
            .observe(predicted, actual);
    }
}

/// Load the persisted aggregate for a project. Fail-open: a missing or unparseable file
/// yields an empty (default) aggregate — never an error, never a panic.
#[must_use]
pub fn load(project_root: &Path) -> SizingStats {
    let path = project_root.join(STATS_FILE);
    let Ok(text) = crate::bounded_fs::read_utf8_beneath(project_root, &path, MAX_STATS_BYTES)
    else {
        return SizingStats::default();
    };
    serde_json::from_str::<SizingStats>(&text).unwrap_or_default()
}

/// Record one scored run for `class`, merging into the persisted aggregate
/// (read-modify-write) and atomically rewriting the file.
///
/// Serialised by a process-wide lock so two concurrent recorders can't clobber each
/// other's read-modify-write. The cross-process lock tolerates a normal,
/// millisecond-scale hand-off but has a hard wait bound. FAIL-OPEN throughout: a
/// missing dir, unreadable file, busy lock past the bound, or write error is swallowed
/// — recording is telemetry and never changes the run outcome.
pub fn record(project_root: &Path, class: &str, predicted: SizeRank, actual: SizeRank) {
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
    let Ok(lock) = umadev_state::fs::open_private_lock(&dir.join(LOCK_FILENAME)) else {
        return;
    };
    if !umadev_state::fs::try_lock_exclusive_bounded(&lock, LOCK_RETRY_BUDGET)
        .is_ok_and(|acquired| acquired)
    {
        return;
    }

    let mut stats = load(&root);
    stats.observe(class, predicted, actual);

    let Ok(body) = serde_json::to_string_pretty(&stats) else {
        return;
    };
    if body.len() > MAX_STATS_BYTES {
        return;
    }
    let _ = umadev_state::fs::atomic_write(&dir.join(STATS_FILENAME), body.as_bytes());
}

/// Record one scored run for a [`RoutePlan`] — the convenience the director loop calls.
/// Computes the PREDICTED size from the route ([`predicted_size`]) and pairs it with the
/// observed `actual`. Pure telemetry, fail-open (see [`record`]).
pub fn record_route(project_root: &Path, route: &RoutePlan, actual: SizeRank) {
    record(
        project_root,
        route.class.as_str(),
        predicted_size(route),
        actual,
    );
}

/// The advisory sizing adjustment for a route-class, loaded fresh — `None` until at
/// least [`MIN_SAMPLES`] runs with a systematic miss (or on any read/parse failure).
/// The single advisory read the router / planner consult.
///
/// ADVISORY ONLY: a class that consistently UNDER-sizes returns
/// [`SizingAdjustment::Heavier`] (nudge its default heavier); one that consistently
/// OVER-sizes returns [`SizingAdjustment::Lighter`]. The caller may use it to adjust a
/// DEFAULT / PRIOR ([`calibrated_default`]); it NEVER overrides the brain's per-turn
/// decision and NEVER touches the deterministic floor or any gate. Pure + fail-open.
#[must_use]
pub fn sizing_calibration(project_root: &Path, class: &str) -> Option<SizingAdjustment> {
    load(project_root).adjustment(class)
}

/// Apply a class's measured calibration to a DEFAULT size rank — the concrete
/// self-correction the router / planner may use when choosing a default/prior. Nudges
/// one step heavier on a systematic under-size, one step lighter on a systematic
/// over-size, else returns `default` unchanged.
///
/// This is the sizing analogue of [`crate::first_pass::autonomy_default`]. It is only
/// ever meant to bias a DEFAULT/PRIOR — callers must NOT consult it on the deterministic
/// floor or to override a brain verdict. Fail-open: no signal (too few runs / no
/// systematic miss / a missing file) returns `default` unchanged.
#[must_use]
pub fn calibrated_default(project_root: &Path, class: &str, default: SizeRank) -> SizeRank {
    match sizing_calibration(project_root, class) {
        Some(SizingAdjustment::Heavier) => default.heavier(),
        Some(SizingAdjustment::Lighter) => default.lighter(),
        None => default,
    }
}

/// An advisory human NUDGE for a class: `Some(reason)` when the class has a trustworthy,
/// systematic sizing miss, else `None` (no signal → caller behaviour unchanged). Surfaced
/// as an [`crate::events::EngineEvent::Note`] so the user sees the self-evolution signal.
///
/// ADVISORY ONLY: the string informs the user + a future default; it never changes the
/// current run's route, the deterministic floor, loop termination, or any gate.
#[must_use]
pub fn advisory_nudge(project_root: &Path, class: &str) -> Option<String> {
    match sizing_calibration(project_root, class)? {
        SizingAdjustment::Heavier => Some(
            "signal · 这一类需求历史上常被低估体量(轻量路径多次升级成真实多步构建)\
             — 以后默认体量可更重一档(仅供参考,不改变本次处理与确定性底线)"
                .to_string(),
        ),
        SizingAdjustment::Lighter => Some(
            "signal · 这一类需求历史上常被高估体量(重型路径多次只产出微小结果)\
             — 以后默认体量可更轻一档(仅供参考,不改变本次处理与确定性底线)"
                .to_string(),
        ),
    }
}

/// A compact human summary of every tracked class with a trusted calibration (≥
/// [`MIN_SAMPLES`] runs), for `/status`-style reporting. Empty when nothing is trusted
/// yet. Pure + fail-open.
#[must_use]
pub fn summary(project_root: &Path) -> Vec<String> {
    load(project_root)
        .classes
        .iter()
        .filter_map(|(k, s)| {
            s.adjustment().map(|adj| {
                format!(
                    "{k}: sizing {} ({} under / {} over of {} runs)",
                    adj.as_str(),
                    s.under,
                    s.over,
                    s.samples
                )
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner::TaskKind;
    use crate::router::{Budget, Depth};
    use tempfile::TempDir;

    fn persist_fixture(root: &Path, stats: &SizingStats) {
        std::fs::create_dir_all(root.join(".umadev")).unwrap();
        let body = serde_json::to_vec_pretty(stats).unwrap();
        std::fs::write(root.join(STATS_FILE), body).unwrap();
    }

    /// A minimal [`RoutePlan`] for a (class, depth) — only the fields the calibration
    /// reads matter; the rest are filler.
    fn route(class: RouteClass, depth: Depth) -> RoutePlan {
        RoutePlan {
            class,
            kind: TaskKind::Light,
            depth,
            team: Vec::new(),
            scope: Vec::new(),
            needs_clarify: None,
            est_budget: Budget::for_route(class, depth),
            confidence: 0.5,
        }
    }

    #[test]
    fn predicted_size_maps_class_and_depth() {
        // Read-only classes are trivial; a fast mutating route is light; a deliberate
        // build is heavy.
        assert_eq!(
            predicted_size(&route(RouteClass::Chat, Depth::Fast)),
            SizeRank::Trivial
        );
        assert_eq!(
            predicted_size(&route(RouteClass::QuickEdit, Depth::Fast)),
            SizeRank::Light
        );
        assert_eq!(
            predicted_size(&route(RouteClass::Build, Depth::Fast)),
            SizeRank::Light
        );
        assert_eq!(
            predicted_size(&route(RouteClass::Build, Depth::Standard)),
            SizeRank::Heavy
        );
        assert_eq!(
            predicted_size(&route(RouteClass::Build, Depth::Deep)),
            SizeRank::Heavy
        );
    }

    #[test]
    fn under_sized_run_nudges_the_class_heavier_after_min_samples() {
        // A Light-sized route (QuickEdit/Fast) that ACTUALLY turned into a heavy build,
        // recorded MIN_SAMPLES times → the class systematically under-sizes → nudge
        // heavier. The default size for that class is bumped one step up.
        let tmp = TempDir::new().unwrap();
        let mut stats = SizingStats::default();
        for _ in 0..MIN_SAMPLES {
            stats.observe("build", SizeRank::Light, SizeRank::Heavy);
        }
        persist_fixture(tmp.path(), &stats);
        assert_eq!(
            sizing_calibration(tmp.path(), "build"),
            Some(SizingAdjustment::Heavier),
            "a chronically under-sized class nudges heavier"
        );
        // …and that nudge bumps a DEFAULT one step heavier (advisory prior).
        assert_eq!(
            calibrated_default(tmp.path(), "build", SizeRank::Light),
            SizeRank::Heavy
        );
        assert!(advisory_nudge(tmp.path(), "build").is_some());
    }

    #[test]
    fn over_sized_run_nudges_the_class_lighter_after_min_samples() {
        // A Heavy-sized route (deliberate Build) that ACTUALLY finished trivially,
        // recorded MIN_SAMPLES times → the class systematically over-sizes → nudge
        // lighter, and the default size drops one step.
        let tmp = TempDir::new().unwrap();
        let mut stats = SizingStats::default();
        for _ in 0..MIN_SAMPLES {
            stats.observe("build", SizeRank::Heavy, SizeRank::Trivial);
        }
        persist_fixture(tmp.path(), &stats);
        assert_eq!(
            sizing_calibration(tmp.path(), "build"),
            Some(SizingAdjustment::Lighter),
            "a chronically over-sized class nudges lighter"
        );
        assert_eq!(
            calibrated_default(tmp.path(), "build", SizeRank::Heavy),
            SizeRank::Light
        );
    }

    #[test]
    fn calibration_is_none_below_min_samples() {
        // Under-sizing every time, but fewer than MIN_SAMPLES runs → no trusted signal.
        let tmp = TempDir::new().unwrap();
        let mut stats = SizingStats::default();
        for _ in 0..(MIN_SAMPLES - 1) {
            stats.observe("quick_edit", SizeRank::Light, SizeRank::Heavy);
        }
        persist_fixture(tmp.path(), &stats);
        assert_eq!(
            sizing_calibration(tmp.path(), "quick_edit"),
            None,
            "below the min sample the calibration is untrusted"
        );
        // And the default is returned unchanged (fail-open, behaves as today).
        assert_eq!(
            calibrated_default(tmp.path(), "quick_edit", SizeRank::Light),
            SizeRank::Light
        );
        assert_eq!(advisory_nudge(tmp.path(), "quick_edit"), None);
    }

    #[test]
    fn mixed_misses_below_threshold_yield_no_nudge() {
        // 2 under + 2 over + 1 match over 5 runs: neither direction dominates / clears
        // the threshold → no nudge (the misses are noise, not a systematic bias).
        let tmp = TempDir::new().unwrap();
        let mut stats = SizingStats::default();
        stats.observe("build", SizeRank::Light, SizeRank::Heavy); // under
        stats.observe("build", SizeRank::Light, SizeRank::Heavy); // under
        stats.observe("build", SizeRank::Light, SizeRank::Trivial); // over
        stats.observe("build", SizeRank::Light, SizeRank::Trivial); // over
        stats.observe("build", SizeRank::Light, SizeRank::Light); // match
        persist_fixture(tmp.path(), &stats);
        assert_eq!(sizing_calibration(tmp.path(), "build"), None);
    }

    #[test]
    fn aggregate_persists_and_reloads() {
        // Separate record calls MERGE on disk (read-modify-write), proving persistence
        // + reload across calls.
        let tmp = TempDir::new().unwrap();
        let r = route(RouteClass::Build, Depth::Fast); // predicted Light
        record_route(tmp.path(), &r, SizeRank::Heavy); // under
        record_route(tmp.path(), &r, SizeRank::Light); // match
        let stats = load(tmp.path());
        let c = stats.classes.get("build").copied().unwrap();
        assert_eq!((c.samples, c.under, c.over), (2, 1, 0));
        assert!(tmp.path().join(STATS_FILE).exists());
    }

    #[test]
    fn fail_open_on_a_corrupt_file() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".umadev")).unwrap();
        std::fs::write(tmp.path().join(STATS_FILE), "{ not valid json ]").unwrap();
        // A corrupt file → no signal, never a panic.
        assert_eq!(load(tmp.path()), SizingStats::default());
        assert_eq!(sizing_calibration(tmp.path(), "build"), None);
        // A subsequent record still works (overwrites the garbage cleanly).
        let r = route(RouteClass::Build, Depth::Fast);
        record_route(tmp.path(), &r, SizeRank::Heavy);
        assert_eq!(load(tmp.path()).classes.get("build").unwrap().samples, 1);
    }

    #[test]
    fn oversized_stats_fail_open_without_becoming_a_signal() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".umadev")).unwrap();
        std::fs::File::create(tmp.path().join(STATS_FILE))
            .unwrap()
            .set_len(u64::try_from(MAX_STATS_BYTES + 1).unwrap())
            .unwrap();
        assert_eq!(load(tmp.path()), SizingStats::default());
        assert_eq!(sizing_calibration(tmp.path(), "build"), None);
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
        record(root.path(), "build", SizeRank::Light, SizeRank::Heavy);
        assert_eq!(std::fs::read(&outside_stats).unwrap(), b"outside");

        std::fs::remove_file(root.path().join(".umadev")).unwrap();
        std::fs::create_dir(root.path().join(".umadev")).unwrap();
        symlink(&outside_stats, root.path().join(STATS_FILE)).unwrap();
        record(root.path(), "build", SizeRank::Light, SizeRank::Heavy);
        assert_eq!(std::fs::read(&outside_stats).unwrap(), b"outside");
    }

    #[test]
    fn missing_file_yields_no_signal() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(load(tmp.path()), SizingStats::default());
        assert_eq!(sizing_calibration(tmp.path(), "build"), None);
        assert_eq!(advisory_nudge(tmp.path(), "build"), None);
        assert_eq!(
            calibrated_default(tmp.path(), "build", SizeRank::Heavy),
            SizeRank::Heavy
        );
    }

    #[test]
    fn a_busy_cross_process_lock_never_blocks_the_run() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".umadev");
        std::fs::create_dir(&dir).unwrap();
        let held = umadev_state::fs::open_private_lock(&dir.join(LOCK_FILENAME)).unwrap();
        fs2::FileExt::lock_exclusive(&held).unwrap();

        let started = std::time::Instant::now();
        record(tmp.path(), "build", SizeRank::Light, SizeRank::Heavy);
        assert!(
            started.elapsed() < std::time::Duration::from_millis(500),
            "best-effort telemetry must skip a contended sample"
        );
        assert_eq!(load(tmp.path()), SizingStats::default());
    }

    #[test]
    fn a_transient_cross_process_lock_handoff_does_not_drop_the_sample() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".umadev");
        std::fs::create_dir(&dir).unwrap();
        let held = umadev_state::fs::open_private_lock(&dir.join(LOCK_FILENAME)).unwrap();
        fs2::FileExt::lock_exclusive(&held).unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(5));
            drop(held);
        });

        record(tmp.path(), "build", SizeRank::Light, SizeRank::Heavy);
        release.join().unwrap();

        let value = load(tmp.path())
            .classes
            .get("build")
            .copied()
            .expect("the hand-off should complete inside the bounded retry window");
        assert_eq!((value.samples, value.under, value.over), (1, 1, 0));
    }

    #[test]
    fn advisory_does_not_change_a_specific_runs_route_or_floor() {
        // The deterministic floor must NOT consult the calibration: even with a strong
        // OVER-size signal for the `build` class on disk, the router's `for_run` floor
        // produces the IDENTICAL route it would with no data — the adjustment is purely
        // an advisory default, never a control input to a run's sizing.
        let tmp = TempDir::new().unwrap();
        let req = "做一个完整的电商网站,带账号、商品、购物车、支付和后台管理";
        let baseline = crate::router::for_run(req);
        let mut stats = SizingStats::default();
        for _ in 0..(MIN_SAMPLES * 2) {
            stats.observe("build", SizeRank::Heavy, SizeRank::Trivial);
        }
        persist_fixture(tmp.path(), &stats);
        assert_eq!(
            sizing_calibration(tmp.path(), "build"),
            Some(SizingAdjustment::Lighter),
            "the over-size signal is present on disk"
        );
        let after = crate::router::for_run(req);
        assert_eq!(
            (baseline.class, baseline.depth, baseline.team.len()),
            (after.class, after.depth, after.team.len()),
            "the floor route is unchanged by the calibration (advisory only)"
        );
    }

    #[test]
    fn the_class_cap_bounds_growth_but_updates_existing() {
        let mut stats = SizingStats::default();
        for i in 0..(MAX_CLASSES + 10) {
            stats.observe(&format!("class-{i}"), SizeRank::Light, SizeRank::Heavy);
        }
        assert_eq!(stats.classes.len(), MAX_CLASSES, "new classes capped");
        let before = stats.classes.get("class-0").copied().unwrap();
        stats.observe("class-0", SizeRank::Light, SizeRank::Heavy);
        let after = stats.classes.get("class-0").copied().unwrap();
        assert_eq!(
            after.samples,
            before.samples + 1,
            "existing class still updates"
        );
    }

    #[test]
    fn rank_nudges_saturate() {
        assert_eq!(SizeRank::Trivial.heavier(), SizeRank::Light);
        assert_eq!(SizeRank::Light.heavier(), SizeRank::Heavy);
        assert_eq!(SizeRank::Heavy.heavier(), SizeRank::Heavy);
        assert_eq!(SizeRank::Heavy.lighter(), SizeRank::Light);
        assert_eq!(SizeRank::Light.lighter(), SizeRank::Trivial);
        assert_eq!(SizeRank::Trivial.lighter(), SizeRank::Trivial);
    }

    #[test]
    fn summary_lists_only_trusted_classes() {
        let tmp = TempDir::new().unwrap();
        let mut stats = SizingStats::default();
        for _ in 0..MIN_SAMPLES {
            stats.observe("build", SizeRank::Light, SizeRank::Heavy);
        }
        // A different class with too few samples stays untrusted.
        stats.observe("debug", SizeRank::Light, SizeRank::Heavy);
        persist_fixture(tmp.path(), &stats);
        let lines = summary(tmp.path());
        assert_eq!(
            lines.len(),
            1,
            "only the trusted class is summarised: {lines:?}"
        );
        assert!(lines[0].contains("build"));
    }
}
