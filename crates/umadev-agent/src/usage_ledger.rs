//! Durable, quality-aware accounting for base-reported usage.
//!
//! The ledger is JSONL schema v2. Every token number carries an explicit
//! measurement quality, and cost is persisted only when the base reported a
//! complete, trustworthy value. Legacy schema-v1 rows are retained as
//! `estimated`; they can never be promoted to exact usage after migration.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use umadev_runtime::Usage;
use umadev_spec::Phase;

const SCHEMA_VERSION: u8 = 2;
const DEFAULT_MAX_BYTES: u64 = 5 * 1024 * 1024;
const DEFAULT_MAX_ARCHIVES: usize = 3;
const LOCK_DIR: &str = ".usage-ledger.lock";
const LOCK_OWNER: &str = "owner.json";
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);
const READ_LOCK_TIMEOUT: Duration = Duration::from_millis(100);
const LOCK_POLL: Duration = Duration::from_millis(2);
const LOCK_STALE_AFTER: Duration = Duration::from_secs(5 * 60);
const MAX_OWNER_BYTES: u64 = 4 * 1024;
const MAX_LABEL_CHARS: usize = 128;
const MAX_ROW_BYTES: usize = 16 * 1024;
const RUN_GAP_MS: u64 = 30 * 60 * 1_000;
const USD_TICKS_PER_DOLLAR: u128 = 10_000_000_000;

static NONCE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Confidence attached to one token measurement.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MeasurementQuality {
    /// Complete whole-turn counters reported by the base.
    Exact,
    /// Base-reported counters that may omit some work.
    LowerBound,
    /// A heuristic value, never suitable for billing or exact comparisons.
    Estimated,
    /// No defensible token value is available.
    #[default]
    Unknown,
}

/// Provenance for a token measurement.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MeasurementSource {
    BaseReported,
    TextHeuristic,
    LegacyV1,
    #[default]
    Unavailable,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CostQuality {
    Exact,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TokenMeasurement {
    quality: MeasurementQuality,
    source: MeasurementSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    total_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cached_read_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cached_write_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reasoning_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CostMeasurement {
    quality: CostQuality,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    usd_ticks: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct UsageRecordV2 {
    schema_version: u8,
    record_id: String,
    ts_ms: u64,
    backend: String,
    phase: String,
    tokens: TokenMeasurement,
    cost: CostMeasurement,
    #[serde(default)]
    model_calls: u64,
    #[serde(default)]
    num_turns: u64,
}

#[derive(Debug, Deserialize)]
struct LegacyUsageRecordV1 {
    #[serde(default)]
    ts: u64,
    backend: String,
    phase: String,
    tokens: u64,
}

impl UsageRecordV2 {
    fn from_runtime(backend: &str, phase: &str, usage: Usage) -> Self {
        let component_total = usage.input_tokens.saturating_add(usage.output_tokens);
        let known_total = usage.total_tokens.max(component_total);
        let components_consistent = usage.total_tokens == component_total
            && usage.cached_read_tokens <= usage.input_tokens
            && usage.cached_write_tokens <= usage.input_tokens
            && usage.reasoning_tokens <= usage.output_tokens;
        let quality = if usage.usage_incomplete {
            if known_total == 0 {
                MeasurementQuality::Unknown
            } else {
                MeasurementQuality::LowerBound
            }
        } else if components_consistent {
            MeasurementQuality::Exact
        } else if known_total == 0 {
            MeasurementQuality::Unknown
        } else {
            // A contradictory supposedly-complete snapshot is not exact. Keep
            // the defensible counters as a lower bound and discard its cost.
            MeasurementQuality::LowerBound
        };
        let has_tokens = quality != MeasurementQuality::Unknown;
        let exact_cost = (quality == MeasurementQuality::Exact)
            .then(|| usage.trusted_cost_usd_ticks())
            .flatten();
        Self {
            schema_version: SCHEMA_VERSION,
            record_id: new_record_id(),
            ts_ms: now_ms(),
            backend: bounded_label(backend),
            phase: bounded_label(phase),
            tokens: TokenMeasurement {
                quality,
                source: if has_tokens {
                    MeasurementSource::BaseReported
                } else {
                    MeasurementSource::Unavailable
                },
                input_tokens: has_tokens.then_some(usage.input_tokens),
                output_tokens: has_tokens.then_some(usage.output_tokens),
                total_tokens: has_tokens.then_some(known_total),
                cached_read_tokens: has_tokens.then_some(usage.cached_read_tokens),
                cached_write_tokens: has_tokens.then_some(usage.cached_write_tokens),
                reasoning_tokens: has_tokens.then_some(usage.reasoning_tokens),
            },
            cost: CostMeasurement {
                quality: if exact_cost.is_some() {
                    CostQuality::Exact
                } else {
                    CostQuality::Unknown
                },
                usd_ticks: exact_cost,
            },
            model_calls: usage.model_calls,
            num_turns: usage.num_turns,
        }
    }

    fn estimated(backend: &str, phase: &str, tokens: u64) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            record_id: new_record_id(),
            ts_ms: now_ms(),
            backend: bounded_label(backend),
            phase: bounded_label(phase),
            tokens: TokenMeasurement {
                quality: MeasurementQuality::Estimated,
                source: MeasurementSource::TextHeuristic,
                input_tokens: None,
                output_tokens: None,
                total_tokens: Some(tokens),
                cached_read_tokens: None,
                cached_write_tokens: None,
                reasoning_tokens: None,
            },
            cost: CostMeasurement {
                quality: CostQuality::Unknown,
                usd_ticks: None,
            },
            model_calls: 0,
            num_turns: 0,
        }
    }

    fn from_legacy(legacy: &LegacyUsageRecordV1, source: &str, line: usize) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            record_id: legacy_record_id(source, line, legacy),
            ts_ms: legacy.ts.saturating_mul(1_000),
            backend: bounded_label(&legacy.backend),
            phase: bounded_label(&legacy.phase),
            tokens: TokenMeasurement {
                quality: MeasurementQuality::Estimated,
                source: MeasurementSource::LegacyV1,
                input_tokens: None,
                output_tokens: None,
                total_tokens: Some(legacy.tokens),
                cached_read_tokens: None,
                cached_write_tokens: None,
                reasoning_tokens: None,
            },
            cost: CostMeasurement {
                quality: CostQuality::Unknown,
                usd_ticks: None,
            },
            model_calls: 0,
            num_turns: 0,
        }
    }

    fn valid(&self) -> bool {
        if self.schema_version != SCHEMA_VERSION
            || self.record_id.is_empty()
            || self.record_id.len() > 192
            || self.backend.chars().count() > MAX_LABEL_CHARS
            || self.phase.chars().count() > MAX_LABEL_CHARS
        {
            return false;
        }
        let t = &self.tokens;
        let token_shape_valid = match t.quality {
            MeasurementQuality::Exact => {
                t.source == MeasurementSource::BaseReported
                    && t.input_tokens
                        .zip(t.output_tokens)
                        .zip(t.total_tokens)
                        .is_some_and(|((input, output), total)| {
                            input.saturating_add(output) == total
                                && t.cached_read_tokens.is_some_and(|v| v <= input)
                                && t.cached_write_tokens.is_some_and(|v| v <= input)
                                && t.reasoning_tokens.is_some_and(|v| v <= output)
                        })
            }
            MeasurementQuality::LowerBound => {
                t.source == MeasurementSource::BaseReported && t.total_tokens.is_some()
            }
            MeasurementQuality::Estimated => {
                matches!(
                    t.source,
                    MeasurementSource::TextHeuristic | MeasurementSource::LegacyV1
                ) && t.total_tokens.is_some()
                    && t.input_tokens.is_none()
                    && t.output_tokens.is_none()
                    && t.cached_read_tokens.is_none()
                    && t.cached_write_tokens.is_none()
                    && t.reasoning_tokens.is_none()
            }
            MeasurementQuality::Unknown => {
                t.source == MeasurementSource::Unavailable
                    && t.input_tokens.is_none()
                    && t.output_tokens.is_none()
                    && t.total_tokens.is_none()
                    && t.cached_read_tokens.is_none()
                    && t.cached_write_tokens.is_none()
                    && t.reasoning_tokens.is_none()
            }
        };
        token_shape_valid
            && match self.cost.quality {
                CostQuality::Exact => {
                    t.quality == MeasurementQuality::Exact
                        && self.cost.usd_ticks.is_some_and(|ticks| ticks > 0)
                }
                CostQuality::Unknown => self.cost.usd_ticks.is_none(),
            }
    }
}

