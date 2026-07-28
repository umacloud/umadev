//! Durable per-project **FACT** memory — the store that stops the team from
//! re-searching something it already resolved.
//!
//! ## Why this exists (a memory-loss bug)
//!
//! The team would resolve a concrete fact in one turn ("JDK17 lives at
//! `/usr/lib/jvm/jdk-17`", "the build is `mvn -q package`", "the dev server is
//! on port 5173") and then, several turns later, **re-search for it** — because
//! the fact had fallen out of UmaDev's bounded transcript AND out of the base's
//! own context window, and [`crate::lessons`] only persists *pitfalls*, never
//! plain *facts*. There was nowhere durable for a project fact to live.
//!
//! This module is that place: a small, UmaDev-managed per-project store of
//! durable facts the team should never lose — resolved tool/binary locations,
//! build/run/test commands, environment constraints (required versions, ports),
//! architecture decisions, user preferences. It is the *fact* sibling of the
//! *pitfall* ledger in [`crate::lessons`].
//!
//! ## The loop
//!
//! - **RECORD** — [`record_fact`] / [`record_facts`] persist facts accepted by
//!   UmaDev's controlled extractor via an atomic temp+rename write, rejecting
//!   credentials, deduping by key, and enforcing the bounds.
//! - **RECALL** — [`facts_firmware_block`] renders the stored facts as a
//!   compact, token-budgeted block that [`crate::context::compose_firmware`]
//!   injects into the **always-on work-class head** on EVERY work turn, so the
//!   base always sees the facts regardless of the bounded transcript or a base
//!   context rotation — and never re-searches a known fact.
//!
//! ## Bounded + fail-open by contract
//!
//! The store is capped at the internal fact limit (oldest evicted) with each
//! field bounded by internal key/value/category/provenance limits,
//! and the firmware block is capped at [`FACTS_FIRMWARE_BUDGET`] characters — so
//! the prompt can never bloat. Every path is fail-open: a missing dir, an
//! unreadable file, a corrupt/garbage line, or a failed write degrades to "no
//! facts" and behaves exactly as before — this module NEVER panics and NEVER
//! returns an error that could block the base.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use umadev_governance::redaction::{redact_json, redact_text};

use crate::memory_control::{capture_enabled, recall_enabled, MemoryScope, MemoryStore};

/// Repo-relative directory holding the durable per-project memory.
pub const MEMORY_DIR: &str = ".umadev/memory";

/// Repo-relative path of the UmaDev-managed durable fact store. Kept as one
/// constant so the recorder, recall block, and diagnostics name the same file.
pub const FACTS_REL_PATH: &str = ".umadev/memory/facts.jsonl";

/// Hard cap on distinct facts retained on disk so a long-lived project's store
/// never bloats. When exceeded, the OLDEST facts (by last update) are evicted.
const MAX_FACTS: usize = 64;

/// Per-fact cap on the key length (chars). A fact key is a short name
/// ("JDK17", "build", "db_port"), so this is generous head-room, not a target.
const MAX_KEY_CHARS: usize = 80;

/// Per-fact cap on the value length (chars). A value is a path / command /
/// constraint / decision — bounded so one runaway value can't dominate the
/// store or the firmware budget.
const MAX_VALUE_CHARS: usize = 400;

/// Per-fact cap on the optional category length (chars).
const MAX_CATEGORY_CHARS: usize = 32;
const MAX_PROVENANCE_CHARS: usize = 32;
const MAX_EVIDENCE_CHARS: usize = 240;
const MAX_FACTS_FILE_BYTES: u64 = 256 * 1024;

/// Character budget for the firmware recall block. Deliberately tight: the facts
/// block rides in the always-on head on TOP of the identity + craft law, so it
/// must stay a small, high-signal overlay. [`facts_firmware_block`] fills the
/// fact list up to this budget and never exceeds it.
pub const FACTS_FIRMWARE_BUDGET: usize = 1_200;

/// One durable project fact with optional source evidence.
///
/// The on-disk form is one of these serialized to a single JSON line, e.g.
/// `{"key":"JDK17","value":"/usr/lib/jvm/jdk-17","category":"path"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fact {
    /// Short, stable name of the fact ("JDK17", "build", "api_port").
    pub key: String,
    /// The resolved value (a path, command, version, port, decision, preference).
    pub value: String,
    /// Optional type/category hint ("path" / "version" / "port" / "command" /
    /// "decision" / "preference"). `None` when the base/recorder left it blank;
    /// skipped from the serialized line so the on-disk shape stays minimal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// How the value was established (`user_stated`, `repository_verified`, or
    /// `filesystem_verified`). Legacy rows omit this field and remain readable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<String>,
    /// Bounded source reference, such as `current_request`, `Cargo.toml`, or an
    /// existing absolute path. It is evidence metadata, never an instruction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
    /// `true` once a run OBSERVED this fact to be CONTRADICTED — its asserted
    /// `path` no longer exists on disk, or a fresh observation this run reported a
    /// clearly different value for the same key (see [`mark_stale_facts`]). A stale
    /// fact is a TOMBSTONE: [`load_facts`] / [`facts_firmware_block`] exclude it
    /// from recall (demoted below the cut) but it is KEPT on disk for provenance —
    /// the same non-destructive posture the lessons ledger uses for an invalidated
    /// lesson. It is cleared automatically when the key is re-recorded with a fresh
    /// value ([`record_facts`] replaces the row). `#[serde(default)]` keeps every
    /// pre-existing JSONL row readable, and `skip_serializing_if` keeps a LIVE
    /// fact's on-disk shape byte-for-byte as before (no `stale` key emitted).
    #[serde(default, skip_serializing_if = "is_false")]
    pub stale: bool,
}

/// `skip_serializing_if` predicate — true for the default `false` so a LIVE fact
/// never emits a `stale` key and its on-disk line stays byte-for-byte as before.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(b: &bool) -> bool {
    !*b
}

impl Fact {
    /// Build a fact from string-like parts, with an optional category. Helper for
    /// callers + tests so the common case reads cleanly.
    pub fn new(
        key: impl Into<String>,
        value: impl Into<String>,
        category: Option<impl Into<String>>,
    ) -> Self {
        Self {
            key: key.into(),
            value: value.into(),
            category: category.map(Into::into),
            provenance: None,
            evidence: None,
            stale: false,
        }
    }

    /// Attach the bounded origin kind and supporting source reference used by
    /// the controlled fact extractor.
    #[must_use]
    pub fn with_provenance(
        mut self,
        provenance: impl Into<String>,
        evidence: impl Into<String>,
    ) -> Self {
        self.provenance = Some(provenance.into());
        self.evidence = Some(evidence.into());
        self
    }
}

/// Serialises the read-modify-write of the fact store so concurrent callers (a
/// forked critic, a parallel step, the staleness sweep vs. the recorder) can't
/// clobber each other. Recovers from poison so a panic elsewhere never blocks
/// this fail-open path. Shared by [`record_facts`] and [`mark_stale_facts`].
static FACTS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn real_dir_no_follow(path: &Path) -> bool {
    umadev_state::fs::real_dir(path)
}

