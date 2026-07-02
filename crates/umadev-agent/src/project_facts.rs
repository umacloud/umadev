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
//! This module is that place: a small, append-friendly, per-project store of
//! durable facts the team should never lose — resolved tool/binary locations,
//! build/run/test commands, environment constraints (required versions, ports),
//! architecture decisions, user preferences. It is the *fact* sibling of the
//! *pitfall* ledger in [`crate::lessons`].
//!
//! ## The loop
//!
//! - **RECORD** — [`record_fact`] / [`record_facts`] persist a fact via an
//!   atomic temp+rename write, deduping by key and enforcing the bounds. The
//!   base can ALSO record on its own: the on-disk shape ([`FACTS_REL_PATH`]) is
//!   one self-contained JSON object per line, so the base appends a fact with
//!   its normal file tools (the firmware block documents the exact shape).
//! - **RECALL** — [`facts_firmware_block`] renders the stored facts as a
//!   compact, token-budgeted block that [`crate::context::compose_firmware`]
//!   injects into the **always-on work-class head** on EVERY work turn, so the
//!   base always sees the facts regardless of the bounded transcript or a base
//!   context rotation — and never re-searches a known fact.
//!
//! ## Bounded + fail-open by contract
//!
//! The store is capped at [`MAX_FACTS`] entries (oldest evicted) with each
//! field truncated ([`MAX_KEY_CHARS`] / [`MAX_VALUE_CHARS`] / [`MAX_CATEGORY_CHARS`]),
//! and the firmware block is capped at [`FACTS_FIRMWARE_BUDGET`] characters — so
//! the prompt can never bloat. Every path is fail-open: a missing dir, an
//! unreadable file, a corrupt/garbage line, or a failed write degrades to "no
//! facts" and behaves exactly as before — this module NEVER panics and NEVER
//! returns an error that could block the base.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Repo-relative directory holding the durable per-project memory.
pub const MEMORY_DIR: &str = ".umadev/memory";

/// Repo-relative path of the durable fact store — an append-friendly JSONL file
/// (one self-contained JSON fact per line) the base can append to with its own
/// file tools. Kept as one constant so the recorder, the recall block, and the
/// tests all name the same file.
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

/// Character budget for the firmware recall block. Deliberately tight: the facts
/// block rides in the always-on head on TOP of the identity + craft law, so it
/// must stay a small, high-signal overlay. [`facts_firmware_block`] fills the
/// fact list up to this budget and never exceeds it.
pub const FACTS_FIRMWARE_BUDGET: usize = 1_200;

/// One durable project fact — a short key, its value, and an optional category.
///
/// The on-disk form is one of these serialized to a single JSON line, e.g.
/// `{"key":"JDK17","value":"/usr/lib/jvm/jdk-17","category":"path"}`. The shape
/// is intentionally tiny + stable so the base can append a fact with its own
/// file tools (see the module docs).
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
    /// `true` once a run OBSERVED this fact to be CONTRADICTED — its asserted
    /// `path` no longer exists on disk, or a fresh observation this run reported a
    /// clearly different value for the same key (see [`mark_stale_facts`]). A stale
    /// fact is a TOMBSTONE: [`load_facts`] / [`facts_firmware_block`] exclude it
    /// from recall (demoted below the cut) but it is KEPT on disk for provenance —
    /// the same non-destructive posture the lessons ledger uses for an invalidated
    /// lesson. It is cleared automatically when the key is re-recorded with a fresh
    /// value ([`record_facts`] replaces the row). `#[serde(default)]` keeps every
    /// pre-existing JSONL row readable, and `skip_serializing_if` keeps a LIVE
    /// fact's on-disk shape byte-for-byte as before (no `stale` key emitted), so
    /// the documented base-append contract is unchanged.
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
            stale: false,
        }
    }
}

/// Serialises the read-modify-write of the fact store so concurrent callers (a
/// forked critic, a parallel step, the staleness sweep vs. the recorder) can't
/// clobber each other. Recovers from poison so a panic elsewhere never blocks
/// this fail-open path. Shared by [`record_facts`] and [`mark_stale_facts`].
static FACTS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Absolute path of the fact store for a given project root.
fn facts_path(root: &Path) -> PathBuf {
    root.join(FACTS_REL_PATH)
}