/// Strict aggregate of token values without collapsing confidence classes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenBreakdown {
    /// Tokens from complete whole-turn base reports.
    pub exact_tokens: u128,
    /// Calls contributing [`Self::exact_tokens`].
    pub exact_calls: u64,
    /// Known token floor from incomplete base reports.
    pub lower_bound_tokens: u128,
    /// Calls contributing [`Self::lower_bound_tokens`].
    pub lower_bound_calls: u64,
    /// Heuristic or schema-v1 token values.
    pub estimated_tokens: u128,
    /// Calls contributing [`Self::estimated_tokens`].
    pub estimated_calls: u64,
    /// Calls for which no token value is defensible.
    pub unknown_calls: u64,
}

impl TokenBreakdown {
    /// Sum of all numeric buckets. The buckets must still be shown separately;
    /// this number alone is not an exact total unless [`Self::quality`] is exact.
    #[must_use]
    pub fn known_numeric_sum(self) -> u128 {
        self.exact_tokens
            .saturating_add(self.lower_bound_tokens)
            .saturating_add(self.estimated_tokens)
    }

    /// Conservative quality for the aggregate. Bucket fields retain the full
    /// mixed-quality detail.
    #[must_use]
    pub fn quality(self) -> MeasurementQuality {
        if self.unknown_calls > 0 {
            MeasurementQuality::Unknown
        } else if self.estimated_calls > 0 {
            MeasurementQuality::Estimated
        } else if self.lower_bound_calls > 0 {
            MeasurementQuality::LowerBound
        } else {
            MeasurementQuality::Exact
        }
    }

    fn add(&mut self, measurement: &TokenMeasurement) {
        let value = u128::from(measurement.total_tokens.unwrap_or(0));
        match measurement.quality {
            MeasurementQuality::Exact => {
                self.exact_tokens = self.exact_tokens.saturating_add(value);
                self.exact_calls = self.exact_calls.saturating_add(1);
            }
            MeasurementQuality::LowerBound => {
                self.lower_bound_tokens = self.lower_bound_tokens.saturating_add(value);
                self.lower_bound_calls = self.lower_bound_calls.saturating_add(1);
            }
            MeasurementQuality::Estimated => {
                self.estimated_tokens = self.estimated_tokens.saturating_add(value);
                self.estimated_calls = self.estimated_calls.saturating_add(1);
            }
            MeasurementQuality::Unknown => {
                self.unknown_calls = self.unknown_calls.saturating_add(1);
            }
        }
    }

    fn merge(&mut self, other: Self) {
        self.exact_tokens = self.exact_tokens.saturating_add(other.exact_tokens);
        self.exact_calls = self.exact_calls.saturating_add(other.exact_calls);
        self.lower_bound_tokens = self
            .lower_bound_tokens
            .saturating_add(other.lower_bound_tokens);
        self.lower_bound_calls = self
            .lower_bound_calls
            .saturating_add(other.lower_bound_calls);
        self.estimated_tokens = self.estimated_tokens.saturating_add(other.estimated_tokens);
        self.estimated_calls = self.estimated_calls.saturating_add(other.estimated_calls);
        self.unknown_calls = self.unknown_calls.saturating_add(other.unknown_calls);
    }
}

/// Trusted cost values reported directly by bases.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CostBreakdown {
    /// Sum of trustworthy base-reported USD ticks (`10^10` ticks = USD 1).
    pub reported_usd_ticks: u128,
    /// Calls covered by [`Self::reported_usd_ticks`].
    pub exact_calls: u64,
    /// Calls without a trustworthy complete cost.
    pub unknown_calls: u64,
}