fn ensure_real_child_dir(parent: &Path, child: &Path, create: bool) -> bool {
    let Some(name) = child
        .parent()
        .filter(|candidate| *candidate == parent)
        .and_then(|_| child.file_name())
        .and_then(|name| name.to_str())
    else {
        return false;
    };
    if create {
        umadev_state::fs::ensure_real_child_dir(parent, name).is_ok()
    } else {
        real_dir_no_follow(parent) && real_dir_no_follow(child)
    }
}

/// Resolve only UmaDev-owned path components and never follow a link in them.
fn managed_facts_path(root: &Path, create_parent: bool) -> Option<PathBuf> {
    let root = std::fs::canonicalize(root).ok()?;
    if !real_dir_no_follow(&root) {
        return None;
    }
    let umadev = root.join(".umadev");
    if !ensure_real_child_dir(&root, &umadev, create_parent) {
        return None;
    }
    let memory = umadev.join("memory");
    if !ensure_real_child_dir(&umadev, &memory, create_parent) {
        return None;
    }
    let path = memory.join("facts.jsonl");
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if umadev_state::fs::metadata_is_real_file(&meta) => Some(path),
        Ok(_) => None,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(path),
        Err(_) => None,
    }
}

fn read_managed_facts(root: &Path) -> Option<String> {
    let path = managed_facts_path(root, false)?;
    String::from_utf8(umadev_state::fs::read_bounded(&path, MAX_FACTS_FILE_BYTES).ok()?).ok()
}

/// Load the durable, RECALLABLE facts for `root`, newest LAST — the LIVE facts
/// only, with stale tombstones excluded so a contradicted fact never surfaces.
///
/// Fail-open + forgiving: a missing file yields an empty vec; a corrupt/garbage
/// line is skipped (a single bad append never loses the rest of the store). The
/// result is deduped by key (case-insensitive, last occurrence wins so a re-record
/// supersedes the older value) and capped at the internal fact limit (oldest
/// dropped), so legacy or externally modified stores are normalised on read. Stale
/// (tombstoned) facts are filtered LAST — they stay on disk for provenance (see
/// the internal raw loader) but are demoted below recall.
#[must_use]
pub fn load_facts(root: &Path) -> Vec<Fact> {
    load_facts_raw(root)
        .into_iter()
        .filter(|f| !f.stale)
        .collect()
}

/// Load ALL durable facts for `root` (LIVE and stale) newest LAST, one row per key.
///
/// The provenance-preserving loader the read-modify-write mutators
/// ([`record_facts`], [`mark_stale_facts`]) read through so a rewrite never drops a
/// stale tombstone. Recall goes through [`load_facts`] (which filters stale on top
/// of this); callers that must see the tombstones read here directly. Same
/// fail-open + dedup + cap normalisation as [`load_facts`].
#[must_use]
fn load_facts_raw(root: &Path) -> Vec<Fact> {
    let Some(text) = read_managed_facts(root) else {
        return Vec::new();
    };
    let parsed: Vec<Fact> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Fact>(l).ok())
        .filter(fact_is_safe)
        .map(normalize)
        .filter(|f| !f.key.is_empty() && !f.value.is_empty())
        .collect();
    dedup_cap(parsed)
}

/// Record one durable fact. Returns `true` when a (non-empty) fact was applied.
/// Convenience wrapper over [`record_facts`].
pub fn record_fact(root: &Path, fact: Fact) -> bool {
    record_facts(root, &[fact]) > 0
}

/// Record durable facts into the store via an atomic read-modify-write.
///
/// Each incoming fact is credential-checked, trimmed, and field-truncated; empty
/// or sensitive facts are dropped. Recording an existing key updates it (and
/// moves it to newest), so the store holds one entry per key. After merging, the
/// store is capped at the internal fact limit and written atomically. Returns how
/// many valid facts were committed.
///
/// Fail-open: invalid-only input is a no-op (`0`); a write error is swallowed —
/// recording a fact must never block the team.
pub fn record_facts(root: &Path, incoming: &[Fact]) -> usize {
    if !capture_enabled(root, MemoryScope::Project, MemoryStore::Facts) {
        return 0;
    }
    // Serialize the read-modify-write so concurrent callers (a forked critic, a
    // parallel step, the staleness sweep) can't clobber each other. Recover from
    // poison so a panic elsewhere never blocks this fail-open path.
    let _guard = FACTS_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Ok(_cross_process) = umadev_state::store_lock::acquire(root, MemoryStore::Facts) else {
        return 0;
    };

    let valid: Vec<Fact> = incoming
        .iter()
        .filter(|fact| fact_is_safe(fact))
        .cloned()
        .map(normalize)
        .filter(|f| !f.key.is_empty() && !f.value.is_empty())
        .collect();
    if valid.is_empty() {
        return 0;
    }

    // Read the RAW store (LIVE + stale) so a rewrite preserves tombstones for keys
    // we don't touch; a fresh value for a key drops its old (live or stale) row and
    // replaces it with a LIVE one below, so re-recording a key revives it.
    let mut store = load_facts_raw(root);
    for f in &valid {
        let key_l = f.key.to_lowercase();
        store.retain(|e| e.key.to_lowercase() != key_l);
        store.push(f.clone());
    }
    let len = store.len();
    if len > MAX_FACTS {
        store.drain(0..len - MAX_FACTS);
    }

    let Some(path) = managed_facts_path(root, true) else {
        return 0;
    };
    write_atomic(&path, &render_jsonl(&store)).map_or(0, |()| valid.len())
}

