//! Retrieval-quality feedback — a per-chunk **usefulness prior** that lets the
//! curated-knowledge ranking SELF-TUNE from build outcomes.
//!
//! Static BM25 / vector / RRF ranking answers "which chunk best matches the
//! query?" but never learns from what happened after a chunk was injected. This
//! module adds a thin, bounded, fail-open memory: a chunk that preceded a CLEAN
//! step earns a usefulness boost; one that preceded a FAILURE is demoted. Over
//! many runs the corpus's own track record nudges ranking, WITHOUT discarding
//! lexical/semantic relevance. Retrieval blends only 10% of this prior, so it can
//! resolve a near tie but cannot overrule materially stronger current evidence.
//!
//! ## Where it lives (cross-project)
//!
//! The curated `knowledge/` corpus is shared across every project, so its track
//! record is too: the store is a single JSON file under the user home
//! (`~/.umadev/knowledge-usefulness.json`), NOT per-project. A per-project
//! transient "which chunks were surfaced for this step" snapshot lives in the
//! agent crate (mirroring its surfaced-lesson-identity snapshot); this module
//! only owns the durable, cross-project prior it feeds.
//!
//! ## Conservatism contract
//!
//! - **Sample-gated.** Below [`crate::usefulness::MIN_SAMPLES`] observations a chunk's weight is
//!   NEUTRAL (`1.0`) — a single observation never moves ranking, and a fresh
//!   corpus (no observations at all) ranks byte-for-byte as before.
//! - **Bounded weight.** Once well-sampled the weight stays within
//!   `[WEIGHT_MIN, WEIGHT_MAX]` (`0.3..=1.2`) — a proven-helpful chunk lifts, a
//!   proven-harmful one sinks, but relevance still dominates.
//! - **Bounded store.** At most [`crate::usefulness::MAX_ENTRIES`] chunk keys are retained
//!   (least-recently-updated evicted first, deterministically), and immutable
//!   outcome evidence is retained in a bounded newest-first window.
//! - **Fail-open.** A missing / corrupt / unwritable store degrades to the
//!   neutral prior (today's static ranking) — never a panic, never an error.
//! - **Deterministic scoring.** Pure integer bookkeeping + a fixed weight map;
//!   no clock enters the weight formula and no brain is consulted. File time is
//!   used only to retain the newest evidence when the hard history cap is hit.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Minimum outcome observations (`helpful + harmful`) before a chunk's prior may
/// move its rank. Below this the weight is neutral `1.0`, so a single observation
/// can never dominate and an unobserved corpus is unchanged.
pub const MIN_SAMPLES: u32 = 3;

/// Hard cap on distinct chunk keys the store retains (bounded memory). When
/// exceeded, the least-recently-updated entries are evicted first.
pub const MAX_ENTRIES: usize = 4096;

/// Defensive cap on how many chunk keys ONE outcome record processes, so a
/// caller can never explode the store in a single call (the caller already caps
/// the per-step snapshot, this is belt-and-suspenders).
pub const MAX_RECORD_BATCH: usize = 64;

/// Lowest multiplicative weight a proven-HARMFUL chunk can sink to.
const WEIGHT_MIN: f32 = 0.3;
/// Highest multiplicative weight a proven-HELPFUL chunk can rise to.
const WEIGHT_MAX: f32 = 1.2;

/// The neutral weight applied to an unobserved / thinly-sampled chunk — leaves
/// the BM25/vector ranking exactly as it was.
pub const NEUTRAL_WEIGHT: f32 = 1.0;

/// Store filename under the `.umadev` state dir in the user home.
const USEFULNESS_FILE: &str = "knowledge-usefulness.json";
/// The `.umadev` state subdir under the user home the store file lives in.
const STATE_SUBDIR: &str = ".umadev";

/// Immutable outcome records live here. One file is published per sent-turn
/// receipt, so concurrent processes never perform a shared read-modify-write and
/// replaying a crash cannot count the same receipt twice.
const OUTCOMES_SUBDIR: &str = "knowledge-outcomes";

/// Per-installation secret used only to pseudonymise canonical project
/// identities before they enter the global outcome store.
const SCOPE_KEY_FILE: &str = "scope-key-v1";
const SCOPE_KEY_BYTES: usize = 32;
const PROJECT_SCOPE_ID_VERSION: &str = "kps1";
const OUTCOME_RECORD_VERSION: u8 = 2;
const MAX_OUTCOME_BYTES: u64 = 256 * 1024;
const MAX_LEGACY_STORE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_OUTCOME_RECORDS: usize = 20_000;
const OUTCOME_PRUNE_TARGET: usize = 18_000;

/// Version tag for exact knowledge-memory identities.
const MEMORY_ID_VERSION: &str = "km1";

static OUTCOME_TEMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static SCOPE_KEY_TEMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Exact identity of one knowledge chunk that actually reached a host turn.
///
/// `path + section` is not sufficient: two markdown chunks can legitimately
/// share both, and an edited chunk must not inherit an older body's outcome.
/// `id` is therefore a versioned SHA-256 over path, section, and the complete
/// chunk body. The human-readable fields remain for diagnostics and legacy
/// compatibility only.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MemoryRef {
    /// Stable, content-bound identity (`km1-<sha256>`).
    pub id: String,
    /// Corpus-relative source path.
    pub path: String,
    /// Markdown section heading.
    pub section: String,
}

impl MemoryRef {
    /// Build an exact identity from the complete retrieved chunk, not its
    /// prompt-truncated excerpt.
    #[must_use]
    pub fn from_parts(path: &str, section: &str, body: &str) -> Self {
        Self {
            id: memory_id(path, section, body),
            path: path.to_string(),
            section: section.to_string(),
        }
    }
}