impl CostBreakdown {
    /// Return a complete trusted total only when every recorded call supplied
    /// trustworthy cost. Missing cost is unknown, never zero.
    #[must_use]
    pub fn complete_total_usd_ticks(self) -> Option<u128> {
        (self.unknown_calls == 0 && self.exact_calls > 0).then_some(self.reported_usd_ticks)
    }

    fn add(&mut self, cost: &CostMeasurement) {
        match (cost.quality, cost.usd_ticks) {
            (CostQuality::Exact, Some(ticks)) if ticks > 0 => {
                self.reported_usd_ticks = self
                    .reported_usd_ticks
                    .saturating_add(u128::try_from(ticks).unwrap_or(0));
                self.exact_calls = self.exact_calls.saturating_add(1);
            }
            _ => self.unknown_calls = self.unknown_calls.saturating_add(1),
        }
    }

    fn merge(&mut self, other: Self) {
        self.reported_usd_ticks = self
            .reported_usd_ticks
            .saturating_add(other.reported_usd_ticks);
        self.exact_calls = self.exact_calls.saturating_add(other.exact_calls);
        self.unknown_calls = self.unknown_calls.saturating_add(other.unknown_calls);
    }
}

/// Usage for one phase within a contiguous run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseUsage {
    /// Phase id (`research` / `frontend` / ...).
    pub phase: String,
    /// Worker calls recorded in this phase.
    pub calls: u64,
    /// Compatibility numeric sum. Inspect [`Self::token_breakdown`] before
    /// displaying it; mixed values are not an exact total.
    pub tokens: u64,
    /// Exact/lower-bound/estimated/unknown token buckets.
    pub token_breakdown: TokenBreakdown,
    /// Trusted reported cost coverage.
    pub cost_breakdown: CostBreakdown,
}

/// Usage for one contiguous run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunUsage {
    /// 1-based run ordinal, oldest first.
    pub index: usize,
    /// Backends observed in the run, sorted.
    pub backends: Vec<String>,
    /// Per-phase usage in pipeline order where known.
    pub phases: Vec<PhaseUsage>,
    /// Calls in this run.
    pub calls: u64,
    /// Compatibility numeric sum; see [`Self::token_breakdown`].
    pub tokens: u64,
    /// Exact/lower-bound/estimated/unknown token buckets.
    pub token_breakdown: TokenBreakdown,
    /// Trusted reported cost coverage.
    pub cost_breakdown: CostBreakdown,
}

/// Structured view of the bounded usage ledger.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageReport {
    /// Contiguous runs, oldest first.
    pub runs: Vec<RunUsage>,
    /// Calls across retained live and archive files.
    pub total_calls: u64,
    /// Compatibility numeric sum; see [`Self::token_breakdown`].
    pub total_tokens: u64,
    /// Exact/lower-bound/estimated/unknown token buckets.
    pub token_breakdown: TokenBreakdown,
    /// Trusted reported cost coverage.
    pub cost_breakdown: CostBreakdown,
    /// Distinct backends, sorted.
    pub backends: Vec<String>,
    /// Malformed/unsupported rows skipped while recovering valid records.
    pub corrupt_rows: u64,
    /// Retained schema-v1 rows, always represented as estimates.
    pub migrated_v1_calls: u64,
}

impl UsageReport {
    /// Whether no valid usage row was recovered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.runs.is_empty()
    }
}

#[derive(Debug, Clone, Copy)]
struct LedgerConfig {
    max_bytes: u64,
    max_archives: usize,
}

impl Default for LedgerConfig {
    fn default() -> Self {
        let max_bytes = std::env::var("UMADEV_USAGE_MAX_BYTES")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_MAX_BYTES);
        Self {
            max_bytes,
            max_archives: DEFAULT_MAX_ARCHIVES,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct LockOwner {
    created_at_ms: u64,
    pid: u32,
    nonce: String,
}

struct LedgerLock {
    path: PathBuf,
    nonce: String,
}

impl Drop for LedgerLock {
    fn drop(&mut self) {
        let owner_path = self.path.join(LOCK_OWNER);
        let owned = umadev_state::fs::read_bounded(&owner_path, MAX_OWNER_BYTES)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<LockOwner>(&bytes).ok())
            .is_some_and(|owner| owner.nonce == self.nonce);
        if owned {
            let _ = umadev_state::fs::remove_regular_file(&owner_path);
            let _ = umadev_state::fs::remove_empty_dir(&self.path);
        }
    }
}

fn process_mutex() -> MutexGuard<'static, ()> {
    static MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
    MUTEX
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

fn new_nonce(tag: &str) -> String {
    let sequence = NONCE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    format!("{tag}-{:x}-{nanos:x}-{sequence:x}", std::process::id())
}

fn new_record_id() -> String {
    new_nonce("usage-v2")
}

fn bounded_label(value: &str) -> String {
    value
        .trim()
        .chars()
        .filter(|ch| !ch.is_control())
        .take(MAX_LABEL_CHARS)
        .collect()
}

fn legacy_record_id(source: &str, line: usize, legacy: &LegacyUsageRecordV1) -> String {
    let mut digest = Sha256::new();
    digest.update(b"umadev-usage-legacy-v1\0");
    digest.update(source.as_bytes());
    digest.update(line.to_le_bytes());
    digest.update(legacy.ts.to_le_bytes());
    digest.update(legacy.backend.as_bytes());
    digest.update(legacy.phase.as_bytes());
    digest.update(legacy.tokens.to_le_bytes());
    let bytes = digest.finalize();
    let mut encoded = String::with_capacity(32);
    for byte in bytes.iter().take(16) {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    format!("legacy-v1-{encoded}")
}

fn lock_age_ms(lock_path: &Path) -> Option<u64> {
    let owner_created =
        umadev_state::fs::read_bounded(&lock_path.join(LOCK_OWNER), MAX_OWNER_BYTES)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<LockOwner>(&bytes).ok())
            .map(|owner| owner.created_at_ms);
    let modified = fs::symlink_metadata(lock_path)
        .ok()
        .filter(umadev_state::fs::metadata_is_real_dir)
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .and_then(|duration| u64::try_from(duration.as_millis()).ok());
    owner_created
        .or(modified)
        .map(|created| now_ms().saturating_sub(created))
}

fn reclaim_stale_lock(lock_path: &Path) -> bool {
    let stale_ms = u64::try_from(LOCK_STALE_AFTER.as_millis()).unwrap_or(u64::MAX);
    if !umadev_state::fs::real_dir(lock_path)
        || lock_age_ms(lock_path).is_none_or(|age| age <= stale_ms)
    {
        return false;
    }
    let Some(parent) = lock_path.parent() else {
        return false;
    };
    let tomb = parent.join(format!(".usage-ledger.stale.{}", new_nonce("reclaim")));
    if fs::rename(lock_path, &tomb).is_err() {
        return false;
    }
    let _ = umadev_state::fs::remove_regular_file(&tomb.join(LOCK_OWNER));
    let _ = umadev_state::fs::remove_empty_dir(&tomb);
    true
}

fn acquire_ledger_lock_with_timeout(
    parent: &Path,
    timeout: Duration,
) -> std::io::Result<LedgerLock> {
    if !umadev_state::fs::real_dir(parent) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "usage ledger parent is not a real directory",
        ));
    }
    let path = parent.join(LOCK_DIR);
    let started = Instant::now();
    loop {
        match fs::create_dir(&path) {
            Ok(()) => {
                let owner = LockOwner {
                    created_at_ms: now_ms(),
                    pid: std::process::id(),
                    nonce: new_nonce("owner"),
                };
                let bytes = serde_json::to_vec(&owner).map_err(std::io::Error::other)?;
                if let Err(error) = umadev_state::fs::atomic_write(&path.join(LOCK_OWNER), &bytes) {
                    let _ = umadev_state::fs::remove_empty_dir(&path);
                    return Err(error);
                }
                return Ok(LedgerLock {
                    path,
                    nonce: owner.nonce,
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let _ = reclaim_stale_lock(&path);
                if started.elapsed() >= timeout {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        "usage ledger is busy in another process",
                    ));
                }
                std::thread::sleep(LOCK_POLL);
            }
            Err(error) => return Err(error),
        }
    }
}