/// STALENESS SWEEP — tombstone stored LIVE facts a run OBSERVED to be contradicted
/// so a rotten fact stops being recalled, WITHOUT physically deleting it.
///
/// Two deterministic, conservative contradiction signals — a fact is demoted to a
/// stale tombstone (kept on disk for provenance, excluded from [`load_facts`] /
/// [`facts_firmware_block`]) only on a CLEAR one, never a weak/ambiguous hint:
///
/// - **Observed value contradiction** — an `observed` fact this run reported the
///   SAME key with a clearly DIFFERENT value (after whitespace/case
///   normalisation). A pure refinement (one value contains the other, e.g. adding
///   `--prod`) or a mere formatting variant is NOT a contradiction, so a caller
///   that keeps refining a command never over-prunes its own fact. In the pipeline
///   the fresh value is then re-[`record_facts`]ed, which supersedes the tombstone
///   anyway; the tombstone matters when no clean replacement is recorded.
/// - **Dead path** — a `category == "path"` fact whose value is a single
///   ABSOLUTE path token that `try_exists()` reports as definitively absent
///   (`Ok(false)`). Relative paths (could be a not-yet-built artifact), values
///   with arguments/globs/`~`/`$`, and any I/O error (`Err`) are all left ALONE —
///   we demote only on unambiguous non-existence.
///
/// Bounded (one pass over the bounded fact store, one `try_exists` per path
/// fact), deterministic, and fail-open at every step: an empty/unreadable store,
/// no observations, or a failed write all yield `0` and never panic. Returns how
/// many LIVE facts were newly tombstoned.
pub fn mark_stale_facts(root: &Path, observed: &[Fact]) -> usize {
    // Share the recorder's lock so a concurrent record/sweep can't clobber the
    // rewrite. Poison-tolerant (fail-open).
    let _guard = FACTS_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Ok(_cross_process) = umadev_state::store_lock::acquire(root, MemoryStore::Facts) else {
        return 0;
    };

    let mut store = load_facts_raw(root);
    if store.is_empty() {
        return 0;
    }

    // The run's OBSERVED truth: key (lowercased) → normalised value. Empty
    // keys/values are dropped so a blank observation never contradicts anything.
    let observed_values: std::collections::HashMap<String, String> = observed
        .iter()
        .filter(|f| fact_is_safe(f))
        .filter(|f| !f.key.trim().is_empty() && !f.value.trim().is_empty())
        .map(|f| (f.key.trim().to_lowercase(), norm_value(&f.value)))
        .collect();

    let mut marked = 0usize;
    for f in &mut store {
        if f.stale {
            continue; // already a tombstone — never re-mark
        }
        if fact_is_contradicted(f, &observed_values) {
            f.stale = true;
            marked += 1;
        }
    }
    if marked == 0 {
        return 0; // nothing clearly contradicted → no rewrite (byte-for-byte stable)
    }

    let Some(path) = managed_facts_path(root, true) else {
        return 0;
    };
    write_atomic(&path, &render_jsonl(&store)).map_or(0, |()| marked)
}

/// Whether a stored LIVE fact is CLEARLY contradicted by the run's observations —
/// the shared conservative test both staleness signals fold into. See
/// [`mark_stale_facts`] for the full rationale.
fn fact_is_contradicted(
    f: &Fact,
    observed_values: &std::collections::HashMap<String, String>,
) -> bool {
    // Signal 1 — a fresh observation reported a clearly different value for this key.
    if let Some(observed) = observed_values.get(&f.key.trim().to_lowercase()) {
        if values_contradict(&norm_value(&f.value), observed) {
            return true;
        }
    }
    // Signal 2 — a `path` fact whose asserted absolute path no longer exists.
    if f.category.as_deref().map(str::to_lowercase).as_deref() == Some("path")
        && path_is_dead(&f.value)
    {
        return true;
    }
    false
}