/// Load the durable, RECALLABLE facts for `root`, newest LAST — the LIVE facts
/// only, with stale tombstones excluded so a contradicted fact never surfaces.
///
/// Fail-open + forgiving: a missing file yields an empty vec; a corrupt/garbage
/// line is skipped (a single bad append never loses the rest of the store). The
/// result is deduped by key (case-insensitive, last occurrence wins so a re-record
/// supersedes the older value) and capped at [`MAX_FACTS`] (oldest dropped), so a
/// store grown by raw base appends is always normalised on read. Stale
/// (tombstoned) facts are filtered LAST — they stay on disk for provenance (see
/// [`load_facts_raw`]) but are demoted below recall.
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
    let Ok(text) = std::fs::read_to_string(facts_path(root)) else {
        return Vec::new();
    };
    let parsed: Vec<Fact> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Fact>(l).ok())
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
/// Each incoming fact is trimmed + field-truncated; empty (no key or no value)
/// facts are dropped. Recording an existing key UPDATES it (and moves it to
/// newest), so the store holds one entry per key. After merging, the store is
/// capped at [`MAX_FACTS`] (oldest evicted) and written atomically (temp+rename),
/// which ALSO compacts any extra lines the base appended directly. Returns how
/// many valid facts were applied.
///
/// Fail-open: invalid-only input is a no-op (`0`); a write error is swallowed —
/// recording a fact must never block the team.
pub fn record_facts(root: &Path, incoming: &[Fact]) -> usize {
    // Serialize the read-modify-write so concurrent callers (a forked critic, a
    // parallel step, the staleness sweep) can't clobber each other. Recover from
    // poison so a panic elsewhere never blocks this fail-open path.
    let _guard = FACTS_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let valid: Vec<Fact> = incoming
        .iter()
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

    let path = facts_path(root);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = write_atomic(&path, &render_jsonl(&store));
    valid.len()
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
/// Bounded (one pass over the ≤ [`MAX_FACTS`] store, one `try_exists` per path
/// fact), deterministic, and fail-open at every step: an empty/unreadable store,
/// no observations, or a failed write all yield `0` and never panic. Returns how
/// many LIVE facts were newly tombstoned.
pub fn mark_stale_facts(root: &Path, observed: &[Fact]) -> usize {
    // Share the recorder's lock so a concurrent record/sweep can't clobber the
    // rewrite. Poison-tolerant (fail-open).
    let _guard = FACTS_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let mut store = load_facts_raw(root);
    if store.is_empty() {
        return 0;
    }

    // The run's OBSERVED truth: key (lowercased) → normalised value. Empty
    // keys/values are dropped so a blank observation never contradicts anything.
    let observed_values: std::collections::HashMap<String, String> = observed
        .iter()
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

    let path = facts_path(root);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = write_atomic(&path, &render_jsonl(&store));
    marked
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

/// The firmware **recall** block: the stored facts as a compact, token-budgeted
/// block to inject over the base's system-prompt face, PLUS the record-guidance
/// that tells the base how to persist a new durable fact.
///
/// Empty string when the store has no facts (fail-open / first-ever turn) — the
/// firmware then behaves exactly as before. When facts exist, the block leads
/// with a recall list ("use these directly; do NOT re-derive / re-search") and
/// closes with the append shape for [`FACTS_REL_PATH`]. The whole block is
/// bounded by `budget_chars` (typically [`FACTS_FIRMWARE_BUDGET`]): the fact list
/// is filled until the budget is reached, so a huge store can never bloat the
/// prompt. Deterministic (no timestamps / no I/O beyond the one store read).
#[must_use]
pub fn facts_firmware_block(root: &Path, budget_chars: usize) -> String {
    let facts = load_facts(root);
    if facts.is_empty() {
        return String::new();
    }

    let header =
        "## KNOWN PROJECT FACTS (已知项目事实 — use directly; do NOT re-derive or re-search)\n\n\
         These facts were already resolved on THIS project and persist across turns. Use them \
         as-is — do NOT re-search, re-detect, or re-derive a fact listed here:\n";
    let footer = format!(
        "\nTo REMEMBER a new durable fact you resolve (a tool/binary path, a required version, a \
         port, a build/run/test command, an architecture decision, a user preference), append ONE \
         JSON line to `{FACTS_REL_PATH}` with your file tools:\n\
         {{\"key\":\"<short name>\",\"value\":\"<value>\",\"category\":\"<path|version|port|command|decision|preference>\"}}\n\
         Record only STABLE, reusable facts — never transient state. Recorded facts are recalled \
         here on every later turn, so the team never loses them."
    );

    // Reserve room for the header + footer so the fact list never crowds out the
    // record-guidance; fill the list with the remaining budget.
    let reserve = header.chars().count() + footer.chars().count() + 8;
    let list_budget = budget_chars.saturating_sub(reserve);
    let mut list = String::new();
    // Newest facts are most relevant — render them first.
    for f in facts.iter().rev() {
        let line = render_fact_line(f);
        if list.chars().count() + line.chars().count() > list_budget {
            break;
        }
        list.push_str(&line);
    }
    if list.is_empty() {
        // Budget too tight for a whole fact — surface the newest one, truncated,
        // so the block is never just a header+footer with no recall.
        if let Some(f) = facts.last() {
            list = crate::experts::excerpt(&render_fact_line(f), list_budget.max(1));
        }
    }

    // header + list (≤ list_budget) + footer ≤ budget by construction; the final
    // excerpt is a hard backstop for a pathologically small budget.
    crate::experts::excerpt(&format!("{header}{list}{footer}"), budget_chars)
}

/// Render one fact as a recall bullet: `- key [category] → value` (the category
/// is omitted when absent).
fn render_fact_line(f: &Fact) -> String {
    match &f.category {
        Some(c) => format!("- {} [{}] → {}\n", f.key, c, f.value),
        None => format!("- {} → {}\n", f.key, f.value),
    }
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
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = dir.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("facts"),
        std::process::id(),
        stamp,
    ));
    std::fs::write(&tmp, body)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // The store lives at the documented append-friendly path.
        assert!(tmp.path().join(FACTS_REL_PATH).exists());
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
        // A garbage line is skipped; a valid line on either side survives — a single
        // bad base append never loses the rest of the store.
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
    fn firmware_block_is_empty_without_facts() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(facts_firmware_block(tmp.path(), FACTS_FIRMWARE_BUDGET).is_empty());
    }

    #[test]
    fn firmware_block_recalls_facts_and_carries_record_guidance() {
        let tmp = tempfile::TempDir::new().unwrap();
        record_fact(
            tmp.path(),
            Fact::new("JDK17", "/usr/lib/jvm/jdk-17", Some("path")),
        );
        record_fact(
            tmp.path(),
            Fact::new("build", "mvn -q package", Some("command")),
        );
        let block = facts_firmware_block(tmp.path(), FACTS_FIRMWARE_BUDGET);
        // RECALL: the resolved facts are listed verbatim.
        assert!(block.contains("KNOWN PROJECT FACTS"), "labelled: {block}");
        assert!(
            block.contains("/usr/lib/jvm/jdk-17"),
            "recalls the JDK path: {block}"
        );
        assert!(
            block.contains("mvn -q package"),
            "recalls the build command: {block}"
        );
        assert!(
            block.contains("do NOT re-"),
            "tells the base not to re-search: {block}"
        );
        // RECORD: the append shape + path is documented so the base can persist more.
        assert!(
            block.contains(FACTS_REL_PATH),
            "documents the store path: {block}"
        );
        assert!(
            block.contains("\"key\""),
            "documents the JSON shape: {block}"
        );
    }

    #[test]
    fn firmware_block_is_token_budgeted() {
        // Even a maxed-out store of large facts must keep the block within budget.
        let tmp = tempfile::TempDir::new().unwrap();
        for i in 0..MAX_FACTS {
            record_fact(
                tmp.path(),
                Fact::new(format!("key{i}"), "v".repeat(MAX_VALUE_CHARS), Some("path")),
            );
        }
        let block = facts_firmware_block(tmp.path(), FACTS_FIRMWARE_BUDGET);
        assert!(
            block.chars().count() <= FACTS_FIRMWARE_BUDGET,
            "block must stay within budget ({} > {FACTS_FIRMWARE_BUDGET})",
            block.chars().count()
        );
        // …and still carry the record-guidance (footer survives the budgeting).
        assert!(
            block.contains(FACTS_REL_PATH),
            "record-guidance survives: {block}"
        );
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
        // Returns the applied count but the write silently fails (fail-open).
        let _ = record_fact(&unwritable, Fact::new("k", "v", None::<String>));
        assert!(load_facts(&unwritable).is_empty());
    }

    #[test]
    fn base_style_append_is_compacted_on_next_record() {
        // Simulate the base appending raw lines (its own file tools), incl. a dup.
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
}