fn acquire_ledger_lock(parent: &Path) -> std::io::Result<LedgerLock> {
    acquire_ledger_lock_with_timeout(parent, LOCK_TIMEOUT)
}

fn archive_path(path: &Path, index: usize) -> PathBuf {
    let name = path
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    path.with_file_name(format!("{name}.{index}"))
}

fn source_paths(path: &Path, config: LedgerConfig) -> Vec<PathBuf> {
    (1..=config.max_archives)
        .rev()
        .map(|index| archive_path(path, index))
        .chain(std::iter::once(path.to_path_buf()))
        .collect()
}

fn safe_read(path: &Path, max_bytes: u64) -> std::io::Result<Vec<u8>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if umadev_state::fs::metadata_is_real_file(&metadata) => {
            umadev_state::fs::read_bounded(path, max_bytes)
        }
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "usage ledger path is not a regular file",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

fn safe_read_tail(path: &Path, max_bytes: u64) -> std::io::Result<Vec<u8>> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let mut file = options.open(path)?;
    let metadata = file.metadata()?;
    if !umadev_state::fs::metadata_is_real_file(&metadata) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "usage ledger path is not a regular file",
        ));
    }
    let start = metadata.len().saturating_sub(max_bytes);
    let aligned = if start == 0 {
        true
    } else {
        file.seek(SeekFrom::Start(start - 1))?;
        let mut previous = [0_u8; 1];
        file.read_exact(&mut previous)?;
        previous[0] == b'\n'
    };
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::with_capacity(usize::try_from(max_bytes).unwrap_or(0));
    file.take(max_bytes).read_to_end(&mut bytes)?;
    if !aligned {
        if let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') {
            bytes.drain(..=newline);
        } else {
            bytes.clear();
        }
    }
    Ok(bytes)
}

enum ParsedLine {
    V2(UsageRecordV2),
    Legacy(UsageRecordV2),
    Corrupt,
}

fn parse_line(raw: &str, source: &str, line: usize) -> ParsedLine {
    if raw.trim().is_empty() || raw.len() > MAX_ROW_BYTES {
        return ParsedLine::Corrupt;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return ParsedLine::Corrupt;
    };
    if value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        == Some(2)
    {
        return serde_json::from_value::<UsageRecordV2>(value)
            .ok()
            .filter(UsageRecordV2::valid)
            .map_or(ParsedLine::Corrupt, ParsedLine::V2);
    }
    serde_json::from_value::<LegacyUsageRecordV1>(value).map_or(ParsedLine::Corrupt, |legacy| {
        ParsedLine::Legacy(UsageRecordV2::from_legacy(&legacy, source, line))
    })
}

#[derive(Default)]
struct ParsedFile {
    records: Vec<UsageRecordV2>,
    corrupt_rows: u64,
    changed: bool,
}

fn parse_file(path: &Path, config: LedgerConfig) -> std::io::Result<ParsedFile> {
    let read_limit = config
        .max_bytes
        .saturating_add(u64::try_from(MAX_ROW_BYTES).unwrap_or(u64::MAX));
    let oversized = fs::symlink_metadata(path)
        .ok()
        .filter(umadev_state::fs::metadata_is_real_file)
        .is_some_and(|metadata| metadata.len() > read_limit);
    let bytes = if oversized {
        safe_read_tail(path, read_limit)?
    } else {
        safe_read(path, read_limit)?
    };
    if bytes.is_empty() {
        return Ok(ParsedFile {
            corrupt_rows: u64::from(oversized),
            changed: oversized,
            ..ParsedFile::default()
        });
    }
    let body = String::from_utf8_lossy(&bytes);
    let source = path.to_string_lossy();
    let mut parsed = ParsedFile {
        corrupt_rows: u64::from(oversized),
        changed: oversized,
        ..ParsedFile::default()
    };
    for (index, line) in body.lines().enumerate() {
        match parse_line(line, &source, index) {
            ParsedLine::V2(record) => parsed.records.push(record),
            ParsedLine::Legacy(record) => {
                parsed.changed = true;
                parsed.records.push(record);
            }
            ParsedLine::Corrupt => {
                parsed.changed = true;
                parsed.corrupt_rows = parsed.corrupt_rows.saturating_add(1);
            }
        }
    }
    // A non-newline-terminated tail is a crash-torn row. `lines()` still
    // parsed it if it happened to be valid; otherwise it is counted above.
    Ok(parsed)
}