/// Normalise a fact value for comparison: trim, lowercase, collapse internal
/// whitespace runs to a single space. So `"npm run build"` and `" NPM  run build "`
/// compare EQUAL and a mere formatting difference never reads as a contradiction.
fn norm_value(v: &str) -> String {
    v.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Whether two already-normalised values CLEARLY contradict (conservative). They
/// must be non-trivial, unequal, and NEITHER a substring of the other — so a pure
/// refinement/superset (`"vite build"` → `"vite build --prod"`) is treated as an
/// extension, not a contradiction. This is the over-pruning guard: an ambiguous or
/// weak difference abstains rather than demote a still-good fact.
fn values_contradict(stored: &str, observed: &str) -> bool {
    const MIN_VALUE_LEN: usize = 2;
    if stored.len() < MIN_VALUE_LEN || observed.len() < MIN_VALUE_LEN {
        return false; // too thin to judge → abstain
    }
    stored != observed && !stored.contains(observed) && !observed.contains(stored)
}

/// Whether `value` is a single ABSOLUTE path token that definitively does not
/// exist — the conservative dead-path signal. Values with whitespace (a command
/// with args), globs, or shell/`~`/`$` expansion, and RELATIVE paths (which could
/// be a not-yet-built artifact) are all skipped: they can't be resolved
/// unambiguously, so we never demote on them. Only `try_exists() == Ok(false)`
/// (definitely absent) is a contradiction; any `Err` (permission, transient mount)
/// leaves the fact ALONE (fail-open).
fn path_is_dead(value: &str) -> bool {
    let v = value.trim();
    if v.len() < 2
        || v.chars().any(char::is_whitespace)
        || v.starts_with('~')
        || v.contains('$')
        || v.contains('*')
        || v.contains('?')
    {
        return false;
    }
    let p = Path::new(v);
    if !p.is_absolute() {
        return false;
    }
    matches!(p.try_exists(), Ok(false))
}

/// The firmware **recall** block: stored facts as a compact, token-budgeted block
/// to inject over the base's system-prompt face.
///
/// Empty string when the store has no facts (fail-open / first-ever turn) — the
/// firmware then behaves exactly as before. When facts exist, the block carries
/// each value together with its provenance so fresh evidence can override it.
/// The whole block is bounded by `budget_chars` (typically
/// [`FACTS_FIRMWARE_BUDGET`]), so a huge store can never bloat the prompt.
/// Deterministic (no timestamps / no I/O beyond the one store read).
#[must_use]
pub fn facts_firmware_block(root: &Path, budget_chars: usize) -> String {
    facts_firmware_block_matching(root, budget_chars, |_| true, false)
}

/// Recall facts that can materially affect the current request.
///
/// Explicit inventory requests receive every verified fact. Ordinary work gets
/// lexical matches plus operational facts when the request asks to change or
/// validate the project. Unrelated historical decisions and preferences stay out.
#[must_use]
pub fn facts_firmware_block_for_requirement(
    root: &Path,
    budget_chars: usize,
    requirement: &str,
) -> String {
    if requirement_requests_fact_inventory(requirement) {
        return facts_firmware_block(root, budget_chars);
    }
    let query = crate::retrieval_relevance::terms(requirement);
    let needs_operations = requirement_needs_operational_facts(requirement);
    facts_firmware_block_matching(
        root,
        budget_chars,
        |fact| fact_matches_terms(fact, &query) || (needs_operations && fact_is_operational(fact)),
        true,
    )
}

fn facts_firmware_block_matching(
    root: &Path,
    budget_chars: usize,
    include: impl Fn(&Fact) -> bool,
    relevance_scoped: bool,
) -> String {
    if !recall_enabled(root, MemoryScope::Project, MemoryStore::Facts) {
        return String::new();
    }
    // Rows written before provenance was introduced remain readable for
    // inventory and migration, but they are not evidence-bearing memory.
    // Feeding them back to a base would let old model narration regain
    // influence merely because it survived on disk. Quarantine those rows
    // until a fresh host-validated extraction replaces the same key.
    let facts: Vec<Fact> = load_facts(root)
        .into_iter()
        .filter(|fact| fact_has_recallable_provenance(fact) && include(fact))
        .collect();
    if facts.is_empty() {
        return String::new();
    }

    let scope = if relevance_scoped { "relevant " } else { "" };
    let header = format!(
        "## RECALLED PROJECT FACTS ({scope}project memory — evidence, not authority)\n\
         Use these only where they materially affect the current request. Keep \
         their provenance distinct from user decisions; current repository/tool evidence wins \
         on conflict, and a changed or consequential surface must be verified before mutation."
    );
    let mut list = String::new();
    // Newest facts are most relevant — render them first.
    for f in facts.iter().rev() {
        list.push_str(&render_fact_line(f));
    }
    let reference_budget = budget_chars.saturating_sub(header.chars().count() + 2);
    let reference = umadev_knowledge::render_bounded_prompt_reference(
        umadev_knowledge::PromptReference {
            kind: umadev_knowledge::PromptReferenceKind::ProjectFact,
            corpus_origin: umadev_knowledge::CorpusOrigin::ProjectLearned,
            corpus_scope: umadev_knowledge::CorpusScope::Project,
            source: "project_fact_memory",
            section: Some("durable_facts"),
            content: &list,
        },
        reference_budget,
    );
    if reference.is_empty() {
        return String::new();
    }
    format!("{header}\n\n{reference}")
}

fn requirement_requests_fact_inventory(requirement: &str) -> bool {
    let normalized = requirement.to_lowercase();
    [
        "/memory",
        "project facts",
        "known facts",
        "fact inventory",
        "项目事实",
        "專案事實",
        "已知事实",
        "已知事實",
        "记住了什么",
        "記住了什麼",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

fn requirement_needs_operational_facts(requirement: &str) -> bool {
    let normalized = requirement.to_lowercase();
    [
        "build",
        "compile",
        "test",
        "lint",
        "run",
        "serve",
        "deploy",
        "install",
        "fix",
        "implement",
        "refactor",
        "update",
        "edit",
        "编译",
        "編譯",
        "构建",
        "構建",
        "测试",
        "測試",
        "运行",
        "運行",
        "部署",
        "安装",
        "安裝",
        "修复",
        "修復",
        "实现",
        "實現",
        "修改",
        "新增",
        "开发",
        "開發",
        "重构",
        "重構",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

fn fact_is_operational(fact: &Fact) -> bool {
    fact.category.as_deref().is_some_and(|category| {
        matches!(
            category.to_lowercase().as_str(),
            "command" | "path" | "version" | "port" | "toolchain" | "environment"
        )
    })
}

fn fact_matches_terms(fact: &Fact, query: &std::collections::HashSet<String>) -> bool {
    if query.is_empty() {
        return false;
    }
    let text = [
        Some(fact.key.as_str()),
        Some(fact.value.as_str()),
        fact.category.as_deref(),
        fact.evidence.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ");
    crate::retrieval_relevance::shares_term(&text, query)
}

/// Render one fact as a recall bullet: `- key [category] → value` (the category
/// is omitted when absent).
fn render_fact_line(f: &Fact) -> String {
    let mut metadata = Vec::new();
    if let Some(category) = &f.category {
        metadata.push(category.clone());
    }
    match (&f.provenance, &f.evidence) {
        (Some(provenance), Some(evidence)) => {
            metadata.push(format!("source={provenance}@{evidence}"));
        }
        (Some(provenance), None) => metadata.push(format!("source={provenance}")),
        (None, _) => metadata.push("source=legacy_unverified".to_string()),
    }
    format!("- {} [{}] → {}\n", f.key, metadata.join("; "), f.value)
}

fn fact_has_recallable_provenance(fact: &Fact) -> bool {
    matches!(
        (&fact.provenance, &fact.evidence),
        (Some(provenance), Some(evidence))
            if !evidence.trim().is_empty()
                && matches!(
                    provenance.as_str(),
                    "user_stated" | "repository_verified" | "filesystem_verified"
                )
    )
}

fn fact_is_safe(f: &Fact) -> bool {
    let key = f.key.trim();
    let value = f.value.trim();
    let provenance_is_safe = match (&f.provenance, &f.evidence) {
        (None, None) => true,
        (Some(provenance), Some(evidence)) => {
            matches!(
                provenance.as_str(),
                "user_stated" | "repository_verified" | "filesystem_verified"
            ) && !sensitive_key(provenance)
                && !contains_redaction_marker(provenance)
                && redact_text(provenance) == *provenance
                && !sensitive_key(evidence)
                && !contains_redaction_marker(evidence)
                && redact_text(evidence) == *evidence
        }
        _ => false,
    };
    !key.is_empty()
        && !value.is_empty()
        && !sensitive_key(key)
        && !contains_redaction_marker(value)
        && redact_text(value) == value
        && f.category.as_deref().is_none_or(|category| {
            !sensitive_key(category)
                && !contains_redaction_marker(category)
                && redact_text(category) == category
        })
        && provenance_is_safe
}

fn sensitive_key(key: &str) -> bool {
    if redact_text(key) != key {
        return true;
    }
    const PROBE: &str = "umadev-fact-key-probe";
    let mut object = serde_json::Map::new();
    object.insert(
        key.to_string(),
        serde_json::Value::String(PROBE.to_string()),
    );
    match redact_json(serde_json::Value::Object(object)) {
        serde_json::Value::Object(redacted) => {
            redacted.get(key).and_then(serde_json::Value::as_str) != Some(PROBE)
        }
        _ => true,
    }
}

fn contains_redaction_marker(value: &str) -> bool {
    value.to_ascii_lowercase().contains("[redacted")
}

/// Trim + field-truncate a fact to the per-fact bounds; normalise a blank
/// category to `None`.
fn normalize(f: Fact) -> Fact {
    use crate::experts::excerpt;
    Fact {
        key: excerpt(f.key.trim(), MAX_KEY_CHARS),
        value: excerpt(f.value.trim(), MAX_VALUE_CHARS),
        category: f
            .category
            .map(|c| excerpt(c.trim(), MAX_CATEGORY_CHARS))
            .filter(|c| !c.is_empty()),
        provenance: f
            .provenance
            .map(|p| excerpt(p.trim(), MAX_PROVENANCE_CHARS))
            .filter(|p| !p.is_empty()),
        evidence: f
            .evidence
            .map(|e| excerpt(e.trim(), MAX_EVIDENCE_CHARS))
            .filter(|e| !e.is_empty()),
        stale: f.stale,
    }
}

/// Dedup by key (case-insensitive, last occurrence wins → newest LAST) and cap to
/// [`MAX_FACTS`] keeping the most-recent ones. Shared by [`load_facts`] and the
/// record path so a store grown by raw appends is always normalised.
fn dedup_cap(facts: Vec<Fact>) -> Vec<Fact> {
    let mut out: Vec<Fact> = Vec::new();
    for f in facts {
        let key_l = f.key.to_lowercase();
        out.retain(|e| e.key.to_lowercase() != key_l);
        out.push(f);
    }
    let len = out.len();
    if len > MAX_FACTS {
        out.drain(0..len - MAX_FACTS);
    }
    out
}

/// Render the store as JSONL (one fact per line). A fact that fails to serialize
/// is skipped (fail-open).
fn render_jsonl(facts: &[Fact]) -> String {
    let mut buf = String::new();
    for f in facts {
        if let Ok(line) = serde_json::to_string(f) {
            buf.push_str(&line);
            buf.push('\n');
        }
    }
    buf
}

/// Atomically write `body` to `path` via a unique temp file + rename, so a reader
/// (or a concurrent writer) never observes a torn / partially-written file. The
/// temp name carries the pid + a high-resolution timestamp so two writers don't
/// collide on the temp itself. Best-effort cleanup of the temp on rename failure.
fn write_atomic(path: &Path, body: &str) -> std::io::Result<()> {
    if u64::try_from(body.len()).unwrap_or(u64::MAX) > MAX_FACTS_FILE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "fact store exceeds its byte limit",
        ));
    }
    umadev_state::fs::atomic_write(path, body.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cross_process_fact_writer_child() {
        let Some(root) = std::env::var_os("UMADEV_FACTS_MP_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        let index = std::env::var("UMADEV_FACTS_MP_INDEX").unwrap();
        std::fs::write(root.join(format!("facts-ready-{index}")), b"").unwrap();
        let started = std::time::Instant::now();
        while !root.join("facts-start").exists()
            && started.elapsed() < std::time::Duration::from_secs(10)
        {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(record_fact(
            &root,
            Fact::new(
                format!("worker-{index}"),
                format!("value-{index}"),
                None::<String>
            ),
        ));
    }

    #[test]
    fn four_process_fact_writes_have_zero_lost_updates() {
        const WRITERS: usize = 4;
        let temp = tempfile::TempDir::new().unwrap();
        let exe = std::env::current_exe().unwrap();
        let mut children = Vec::new();
        for index in 0..WRITERS {
            children.push(
                std::process::Command::new(&exe)
                    .args([
                        "--exact",
                        "project_facts::tests::cross_process_fact_writer_child",
                    ])
                    .env("UMADEV_FACTS_MP_ROOT", temp.path())
                    .env("UMADEV_FACTS_MP_INDEX", index.to_string())
                    .spawn()
                    .unwrap(),
            );
        }
        let started = std::time::Instant::now();
        loop {
            let ready = (0..WRITERS)
                .filter(|index| temp.path().join(format!("facts-ready-{index}")).exists())
                .count();
            if ready == WRITERS || started.elapsed() >= std::time::Duration::from_secs(10) {
                assert_eq!(
                    ready, WRITERS,
                    "not every fact writer reached the start gate"
                );
                std::fs::write(temp.path().join("facts-start"), b"").unwrap();
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        for mut child in children {
            assert!(child.wait().unwrap().success());
        }
        let facts = load_facts(temp.path());
        for index in 0..WRITERS {
            assert!(
                facts
                    .iter()
                    .any(|fact| fact.key == format!("worker-{index}")),
                "writer {index} was lost"
            );
        }
    }

    #[test]
    fn record_then_reload_persists_a_fact() {
        // The core contract: a fact recorded in one "turn" survives + reloads in a
        // later one (the memory-loss bug this module fixes).
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(record_fact(
            tmp.path(),
            Fact::new("JDK17", "/usr/lib/jvm/jdk-17", Some("path")),
        ));
        let facts = load_facts(tmp.path());
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].key, "JDK17");
        assert_eq!(facts[0].value, "/usr/lib/jvm/jdk-17");
        assert_eq!(facts[0].category.as_deref(), Some("path"));
        // The store lives at the managed path.
        assert!(tmp.path().join(FACTS_REL_PATH).exists());
    }

    #[test]
    fn provenance_round_trips_and_legacy_rows_are_quarantined_from_recall() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(record_fact(
            tmp.path(),
            Fact::new("build", "cargo build", Some("command"))
                .with_provenance("repository_verified", "README.md"),
        ));
        let facts = load_facts(tmp.path());
        assert_eq!(facts[0].provenance.as_deref(), Some("repository_verified"));
        assert_eq!(facts[0].evidence.as_deref(), Some("README.md"));
        assert!(facts_firmware_block(tmp.path(), FACTS_FIRMWARE_BUDGET)
            .contains("source=repository_verified@README.md"));
        assert!(!record_fact(
            tmp.path(),
            Fact::new("guess", "probably true", Some("decision"))
                .with_provenance("assistant_inferred", "maker_report"),
        ));

        std::fs::write(
            tmp.path().join(FACTS_REL_PATH),
            r#"{"key":"port","value":"5173"}"#,
        )
        .unwrap();
        assert_eq!(
            facts_firmware_block(tmp.path(), FACTS_FIRMWARE_BUDGET),
            "",
            "legacy rows remain readable on disk but must not regain prompt influence"
        );
        assert_eq!(load_facts(tmp.path()).len(), 1);
    }

    #[test]
    fn facts_policy_controls_capture_and_prompt_recall_but_not_inventory_reads() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(record_fact(
            tmp.path(),
            Fact::new("build", "cargo build", Some("command"))
                .with_provenance("repository_verified", "Cargo.toml"),
        ));

        crate::memory_control::update_capture(
            tmp.path(),
            MemoryScope::Project,
            Some(MemoryStore::Facts),
            false,
        )
        .unwrap();
        assert!(!record_fact(
            tmp.path(),
            Fact::new("test", "cargo test", Some("command")),
        ));
        assert_eq!(
            load_facts(tmp.path()).len(),
            1,
            "raw reporting stays visible"
        );

        crate::memory_control::update_recall(
            tmp.path(),
            MemoryScope::Project,
            Some(MemoryStore::Facts),
            false,
        )
        .unwrap();
        assert!(facts_firmware_block(tmp.path(), FACTS_FIRMWARE_BUDGET).is_empty());
        assert_eq!(load_facts(tmp.path()).len(), 1, "recall is not deletion");

        crate::memory_control::update_recall(
            tmp.path(),
            MemoryScope::Project,
            Some(MemoryStore::Facts),
            true,
        )
        .unwrap();
        assert!(facts_firmware_block(tmp.path(), FACTS_FIRMWARE_BUDGET).contains("cargo build"));

        std::fs::write(
            tmp.path().join(".umadev/memory/policy.toml"),
            "invalid = [toml",
        )
        .unwrap();
        assert!(!record_fact(
            tmp.path(),
            Fact::new("lint", "cargo clippy", Some("command")),
        ));
        assert!(facts_firmware_block(tmp.path(), FACTS_FIRMWARE_BUDGET).is_empty());
        assert_eq!(
            load_facts(tmp.path()).len(),
            1,
            "corrupt policy hides no audit data"
        );
    }

    #[test]
    fn recording_an_existing_key_updates_not_duplicates() {
        let tmp = tempfile::TempDir::new().unwrap();
        record_fact(
            tmp.path(),
            Fact::new("build", "mvn -q package", Some("command")),
        );
        // Re-record the same key (different case) with a new value → update, not dup.
        record_fact(
            tmp.path(),
            Fact::new("BUILD", "mvn -q -DskipTests package", None::<String>),
        );
        let facts = load_facts(tmp.path());
        assert_eq!(facts.len(), 1, "same key deduped: {facts:?}");
        assert_eq!(
            facts[0].value, "mvn -q -DskipTests package",
            "newest value wins"
        );
    }

    #[test]
    fn the_cap_is_enforced_evicting_oldest() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Record more than the cap, distinct keys, in order.
        for i in 0..(MAX_FACTS + 10) {
            record_fact(
                tmp.path(),
                Fact::new(format!("k{i}"), format!("v{i}"), Some("path")),
            );
        }
        let facts = load_facts(tmp.path());
        assert_eq!(facts.len(), MAX_FACTS, "store capped at MAX_FACTS");
        // The oldest (k0..k9) were evicted; the newest (last recorded) survives.
        assert!(facts.iter().all(|f| f.key != "k0"), "oldest evicted");
        assert_eq!(facts.last().unwrap().key, format!("k{}", MAX_FACTS + 9));
    }

    #[test]
    fn per_field_lengths_are_bounded() {
        let tmp = tempfile::TempDir::new().unwrap();
        record_fact(
            tmp.path(),
            Fact::new("k".repeat(500), "v".repeat(5_000), Some("c".repeat(500))),
        );
        let f = &load_facts(tmp.path())[0];
        assert!(f.key.chars().count() <= MAX_KEY_CHARS);
        assert!(f.value.chars().count() <= MAX_VALUE_CHARS);
        assert!(f.category.as_ref().unwrap().chars().count() <= MAX_CATEGORY_CHARS);
    }

    #[test]
    fn empty_facts_are_dropped() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(!record_fact(tmp.path(), Fact::new("", "v", None::<String>)));
        assert!(!record_fact(
            tmp.path(),
            Fact::new("k", "   ", None::<String>)
        ));
        assert!(load_facts(tmp.path()).is_empty());
    }

    #[test]
    fn load_is_fail_open_on_a_missing_store() {
        // No file → empty vec, never an error/panic.
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(load_facts(tmp.path()).is_empty());
        let missing = Path::new("/nonexistent/umadev/facts/root/xyz");
        assert!(load_facts(missing).is_empty());
    }

    #[test]
    fn load_is_forgiving_of_corrupt_lines() {
        // A garbage line is skipped; a valid line on either side survives.
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join(FACTS_REL_PATH);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "{\"key\":\"JDK17\",\"value\":\"/jvm/17\"}\n\
             this is not json at all {{{\n\
             {\"key\":\"port\",\"value\":\"5173\",\"category\":\"port\"}\n",
        )
        .unwrap();
        let facts = load_facts(tmp.path());
        assert_eq!(
            facts.len(),
            2,
            "good lines kept, garbage skipped: {facts:?}"
        );
        assert!(facts.iter().any(|f| f.key == "JDK17"));
        assert!(facts.iter().any(|f| f.key == "port"));
    }

    #[test]
    fn fully_corrupt_store_yields_no_facts() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join(FACTS_REL_PATH);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "not json\n<<<garbage>>>\n").unwrap();
        assert!(load_facts(tmp.path()).is_empty());
        assert!(facts_firmware_block(tmp.path(), FACTS_FIRMWARE_BUDGET).is_empty());
    }

    #[test]
    fn credentials_are_rejected_instead_of_persisting_redaction_markers() {
        let tmp = tempfile::TempDir::new().unwrap();
        let count = record_facts(
            tmp.path(),
            &[
                Fact::new("build", "cargo test --workspace", Some("command")),
                Fact::new("api_key", "not-safe-memory", None::<String>),
                Fact::new("remote_auth", "Bearer abcdefghijklmnop", None::<String>),
                Fact::new(
                    "signing_material",
                    "-----BEGIN PRIVATE KEY-----\nabc123\n-----END PRIVATE KEY-----",
                    None::<String>,
                ),
                Fact::new("repo_hint", "ghp_1234567890abcdef", None::<String>),
                Fact::new("masked", "[redacted]", None::<String>),
                Fact::new("category_leak", "safe", Some("password")),
            ],
        );
        assert_eq!(count, 1, "only the non-sensitive fact is committed");
        let facts = load_facts(tmp.path());
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].key, "build");
        let disk = std::fs::read_to_string(tmp.path().join(FACTS_REL_PATH)).unwrap();
        assert!(!disk.contains("Bearer"));
        assert!(!disk.contains("PRIVATE KEY"));
        assert!(!disk.contains("ghp_"));
        assert!(!disk.to_ascii_lowercase().contains("[redacted"));
    }

    #[test]
    fn legacy_sensitive_rows_are_never_loaded_or_reinjected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join(FACTS_REL_PATH);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body = render_jsonl(&[
            Fact::new("build", "cargo build", Some("command")),
            Fact::new("password", "old-memory-value", None::<String>),
            Fact::new("auth", "Bearer abcdefghijklmnop", None::<String>),
            Fact::new("token_hint", "xai-123456789abcdef", None::<String>),
            Fact::new(
                "signing",
                "-----BEGIN PRIVATE KEY-----\nabc123\n-----END PRIVATE KEY-----",
                None::<String>,
            ),
            Fact::new("category_leak", "safe", Some("Bearer abcdefghijklmnop")),
        ]);
        std::fs::write(&path, body).unwrap();

        let facts = load_facts(tmp.path());
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].key, "build");
        let firmware = facts_firmware_block(tmp.path(), FACTS_FIRMWARE_BUDGET);
        assert!(
            firmware.is_empty(),
            "safe-but-unverified legacy rows are retained but not reinjected"
        );
        assert!(!firmware.contains("old-memory-value"));
        assert!(!firmware.contains("Bearer"));
        assert!(!firmware.contains("PRIVATE KEY"));
        assert!(!firmware.contains("xai-"));

        assert!(record_fact(
            tmp.path(),
            Fact::new("test", "cargo test", Some("command")),
        ));
        let compacted = std::fs::read_to_string(path).unwrap();
        assert!(!compacted.contains("old-memory-value"));
        assert!(!compacted.contains("Bearer"));
        assert!(!compacted.contains("PRIVATE KEY"));
        assert!(!compacted.contains("xai-"));
    }

    #[test]
    fn firmware_block_is_empty_without_facts() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(facts_firmware_block(tmp.path(), FACTS_FIRMWARE_BUDGET).is_empty());
    }

    #[test]
    fn firmware_block_recalls_facts_without_direct_write_guidance() {
        let tmp = tempfile::TempDir::new().unwrap();
        record_fact(
            tmp.path(),
            Fact::new("JDK17", "/usr/lib/jvm/jdk-17", Some("path"))
                .with_provenance("filesystem_verified", "/usr/lib/jvm/jdk-17"),
        );
        record_fact(
            tmp.path(),
            Fact::new("build", "mvn -q package", Some("command"))
                .with_provenance("repository_verified", "pom.xml"),
        );
        let block = facts_firmware_block(tmp.path(), FACTS_FIRMWARE_BUDGET);
        // RECALL: the resolved facts are listed verbatim.
        assert!(
            block.contains("RECALLED PROJECT FACTS"),
            "labelled: {block}"
        );
        assert!(
            block.contains("/usr/lib/jvm/jdk-17"),
            "recalls the JDK path: {block}"
        );
        assert!(
            block.contains("mvn -q package"),
            "recalls the build command: {block}"
        );
        assert!(
            block.contains("authority=none"),
            "facts remain evidence: {block}"
        );
        assert!(
            block.contains("current repository/tool evidence wins"),
            "fresh evidence beats stale recall: {block}"
        );
        assert!(
            !block.contains(FACTS_REL_PATH) && !block.contains("append ONE JSON"),
            "the base is never asked to write durable facts directly: {block}"
        );
    }

    #[test]
    fn request_scoped_recall_excludes_unrelated_history() {
        let tmp = tempfile::TempDir::new().unwrap();
        record_fact(
            tmp.path(),
            Fact::new("login-policy", "keep cookie sessions", Some("decision"))
                .with_provenance("user_stated", "current_request"),
        );
        record_fact(
            tmp.path(),
            Fact::new(
                "sitemap-format",
                "split sitemap by locale",
                Some("decision"),
            )
            .with_provenance("user_stated", "current_request"),
        );

        let seo = facts_firmware_block_for_requirement(
            tmp.path(),
            FACTS_FIRMWARE_BUDGET,
            "优化 sitemap SEO",
        );
        assert!(seo.contains("split sitemap by locale"), "{seo}");
        assert!(!seo.contains("keep cookie sessions"), "{seo}");
    }

    #[test]
    fn change_turns_recall_operational_facts_but_not_unrelated_preferences() {
        let tmp = tempfile::TempDir::new().unwrap();
        record_fact(
            tmp.path(),
            Fact::new("test", "cargo test --workspace", Some("command"))
                .with_provenance("repository_verified", "README.md"),
        );
        record_fact(
            tmp.path(),
            Fact::new("dashboard-color", "purple", Some("preference"))
                .with_provenance("user_stated", "current_request"),
        );

        let block =
            facts_firmware_block_for_requirement(tmp.path(), FACTS_FIRMWARE_BUDGET, "修复登录接口");
        assert!(block.contains("cargo test --workspace"), "{block}");
        assert!(!block.contains("purple"), "{block}");
    }

    #[test]
    fn explicit_fact_inventory_recalls_every_verified_fact() {
        let tmp = tempfile::TempDir::new().unwrap();
        for (key, value) in [
            ("login-policy", "cookies"),
            ("sitemap-format", "per locale"),
        ] {
            record_fact(
                tmp.path(),
                Fact::new(key, value, Some("decision"))
                    .with_provenance("user_stated", "current_request"),
            );
        }
        let block =
            facts_firmware_block_for_requirement(tmp.path(), FACTS_FIRMWARE_BUDGET, "查看项目事实");
        assert!(block.contains("cookies"), "{block}");
        assert!(block.contains("per locale"), "{block}");
    }

    #[test]
    fn firmware_block_is_token_budgeted() {
        // Even a maxed-out store of large facts must keep the block within budget.
        let tmp = tempfile::TempDir::new().unwrap();
        for i in 0..MAX_FACTS {
            record_fact(
                tmp.path(),
                Fact::new(format!("key{i}"), "v".repeat(MAX_VALUE_CHARS), Some("path"))
                    .with_provenance("repository_verified", "Cargo.toml"),
            );
        }
        let block = facts_firmware_block(tmp.path(), FACTS_FIRMWARE_BUDGET);
        assert!(
            block.chars().count() <= FACTS_FIRMWARE_BUDGET,
            "block must stay within budget ({} > {FACTS_FIRMWARE_BUDGET})",
            block.chars().count()
        );
        assert!(block.contains("RECALLED PROJECT FACTS"));
    }

    #[test]
    fn record_is_fail_open_on_an_unwritable_root() {
        // A root whose PARENT is a regular file can never be created/written
        // (making a directory under a file fails on every OS); recording is a
        // no-op and a later load is empty — never a panic. (A bare `/nonexistent`
        // path is not cross-platform: on windows a leading `/` is drive-relative
        // and `C:\nonexistent\...` is usually creatable, so the write would
        // unexpectedly succeed and the store would be non-empty.)
        let tmp = tempfile::TempDir::new().unwrap();
        let blocker = tmp.path().join("not-a-dir");
        std::fs::write(&blocker, b"x").unwrap();
        let unwritable = blocker.join("umadev/facts/unwritable/xyz");
        assert!(
            !record_fact(&unwritable, Fact::new("k", "v", None::<String>)),
            "a failed write must not report a committed fact"
        );
        assert!(load_facts(&unwritable).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn managed_symlinks_are_never_followed_or_replaced() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join(FACTS_REL_PATH);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let outside = tmp.path().join("outside.jsonl");
        let outside_body = "{\"key\":\"outside\",\"value\":\"must-not-load\"}\n";
        std::fs::write(&outside, outside_body).unwrap();
        symlink(&outside, &path).unwrap();

        assert!(load_facts(tmp.path()).is_empty());
        assert!(facts_firmware_block(tmp.path(), FACTS_FIRMWARE_BUDGET).is_empty());
        assert!(!record_fact(
            tmp.path(),
            Fact::new("build", "cargo build", Some("command")),
        ));
        assert_eq!(std::fs::read_to_string(outside).unwrap(), outside_body);
        assert!(std::fs::symlink_metadata(path)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn managed_parent_symlink_is_rejected() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::TempDir::new().unwrap();
        let outside = tmp.path().join("outside-memory");
        std::fs::create_dir(&outside).unwrap();
        let umadev = tmp.path().join(".umadev");
        std::fs::create_dir(&umadev).unwrap();
        symlink(&outside, umadev.join("memory")).unwrap();

        assert!(!record_fact(
            tmp.path(),
            Fact::new("build", "cargo build", Some("command")),
        ));
        assert!(!outside.join("facts.jsonl").exists());
    }

    #[test]
    fn legacy_direct_rows_are_compacted_on_next_record() {
        // Old versions and external writers may have left duplicate rows.
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join(FACTS_REL_PATH);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "{\"key\":\"JDK17\",\"value\":\"/old/path\"}\n\
             {\"key\":\"JDK17\",\"value\":\"/usr/lib/jvm/jdk-17\",\"category\":\"path\"}\n",
        )
        .unwrap();
        // Reading dedups (newest wins) even before we record.
        let facts = load_facts(tmp.path());
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].value, "/usr/lib/jvm/jdk-17");
        // A subsequent record compacts the on-disk file to the canonical set.
        record_fact(tmp.path(), Fact::new("port", "5173", Some("port")));
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk.lines().count(), 2, "compacted to 2 canonical lines");
    }

    // ── Staleness sweep (contradiction control for facts) ──────────────────────

    #[test]
    fn observed_contradiction_tombstones_a_fact_and_demotes_it_from_recall() {
        // A stored fact whose asserted value the run OBSERVES to be clearly
        // different is demoted from recall but KEPT on disk for provenance.
        let tmp = tempfile::TempDir::new().unwrap();
        record_fact(
            tmp.path(),
            Fact::new("build", "npm run build", Some("command")),
        );
        let marked = mark_stale_facts(
            tmp.path(),
            &[Fact::new("build", "vite build", None::<String>)],
        );
        assert_eq!(marked, 1, "the contradicted fact is tombstoned");
        // Demoted below recall: neither the recall load nor the firmware sees it.
        assert!(
            load_facts(tmp.path()).iter().all(|f| f.key != "build"),
            "a stale fact is excluded from recall"
        );
        let block = facts_firmware_block(tmp.path(), FACTS_FIRMWARE_BUDGET);
        assert!(
            !block.contains("npm run build"),
            "the stale value never surfaces in the firmware: {block}"
        );
        // Provenance: the row (old value) survives on disk, flagged stale.
        let on_disk = std::fs::read_to_string(tmp.path().join(FACTS_REL_PATH)).unwrap();
        assert!(
            on_disk.contains("npm run build") && on_disk.contains("\"stale\":true"),
            "the tombstone is kept on disk for provenance: {on_disk}"
        );
    }

    #[test]
    fn a_weak_or_ambiguous_signal_never_demotes_a_good_fact() {
        // Over-pruning guard: a formatting-only variant, a refinement/superset, an
        // equal value, and an unrelated observed key must all leave the fact LIVE.
        let tmp = tempfile::TempDir::new().unwrap();
        record_fact(
            tmp.path(),
            Fact::new("build", "npm run build", Some("command")),
        );
        // Case/whitespace variant → normalises equal → not a contradiction.
        assert_eq!(
            mark_stale_facts(
                tmp.path(),
                &[Fact::new("build", "  NPM   run   build ", None::<String>)]
            ),
            0,
            "a formatting variant is not a contradiction"
        );
        // A pure refinement (superset) → an extension, not a contradiction.
        assert_eq!(
            mark_stale_facts(
                tmp.path(),
                &[Fact::new("build", "npm run build --prod", None::<String>)]
            ),
            0,
            "a refinement is not a contradiction"
        );
        // An unrelated observed key never touches this fact.
        assert_eq!(
            mark_stale_facts(tmp.path(), &[Fact::new("test", "npm test", None::<String>)]),
            0,
            "an unrelated observation contradicts nothing"
        );
        // The fact is still recalled after all three weak signals.
        assert!(
            load_facts(tmp.path())
                .iter()
                .any(|f| f.key == "build" && f.value == "npm run build"),
            "a good fact survives every weak/ambiguous signal"
        );
    }

    #[test]
    fn a_dead_absolute_path_fact_is_tombstoned_but_a_live_one_survives() {
        let tmp = tempfile::TempDir::new().unwrap();
        // An absolute path that certainly does not exist → dead → tombstoned.
        let dead = tmp.path().join("definitely-not-here-xyz");
        record_fact(
            tmp.path(),
            Fact::new("jdk", dead.to_string_lossy(), Some("path")),
        );
        // An absolute path that DOES exist (the temp dir itself) → live.
        record_fact(
            tmp.path(),
            Fact::new("workspace", tmp.path().to_string_lossy(), Some("path")),
        );
        // A RELATIVE path (could be a not-yet-built artifact) → never demoted.
        record_fact(tmp.path(), Fact::new("dist", "./dist", Some("path")));
        let marked = mark_stale_facts(tmp.path(), &[]);
        assert_eq!(marked, 1, "only the dead absolute path is tombstoned");
        let live = load_facts(tmp.path());
        assert!(live.iter().all(|f| f.key != "jdk"), "dead path demoted");
        assert!(
            live.iter().any(|f| f.key == "workspace"),
            "an existing absolute path stays live"
        );
        assert!(
            live.iter().any(|f| f.key == "dist"),
            "a relative path is never demoted (could be a build artifact)"
        );
    }

    #[test]
    fn re_recording_a_key_revives_a_stale_fact() {
        // A fresh observation for a tombstoned key supersedes the tombstone: the
        // store holds one LIVE row again, recalled normally.
        let tmp = tempfile::TempDir::new().unwrap();
        record_fact(
            tmp.path(),
            Fact::new("build", "npm run build", Some("command")),
        );
        mark_stale_facts(
            tmp.path(),
            &[Fact::new("build", "vite build", None::<String>)],
        );
        assert!(load_facts(tmp.path()).iter().all(|f| f.key != "build"));
        // Record the fresh value → the key is live again.
        record_fact(
            tmp.path(),
            Fact::new("build", "vite build", Some("command")),
        );
        let live = load_facts(tmp.path());
        let build: Vec<_> = live.iter().filter(|f| f.key == "build").collect();
        assert_eq!(build.len(), 1, "one live row for the revived key");
        assert_eq!(build[0].value, "vite build", "the fresh value is recalled");
    }

    #[test]
    fn mark_stale_is_fail_open_on_an_empty_or_missing_store() {
        // No store → nothing to sweep → 0, no file created, never a panic.
        let tmp = tempfile::TempDir::new().unwrap();
        assert_eq!(
            mark_stale_facts(tmp.path(), &[Fact::new("build", "x", None::<String>)]),
            0
        );
        assert!(!tmp.path().join(FACTS_REL_PATH).exists());
        let missing = Path::new("/nonexistent/umadev/facts/root/xyz");
        assert_eq!(mark_stale_facts(missing, &[]), 0);
    }

    #[cfg(unix)]
    #[test]
    fn mark_stale_does_not_report_success_when_commit_fails() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::TempDir::new().unwrap();
        assert!(record_fact(
            tmp.path(),
            Fact::new("build", "npm run build", Some("command")),
        ));
        let memory = tmp.path().join(MEMORY_DIR);
        let original = std::fs::metadata(&memory).unwrap().permissions();
        std::fs::set_permissions(&memory, std::fs::Permissions::from_mode(0o555)).unwrap();
        let probe = memory.join("write-probe");
        if std::fs::write(&probe, b"probe").is_ok() {
            let _ = std::fs::remove_file(probe);
            std::fs::set_permissions(&memory, original).unwrap();
            return;
        }

        let marked = mark_stale_facts(
            tmp.path(),
            &[Fact::new("build", "vite build", None::<String>)],
        );
        std::fs::set_permissions(&memory, original).unwrap();
        assert_eq!(marked, 0, "failed persistence cannot report a tombstone");
        assert!(load_facts(tmp.path())
            .iter()
            .any(|fact| fact.key == "build" && fact.value == "npm run build"));
    }
}