/// Derive the stable content identity used by both retrieval ranking and sent
/// receipts.
#[must_use]
pub fn memory_id(path: &str, section: &str, body: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(MEMORY_ID_VERSION.as_bytes());
    for value in [path, section, body] {
        hasher.update([0]);
        hasher.update(value.as_bytes());
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    format!("{MEMORY_ID_VERSION}-{hex}")
}

/// Result of atomically publishing one receipt's immutable outcome record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiptOutcomeWrite {
    /// This call published the record.
    Recorded,
    /// The same receipt and payload had already been published.
    AlreadyRecorded,
    /// Global utility capture is disabled or its policy is unreadable. This is
    /// a terminal privacy decision, not a storage failure to retry.
    SuppressedByPolicy,
    /// Storage was unavailable; the caller should retain its local journal and
    /// retry later.
    Unavailable,
    /// The receipt id already exists with a different payload. No mutation was
    /// made; first writer wins.
    Conflict,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiptOutcomeRecord {
    version: u8,
    receipt_id: String,
    project_scope_id: String,
    helpful: bool,
    memory_ids: Vec<String>,
}

/// Version-1 reader only. New records never persist these human-readable
/// fields, but existing installations retain their accumulated utility.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct LegacyReceiptOutcomeRecord {
    version: u8,
    receipt_id: String,
    helpful: bool,
    memories: Vec<MemoryRef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AppliedOutcomeRecord {
    receipt_id: String,
    helpful: bool,
    memory_ids: Vec<String>,
}

fn valid_project_scope_id(scope_id: &str) -> bool {
    scope_id
        .strip_prefix(&format!("{PROJECT_SCOPE_ID_VERSION}-"))
        .is_some_and(|digest| {
            digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
}

/// The helpful / harmful tally for one chunk, plus a monotone update stamp used
/// purely for deterministic eviction (NOT a wall clock).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
struct ChunkStat {
    /// Times this chunk was surfaced into a step that then PASSED.
    #[serde(default)]
    helpful: u32,
    /// Times this chunk was surfaced into a step that then FAILED.
    #[serde(default)]
    harmful: u32,
    /// Store-local monotone sequence at the last update — the eviction key.
    #[serde(default)]
    updated: u64,
}

/// The per-chunk usefulness prior, keyed by chunk identity
/// (`corpus-relative path` + `section heading`). A durable, cross-project map
/// loaded fail-open from `~/.umadev/knowledge-usefulness.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsefulnessStore {
    /// Monotone counter stamped onto every touched entry, so eviction has a
    /// deterministic recency order without reading a clock.
    #[serde(default)]
    seq: u64,
    /// `chunk_key -> tally`. Bounded to [`MAX_ENTRIES`].
    #[serde(default)]
    entries: HashMap<String, ChunkStat>,
}

/// Compose the stable identity key for a chunk: `path` + `section`, joined by a
/// unit-separator that cannot appear in either field. This is the SAME identity
/// both the snapshot writer and the ranking reader key on, so they always agree.
#[must_use]
pub fn chunk_key(path: &str, section: &str) -> String {
    format!("{path}\u{1f}{section}")
}

/// Compose the exact-key namespace used by receipt-based feedback. Keeping it
/// distinct from the legacy path/section key makes old stores readable without
/// letting an edited or colliding chunk inherit unrelated evidence.
fn exact_chunk_key(id: &str) -> String {
    format!("id\u{1f}{id}")
}

/// Resolve the store file path under an explicit home dir.
fn usefulness_path(home: &Path) -> PathBuf {
    home.join(STATE_SUBDIR).join(USEFULNESS_FILE)
}

fn outcomes_dir(home: &Path) -> PathBuf {
    home.join(STATE_SUBDIR).join(OUTCOMES_SUBDIR)
}

/// Resolve the user home dir the cross-project store lives under. Honors an
/// explicit `UMADEV_HOME` override first (so callers + tests can redirect it),
/// then `HOME` (Unix) / `USERPROFILE` (Windows). `None` when none is set —
/// callers then no-op (fail-open).
fn usefulness_home() -> Option<PathBuf> {
    std::env::var("UMADEV_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOME").ok())
        .or_else(|| std::env::var("USERPROFILE").ok())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

/// Whether the global usefulness store may accept new feedback under the
/// resolved UmaDev home. Missing/malformed policy is privacy-conservative off.
#[must_use]
pub fn knowledge_utility_capture_enabled() -> bool {
    usefulness_home().is_some_and(|home| knowledge_utility_capture_enabled_in(&home))
}

/// Explicit-home policy check used by the agent's durable receipt bridge.
#[must_use]
pub fn knowledge_utility_capture_enabled_in(home: &Path) -> bool {
    umadev_state::memory::capture_enabled(home, umadev_state::memory::MemoryStore::KnowledgeUtility)
}

fn knowledge_utility_recall_enabled_in(home: &Path) -> bool {
    umadev_state::memory::recall_enabled(home, umadev_state::memory::MemoryStore::KnowledgeUtility)
}

fn scope_key_path(home: &Path) -> Option<PathBuf> {
    let root = std::fs::canonicalize(home).ok()?;
    if !umadev_state::fs::real_dir(&root) {
        return None;
    }
    let state = umadev_state::fs::ensure_real_child_dir(&root, STATE_SUBDIR).ok()?;
    let memory = umadev_state::fs::ensure_real_child_dir(&state, "memory").ok()?;
    Some(memory.join(SCOPE_KEY_FILE))
}

fn read_scope_key(path: &Path) -> Option<[u8; SCOPE_KEY_BYTES]> {
    umadev_state::fs::read_bounded(path, SCOPE_KEY_BYTES as u64)
        .ok()?
        .try_into()
        .ok()
}

/// Create the per-installation scope key with atomic no-replace publication.
/// The candidate itself is written by the shared state layer (no-follow, 0600
/// on Unix, reparse-safe on Windows), then hard-linked into the final name so
/// concurrent processes converge on one key rather than rotating identities.
fn read_or_create_scope_key(home: &Path) -> Option<[u8; SCOPE_KEY_BYTES]> {
    let path = scope_key_path(home)?;
    if let Some(key) = read_scope_key(&path) {
        return Some(key);
    }
    if std::fs::symlink_metadata(&path)
        .is_ok_and(|meta| !umadev_state::fs::metadata_is_real_file(&meta))
    {
        return None;
    }
    let mut key = [0_u8; SCOPE_KEY_BYTES];
    getrandom::getrandom(&mut key).ok()?;
    let parent = path.parent()?;
    if !umadev_state::fs::real_dir(parent) {
        return None;
    }
    let sequence = SCOPE_KEY_TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let candidate = parent.join(format!(
        ".{SCOPE_KEY_FILE}.{}.{}.{}.candidate",
        std::process::id(),
        stamp,
        sequence
    ));
    if umadev_state::fs::atomic_write(&candidate, &key).is_err() {
        return None;
    }
    let published = std::fs::hard_link(&candidate, &path);
    let _ = umadev_state::fs::remove_regular_file(&candidate);
    match published {
        Ok(()) => {
            #[cfg(unix)]
            if let Ok(directory) = std::fs::File::open(parent) {
                let _ = directory.sync_all();
            }
            Some(key)
        }
        Err(_) => read_scope_key(&path),
    }
}

fn canonical_project_identity(project_root: &Path) -> Option<Vec<u8>> {
    let canonical = std::fs::canonicalize(project_root).ok()?;
    if !umadev_state::fs::real_dir(&canonical) {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        Some(canonical.as_os_str().as_bytes().to_vec())
    }
    #[cfg(windows)]
    {
        let mut bytes = Vec::new();
        for unit in canonical
            .as_os_str()
            .to_string_lossy()
            .to_lowercase()
            .encode_utf16()
        {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        Some(bytes)
    }
    #[cfg(not(any(unix, windows)))]
    {
        Some(canonical.to_string_lossy().as_bytes().to_vec())
    }
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut key_block = [0_u8; BLOCK];
    if key.len() > BLOCK {
        key_block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = [0x36_u8; BLOCK];
    let mut outer_pad = [0x5c_u8; BLOCK];
    for index in 0..BLOCK {
        inner_pad[index] ^= key_block[index];
        outer_pad[index] ^= key_block[index];
    }
    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(message);
    let inner_digest = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner_digest);
    outer.finalize().into()
}

fn hex_digest(digest: &[u8]) -> String {
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Installation-keyed, versioned pseudonym for one canonical project root.
/// Called only after global capture was explicitly authorized, so recall-only
/// and default-opt-out paths never create the secret key.
fn project_scope_id_in(home: &Path, project_root: &Path) -> Option<String> {
    let key = read_or_create_scope_key(home)?;
    let identity = canonical_project_identity(project_root)?;
    let mut message = Vec::with_capacity(PROJECT_SCOPE_ID_VERSION.len() + identity.len() + 1);
    message.extend_from_slice(PROJECT_SCOPE_ID_VERSION.as_bytes());
    message.push(0);
    message.extend_from_slice(&identity);
    Some(format!(
        "{PROJECT_SCOPE_ID_VERSION}-{}",
        hex_digest(&hmac_sha256(&key, &message))
    ))
}

impl UsefulnessStore {
    /// Load the cross-project store from the user home. Fail-open: no home, a
    /// missing file, or a corrupt/unreadable blob all yield an EMPTY store (every
    /// weight then neutral — today's static ranking), never an error.
    #[must_use]
    pub fn load() -> Self {
        usefulness_home().map_or_else(Self::default, |home| Self::load_from(&home))
    }

    /// Load the store from an explicit home dir (the durable file is at
    /// `<home>/.umadev/knowledge-usefulness.json`). Fail-open to an empty store.
    /// Exposed so the record bridge + tests can point at a temp home.
    #[must_use]
    pub fn load_from(home: &Path) -> Self {
        if !knowledge_utility_recall_enabled_in(home) {
            return Self::default();
        }
        let mut store = Self::load_legacy_from(home);
        for record in read_receipt_outcomes(home) {
            store.record_memory_ids(&record.memory_ids, record.helpful);
        }
        store
    }

    /// Load only the legacy aggregate. Receipt outcomes are deliberately not
    /// included here: legacy mutation uses this seam before saving, otherwise a
    /// load→save would bake immutable receipts into the aggregate and the next
    /// load would replay them a second time.
    fn load_legacy_from(home: &Path) -> Self {
        umadev_state::fs::read_bounded(&usefulness_path(home), MAX_LEGACY_STORE_BYTES)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Self>(&bytes).ok())
            .unwrap_or_default()
    }

    /// The multiplicative usefulness weight for a chunk, in `[WEIGHT_MIN,
    /// WEIGHT_MAX]`. NEUTRAL (`1.0`) when the chunk is unobserved or has fewer
    /// than [`MIN_SAMPLES`] observations (so a single observation never
    /// dominates and a fresh corpus is unchanged). Otherwise a linear map of the
    /// helpful ratio: all-helpful → `WEIGHT_MAX`, all-harmful → `WEIGHT_MIN`.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn weight_for(&self, path: &str, section: &str) -> f32 {
        self.weight_for_key(&chunk_key(path, section))
    }

    /// Exact-content variant used by retrieval. Receipt-based evidence wins;
    /// when none exists, a legacy `(path, section)` observation remains a
    /// backwards-compatible fallback.
    #[must_use]
    pub fn weight_for_memory(&self, memory: &MemoryRef) -> f32 {
        let exact = exact_chunk_key(&memory.id);
        if self.entries.contains_key(&exact) {
            self.weight_for_key(&exact)
        } else {
            self.weight_for(&memory.path, &memory.section)
        }
    }

    fn weight_for_key(&self, key: &str) -> f32 {
        let Some(stat) = self.entries.get(key) else {
            return NEUTRAL_WEIGHT;
        };
        let total = stat.helpful.saturating_add(stat.harmful);
        if total < MIN_SAMPLES {
            return NEUTRAL_WEIGHT; // sample-gated: thin evidence stays neutral
        }
        let ratio = f64::from(stat.helpful) / f64::from(total);
        let span = f64::from(WEIGHT_MAX) - f64::from(WEIGHT_MIN);
        let w = (f64::from(WEIGHT_MIN) + span * ratio) as f32;
        w.clamp(WEIGHT_MIN, WEIGHT_MAX)
    }

    /// Record an outcome for a batch of surfaced chunk keys: `helpful = true`
    /// increments each chunk's helpful tally, `false` the harmful tally. Bounded
    /// ([`MAX_RECORD_BATCH`] keys per call) and self-capping ([`MAX_ENTRIES`]).
    /// Pure integer bookkeeping — deterministic, never fails.
    pub fn record(&mut self, keys: &[(String, String)], helpful: bool) {
        let mut seen = HashSet::new();
        for (path, section) in keys.iter().take(MAX_RECORD_BATCH) {
            let key = chunk_key(path, section);
            if !seen.insert(key.clone()) {
                continue;
            }
            self.seq = self.seq.saturating_add(1);
            let stat = self.entries.entry(key).or_default();
            if helpful {
                stat.helpful = stat.helpful.saturating_add(1);
            } else {
                stat.harmful = stat.harmful.saturating_add(1);
            }
            stat.updated = self.seq;
        }
        self.enforce_cap();
    }

    /// Record one immutable receipt outcome against exact content identities.
    /// Duplicate memory ids inside one malformed record are counted once.
    pub fn record_memories(&mut self, memories: &[MemoryRef], helpful: bool) {
        let ids = memories
            .iter()
            .map(|memory| memory.id.clone())
            .collect::<Vec<_>>();
        self.record_memory_ids(&ids, helpful);
    }

    /// Record exact identities from the privacy-minimised global outcome
    /// schema. Human-readable path/section fields are neither needed nor kept.
    fn record_memory_ids(&mut self, memory_ids: &[String], helpful: bool) {
        let mut seen = HashSet::new();
        for memory_id in memory_ids.iter().take(MAX_RECORD_BATCH) {
            if !valid_memory_id(memory_id) || !seen.insert(memory_id.as_str()) {
                continue;
            }
            let key = exact_chunk_key(memory_id);
            self.seq = self.seq.saturating_add(1);
            let stat = self.entries.entry(key).or_default();
            if helpful {
                stat.helpful = stat.helpful.saturating_add(1);
            } else {
                stat.harmful = stat.harmful.saturating_add(1);
            }
            stat.updated = self.seq;
        }
        self.enforce_cap();
    }

    /// Evict least-recently-updated entries down to [`MAX_ENTRIES`]. Tiebreak on
    /// the key so eviction is deterministic even when `updated` stamps collide.
    fn enforce_cap(&mut self) {
        if self.entries.len() <= MAX_ENTRIES {
            return;
        }
        let mut items: Vec<(u64, String)> = self
            .entries
            .iter()
            .map(|(k, v)| (v.updated, k.clone()))
            .collect();
        // Oldest (smallest updated) first; key breaks ties.
        items.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        let remove = self.entries.len() - MAX_ENTRIES;
        for (_, k) in items.into_iter().take(remove) {
            self.entries.remove(&k);
        }
    }

    /// Persist the store to an explicit home dir via an atomic temp+rename write.
    /// Fail-open: an unmakeable dir or a write error is swallowed (the prior just
    /// doesn't advance) — never a panic, never an error surfaced to a caller.
    pub fn save_to(&self, home: &Path) {
        if !knowledge_utility_capture_enabled_in(home) {
            return;
        }
        let Ok(root) = std::fs::canonicalize(home) else {
            return;
        };
        if !umadev_state::fs::real_dir(&root) {
            return;
        }
        let Ok(state_dir) = umadev_state::fs::ensure_real_child_dir(&root, STATE_SUBDIR) else {
            return;
        };
        let path = state_dir.join(USEFULNESS_FILE);
        let Ok(body) = serde_json::to_string(self) else {
            return;
        };
        let _ = umadev_state::fs::atomic_write(&path, body.as_bytes());
    }

    /// Number of distinct chunk keys tracked (for tests / introspection).
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the store has no observations yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

fn valid_receipt_id(receipt_id: &str) -> bool {
    (8..=160).contains(&receipt_id.len())
        && receipt_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn valid_memory_id(memory_id: &str) -> bool {
    memory_id.strip_prefix("km1-").is_some_and(|digest| {
        digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

fn existing_outcome_result(path: &Path, expected: &ReceiptOutcomeRecord) -> ReceiptOutcomeWrite {
    let Ok(body) = umadev_state::fs::read_bounded(path, MAX_OUTCOME_BYTES) else {
        return ReceiptOutcomeWrite::Unavailable;
    };
    if let Ok(actual) = serde_json::from_slice::<ReceiptOutcomeRecord>(&body) {
        return if actual == *expected {
            ReceiptOutcomeWrite::AlreadyRecorded
        } else {
            ReceiptOutcomeWrite::Conflict
        };
    }
    // A v1 record was already published before project-scope pseudonyms were
    // introduced. Treat an exact logical replay as idempotent, but never
    // rewrite it in place; first writer remains authoritative.
    serde_json::from_slice::<LegacyReceiptOutcomeRecord>(&body).map_or(
        ReceiptOutcomeWrite::Unavailable,
        |legacy| {
            let mut legacy_ids = legacy
                .memories
                .iter()
                .map(|memory| memory.id.clone())
                .collect::<Vec<_>>();
            legacy_ids.sort();
            legacy_ids.dedup();
            if legacy.version == 1
                && legacy.receipt_id == expected.receipt_id
                && legacy.helpful == expected.helpful
                && legacy_ids == expected.memory_ids
            {
                ReceiptOutcomeWrite::AlreadyRecorded
            } else {
                ReceiptOutcomeWrite::Conflict
            }
        },
    )
}

fn ensure_outcomes_dir(home: &Path) -> Option<PathBuf> {
    let root = std::fs::canonicalize(home).ok()?;
    if !umadev_state::fs::real_dir(&root) {
        return None;
    }
    let state = umadev_state::fs::ensure_real_child_dir(&root, STATE_SUBDIR).ok()?;
    umadev_state::fs::ensure_real_child_dir(&state, OUTCOMES_SUBDIR).ok()
}

fn outcome_record_path(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension == "json")
        && path
            .file_stem()
            .and_then(|name| name.to_str())
            .is_some_and(valid_receipt_id)
}

fn prune_outcome_records_to(dir: &Path, maximum: usize, target: usize) {
    if maximum == 0 || target >= maximum {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut newest = BinaryHeap::with_capacity(target);
    let mut total = 0_usize;
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        let path = entry.path();
        if !outcome_record_path(&path) {
            continue;
        }
        total = total.saturating_add(1);
        let candidate = (
            entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .unwrap_or(std::time::UNIX_EPOCH),
            path,
        );
        if target == 0 {
            continue;
        }
        if newest.len() < target {
            newest.push(Reverse(candidate));
        } else if newest
            .peek()
            .is_some_and(|Reverse(oldest)| &candidate > oldest)
        {
            newest.pop();
            newest.push(Reverse(candidate));
        }
    }
    if total < maximum {
        return;
    }
    let retained = newest
        .into_iter()
        .map(|Reverse((_, path))| path)
        .collect::<HashSet<_>>();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
                continue;
            }
            let path = entry.path();
            if outcome_record_path(&path) && !retained.contains(&path) {
                let _ = umadev_state::fs::remove_regular_file(&path);
            }
        }
    }
}

fn prune_outcome_records(dir: &Path) {
    prune_outcome_records_to(dir, MAX_OUTCOME_RECORDS, OUTCOME_PRUNE_TARGET);
}

/// Publish a PASS/FAIL outcome for one sent-memory receipt under the resolved
/// user home. The record is immutable and create-new within the bounded evidence
/// window: retrying a retained receipt is idempotent; a different payload with
/// the same id never overwrites the first writer. Missing home or any I/O
/// problem is fail-open `Unavailable`.
#[must_use]
pub fn record_receipt_outcome(
    project_root: &Path,
    receipt_id: &str,
    memories: &[MemoryRef],
    helpful: bool,
) -> ReceiptOutcomeWrite {
    usefulness_home().map_or(ReceiptOutcomeWrite::Unavailable, |home| {
        record_receipt_outcome_in(&home, project_root, receipt_id, memories, helpful)
    })
}

/// Explicit-home variant of [`record_receipt_outcome`], used by the agent's
/// durable receipt journal and regression tests.
#[must_use]
pub fn record_receipt_outcome_in(
    home: &Path,
    project_root: &Path,
    receipt_id: &str,
    memories: &[MemoryRef],
    helpful: bool,
) -> ReceiptOutcomeWrite {
    if !knowledge_utility_capture_enabled_in(home) {
        return ReceiptOutcomeWrite::SuppressedByPolicy;
    }
    if !valid_receipt_id(receipt_id) || memories.is_empty() {
        return ReceiptOutcomeWrite::Unavailable;
    }
    let mut memory_ids = memories
        .iter()
        .filter(|memory| valid_memory_id(&memory.id))
        .take(MAX_RECORD_BATCH)
        .map(|memory| memory.id.clone())
        .collect::<Vec<_>>();
    memory_ids.sort();
    memory_ids.dedup();
    if memory_ids.is_empty() {
        return ReceiptOutcomeWrite::Unavailable;
    }
    let Some(project_scope_id) = project_scope_id_in(home, project_root) else {
        return ReceiptOutcomeWrite::Unavailable;
    };
    let record = ReceiptOutcomeRecord {
        version: OUTCOME_RECORD_VERSION,
        receipt_id: receipt_id.to_string(),
        project_scope_id,
        helpful,
        memory_ids,
    };
    let Some(dir) = ensure_outcomes_dir(home) else {
        return ReceiptOutcomeWrite::Unavailable;
    };
    let Ok(_store_lock) = umadev_state::store_lock::acquire(
        home,
        umadev_state::memory::MemoryStore::KnowledgeUtility,
    ) else {
        return ReceiptOutcomeWrite::Unavailable;
    };
    prune_outcome_records(&dir);
    let final_path = dir.join(format!("{receipt_id}.json"));
    if std::fs::symlink_metadata(&final_path).is_ok() {
        return existing_outcome_result(&final_path, &record);
    }
    let Ok(body) = serde_json::to_vec(&record) else {
        return ReceiptOutcomeWrite::Unavailable;
    };
    let sequence = OUTCOME_TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let temp_path = dir.join(format!(
        ".{receipt_id}.{}.{}.{}.tmp",
        std::process::id(),
        stamp,
        sequence
    ));
    if umadev_state::fs::atomic_write(&temp_path, &body).is_err() {
        return ReceiptOutcomeWrite::Unavailable;
    }
    // `hard_link` is an atomic no-replace publication on the three supported
    // desktop families. Unlike rename, it cannot overwrite a concurrently
    // published outcome on Unix. The temp and final live in the same dir.
    let published = std::fs::hard_link(&temp_path, &final_path);
    let _ = umadev_state::fs::remove_regular_file(&temp_path);
    match published {
        Ok(()) => {
            #[cfg(unix)]
            if let Ok(directory) = std::fs::File::open(&dir) {
                let _ = directory.sync_all();
            }
            ReceiptOutcomeWrite::Recorded
        }
        Err(_) if std::fs::symlink_metadata(&final_path).is_ok() => {
            existing_outcome_result(&final_path, &record)
        }
        Err(_) => ReceiptOutcomeWrite::Unavailable,
    }
}

fn read_receipt_outcomes(home: &Path) -> Vec<AppliedOutcomeRecord> {
    let dir = outcomes_dir(home);
    if !umadev_state::fs::real_dir(&dir) {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut newest = BinaryHeap::with_capacity(MAX_OUTCOME_RECORDS);
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        let path = entry.path();
        if !outcome_record_path(&path) {
            continue;
        }
        let candidate = (
            entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .unwrap_or(std::time::UNIX_EPOCH),
            path,
        );
        if newest.len() < MAX_OUTCOME_RECORDS {
            newest.push(Reverse(candidate));
        } else if newest
            .peek()
            .is_some_and(|Reverse(oldest)| &candidate > oldest)
        {
            newest.pop();
            newest.push(Reverse(candidate));
        }
    }
    let mut paths = newest
        .into_iter()
        .map(|Reverse(candidate)| candidate)
        .collect::<Vec<_>>();
    paths.sort();
    let mut seen_receipts = HashSet::new();
    let mut records = Vec::new();
    for (_, path) in paths {
        let Ok(body) = umadev_state::fs::read_bounded(&path, MAX_OUTCOME_BYTES) else {
            continue;
        };
        let applied = if let Ok(record) = serde_json::from_slice::<ReceiptOutcomeRecord>(&body) {
            (record.version == OUTCOME_RECORD_VERSION
                && valid_project_scope_id(&record.project_scope_id)
                && !record.memory_ids.is_empty()
                && record.memory_ids.iter().all(|id| valid_memory_id(id)))
            .then_some(AppliedOutcomeRecord {
                receipt_id: record.receipt_id,
                helpful: record.helpful,
                memory_ids: record.memory_ids,
            })
        } else {
            serde_json::from_slice::<LegacyReceiptOutcomeRecord>(&body)
                .ok()
                .filter(|record| {
                    record.version == 1
                        && !record.memories.is_empty()
                        && record
                            .memories
                            .iter()
                            .all(|memory| valid_memory_id(&memory.id))
                })
                .map(|record| AppliedOutcomeRecord {
                    receipt_id: record.receipt_id,
                    helpful: record.helpful,
                    memory_ids: record
                        .memories
                        .into_iter()
                        .map(|memory| memory.id)
                        .collect(),
                })
        };
        let Some(mut record) = applied else {
            continue;
        };
        let filename_matches = path
            .file_stem()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == record.receipt_id.as_str());
        record.memory_ids.sort();
        record.memory_ids.dedup();
        if filename_matches
            && valid_receipt_id(&record.receipt_id)
            && seen_receipts.insert(record.receipt_id.clone())
            && !record.memory_ids.is_empty()
        {
            records.push(record);
        }
    }
    records
}

/// Record a step outcome for the surfaced chunk keys into the cross-project home
/// store: load → record → save, all fail-open. `helpful = true` on a PASS,
/// `false` on a FAIL. A no-op when there are no keys or no home dir — so it never
/// touches disk (and never pollutes a test home) unless there is real signal.
pub fn record_chunk_outcomes(keys: &[(String, String)], helpful: bool) {
    if keys.is_empty() {
        return;
    }
    let Some(home) = usefulness_home() else {
        return;
    };
    record_chunk_outcomes_in(&home, keys, helpful);
}

/// Explicit-home variant of [`record_chunk_outcomes`] — the durable file is at
/// `<home>/.umadev/knowledge-usefulness.json`. The bridge in the agent crate +
/// tests use this so the cross-project store can be redirected to a temp home.
pub fn record_chunk_outcomes_in(home: &Path, keys: &[(String, String)], helpful: bool) {
    if keys.is_empty() || !knowledge_utility_capture_enabled_in(home) {
        return;
    }
    let mut store = UsefulnessStore::load_legacy_from(home);
    store.record(keys, helpful);
    store.save_to(home);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(path: &str, section: &str) -> (String, String) {
        (path.to_string(), section.to_string())
    }

    fn memory(path: &str, section: &str, body: &str) -> MemoryRef {
        MemoryRef::from_parts(path, section, body)
    }

    fn enable_global_utility_capture(home: &Path) {
        umadev_state::memory::update_policy(home, |policy| {
            policy.set_capture(
                Some(umadev_state::memory::MemoryStore::KnowledgeUtility),
                true,
            );
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn memory_identity_is_stable_and_content_bound() {
        let first = memory("same.md", "Heading", "body one");
        let again = memory("same.md", "Heading", "body one");
        let edited = memory("same.md", "Heading", "body two");
        assert_eq!(first, again);
        assert_ne!(first.id, edited.id);
    }

    #[test]
    fn global_feedback_is_opt_in_and_does_not_create_a_scope_key_when_suppressed() {
        let home = tempfile::TempDir::new().unwrap();
        let project = tempfile::TempDir::new().unwrap();
        let item = memory("private/project-note.md", "Secret heading", "private body");
        assert_eq!(
            record_receipt_outcome_in(
                home.path(),
                project.path(),
                "receipt-opt-out",
                std::slice::from_ref(&item),
                true,
            ),
            ReceiptOutcomeWrite::SuppressedByPolicy
        );
        assert!(!home
            .path()
            .join(STATE_SUBDIR)
            .join("memory")
            .join(SCOPE_KEY_FILE)
            .exists());
        assert!(!outcomes_dir(home.path()).exists());
    }

    #[test]
    fn v2_feedback_persists_only_pseudonymous_scope_and_content_id() {
        let home = tempfile::TempDir::new().unwrap();
        let project = tempfile::TempDir::new().unwrap();
        enable_global_utility_capture(home.path());
        let item = memory("private/project-note.md", "Secret heading", "private body");
        assert_eq!(
            record_receipt_outcome_in(
                home.path(),
                project.path(),
                "receipt-private",
                std::slice::from_ref(&item),
                false,
            ),
            ReceiptOutcomeWrite::Recorded
        );
        let body = umadev_state::fs::read_bounded(
            &outcomes_dir(home.path()).join("receipt-private.json"),
            MAX_OUTCOME_BYTES,
        )
        .unwrap();
        let record: ReceiptOutcomeRecord = serde_json::from_slice(&body).unwrap();
        assert_eq!(record.version, OUTCOME_RECORD_VERSION);
        assert!(valid_project_scope_id(&record.project_scope_id));
        assert_eq!(record.memory_ids, vec![item.id]);
        let text = String::from_utf8(body).unwrap();
        assert!(!text.contains(&project.path().to_string_lossy().to_string()));
        assert!(!text.contains("private/project-note.md"));
        assert!(!text.contains("Secret heading"));
        assert!(!text.contains("private body"));
    }

    #[test]
    fn recall_policy_can_neutralize_existing_global_feedback() {
        let home = tempfile::TempDir::new().unwrap();
        let project = tempfile::TempDir::new().unwrap();
        enable_global_utility_capture(home.path());
        let item = memory("a.md", "S", "body");
        for index in 0..MIN_SAMPLES {
            assert_eq!(
                record_receipt_outcome_in(
                    home.path(),
                    project.path(),
                    &format!("receipt-recall-{index}"),
                    std::slice::from_ref(&item),
                    true,
                ),
                ReceiptOutcomeWrite::Recorded
            );
        }
        assert!(UsefulnessStore::load_from(home.path()).weight_for_memory(&item) > NEUTRAL_WEIGHT);
        umadev_state::memory::update_policy(home.path(), |policy| {
            policy.set_recall(
                Some(umadev_state::memory::MemoryStore::KnowledgeUtility),
                false,
            );
            Ok(())
        })
        .unwrap();
        assert!(UsefulnessStore::load_from(home.path()).is_empty());
    }

    #[test]
    fn immutable_receipt_outcome_is_idempotent_and_collision_safe() {
        let home = tempfile::TempDir::new().unwrap();
        enable_global_utility_capture(home.path());
        let first = memory("same.md", "Heading", "body one");
        let edited = memory("same.md", "Heading", "body two");
        assert_eq!(
            record_receipt_outcome_in(
                home.path(),
                home.path(),
                "receipt-001",
                std::slice::from_ref(&first),
                true,
            ),
            ReceiptOutcomeWrite::Recorded
        );
        assert_eq!(
            record_receipt_outcome_in(
                home.path(),
                home.path(),
                "receipt-001",
                std::slice::from_ref(&first),
                true,
            ),
            ReceiptOutcomeWrite::AlreadyRecorded
        );
        assert_eq!(
            record_receipt_outcome_in(
                home.path(),
                home.path(),
                "receipt-001",
                std::slice::from_ref(&first),
                false,
            ),
            ReceiptOutcomeWrite::Conflict
        );
        let store = UsefulnessStore::load_from(home.path());
        assert_eq!(store.entries[&exact_chunk_key(&first.id)].helpful, 1);
        assert_eq!(store.entries[&exact_chunk_key(&first.id)].harmful, 0);
        assert!((store.weight_for_memory(&edited) - NEUTRAL_WEIGHT).abs() < f32::EPSILON);
    }

    #[test]
    fn concurrent_outcome_publish_has_one_winner() {
        let home = tempfile::TempDir::new().unwrap();
        enable_global_utility_capture(home.path());
        let memory = memory("a.md", "S", "body");
        let writes = std::thread::scope(|scope| {
            let mut joins = Vec::new();
            for _ in 0..12 {
                joins.push(scope.spawn(|| {
                    record_receipt_outcome_in(
                        home.path(),
                        home.path(),
                        "receipt-race",
                        std::slice::from_ref(&memory),
                        true,
                    )
                }));
            }
            joins
                .into_iter()
                .map(|join| join.join().unwrap())
                .collect::<Vec<_>>()
        });
        // The real invariant: exactly ONE thread publishes. `hard_link` is an
        // atomic no-replace, so this holds regardless of scheduling.
        assert_eq!(
            writes
                .iter()
                .filter(|write| **write == ReceiptOutcomeWrite::Recorded)
                .count(),
            1
        );
        // Every loser settles BENIGNLY: it either saw the winner's record
        // (AlreadyRecorded), or — under heavy contention on a loaded runner — the
        // BOUNDED store lock gave up within its timeout (Unavailable). Unavailable
        // is a correct, harmless outcome here: the record already exists via the
        // winner, so nothing was lost; the lock is deliberately time-bounded so a
        // turn never blocks forever on a busy store. (This was the CI flake: the
        // old assertion assumed no thread ever hit the timeout.) What must NEVER
        // happen is a Conflict — two divergent records for one receipt — which
        // would be a genuine dedup bug, so that stays forbidden.
        assert!(
            writes.iter().all(|write| matches!(
                write,
                ReceiptOutcomeWrite::Recorded
                    | ReceiptOutcomeWrite::AlreadyRecorded
                    | ReceiptOutcomeWrite::Unavailable
            )),
            "a loser must settle benignly (already-recorded or a bounded-lock \
             timeout), never a divergent conflict: {writes:?}"
        );
        assert!(
            !writes.contains(&ReceiptOutcomeWrite::Conflict),
            "no thread may see a divergent record for the same receipt: {writes:?}"
        );
        let store = UsefulnessStore::load_from(home.path());
        assert_eq!(store.entries[&exact_chunk_key(&memory.id)].helpful, 1);
    }

    #[test]
    fn immutable_outcome_retention_prunes_to_a_bounded_window() {
        let home = tempfile::TempDir::new().unwrap();
        let dir = home.path().join("outcomes");
        std::fs::create_dir(&dir).unwrap();
        for index in 0..5 {
            std::fs::write(dir.join(format!("receipt-{index}.json")), b"{}").unwrap();
        }
        std::fs::write(dir.join("unmanaged.txt"), b"keep").unwrap();
        prune_outcome_records_to(&dir, 3, 2);
        let remaining = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| outcome_record_path(path))
            .count();
        assert_eq!(remaining, 2);
        assert!(dir.join("unmanaged.txt").is_file());
    }

    #[test]
    fn immutable_outcome_retention_prunes_at_the_exact_limit() {
        let home = tempfile::TempDir::new().unwrap();
        let dir = home.path().join("outcomes");
        std::fs::create_dir(&dir).unwrap();
        for index in 0..3 {
            std::fs::write(dir.join(format!("receipt-{index}.json")), b"{}").unwrap();
        }
        prune_outcome_records_to(&dir, 3, 2);
        let remaining = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| outcome_record_path(path))
            .count();
        assert_eq!(remaining, 2);
    }

    #[test]
    fn legacy_save_does_not_bake_and_replay_immutable_receipts() {
        let home = tempfile::TempDir::new().unwrap();
        enable_global_utility_capture(home.path());
        let exact = memory("exact.md", "S", "body");
        assert_eq!(
            record_receipt_outcome_in(
                home.path(),
                home.path(),
                "receipt-keep-one",
                std::slice::from_ref(&exact),
                true,
            ),
            ReceiptOutcomeWrite::Recorded
        );
        record_chunk_outcomes_in(home.path(), &[key("legacy.md", "S")], true);
        let store = UsefulnessStore::load_from(home.path());
        assert_eq!(store.entries[&exact_chunk_key(&exact.id)].helpful, 1);
        assert_eq!(store.entries[&chunk_key("legacy.md", "S")].helpful, 1);
    }

    #[test]
    fn unobserved_chunk_is_neutral() {
        let store = UsefulnessStore::default();
        assert!((store.weight_for("a.md", "S") - NEUTRAL_WEIGHT).abs() < f32::EPSILON);
    }

    #[test]
    fn a_single_observation_does_not_move_ranking() {
        // One helpful observation is below MIN_SAMPLES → still neutral, so a single
        // outcome can never dominate the ranking.
        let mut store = UsefulnessStore::default();
        store.record(&[key("a.md", "S")], true);
        assert!(
            (store.weight_for("a.md", "S") - NEUTRAL_WEIGHT).abs() < f32::EPSILON,
            "a single observation must stay neutral (sample-gated)"
        );
    }

    #[test]
    fn well_sampled_helpful_chunk_lifts_weight() {
        let mut store = UsefulnessStore::default();
        for _ in 0..MIN_SAMPLES {
            store.record(&[key("a.md", "S")], true);
        }
        let w = store.weight_for("a.md", "S");
        assert!(
            w > NEUTRAL_WEIGHT,
            "all-helpful must lift above neutral: {w}"
        );
        assert!(
            (w - WEIGHT_MAX).abs() < 1e-4,
            "all-helpful maps to WEIGHT_MAX"
        );
    }

    #[test]
    fn well_sampled_harmful_chunk_sinks_weight() {
        let mut store = UsefulnessStore::default();
        for _ in 0..MIN_SAMPLES {
            store.record(&[key("bad.md", "S")], false);
        }
        let w = store.weight_for("bad.md", "S");
        assert!(
            w < NEUTRAL_WEIGHT,
            "all-harmful must sink below neutral: {w}"
        );
        assert!(
            (w - WEIGHT_MIN).abs() < 1e-4,
            "all-harmful maps to WEIGHT_MIN"
        );
    }

    #[test]
    fn weight_stays_within_bounds_for_mixed_signal() {
        let mut store = UsefulnessStore::default();
        store.record(&[key("m.md", "S")], true);
        store.record(&[key("m.md", "S")], true);
        store.record(&[key("m.md", "S")], false);
        let w = store.weight_for("m.md", "S");
        assert!(
            (WEIGHT_MIN..=WEIGHT_MAX).contains(&w),
            "weight in bounds: {w}"
        );
    }

    #[test]
    fn record_round_trips_through_an_explicit_home() {
        let home = tempfile::TempDir::new().unwrap();
        enable_global_utility_capture(home.path());
        for _ in 0..MIN_SAMPLES {
            record_chunk_outcomes_in(home.path(), &[key("a.md", "S")], true);
        }
        let store = UsefulnessStore::load_from(home.path());
        assert!(
            store.weight_for("a.md", "S") > NEUTRAL_WEIGHT,
            "persisted helpful observations lift the weight on reload"
        );
    }

    #[test]
    fn a_passing_step_gains_and_a_failing_step_loses_usefulness() {
        let home = tempfile::TempDir::new().unwrap();
        enable_global_utility_capture(home.path());
        // A chunk in front of passing steps climbs; a different chunk in front of
        // failing steps sinks — the two diverge.
        for _ in 0..MIN_SAMPLES {
            record_chunk_outcomes_in(home.path(), &[key("good.md", "S")], true);
            record_chunk_outcomes_in(home.path(), &[key("bad.md", "S")], false);
        }
        let store = UsefulnessStore::load_from(home.path());
        assert!(store.weight_for("good.md", "S") > store.weight_for("bad.md", "S"));
        assert!(store.weight_for("good.md", "S") > NEUTRAL_WEIGHT);
        assert!(store.weight_for("bad.md", "S") < NEUTRAL_WEIGHT);
    }

    #[test]
    fn load_is_fail_open_on_a_corrupt_store() {
        let home = tempfile::TempDir::new().unwrap();
        let path = usefulness_path(home.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ this is not valid json ][").unwrap();
        let store = UsefulnessStore::load_from(home.path());
        assert!(
            store.is_empty(),
            "a corrupt store loads as empty (fail-open)"
        );
        assert!((store.weight_for("x.md", "S") - NEUTRAL_WEIGHT).abs() < f32::EPSILON);
    }

    #[test]
    fn record_is_fail_open_on_a_missing_home() {
        // A home that cannot be created (a FILE where the dir must be) must not
        // panic; the outcome is simply dropped.
        let tmp = tempfile::TempDir::new().unwrap();
        let file_as_home = tmp.path().join("iam-a-file");
        std::fs::write(&file_as_home, b"x").unwrap();
        record_chunk_outcomes_in(&file_as_home, &[key("a.md", "S")], true);
        // No panic == pass; the store never materialised.
        assert!(UsefulnessStore::load_from(&file_as_home).is_empty());
    }

    #[test]
    fn empty_keys_never_touch_disk() {
        let home = tempfile::TempDir::new().unwrap();
        record_chunk_outcomes_in(home.path(), &[], true);
        assert!(
            !usefulness_path(home.path()).exists(),
            "an empty batch must not create the store file"
        );
    }

    #[test]
    fn store_is_bounded_and_evicts_oldest() {
        let mut store = UsefulnessStore::default();
        // Insert well over the cap; the store must never exceed MAX_ENTRIES.
        for i in 0..(MAX_ENTRIES + 50) {
            store.record(&[key(&format!("f{i}.md"), "S")], true);
        }
        assert!(store.len() <= MAX_ENTRIES, "store size stays bounded");
        // The most recently inserted key survived; the very first was evicted.
        assert!(store
            .entries
            .contains_key(&chunk_key(&format!("f{}.md", MAX_ENTRIES + 49), "S")));
        assert!(!store.entries.contains_key(&chunk_key("f0.md", "S")));
    }

    #[test]
    fn record_batch_is_capped() {
        let mut store = UsefulnessStore::default();
        let keys: Vec<(String, String)> = (0..(MAX_RECORD_BATCH + 20))
            .map(|i| key(&format!("f{i}.md"), "S"))
            .collect();
        store.record(&keys, true);
        assert_eq!(
            store.len(),
            MAX_RECORD_BATCH,
            "one record call processes at most MAX_RECORD_BATCH keys"
        );
    }

    #[test]
    fn duplicate_keys_in_one_outcome_count_once() {
        let mut store = UsefulnessStore::default();
        let duplicate = key("same.md", "S");
        store.record(&[duplicate.clone(), duplicate.clone(), duplicate], false);
        let stat = &store.entries[&chunk_key("same.md", "S")];
        assert_eq!((stat.helpful, stat.harmful), (0, 1));
        assert!(
            (store.weight_for("same.md", "S") - NEUTRAL_WEIGHT).abs() < f32::EPSILON,
            "one malformed duplicate batch cannot bypass the sample gate"
        );
    }
}