fn render_records(records: &[UsageRecordV2]) -> std::io::Result<Vec<u8>> {
    let mut body = Vec::new();
    for record in records {
        if !record.valid() {
            continue;
        }
        let row = serde_json::to_vec(record).map_err(std::io::Error::other)?;
        if row.len() > MAX_ROW_BYTES {
            continue;
        }
        body.extend_from_slice(&row);
        body.push(b'\n');
    }
    Ok(body)
}

fn normalize_existing_files(path: &Path, config: LedgerConfig) -> std::io::Result<u64> {
    let mut corrupt = 0_u64;
    for source in source_paths(path, config) {
        let parsed = match parse_file(&source, config) {
            Ok(parsed) => parsed,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        corrupt = corrupt.saturating_add(parsed.corrupt_rows);
        if parsed.changed {
            let body = render_records(&parsed.records)?;
            umadev_state::fs::atomic_write(&source, &body)?;
        }
    }
    Ok(corrupt)
}

fn rotate_locked(path: &Path, config: LedgerConfig) -> std::io::Result<()> {
    if config.max_archives == 0 {
        let _ = umadev_state::fs::remove_regular_file(path)?;
        return Ok(());
    }
    let oldest = archive_path(path, config.max_archives);
    let _ = umadev_state::fs::remove_regular_file(&oldest)?;
    for index in (1..config.max_archives).rev() {
        let source = archive_path(path, index);
        let target = archive_path(path, index + 1);
        match fs::symlink_metadata(&source) {
            Ok(metadata) if umadev_state::fs::metadata_is_real_file(&metadata) => {
                fs::rename(source, target)?;
            }
            Ok(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "usage archive is not a regular file",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if umadev_state::fs::metadata_is_real_file(&metadata) => {
            fs::rename(path, archive_path(path, 1))?;
        }
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "usage ledger is not a regular file",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    Ok(())
}

fn append_record_to_path(
    path: &Path,
    record: &UsageRecordV2,
    config: LedgerConfig,
) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "usage ledger has no parent directory",
        )
    })?;
    fs::create_dir_all(parent)?;
    if !umadev_state::fs::real_dir(parent) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "usage ledger parent is not a real directory",
        ));
    }
    let _process = process_mutex();
    let _cross_process = acquire_ledger_lock(parent)?;
    let _ = normalize_existing_files(path, config)?;
    let mut body = safe_read(
        path,
        config
            .max_bytes
            .saturating_add(u64::try_from(MAX_ROW_BYTES).unwrap_or(u64::MAX)),
    )?;
    let row = render_records(std::slice::from_ref(record))?;
    if row.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid usage record",
        ));
    }
    let projected = u64::try_from(body.len().saturating_add(row.len())).unwrap_or(u64::MAX);
    if !body.is_empty() && projected > config.max_bytes {
        rotate_locked(path, config)?;
        body.clear();
    }
    body.extend_from_slice(&row);
    umadev_state::fs::atomic_write(path, &body)
}

/// Path to the durable usage ledger.
#[must_use]
pub fn usage_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|value| !value.is_empty()))
        .or_else(|| {
            let drive = std::env::var_os("HOMEDRIVE")?;
            let path = std::env::var_os("HOMEPATH")?;
            if drive.is_empty() || path.is_empty() {
                return None;
            }
            let mut combined = drive;
            combined.push(path);
            Some(combined)
        });
    home.map_or_else(
        || PathBuf::from(".umadev").join("usage.jsonl"),
        |home| PathBuf::from(home).join(".umadev").join("usage.jsonl"),
    )
}

/// Persist a complete runtime usage snapshot with its native quality flags.
/// Fail-open: metering failure never changes the worker result.
pub fn record_runtime_usage(backend: &str, phase: Phase, usage: Usage) {
    let record = UsageRecordV2::from_runtime(backend, phase.id(), usage);
    if let Err(error) = append_record_to_path(&usage_path(), &record, LedgerConfig::default()) {
        tracing::warn!(%error, "usage ledger append failed");
    }
}

/// Persist a heuristic token value. It is always labelled `estimated`, never
/// base-reported or exact, and never receives a fabricated cost.
pub fn record_estimated_usage(backend: &str, phase: Phase, tokens: u64) {
    let record = UsageRecordV2::estimated(backend, phase.id(), tokens);
    if let Err(error) = append_record_to_path(&usage_path(), &record, LedgerConfig::default()) {
        tracing::warn!(%error, "estimated usage ledger append failed");
    }
}

/// Backwards-compatible estimated-value entry point.
///
/// This three-argument API lacks the base's quality flags, input/output split,
/// and trustworthy cost. It therefore cannot write an exact row. New native
/// host paths must use [`record_runtime_usage`].
pub fn record_usage(backend: &str, phase: Phase, tokens: u64) {
    record_estimated_usage(backend, phase, tokens);
}

fn phase_order(phase: &str) -> usize {
    umadev_spec::PHASE_CHAIN
        .iter()
        .position(|candidate| candidate.id() == phase)
        .unwrap_or(usize::MAX)
}

#[derive(Default)]
struct ParsedLedger {
    records: Vec<UsageRecordV2>,
    corrupt_rows: u64,
}

fn read_ledger(path: &Path, config: LedgerConfig) -> ParsedLedger {
    let mut parsed = ParsedLedger::default();
    let mut seen = BTreeSet::new();
    for source in source_paths(path, config) {
        match parse_file(&source, config) {
            Ok(file) => {
                parsed.corrupt_rows = parsed.corrupt_rows.saturating_add(file.corrupt_rows);
                for record in file.records {
                    if seen.insert(record.record_id.clone()) {
                        parsed.records.push(record);
                    }
                }
            }
            Err(error) => {
                tracing::warn!(path = %source.display(), %error, "usage ledger source skipped");
                parsed.corrupt_rows = parsed.corrupt_rows.saturating_add(1);
            }
        }
    }
    parsed
}

#[derive(Default)]
struct Aggregate {
    calls: u64,
    tokens: TokenBreakdown,
    cost: CostBreakdown,
}

impl Aggregate {
    fn add(&mut self, record: &UsageRecordV2) {
        self.calls = self.calls.saturating_add(1);
        self.tokens.add(&record.tokens);
        self.cost.add(&record.cost);
    }
}

fn compat_u64(value: u128) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn build_usage_report(mut parsed: ParsedLedger) -> UsageReport {
    if parsed.records.is_empty() {
        return UsageReport {
            corrupt_rows: parsed.corrupt_rows,
            ..UsageReport::default()
        };
    }
    parsed.records.sort_by_key(|record| record.ts_ms);
    let migrated_v1_calls = parsed
        .records
        .iter()
        .filter(|record| record.tokens.source == MeasurementSource::LegacyV1)
        .count() as u64;
    let mut runs = Vec::new();
    let mut run_records: Vec<UsageRecordV2> = Vec::new();
    let mut previous_ts = None;

    let flush = |records: &[UsageRecordV2], index: usize| -> RunUsage {
        let mut phases: BTreeMap<String, Aggregate> = BTreeMap::new();
        let mut backends = BTreeSet::new();
        let mut total = Aggregate::default();
        for record in records {
            if !record.backend.is_empty() {
                backends.insert(record.backend.clone());
            }
            phases.entry(record.phase.clone()).or_default().add(record);
            total.add(record);
        }
        let mut phase_rows: Vec<_> = phases
            .into_iter()
            .map(|(phase, aggregate)| PhaseUsage {
                phase,
                calls: aggregate.calls,
                tokens: compat_u64(aggregate.tokens.known_numeric_sum()),
                token_breakdown: aggregate.tokens,
                cost_breakdown: aggregate.cost,
            })
            .collect();
        phase_rows.sort_by_key(|row| (phase_order(&row.phase), row.phase.clone()));
        RunUsage {
            index,
            backends: backends.into_iter().collect(),
            phases: phase_rows,
            calls: total.calls,
            tokens: compat_u64(total.tokens.known_numeric_sum()),
            token_breakdown: total.tokens,
            cost_breakdown: total.cost,
        }
    };

    for record in parsed.records {
        let starts_new_run =
            previous_ts.is_some_and(|ts: u64| record.ts_ms.saturating_sub(ts) > RUN_GAP_MS);
        if starts_new_run && !run_records.is_empty() {
            runs.push(flush(&run_records, runs.len() + 1));
            run_records.clear();
        }
        previous_ts = Some(record.ts_ms);
        run_records.push(record);
    }
    if !run_records.is_empty() {
        runs.push(flush(&run_records, runs.len() + 1));
    }

    let mut total_tokens = TokenBreakdown::default();
    let mut total_cost = CostBreakdown::default();
    let mut total_calls = 0_u64;
    let mut backends = BTreeSet::new();
    for run in &runs {
        total_calls = total_calls.saturating_add(run.calls);
        total_tokens.merge(run.token_breakdown);
        total_cost.merge(run.cost_breakdown);
        backends.extend(run.backends.iter().cloned());
    }
    UsageReport {
        runs,
        total_calls,
        total_tokens: compat_u64(total_tokens.known_numeric_sum()),
        token_breakdown: total_tokens,
        cost_breakdown: total_cost,
        backends: backends.into_iter().collect(),
        corrupt_rows: parsed.corrupt_rows,
        migrated_v1_calls,
    }
}

fn usage_report_from_path(path: &Path, config: LedgerConfig) -> UsageReport {
    if !path.exists() && !(1..=config.max_archives).any(|index| archive_path(path, index).exists())
    {
        return UsageReport::default();
    }
    // A locked snapshot cannot observe the middle of archive rotation. If a
    // stale/contended lock cannot be acquired, fail open with an atomic-file
    // snapshot; record-id deduplication prevents rotation duplicates. Do not
    // take the process-wide writer mutex here: a read-only TUI/CLI display
    // must keep the same bounded wait even while an unrelated ledger write is
    // active in this process. The filesystem lock already covers both local
    // and cross-process writers.
    let _cross_process = path
        .parent()
        .and_then(|parent| acquire_ledger_lock_with_timeout(parent, READ_LOCK_TIMEOUT).ok());
    build_usage_report(read_ledger(path, config))
}

/// Read retained usage records from the live ledger and bounded archives.
#[must_use]
pub fn usage_report() -> UsageReport {
    usage_report_from_path(&usage_path(), LedgerConfig::default())
}

/// Render a concise quality-aware summary without inventing token or cost
/// precision. This legacy plain-text surface is intentionally language-neutral
/// enough for diagnostics; CLI/TUI formatters should consume [`UsageReport`].
#[must_use]
pub fn usage_summary() -> String {
    let report = usage_report();
    if report.is_empty() {
        return "还没有使用记录。跑一次需求(run)后会自动统计。".to_string();
    }
    let tokens = report.token_breakdown;
    let mut output = format!("使用统计(共 {} 次宿主调用):\n", report.total_calls);
    for run in &report.runs {
        for phase in &run.phases {
            output.push_str(&format!("  {}: {} 次\n", phase.phase, phase.calls));
        }
    }
    if tokens.exact_calls > 0 {
        output.push_str(&format!("精确 token: {}\n", tokens.exact_tokens));
    }
    if tokens.lower_bound_calls > 0 {
        output.push_str(&format!(
            "底座报告下界 token: 至少 {} ({} 次不完整报告)\n",
            tokens.lower_bound_tokens, tokens.lower_bound_calls
        ));
    }
    if tokens.estimated_calls > 0 {
        output.push_str(&format!(
            "估算 token: 约 {} ({} 次；不可用于计费)\n",
            tokens.estimated_tokens, tokens.estimated_calls
        ));
    }
    if tokens.unknown_calls > 0 {
        output.push_str(&format!("token 未知: {} 次\n", tokens.unknown_calls));
    }
    let cost = report.cost_breakdown;
    if cost.exact_calls == 0 {
        output.push_str("费用: 未知（仅接受底座直接报告的完整费用）");
    } else if let Some(total) = cost.complete_total_usd_ticks() {
        output.push_str(&format!("底座报告的精确费用: {}", format_usd_ticks(total)));
    } else {
        output.push_str(&format!(
            "底座已报告费用: {} (覆盖 {}/{} 次；其余未知)",
            format_usd_ticks(cost.reported_usd_ticks),
            cost.exact_calls,
            report.total_calls
        ));
    }
    output
}

/// Format trusted USD ticks without floating-point rounding.
#[must_use]
pub fn format_usd_ticks(ticks: u128) -> String {
    let dollars = ticks / USD_TICKS_PER_DOLLAR;
    let fraction = ticks % USD_TICKS_PER_DOLLAR;
    if fraction == 0 {
        return format!("${dollars}");
    }
    let mut decimal = format!("{fraction:010}");
    while decimal.ends_with('0') {
        decimal.pop();
    }
    format!("${dollars}.{decimal}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(max_bytes: u64, max_archives: usize) -> LedgerConfig {
        LedgerConfig {
            max_bytes,
            max_archives,
        }
    }

    fn report_for_records(records: Vec<UsageRecordV2>) -> UsageReport {
        build_usage_report(ParsedLedger {
            records,
            corrupt_rows: 0,
        })
    }

    #[test]
    fn runtime_quality_and_trusted_cost_are_never_conflated() {
        let exact = UsageRecordV2::from_runtime(
            "codex",
            "frontend",
            Usage {
                input_tokens: 100,
                output_tokens: 50,
                total_tokens: 150,
                cached_read_tokens: 20,
                cached_write_tokens: 5,
                reasoning_tokens: 10,
                model_calls: 2,
                num_turns: 1,
                cost_usd_ticks: Some(250_000_000),
                usage_incomplete: false,
                cost_partial: false,
                ..Usage::default()
            },
        );
        let lower = UsageRecordV2::from_runtime(
            "opencode",
            "frontend",
            Usage {
                input_tokens: 10,
                output_tokens: 5,
                total_tokens: 15,
                cost_usd_ticks: Some(99),
                usage_incomplete: true,
                ..Usage::default()
            },
        );
        let unknown = UsageRecordV2::from_runtime("grok-build", "frontend", Usage::default());
        let estimated = UsageRecordV2::estimated("claude-code", "frontend", 42);
        let report = report_for_records(vec![exact, lower, unknown, estimated]);
        assert_eq!(report.token_breakdown.exact_tokens, 150);
        assert_eq!(report.token_breakdown.lower_bound_tokens, 15);
        assert_eq!(report.token_breakdown.estimated_tokens, 42);
        assert_eq!(report.token_breakdown.unknown_calls, 1);
        assert_eq!(report.cost_breakdown.reported_usd_ticks, 250_000_000);
        assert_eq!(report.cost_breakdown.exact_calls, 1);
        assert_eq!(report.cost_breakdown.unknown_calls, 3);
        assert_eq!(report.cost_breakdown.complete_total_usd_ticks(), None);
    }

    #[test]
    fn contradictory_complete_usage_is_downgraded_and_cost_scrubbed() {
        let record = UsageRecordV2::from_runtime(
            "codex",
            "research",
            Usage {
                input_tokens: 30,
                output_tokens: 20,
                total_tokens: 7,
                cost_usd_ticks: Some(123),
                usage_incomplete: false,
                cost_partial: false,
                ..Usage::default()
            },
        );
        assert_eq!(record.tokens.quality, MeasurementQuality::LowerBound);
        assert_eq!(record.tokens.total_tokens, Some(50));
        assert_eq!(record.cost.quality, CostQuality::Unknown);
        assert_eq!(record.cost.usd_ticks, None);
    }

    #[test]
    fn legacy_rows_migrate_to_v2_estimates_and_damage_is_removed_on_append() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("usage.jsonl");
        fs::write(
            &path,
            concat!(
                "{\"ts\":1000,\"backend\":\"claude-code\",\"phase\":\"research\",\"tokens\":100}\n",
                "torn json {\n",
                "{\"ts\":1010,\"backend\":\"codex\",\"phase\":\"frontend\",\"tokens\":200}\n"
            ),
        )
        .unwrap();
        let before = usage_report_from_path(&path, config(64 * 1024, 3));
        assert_eq!(before.total_calls, 2);
        assert_eq!(before.migrated_v1_calls, 2);
        assert_eq!(before.token_breakdown.estimated_tokens, 300);
        assert_eq!(before.corrupt_rows, 1);

        append_record_to_path(
            &path,
            &UsageRecordV2::estimated("grok-build", "quality", 50),
            config(64 * 1024, 3),
        )
        .unwrap();
        let body = fs::read_to_string(&path).unwrap();
        assert!(!body.contains("torn json"));
        assert!(body.lines().all(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .and_then(|value| {
                    value
                        .get("schema_version")
                        .and_then(serde_json::Value::as_u64)
                })
                == Some(2)
        }));
        let after = usage_report_from_path(&path, config(64 * 1024, 3));
        assert_eq!(after.total_calls, 3);
        assert_eq!(after.migrated_v1_calls, 2);
        assert_eq!(after.token_breakdown.estimated_tokens, 350);
        assert_eq!(after.corrupt_rows, 0);
    }

    #[test]
    fn oversized_damage_recovers_recent_valid_rows_and_unwedges_future_appends() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("usage.jsonl");
        let mut body = vec![b'x'; 32 * 1024];
        body.push(b'\n');
        body.extend_from_slice(
            b"{\"ts\":1010,\"backend\":\"codex\",\"phase\":\"frontend\",\"tokens\":200}\n",
        );
        fs::write(&path, body).unwrap();
        let settings = config(1_024, 2);
        let recovered = usage_report_from_path(&path, settings);
        assert_eq!(recovered.total_calls, 1);
        assert_eq!(recovered.token_breakdown.estimated_tokens, 200);
        assert_eq!(recovered.corrupt_rows, 1);

        append_record_to_path(
            &path,
            &UsageRecordV2::estimated("grok-build", "quality", 50),
            settings,
        )
        .unwrap();
        assert!(fs::metadata(&path).unwrap().len() <= settings.max_bytes);
        let after = usage_report_from_path(&path, settings);
        assert_eq!(after.total_calls, 2);
        assert_eq!(after.token_breakdown.estimated_tokens, 250);
        assert_eq!(after.corrupt_rows, 0);
    }

    #[test]
    fn bounded_rotation_keeps_only_configured_regular_archives() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("usage.jsonl");
        let settings = config(1_100, 3);
        for index in 0..40_u64 {
            let mut record = UsageRecordV2::estimated("codex", "frontend", index);
            record.ts_ms = index;
            append_record_to_path(&path, &record, settings).unwrap();
        }
        assert!(fs::metadata(&path).unwrap().len() <= settings.max_bytes);
        for index in 1..=settings.max_archives {
            let archive = archive_path(&path, index);
            assert!(archive.exists());
            assert!(fs::metadata(archive).unwrap().len() <= settings.max_bytes);
        }
        assert!(!archive_path(&path, settings.max_archives + 1).exists());
        let report = usage_report_from_path(&path, settings);
        assert!(!report.is_empty());
        assert!(
            report.total_calls < 40,
            "oldest bounded archives are pruned"
        );
        assert_eq!(report.corrupt_rows, 0);
    }

    #[test]
    fn stale_cross_process_lock_is_reclaimed() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("usage.jsonl");
        let lock = temp.path().join(LOCK_DIR);
        fs::create_dir(&lock).unwrap();
        let owner = LockOwner {
            created_at_ms: 0,
            pid: u32::MAX,
            nonce: "dead-owner".to_string(),
        };
        umadev_state::fs::atomic_write(
            &lock.join(LOCK_OWNER),
            &serde_json::to_vec(&owner).unwrap(),
        )
        .unwrap();
        append_record_to_path(
            &path,
            &UsageRecordV2::estimated("codex", "research", 1),
            config(64 * 1024, 3),
        )
        .unwrap();
        assert!(!lock.exists());
        assert_eq!(
            usage_report_from_path(&path, config(64 * 1024, 3)).total_calls,
            1
        );
    }

    #[test]
    fn read_snapshot_falls_back_quickly_behind_a_fresh_writer_lock() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("usage.jsonl");
        let record = UsageRecordV2::estimated("codex", "research", 7);
        umadev_state::fs::atomic_write(&path, &render_records(&[record]).unwrap()).unwrap();
        let lock = temp.path().join(LOCK_DIR);
        fs::create_dir(&lock).unwrap();
        let owner = LockOwner {
            created_at_ms: now_ms(),
            pid: u32::MAX,
            nonce: "live-writer".to_string(),
        };
        umadev_state::fs::atomic_write(
            &lock.join(LOCK_OWNER),
            &serde_json::to_vec(&owner).unwrap(),
        )
        .unwrap();

        let started = Instant::now();
        let report = usage_report_from_path(&path, config(64 * 1024, 3));
        assert_eq!(report.total_calls, 1);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "read-only usage display must not inherit the 10s writer wait"
        );
        fs::remove_file(lock.join(LOCK_OWNER)).unwrap();
        fs::remove_dir(lock).unwrap();
    }

    #[test]
    fn cross_process_append_child() {
        let Some(path) = std::env::var_os("UMADEV_USAGE_CHILD_PATH") else {
            return;
        };
        let child = std::env::var("UMADEV_USAGE_CHILD_INDEX")
            .unwrap()
            .parse::<u64>()
            .unwrap();
        let count = std::env::var("UMADEV_USAGE_CHILD_COUNT")
            .unwrap()
            .parse::<u64>()
            .unwrap();
        for index in 0..count {
            let mut record = UsageRecordV2::estimated(
                &format!("child-{child}"),
                "frontend",
                child * 10_000 + index,
            );
            record.ts_ms = child * count + index;
            append_record_to_path(Path::new(&path), &record, config(8 * 1024 * 1024, 3)).unwrap();
        }
    }

    #[test]
    fn concurrent_processes_append_without_loss_or_torn_rows() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("usage.jsonl");
        let children = 6_u64;
        let per_child = 12_u64;
        let executable = std::env::current_exe().unwrap();
        let mut processes = Vec::new();
        for child in 0..children {
            processes.push(
                std::process::Command::new(&executable)
                    .args([
                        "--exact",
                        "usage_ledger::tests::cross_process_append_child",
                        "--nocapture",
                    ])
                    .env("UMADEV_USAGE_CHILD_PATH", &path)
                    .env("UMADEV_USAGE_CHILD_INDEX", child.to_string())
                    .env("UMADEV_USAGE_CHILD_COUNT", per_child.to_string())
                    .spawn()
                    .unwrap(),
            );
        }
        for mut child in processes {
            assert!(child.wait().unwrap().success());
        }
        let report = usage_report_from_path(&path, config(8 * 1024 * 1024, 3));
        assert_eq!(report.total_calls, children * per_child);
        assert_eq!(report.token_breakdown.estimated_calls, children * per_child);
        assert_eq!(report.corrupt_rows, 0);
        let raw = fs::read_to_string(path).unwrap();
        let ids: BTreeSet<_> = raw
            .lines()
            .map(|line| {
                serde_json::from_str::<UsageRecordV2>(line)
                    .unwrap()
                    .record_id
            })
            .collect();
        assert_eq!(ids.len() as u64, children * per_child);
    }

    #[test]
    fn trusted_cost_formatting_has_no_float_or_flat_rate() {
        assert_eq!(format_usd_ticks(10_000_000_000), "$1");
        assert_eq!(format_usd_ticks(250_000_000), "$0.025");
    }

    #[test]
    fn aggregate_quality_does_not_hide_unknown_or_estimated_calls() {
        let mut breakdown = TokenBreakdown::default();
        breakdown.add(&UsageRecordV2::estimated("x", "y", 10).tokens);
        assert_eq!(breakdown.quality(), MeasurementQuality::Estimated);
        breakdown.add(&UsageRecordV2::from_runtime("x", "y", Usage::default()).tokens);
        assert_eq!(breakdown.quality(), MeasurementQuality::Unknown);
        assert_eq!(breakdown.estimated_tokens, 10);
        assert_eq!(breakdown.unknown_calls, 1);
    }
}
