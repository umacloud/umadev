//! Durable per-project **SUCCESS-RECIPE** memory — the store that lets the team
//! learn from its WINS without leaking project material across workspaces.
//!
//! ## Why this exists (the "learn from success" gap)
//!
//! UmaDev already has a rich failure→pitfall pipeline: a build that trips over a
//! dependency, a flaky test, or a contract drift is captured as a pitfall
//! ([`crate::lessons`]), given a frequency signal, and recalled the next time a
//! similar error looms. But when a deliberate build passes CLEANLY the *winning
//! approach* — the plan shape that worked, the scaffold it produced, the patterns
//! it leaned on — is DISCARDED. A memory audit called this the single biggest
//! "team gets stronger" gap: the reusable-SOP capability a real senior team
//! accretes over many clean deliveries. This module is that place.
//!
//! It is the WIN sibling of the pitfall ledger, modelled on the same
//! read/recall/bound/fail-open pattern as the durable fact + open-decision stores
//! ([`crate::project_facts`] / [`crate::open_decisions`]) — but it lives in the
//! **project-local tier** under `.umadev/memory/recipes` and is keyed by a task
//! **fingerprint** (stack + kind + rough feature shape). Rich data such as plan
//! titles, workspace paths, requirement shape, and model-distilled patterns never
//! enters the old home-global store. Older global rows are quarantined and are
//! never recalled because their provenance/privacy cannot be established.
//! The global projection budget is deliberately zero: a future projection must be
//! rebuilt from a closed product-owned allowlist, never copied from recipe text.
//!
//! ## The loop
//!
//! - **CAPTURE** (through the internal delivery capture) — at the SAME
//!   finalize/delivery seam where memory reconciliation runs,
//!   when a build settled CLEAN on a DELIBERATE route, distill a [`Recipe`] from the
//!   plan the team actually executed (the ordered step titles/seats that reached
//!   `Done`), the scaffold it produced (the concrete files its evidence contracts
//!   named), and the detected stack + requirement shape. The "patterns" +
//!   extra-scaffold enrichment MAY use ONE read-only forked brain consult; if that
//!   consult fails, a MECHANICAL recipe (skeleton + evidence-derived scaffold +
//!   stack) is still stored. A capture error NEVER affects delivery.
//! - **RECALL** ([`prepare_recipe_prior`]) is read-only: it returns one candidate
//!   and an opaque prepared receipt. Only after the complete prior is present in a
//!   directive that the base transport accepted does [`commit_recipe_prior_sent`]
//!   append an immutable `sent` record. Merely retrieving/rendering a candidate is
//!   never counted as use.
//! - **SETTLE** ([`settle_recipe_receipt`]) appends exactly one terminal
//!   `pass`/`fail`/`unknown` outcome for that exact sent receipt. Failures therefore
//!   contribute to the denominator, duplicate/out-of-order settlements do not, and
//!   a later clean capture cannot award an unrelated prior a win.
//!
//! ## Bounded + fail-open by contract
//!
//! The store is capped at an internal recipe limit (lowest-evidence evicted) with
//! every list field bounded by internal skeleton/scaffold/pattern/shape limits,
//! each string field truncated, and the
//! recall block is capped at [`RECIPE_PRIOR_BUDGET`] characters — so neither the
//! store nor the prompt can bloat. A recipe is a PRIOR/suggestion, NEVER a gate: it
//! does not touch loop control, the deterministic floor, or any acceptance verdict.
//! Every path is fail-open: a missing/corrupt store, an offline brain,
//! or a failed write degrades to "no recipe" and behaves exactly as before — this
//! module NEVER panics and NEVER returns an error that could block a delivery.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use umadev_governance::redaction::{redact_json, redact_text};
use umadev_runtime::BaseSession;

use crate::events::{EngineEvent, EventSink};
use crate::experts::excerpt;
use crate::memory_control::{capture_enabled, recall_enabled, MemoryScope, MemoryStore};
use crate::plan_state::{EvidenceContract, Plan, StepKind, StepStatus};
use crate::router::RoutePlan;

/// Repo-relative directory holding rich, project-private recipes.
pub const RECIPES_DIRNAME: &str = ".umadev/memory/recipes";

/// The recipe store filename inside [`RECIPES_DIRNAME`] — an append-friendly JSONL
/// file (one self-contained [`Recipe`] JSON object per line).
pub const RECIPES_FILE: &str = "recipes.jsonl";

/// Immutable causal journal. It contains opaque receipt/recipe hashes and outcomes,
/// never titles, paths, requirement tokens, or model output.
pub const RECIPE_OUTCOMES_FILE: &str = "outcomes.jsonl";

/// Project-local marker retaining the one sent receipt associated with a resumable
/// owned plan. It is removed on terminal settlement.
const ACTIVE_RECEIPT_FILE: &str = "active-receipt.json";

/// Optional environment override for a *parent* of project-isolated stores. UmaDev
/// always appends `projects/<hash>/recipes`; two projects can never share rich rows
/// merely because they use the same override.
pub const RECIPES_DIR_ENV: &str = "UMADEV_RECIPES_DIR";

const MAX_STORE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_JOURNAL_BYTES: u64 = 4 * 1024 * 1024;
const MAX_JOURNAL_EVENTS: usize = 8_192;
const STORE_LOCK_DIR: &str = ".recipes.lock";
const LOCK_OWNER_FILE: &str = "owner.json";
const LOCK_WAIT_ATTEMPTS: usize = 500;
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_millis(2);
const STALE_LOCK_MS: u64 = 300_000;

/// Hard cap on distinct recipes retained on disk. When exceeded, the
/// LOWEST-evidence recipes (fewest clean builds) are
/// evicted first — the most-proven shapes are the ones worth keeping.
const MAX_RECIPES: usize = 128;

/// Per-recipe cap on plan-skeleton step lines. A skeleton is a compact "shape", not
/// the whole build log.
const MAX_SKELETON_STEPS: usize = 16;

/// Per-recipe cap on notable scaffold paths recorded.
const MAX_SCAFFOLD: usize = 24;

/// Per-recipe cap on short pattern notes recorded.
const MAX_PATTERNS: usize = 12;

/// Per-fingerprint cap on rough feature-shape tokens.
const MAX_SHAPE_TOKENS: usize = 12;

/// Per-line char cap for a skeleton step / scaffold path / pattern note, so one
/// runaway string can't dominate a recipe or the recall budget.
const MAX_LINE_CHARS: usize = 160;

/// Per-token char cap for a shape token.
const MAX_TOKEN_CHARS: usize = 40;

/// Character budget for the plan-time RECALL prior block. Tight by design: it rides
/// on TOP of the plan-synthesis prompt as a single adaptable prior, so it must stay a
/// small, high-signal overlay.
pub const RECIPE_PRIOR_BUDGET: usize = 1_400;

/// The minimum score after the strict stack/kind/shape eligibility checks. A prior
/// without positive task-shape evidence is withheld instead of being injected merely
/// because two requests share a broad route class.
const MIN_RECALL_SIMILARITY: f32 = 0.80;

/// A task **fingerprint** — the key a recipe is stored + looked up under. Coarse on
/// purpose: `stack` + `kind` are the strong signals; `shape` is a rough,
/// order-insensitive token set distilled from the requirement so two "todo list"
/// builds on the same stack land near each other.
///
/// `shape` is normalised (lowercased, de-duplicated, SORTED, bounded) at construction
/// so `#[derive(PartialEq, Eq)]` is a true exact-fingerprint comparison.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    /// The detected project stack (`node` / `rust` / `python` / `go` / `deno` /
    /// `none`) — [`crate::verify::ProjectKind::as_str`].
    pub stack: String,
    /// The routed task kind (`greenfield` / `frontend_only` / … ) —
    /// [`crate::planner::TaskKind::id`].
    pub kind: String,
    /// Rough feature-shape tokens distilled from the requirement (sorted, unique,
    /// bounded). May be empty (a bare requirement).
    #[serde(default)]
    pub shape: Vec<String>,
}

/// Outcome statistics for a recipe — the confidence signal a later run reads.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutcomeStats {
    /// How many CLEAN deliberate deliveries have folded into (captured/merged) this
    /// recipe. Every capture is by definition a clean build, so this is a count of
    /// PROVEN-clean uses of this plan shape — the primary confidence weight.
    #[serde(default)]
    pub clean_builds: u32,
    /// How many complete prior blocks were accepted for writing by the base
    /// transport. A read-only lookup never increments this.
    #[serde(default)]
    pub times_reused: u32,
    /// Exact sent receipts that settled PASS.
    #[serde(default)]
    pub reuse_wins: u32,
    /// Exact sent receipts that settled FAIL.
    #[serde(default)]
    pub reuse_failures: u32,
    /// Exact sent receipts whose delivery result became unknowable (for example a
    /// superseded/crashed run). Unknowns are visible but excluded from pass-rate.
    #[serde(default)]
    pub reuse_unknown: u32,
    /// Sent receipts not yet terminal (normally a run paused at a human gate).
    #[serde(default)]
    pub pending_reuses: u32,
}

impl OutcomeStats {
    /// The known-outcome clean-pass rate, `pass / (pass + fail)`. Pending and
    /// unknown receipts are excluded instead of being silently treated as success or
    /// failure.
    #[must_use]
    pub fn clean_pass_rate(&self) -> Option<f32> {
        let known = self.reuse_wins.saturating_add(self.reuse_failures);
        if known == 0 {
            None
        } else {
            Some((self.reuse_wins as f32 / known as f32).min(1.0))
        }
    }
}

/// Terminal result of one exact sent prior. The journal accepts the first result
/// only; later duplicate or contradictory settlements are ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecipeOutcome {
    /// The delivery passed UmaDev's objective terminal floor.
    Pass,
    /// The delivery settled with blocking evidence or a base/runtime failure.
    Fail,
    /// The result cannot be established (crash, superseded run, or abandoned plan).
    Unknown,
}

/// Read-only candidate ready to splice into a plan prompt. Its fields are private:
/// callers can render the block, but cannot manufacture a different recipe identity.
#[derive(Debug, Clone)]
pub struct PreparedRecipePrior {
    block: String,
    recipe_key: String,
    scope: String,
    nonce: String,
}

impl PreparedRecipePrior {
    /// The bounded prior block that must be inserted verbatim into the final planning
    /// directive before the prepared receipt may be committed as sent.
    #[must_use]
    pub fn block(&self) -> &str {
        &self.block
    }
}

/// Opaque proof that one exact prepared prior reached a base transport write seam.
/// Private fields prevent callers from forging a receipt for another recipe/project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecipeReceipt {
    receipt_id: String,
    recipe_key: String,
    scope: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
enum JournalEvent {
    Sent {
        receipt_id: String,
        recipe_key: String,
        scope: String,
        sent_at_ms: u64,
    },
    Settled {
        receipt_id: String,
        outcome: RecipeOutcome,
        settled_at_ms: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ActiveReceipt {
    receipt: RecipeReceipt,
    plan_digest: String,
}

/// One durable success recipe — a proven plan shape for a task fingerprint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recipe {
    /// The task fingerprint this recipe is keyed under.
    pub fingerprint: Fingerprint,
    /// The ordered step titles/seats that WORKED — `seat · title`, in execution
    /// order (bounded by the internal skeleton-step limit).
    pub plan_skeleton: Vec<String>,
    /// Notable files/dirs the clean build created (bounded by the internal scaffold limit).
    #[serde(default)]
    pub key_scaffold: Vec<String>,
    /// Short pattern notes (e.g. "used repository pattern", "vitest + msw for API
    /// tests"), bounded by the internal pattern limit.
    #[serde(default)]
    pub patterns: Vec<String>,
    /// Reuse + clean-pass statistics.
    #[serde(default)]
    pub stats: OutcomeStats,
}

// ── Fingerprint construction ────────────────────────────────────────────────────

/// A small English stopword set stripped from feature-shape tokens so the shape
/// keeps only distinguishing words. Deliberately tiny — generic scaffolding verbs
/// and product nouns that add no signal.
const SHAPE_STOPWORDS: &[&str] = &[
    "the",
    "and",
    "for",
    "with",
    "using",
    "use",
    "add",
    "make",
    "build",
    "create",
    "app",
    "application",
    "system",
    "page",
    "please",
    "that",
    "this",
    "into",
    "from",
    "new",
    "a",
    "an",
    "of",
    "to",
    "in",
    "on",
    "it",
    "is",
];

/// Distil a rough, order-insensitive feature-shape token set from a requirement:
/// split on non-alphanumeric boundaries, expand CJK runs into bigrams, lowercase,
/// drop pure numbers + stopwords + very short tokens, then de-duplicate, sort, and
/// bound. Deterministic and pure.
#[must_use]
pub fn shape_tokens(requirement: &str) -> Vec<String> {
    let mut toks: Vec<String> = requirement
        .split(|c: char| !(c.is_alphanumeric()))
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .flat_map(shape_piece_tokens)
        .map(|token| excerpt(&token.to_lowercase(), MAX_TOKEN_CHARS))
        .filter(|t| t.chars().count() >= 2)
        .filter(|t| !t.chars().all(|c| c.is_ascii_digit()))
        .filter(|t| !SHAPE_STOPWORDS.contains(&t.as_str()))
        .collect();
    toks.sort();
    toks.dedup();
    toks.truncate(MAX_SHAPE_TOKENS);
    toks
}

fn shape_piece_tokens(piece: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut run = String::new();
    let mut run_is_cjk = None;
    for ch in piece.chars() {
        let cjk = is_cjk(ch);
        if run_is_cjk.is_some_and(|current| current != cjk) {
            push_shape_run(&mut out, &run, run_is_cjk == Some(true));
            run.clear();
        }
        run_is_cjk = Some(cjk);
        run.push(ch);
    }
    push_shape_run(&mut out, &run, run_is_cjk == Some(true));
    out
}

fn push_shape_run(out: &mut Vec<String>, run: &str, cjk: bool) {
    if !cjk {
        if !run.is_empty() {
            out.push(run.to_string());
        }
        return;
    }
    let chars: Vec<char> = run.chars().collect();
    out.extend(chars.windows(2).map(|pair| pair.iter().collect::<String>()));
}

fn is_cjk(ch: char) -> bool {
    matches!(
        u32::from(ch),
        0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xF900..=0xFAFF
            | 0x20000..=0x2FA1F
    )
}

/// Build the [`Fingerprint`] for a run: the detected stack, the routed kind, and the
/// requirement's rough shape. Fail-open (stack detection degrades to `none`).
#[must_use]
pub fn fingerprint_for(root: &Path, route: &RoutePlan, requirement: &str) -> Fingerprint {
    Fingerprint {
        stack: crate::verify::detect_project(root).as_str().to_string(),
        kind: route.kind.id().to_string(),
        // Redact before tokenisation so a requirement containing a credential can
        // never be fragmented into apparently-benign tokens and persisted.
        shape: shape_tokens(&redact_text(requirement)),
    }
}

// ── Similarity / matching ───────────────────────────────────────────────────────

/// Jaccard overlap of two sorted-unique token sets, `|A∩B| / |A∪B|` in `0.0..=1.0`.
/// Two empty sets overlap `0.0` (no shared signal, not a spurious match).
fn jaccard(a: &[String], b: &[String]) -> f32 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let inter = a.iter().filter(|t| b.contains(t)).count();
    let union = a.len() + b.len() - inter;
    if union == 0 {
        0.0
    } else {
        inter as f32 / union as f32
    }
}

/// Fingerprint similarity in `0.0..=1.0`. This score is used only after
/// `eligible_match` proves that stack and kind agree and that the two requirements
/// share at least one shape token.
#[must_use]
pub fn similarity(a: &Fingerprint, b: &Fingerprint) -> f32 {
    let stack = if a.stack == b.stack { 0.45 } else { 0.0 };
    let kind = if a.kind == b.kind { 0.35 } else { 0.0 };
    let shape = jaccard(&a.shape, &b.shape) * 0.20;
    stack + kind + shape
}

fn eligible_match(recipe: &Recipe, query: &Fingerprint) -> bool {
    let fp = &recipe.fingerprint;
    if fp.stack != query.stack
        || fp.kind != query.kind
        || fp.shape.is_empty()
        || query.shape.is_empty()
        || jaccard(&fp.shape, &query.shape) == 0.0
    {
        return false;
    }
    let known = recipe
        .stats
        .reuse_wins
        .saturating_add(recipe.stats.reuse_failures);
    known < 2 || recipe.stats.reuse_wins >= recipe.stats.reuse_failures
}

/// Index of the best-matching recipe for `fp` in `store` whose similarity clears
/// [`MIN_RECALL_SIMILARITY`], or `None`. Ties are broken by MORE clean builds
/// (higher evidence), then by earlier index (stable) — so the match is deterministic.
/// The SINGLE match function shared by recall (what to inject) and capture (which
/// recipe a clean build is a reuse-win for), so the two seams stay consistent.
fn best_match(store: &[Recipe], fp: &Fingerprint) -> Option<usize> {
    let mut best: Option<(usize, f32)> = None;
    for (i, r) in store.iter().enumerate() {
        if !eligible_match(r, fp) {
            continue;
        }
        let s = similarity(&r.fingerprint, fp);
        if s < MIN_RECALL_SIMILARITY {
            continue;
        }
        let better = match best {
            None => true,
            Some((bi, bs)) => {
                s > bs || (score_eq(s, bs) && recipe_rank(&store[i]) > recipe_rank(&store[bi]))
            }
        };
        if better {
            best = Some((i, s));
        }
    }
    best.map(|(i, _)| i)
}

fn recipe_rank(recipe: &Recipe) -> (i64, u32, u32) {
    (
        i64::from(recipe.stats.reuse_wins) - i64::from(recipe.stats.reuse_failures),
        recipe.stats.reuse_wins,
        recipe.stats.clean_builds,
    )
}

/// Float equality within a tiny epsilon (score ties). Free function to keep
/// [`best_match`] readable.
fn score_eq(a: f32, b: f32) -> bool {
    (a - b).abs() < f32::EPSILON
}

// ── Store I/O ────────────────────────────────────────────────────────────────────

fn metadata_is_real_dir(meta: &std::fs::Metadata) -> bool {
    if !meta.file_type().is_dir() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return false;
        }
    }
    true
}

fn metadata_is_real_file(meta: &std::fs::Metadata) -> bool {
    if !meta.file_type().is_file() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return false;
        }
    }
    true
}

fn real_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| metadata_is_real_dir(&m))
}

fn ensure_real_child_dir(parent: &Path, child: &Path, create: bool) -> bool {
    if !real_dir(parent) {
        return false;
    }
    match std::fs::symlink_metadata(child) {
        Ok(meta) => metadata_is_real_dir(&meta),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && create => {
            std::fs::create_dir(child).is_ok() && real_dir(parent) && real_dir(child)
        }
        Err(_) => false,
    }
}

fn hash_bytes(parts: &[&[u8]]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update((part.len() as u64).to_le_bytes());
        hash.update(part);
    }
    format!("{:x}", hash.finalize())
}

fn canonical_scope(root: &Path) -> Option<(PathBuf, String)> {
    let root = std::fs::canonicalize(root).ok()?;
    if !real_dir(&root) {
        return None;
    }
    let scope = hash_bytes(&[root.to_string_lossy().as_bytes()]);
    Some((root, scope))
}

/// Resolve/create the rich recipe store for one project. Every UmaDev-owned path
/// component is a real directory (never a symlink/reparse point). When an operator
/// supplies [`RECIPES_DIR_ENV`], the canonical project hash remains an obligatory
/// child, preserving cross-project isolation.
#[must_use]
pub fn project_recipes_dir(root: &Path) -> Option<PathBuf> {
    #[cfg(not(test))]
    quarantine_legacy_global_store();
    let override_parent = std::env::var(RECIPES_DIR_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from);
    project_recipes_dir_with_override(root, override_parent.as_deref())
}

fn project_recipes_dir_with_override(
    root: &Path,
    override_parent: Option<&Path>,
) -> Option<PathBuf> {
    let (root, scope) = canonical_scope(root)?;
    if let Some(requested) = override_parent {
        let base = match std::fs::canonicalize(requested) {
            Ok(base) if real_dir(&base) => base,
            Err(_) => {
                let parent = std::fs::canonicalize(requested.parent()?).ok()?;
                if !real_dir(&parent) || std::fs::create_dir(requested).is_err() {
                    return None;
                }
                std::fs::canonicalize(requested).ok()?
            }
            _ => return None,
        };
        let projects = base.join("projects");
        if !ensure_real_child_dir(&base, &projects, true) {
            return None;
        }
        let project = projects.join(scope);
        if !ensure_real_child_dir(&projects, &project, true) {
            return None;
        }
        let recipes = project.join("recipes");
        return ensure_real_child_dir(&project, &recipes, true).then_some(recipes);
    }

    let umadev = root.join(".umadev");
    if !ensure_real_child_dir(&root, &umadev, true) {
        return None;
    }
    let memory = umadev.join("memory");
    if !ensure_real_child_dir(&umadev, &memory, true) {
        return None;
    }
    let recipes = memory.join("recipes");
    ensure_real_child_dir(&memory, &recipes, true).then_some(recipes)
}

/// Backward-compatible convenience for callers operating in the current workspace.
/// Unlike the historical implementation, this never resolves to a home-global rich
/// store.
#[must_use]
pub fn recipes_dir() -> Option<PathBuf> {
    let root = std::env::current_dir().ok()?;
    project_recipes_dir(&root)
}

/// Cross-platform home directory: `HOME` then `USERPROFILE` (Windows).
#[cfg(not(test))]
fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()
        .map(PathBuf::from)
}

/// Isolate the historical global rich store. It is deliberately not migrated into
/// any project because requirement shapes/titles/paths cannot be attributed safely.
/// A failed quarantine is still safe: no production read path references it.
#[cfg(not(test))]
fn quarantine_legacy_global_store() {
    let Some(home) = home_dir().and_then(|p| std::fs::canonicalize(p).ok()) else {
        return;
    };
    quarantine_legacy_global_store_in(&home);
}

fn quarantine_legacy_global_store_in(home: &Path) {
    if !real_dir(home) {
        return;
    }
    let umadev = home.join(".umadev");
    let old_dir = umadev.join("recipes");
    if !real_dir(&umadev) || !real_dir(&old_dir) {
        return;
    }
    let old = old_dir.join(RECIPES_FILE);
    if !std::fs::symlink_metadata(&old).is_ok_and(|m| metadata_is_real_file(&m)) {
        return;
    }
    let quarantine = old_dir.join("recipes.legacy-private-quarantined.jsonl");
    if std::fs::symlink_metadata(&quarantine).is_err() {
        let _ = std::fs::rename(old, quarantine);
    }
}

fn store_path(dir: &Path) -> PathBuf {
    dir.join(RECIPES_FILE)
}

fn journal_path(dir: &Path) -> PathBuf {
    dir.join(RECIPE_OUTCOMES_FILE)
}

fn active_path(dir: &Path) -> PathBuf {
    dir.join(ACTIVE_RECEIPT_FILE)
}

fn open_no_follow(path: &Path, read: bool, append: bool, create: bool) -> Option<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(read).append(append).create(create);
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
    let file = options.open(path).ok()?;
    file.metadata()
        .is_ok_and(|m| metadata_is_real_file(&m))
        .then_some(file)
}

#[cfg(windows)]
fn atomic_backup_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("recipes");
    path.with_file_name(format!(".{name}.replace.bak"))
}

fn read_bounded(path: &Path, max_bytes: u64) -> Option<String> {
    let selected = match std::fs::symlink_metadata(path) {
        Ok(meta) if metadata_is_real_file(&meta) => path.to_path_buf(),
        #[cfg(windows)]
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let backup = atomic_backup_path(path);
            std::fs::symlink_metadata(&backup)
                .is_ok_and(|meta| metadata_is_real_file(&meta))
                .then_some(backup)?
        }
        _ => return None,
    };
    let bytes = umadev_state::fs::read_bounded(&selected, max_bytes).ok()?;
    String::from_utf8(bytes).ok()
}

fn safe_text(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty()
        || value.to_ascii_lowercase().contains("[redacted")
        || redact_text(value) != value
    {
        return false;
    }
    let wrapped = serde_json::json!({ "candidate": value });
    redact_json(wrapped.clone()) == wrapped
}

fn safe_slug(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_TOKEN_CHARS
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

fn recipe_is_safe(recipe: &Recipe) -> bool {
    safe_slug(&recipe.fingerprint.stack)
        && safe_slug(&recipe.fingerprint.kind)
        && recipe.fingerprint.shape.iter().all(|v| safe_text(v))
        && recipe.plan_skeleton.iter().all(|v| safe_text(v))
        && recipe.key_scaffold.iter().all(|v| safe_text(v))
        && recipe.patterns.iter().all(|v| safe_text(v))
}

fn recipe_key(recipe: &Recipe) -> String {
    let content = serde_json::to_vec(&(
        &recipe.fingerprint,
        &recipe.plan_skeleton,
        &recipe.key_scaffold,
        &recipe.patterns,
    ))
    .unwrap_or_default();
    hash_bytes(&[b"umadev-recipe-v3", &content])
}

fn same_recipe_content(a: &Recipe, b: &Recipe) -> bool {
    a.fingerprint == b.fingerprint
        && a.plan_skeleton == b.plan_skeleton
        && a.key_scaffold == b.key_scaffold
        && a.patterns == b.patterns
}

fn valid_hash(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn load_snapshot(dir: &Path) -> Vec<Recipe> {
    if !real_dir(dir) {
        return Vec::new();
    }
    let Some(text) = read_bounded(&store_path(dir), MAX_STORE_BYTES) else {
        return Vec::new();
    };
    let parsed = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<Recipe>(line).ok())
        .map(normalize)
        .filter(recipe_is_safe)
        .filter(|recipe| !recipe.plan_skeleton.is_empty())
        .map(|mut recipe| {
            // Historical counters were based on retrieval and best-match-at-capture,
            // so they are intentionally quarantined. V2 derives reuse statistics
            // exclusively from immutable sent/outcome journal records.
            recipe.stats.times_reused = 0;
            recipe.stats.reuse_wins = 0;
            recipe.stats.reuse_failures = 0;
            recipe.stats.reuse_unknown = 0;
            recipe.stats.pending_reuses = 0;
            recipe
        })
        .collect();
    cap_store(parsed)
}

fn load_journal(dir: &Path) -> Vec<JournalEvent> {
    let Some(text) = read_bounded(&journal_path(dir), MAX_JOURNAL_BYTES) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str::<JournalEvent>(line).ok())
        .filter(|event| match event {
            JournalEvent::Sent {
                receipt_id,
                recipe_key,
                scope,
                ..
            } => valid_hash(receipt_id) && valid_hash(recipe_key) && valid_hash(scope),
            JournalEvent::Settled { receipt_id, .. } => valid_hash(receipt_id),
        })
        .take(MAX_JOURNAL_EVENTS)
        .collect()
}

#[derive(Default)]
struct JournalState {
    sent: std::collections::HashMap<String, (String, String)>,
    outcomes: std::collections::HashMap<String, RecipeOutcome>,
}

fn fold_journal(events: &[JournalEvent]) -> JournalState {
    let mut state = JournalState::default();
    for event in events {
        match event {
            JournalEvent::Sent {
                receipt_id,
                recipe_key,
                scope,
                ..
            } => {
                state
                    .sent
                    .entry(receipt_id.clone())
                    .or_insert_with(|| (recipe_key.clone(), scope.clone()));
            }
            JournalEvent::Settled {
                receipt_id,
                outcome,
                ..
            } if state.sent.contains_key(receipt_id) => {
                state.outcomes.entry(receipt_id.clone()).or_insert(*outcome);
            }
            JournalEvent::Settled { .. } => {
                // Out-of-order/forged settlement: no earlier sent proof, so ignore.
            }
        }
    }
    state
}

/// Load all safe recipes from `dir`, oldest first, with reuse statistics rebuilt
/// from exact sent/outcome records. Missing, corrupt, oversized, or linked files are
/// a fail-open empty store.
#[must_use]
pub fn load_recipes(dir: &Path) -> Vec<Recipe> {
    let mut recipes = load_snapshot(dir);
    let journal = fold_journal(&load_journal(dir));
    let mut by_key = std::collections::HashMap::<String, usize>::new();
    for (index, recipe) in recipes.iter().enumerate() {
        by_key.insert(recipe_key(recipe), index);
    }
    for (receipt_id, (key, _scope)) in &journal.sent {
        let Some(index) = by_key.get(key).copied() else {
            continue;
        };
        let counters = &mut recipes[index].stats;
        counters.times_reused = counters.times_reused.saturating_add(1);
        match journal.outcomes.get(receipt_id) {
            Some(RecipeOutcome::Pass) => {
                counters.reuse_wins = counters.reuse_wins.saturating_add(1);
            }
            Some(RecipeOutcome::Fail) => {
                counters.reuse_failures = counters.reuse_failures.saturating_add(1);
            }
            Some(RecipeOutcome::Unknown) => {
                counters.reuse_unknown = counters.reuse_unknown.saturating_add(1);
            }
            None => counters.pending_reuses = counters.pending_reuses.saturating_add(1),
        }
    }
    recipes
}

/// Trim + field-truncate + bound a recipe to the per-recipe caps.
fn normalize(mut r: Recipe) -> Recipe {
    r.fingerprint.stack = excerpt(r.fingerprint.stack.trim(), MAX_TOKEN_CHARS);
    r.fingerprint.kind = excerpt(r.fingerprint.kind.trim(), MAX_TOKEN_CHARS);
    r.fingerprint.shape = bound_lines(r.fingerprint.shape, MAX_SHAPE_TOKENS, MAX_TOKEN_CHARS);
    // Re-normalise the shape set so a hand-edited / older line still compares by value.
    r.fingerprint.shape.sort();
    r.fingerprint.shape.dedup();
    r.plan_skeleton = bound_lines(r.plan_skeleton, MAX_SKELETON_STEPS, MAX_LINE_CHARS);
    r.key_scaffold = bound_lines(r.key_scaffold, MAX_SCAFFOLD, MAX_LINE_CHARS);
    r.patterns = bound_lines(r.patterns, MAX_PATTERNS, MAX_LINE_CHARS);
    r
}

/// Trim, drop-empty, truncate each entry, de-duplicate (order-preserving), and cap
/// the length of a string list.
fn bound_lines(items: Vec<String>, max_items: usize, max_chars: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for it in items {
        let t = excerpt(it.trim(), max_chars);
        if t.is_empty() || out.contains(&t) {
            continue;
        }
        out.push(t);
        if out.len() >= max_items {
            break;
        }
    }
    out
}

/// Cap the store at [`MAX_RECIPES`], evicting the LOWEST-evidence recipes first
/// (fewest clean builds; ties broken by earliest position). Deterministic.
fn cap_store(mut store: Vec<Recipe>) -> Vec<Recipe> {
    while store.len() > MAX_RECIPES {
        // Index of the lowest-evidence recipe (min clean_builds, earliest on tie).
        let mut victim = 0usize;
        for i in 1..store.len() {
            if store[i].stats.clean_builds < store[victim].stats.clean_builds {
                victim = i;
            }
        }
        store.remove(victim);
    }
    store
}

/// Serialise the store to JSONL (one recipe per line). A recipe that fails to
/// serialise is skipped (fail-open).
fn render_jsonl(store: &[Recipe]) -> String {
    let mut buf = String::new();
    for r in store {
        let mut durable = r.clone();
        // Reuse counters are a materialized view of the journal, not mutable
        // snapshot state. Persisting them here would double-count on the next load.
        durable.stats.times_reused = 0;
        durable.stats.reuse_wins = 0;
        durable.stats.reuse_failures = 0;
        durable.stats.reuse_unknown = 0;
        durable.stats.pending_reuses = 0;
        if let Ok(line) = serde_json::to_string(&durable) {
            buf.push_str(&line);
            buf.push('\n');
        }
    }
    buf
}

/// Commit recipe state through the shared no-follow, crash-safe state writer.
fn write_atomic(path: &Path, body: &str) -> std::io::Result<()> {
    // Recover backups created by pre-1.0.56 Windows builds before switching to
    // the single shared persistence implementation.
    #[cfg(windows)]
    recover_atomic_backup(path)?;
    umadev_state::fs::atomic_write(path, body.as_bytes())
}

#[cfg(windows)]
fn recover_atomic_backup(path: &Path) -> std::io::Result<()> {
    let backup = atomic_backup_path(path);
    let backup_meta = match std::fs::symlink_metadata(&backup) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !metadata_is_real_file(&backup_meta) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "recipe backup is not a real file",
        ));
    }
    match std::fs::symlink_metadata(path) {
        Ok(meta) if metadata_is_real_file(&meta) => std::fs::remove_file(backup),
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "recipe target is not a real file",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => std::fs::rename(backup, path),
        Err(error) => Err(error),
    }
}

/// Serialize same-process read-modify-write sequences; the directory lock covers
/// other processes. Poison recovery keeps this fail-open path usable.
fn store_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn unique_nonce(tag: &str) -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    hash_bytes(&[
        b"umadev-recipe-nonce-v2",
        tag.as_bytes(),
        &std::process::id().to_le_bytes(),
        &now_ms().to_le_bytes(),
        &counter.to_le_bytes(),
    ])
}

#[derive(Serialize, Deserialize)]
struct LockOwner {
    created_at_ms: u64,
    nonce: String,
}

struct CrossProcessStoreLock {
    path: PathBuf,
    nonce: String,
}

impl Drop for CrossProcessStoreLock {
    fn drop(&mut self) {
        let Some(text) = read_bounded(&self.path.join(LOCK_OWNER_FILE), 4_096) else {
            return;
        };
        let Ok(owner) = serde_json::from_str::<LockOwner>(&text) else {
            return;
        };
        if owner.nonce != self.nonce {
            return;
        }
        let _ = umadev_state::fs::remove_regular_file(&self.path.join(LOCK_OWNER_FILE));
        let _ = umadev_state::fs::remove_empty_dir(&self.path);
    }
}

fn stale_lock(lock: &Path) -> bool {
    if !real_dir(lock) {
        return false;
    }
    let Some(text) = read_bounded(&lock.join(LOCK_OWNER_FILE), 4_096) else {
        return false;
    };
    let Ok(owner) = serde_json::from_str::<LockOwner>(&text) else {
        return false;
    };
    now_ms().saturating_sub(owner.created_at_ms) > STALE_LOCK_MS
}

fn reclaim_stale_lock(lock: &Path) {
    if !stale_lock(lock) {
        return;
    }
    let Some(parent) = lock.parent() else {
        return;
    };
    let tomb = parent.join(format!(".recipes.lock.stale.{}", unique_nonce("stale")));
    if std::fs::rename(lock, &tomb).is_ok() {
        let _ = umadev_state::fs::remove_regular_file(&tomb.join(LOCK_OWNER_FILE));
        // Refuse recursive deletion: an unexpected extra entry is safer isolated
        // under the unique tomb name than followed/deleted.
        let _ = umadev_state::fs::remove_empty_dir(&tomb);
    }
}

fn acquire_cross_process_lock(dir: &Path) -> Option<CrossProcessStoreLock> {
    if !real_dir(dir) {
        return None;
    }
    let lock = dir.join(STORE_LOCK_DIR);
    for _ in 0..LOCK_WAIT_ATTEMPTS {
        match umadev_state::fs::create_dir(&lock) {
            Ok(()) => {
                let owner = LockOwner {
                    created_at_ms: now_ms(),
                    nonce: unique_nonce("lock-owner"),
                };
                let body = serde_json::to_string(&owner).ok()?;
                if write_atomic(&lock.join(LOCK_OWNER_FILE), &body).is_err() {
                    let _ = umadev_state::fs::remove_empty_dir(&lock);
                    return None;
                }
                return Some(CrossProcessStoreLock {
                    path: lock,
                    nonce: owner.nonce,
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                reclaim_stale_lock(&lock);
                std::thread::sleep(LOCK_WAIT);
            }
            Err(_) => return None,
        }
    }
    None
}

/// Persist a store to `dir` atomically, capping it first. Returns false on every
/// write/size/path failure so callers never emit a false learned-success event.
fn persist(dir: &Path, store: Vec<Recipe>) -> bool {
    let store = cap_store(store);
    let body = render_jsonl(&store);
    u64::try_from(body.len()).is_ok_and(|len| len <= MAX_STORE_BYTES)
        && write_atomic(&store_path(dir), &body).is_ok()
}

// ── Capture (write path) ─────────────────────────────────────────────────────────

/// Capture `recipe` into the store at `dir` — the core, dir-explicit write path.
///
/// Under the process + cross-process store locks, an exact content match increments
/// `clean_builds`; an adapted plan is stored as a distinct version. Keeping versions
/// distinct prevents an outcome for one sent prior from being attached to different
/// skeleton/pattern content that happened to share a fingerprint.
///
/// Returns `true` when a recipe was written/updated. Fail-open: an empty skeleton is a
/// no-op (`false`); a secret/path/link/lock/write error returns `false` but never
/// blocks delivery. Capture itself never credits reuse: only an exact sent receipt's
/// terminal settlement can do that.
pub fn capture_recipe(dir: &Path, recipe: Recipe) -> bool {
    if !capture_enabled(
        recipe_policy_root(dir),
        MemoryScope::Project,
        MemoryStore::Recipes,
    ) {
        return false;
    }
    let recipe = normalize(recipe);
    if recipe.plan_skeleton.is_empty() || !recipe_is_safe(&recipe) {
        return false; // nothing proven → nothing to store
    }
    let _guard = store_lock();
    let Some(_cross_process) = acquire_cross_process_lock(dir) else {
        return false;
    };
    let mut store = load_snapshot(dir);

    // Merge only an exact recipe version, or insert the adapted version separately.
    if let Some(existing) = store
        .iter_mut()
        .find(|existing| same_recipe_content(existing, &recipe))
    {
        existing.stats.clean_builds = existing.stats.clean_builds.saturating_add(1);
    } else {
        let mut fresh = recipe;
        // Never trust caller-supplied evidence counters. This successful durable
        // capture is exactly one observed clean build.
        fresh.stats = OutcomeStats {
            clean_builds: 1,
            ..OutcomeStats::default()
        };
        store.push(fresh);
    }

    persist(dir, store)
}

// ── Recall (read path) ───────────────────────────────────────────────────────────

/// Read-only lookup of the closest safe recipe. Merely retrieving a candidate is
/// not use and never changes statistics.
#[must_use]
pub fn recall_best(dir: &Path, fp: &Fingerprint) -> Option<Recipe> {
    if !recall_enabled(
        recipe_policy_root(dir),
        MemoryScope::Project,
        MemoryStore::Recipes,
    ) {
        return None;
    }
    let store = load_recipes(dir);
    let i = best_match(&store, fp)?;
    Some(store[i].clone())
}

/// The plan-time RECALL prior block for `fp`, ready to splice into the plan-synthesis
/// prompt — or `None` when no recipe matches (recall is then a no-op and the plan is
/// synthesised exactly as before). This compatibility helper is pure/read-only;
/// tracked production planning uses [`prepare_recipe_prior`].
///
/// The block frames the recipe as an ADAPTABLE PRIOR, never a template, and is bounded
/// by `budget_chars` (typically [`RECIPE_PRIOR_BUDGET`]).
#[must_use]
pub fn recall_prior_block(dir: &Path, fp: &Fingerprint, budget_chars: usize) -> Option<String> {
    let recipe = recall_best(dir, fp)?;
    Some(recipe_prior_block(&recipe, budget_chars))
}

fn directory_scope(dir: &Path) -> Option<String> {
    let canonical = std::fs::canonicalize(dir).ok()?;
    real_dir(&canonical).then(|| hash_bytes(&[canonical.to_string_lossy().as_bytes()]))
}

/// Resolve the project policy boundary for the normal
/// `<root>/.umadev/memory/recipes` store. Dir-explicit compatibility/test stores
/// use their own real directory as the boundary. Production call sites that use
/// the optional isolated-store override additionally gate with their known
/// project root before reaching these helpers.
fn recipe_policy_root(dir: &Path) -> &Path {
    let Some(memory_dir) = dir.parent() else {
        return dir;
    };
    let Some(umadev_dir) = memory_dir.parent() else {
        return dir;
    };
    if dir.file_name().and_then(|name| name.to_str()) == Some("recipes")
        && memory_dir.file_name().and_then(|name| name.to_str()) == Some("memory")
        && umadev_dir.file_name().and_then(|name| name.to_str()) == Some(".umadev")
    {
        umadev_dir.parent().unwrap_or(dir)
    } else {
        dir
    }
}

fn journal_event_count_unlocked(dir: &Path) -> Option<usize> {
    let path = journal_path(dir);
    match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(0),
        Ok(meta) if metadata_is_real_file(&meta) => read_bounded(&path, MAX_JOURNAL_BYTES)
            .map(|text| text.lines().filter(|line| !line.trim().is_empty()).count()),
        _ => None,
    }
}

fn append_journal_event_unlocked(dir: &Path, event: &JournalEvent) -> bool {
    if !real_dir(dir)
        || journal_event_count_unlocked(dir).is_none_or(|count| count >= MAX_JOURNAL_EVENTS)
    {
        return false;
    }
    let Ok(mut line) = serde_json::to_string(event) else {
        return false;
    };
    line.push('\n');
    let path = journal_path(dir);
    if let Ok(meta) = std::fs::symlink_metadata(&path) {
        if !metadata_is_real_file(&meta)
            || meta
                .len()
                .saturating_add(u64::try_from(line.len()).unwrap_or(u64::MAX))
                > MAX_JOURNAL_BYTES
        {
            return false;
        }
    }
    let Some(mut file) = open_no_follow(&path, false, true, true) else {
        return false;
    };
    file.write_all(line.as_bytes()).is_ok() && file.sync_data().is_ok()
}

fn remove_active_unlocked(dir: &Path, receipt_id: Option<&str>) {
    let path = active_path(dir);
    if let Some(expected) = receipt_id {
        let Some(text) = read_bounded(&path, 16_384) else {
            return;
        };
        let Ok(active) = serde_json::from_str::<ActiveReceipt>(&text) else {
            return;
        };
        if active.receipt.receipt_id != expected {
            return;
        }
    }
    if std::fs::symlink_metadata(&path).is_ok_and(|m| metadata_is_real_file(&m)) {
        let _ = std::fs::remove_file(path);
    }
}

/// Before a genuinely new plan starts, any prior sent receipt left by a superseded
/// or crashed run becomes UNKNOWN exactly once. A gate-resume does not call this
/// function; it reloads the active marker instead.
fn settle_superseded_pending_unlocked(dir: &Path) {
    let state = fold_journal(&load_journal(dir));
    for receipt_id in state.sent.keys() {
        if state.outcomes.contains_key(receipt_id) {
            continue;
        }
        let _ = append_journal_event_unlocked(
            dir,
            &JournalEvent::Settled {
                receipt_id: receipt_id.clone(),
                outcome: RecipeOutcome::Unknown,
                settled_at_ms: now_ms(),
            },
        );
    }
    remove_active_unlocked(dir, None);
}

/// Prepare one read-only recipe prior. This does not increment any counter. It also
/// closes pending receipts from a superseded/crashed older run as UNKNOWN; a normal
/// cross-session resume bypasses preparation and retains its active receipt.
#[must_use]
pub fn prepare_recipe_prior(
    dir: &Path,
    fp: &Fingerprint,
    budget_chars: usize,
) -> Option<PreparedRecipePrior> {
    let _guard = store_lock();
    let _cross_process = acquire_cross_process_lock(dir)?;
    // Receipt hygiene is lifecycle bookkeeping, not recall. Close any abandoned
    // prior before consulting the prompt policy so disabling recall cannot strand
    // an already-sent receipt in `pending` forever.
    settle_superseded_pending_unlocked(dir);
    if !recall_enabled(
        recipe_policy_root(dir),
        MemoryScope::Project,
        MemoryStore::Recipes,
    ) {
        return None;
    }
    let store = load_recipes(dir);
    let recipe = store.get(best_match(&store, fp)?)?.clone();
    let scope = directory_scope(dir)?;
    Some(PreparedRecipePrior {
        block: recipe_prior_block(&recipe, budget_chars),
        recipe_key: recipe_key(&recipe),
        scope,
        nonce: unique_nonce("prepared"),
    })
}

/// Commit a prepared prior only after the base accepted the complete directive for
/// writing. The immutable sent row is the denominator source; a journal failure
/// returns `None`, so untracked use is never presented as measured use.
#[must_use]
pub fn commit_recipe_prior_sent(
    dir: &Path,
    prepared: PreparedRecipePrior,
    delivered_directive: &str,
) -> Option<RecipeReceipt> {
    let scope = directory_scope(dir)?;
    if scope != prepared.scope
        || prepared.block.trim().is_empty()
        || !delivered_directive.contains(&prepared.block)
    {
        return None;
    }
    let receipt = RecipeReceipt {
        receipt_id: hash_bytes(&[
            b"umadev-recipe-receipt-v3",
            prepared.scope.as_bytes(),
            prepared.recipe_key.as_bytes(),
            prepared.nonce.as_bytes(),
        ]),
        recipe_key: prepared.recipe_key,
        scope: prepared.scope,
    };
    let _guard = store_lock();
    let _cross_process = acquire_cross_process_lock(dir)?;
    let recipes = load_snapshot(dir);
    if !recipes
        .iter()
        .any(|recipe| recipe_key(recipe) == receipt.recipe_key)
    {
        return None;
    }
    let state = fold_journal(&load_journal(dir));
    if state.sent.contains_key(&receipt.receipt_id) {
        return None;
    }
    if journal_event_count_unlocked(dir)? >= MAX_JOURNAL_EVENTS.saturating_sub(1) {
        return None;
    }
    append_journal_event_unlocked(
        dir,
        &JournalEvent::Sent {
            receipt_id: receipt.receipt_id.clone(),
            recipe_key: receipt.recipe_key.clone(),
            scope: receipt.scope.clone(),
            sent_at_ms: now_ms(),
        },
    )
    .then_some(receipt)
}

/// Settle the exact sent receipt once. A forged, cross-project, out-of-order,
/// duplicate, or conflicting outcome returns `false` without changing statistics.
pub fn settle_recipe_receipt(dir: &Path, receipt: &RecipeReceipt, outcome: RecipeOutcome) -> bool {
    if directory_scope(dir).as_deref() != Some(receipt.scope.as_str()) {
        return false;
    }
    let _guard = store_lock();
    let Some(_cross_process) = acquire_cross_process_lock(dir) else {
        return false;
    };
    let state = fold_journal(&load_journal(dir));
    let Some((recipe_key, scope)) = state.sent.get(&receipt.receipt_id) else {
        return false;
    };
    if recipe_key != &receipt.recipe_key
        || scope != &receipt.scope
        || state.outcomes.contains_key(&receipt.receipt_id)
    {
        return false;
    }
    let committed = append_journal_event_unlocked(
        dir,
        &JournalEvent::Settled {
            receipt_id: receipt.receipt_id.clone(),
            outcome,
            settled_at_ms: now_ms(),
        },
    );
    if committed {
        remove_active_unlocked(dir, Some(&receipt.receipt_id));
    }
    committed
}

fn stable_plan_digest(plan: &Plan) -> String {
    let mut stable = plan.clone();
    for step in &mut stable.steps {
        step.status = StepStatus::Pending;
    }
    let bytes = serde_json::to_vec(&stable).unwrap_or_default();
    hash_bytes(&[b"umadev-recipe-plan-v2", &bytes])
}

/// Bind a sent receipt to the owned plan that may cross a process/gate boundary.
/// This is durable but contains hashes only.
pub(crate) fn bind_recipe_receipt_to_plan(
    dir: &Path,
    receipt: &RecipeReceipt,
    plan: &Plan,
) -> bool {
    if directory_scope(dir).as_deref() != Some(receipt.scope.as_str()) {
        return false;
    }
    let _guard = store_lock();
    let Some(_cross_process) = acquire_cross_process_lock(dir) else {
        return false;
    };
    let state = fold_journal(&load_journal(dir));
    if !state.sent.contains_key(&receipt.receipt_id)
        || state.outcomes.contains_key(&receipt.receipt_id)
    {
        return false;
    }
    let marker = ActiveReceipt {
        receipt: receipt.clone(),
        plan_digest: stable_plan_digest(plan),
    };
    serde_json::to_string(&marker)
        .is_ok_and(|body| body.len() <= 16_384 && write_atomic(&active_path(dir), &body).is_ok())
}

/// Reload the pending exact receipt for a matching persisted plan during resume.
#[must_use]
pub(crate) fn active_recipe_receipt_for_plan(dir: &Path, plan: &Plan) -> Option<RecipeReceipt> {
    let text = read_bounded(&active_path(dir), 16_384)?;
    let active: ActiveReceipt = serde_json::from_str(&text).ok()?;
    if active.plan_digest != stable_plan_digest(plan)
        || directory_scope(dir).as_deref() != Some(active.receipt.scope.as_str())
    {
        return None;
    }
    let state = fold_journal(&load_journal(dir));
    let (key, scope) = state.sent.get(&active.receipt.receipt_id)?;
    (key == &active.receipt.recipe_key
        && scope == &active.receipt.scope
        && !state.outcomes.contains_key(&active.receipt.receipt_id))
    .then_some(active.receipt)
}

/// Render a recipe as an adaptable plan-time prior. Pure + deterministic; bounded by
/// `budget_chars`.
#[must_use]
pub fn recipe_prior_block(recipe: &Recipe, budget_chars: usize) -> String {
    let fp = &recipe.fingerprint;
    let rate = recipe
        .stats
        .clean_pass_rate()
        .map(|r| format!(", clean-pass rate {:.0}%", r * 100.0))
        .unwrap_or_default();
    let mut block = format!(
        "## PRIOR — a past CLEAN build of a similar stack/feature (ADAPT if it fits — this is NOT a template)\n\n\
         A previous clean delivery on a [{stack} · {kind}] project \
         ({builds} clean build(s){rate}) used this plan shape. Treat it as a hint from a \
         senior teammate: reuse the parts that fit THIS requirement, drop the parts that \
         don't, and never copy it blindly.\n\nPlan shape that worked:\n",
        stack = fp.stack,
        kind = fp.kind,
        builds = recipe.stats.clean_builds,
    );
    for (n, step) in recipe.plan_skeleton.iter().enumerate() {
        block.push_str(&format!("{}. {step}\n", n + 1));
    }
    if !recipe.key_scaffold.is_empty() {
        block.push_str(&format!(
            "Notable scaffold: {}\n",
            recipe.key_scaffold.join(", ")
        ));
    }
    if !recipe.patterns.is_empty() {
        block.push_str(&format!(
            "Patterns that worked: {}\n",
            recipe.patterns.join("; ")
        ));
    }
    excerpt(&block, budget_chars)
}

// ── Capture orchestration at the delivery seam ───────────────────────────────────

/// Distil + capture a success recipe at a CLEAN deliberate delivery — called at the
/// SAME finalize seam as [`crate::self_evolve::reconcile_at_delivery`], AFTER the run
/// has already settled clean. A pure SIDE EFFECT: it never changes the delivery, the
/// gate, the plan status, or any verdict.
///
/// The mechanical recipe (plan skeleton + evidence-derived scaffold + fingerprint) is
/// built deterministically from the executed plan. ONE read-only fork consult MAY then
/// enrich it with `patterns` + extra scaffold; if the consult fails (offline / no fork
/// / timeout / unparseable) the mechanical recipe is stored as-is. Every path is
/// fail-open: an unavailable safe store, an empty skeleton, or a write error records nothing and
/// never affects the just-finished delivery.
pub(crate) async fn capture_at_delivery(
    session: &mut dyn BaseSession,
    root: &Path,
    route: &RoutePlan,
    plan: &Plan,
    requirement: &str,
    events: &Arc<dyn EventSink>,
) {
    if !capture_enabled(root, MemoryScope::Project, MemoryStore::Recipes) {
        return;
    }
    let Some(dir) = project_recipes_dir(root) else {
        return; // unsafe/unavailable project memory → fail-open no-op
    };
    capture_at_delivery_in(session, &dir, root, route, plan, requirement, events).await;
}

/// Dir-explicit core of [`capture_at_delivery`] — the whole capture logic against a
/// caller-supplied recipes dir, so it is exercised in tests with a temp dir (no
/// process-env mutation, no race). The public wrapper just resolves [`recipes_dir`].
async fn capture_at_delivery_in(
    session: &mut dyn BaseSession,
    dir: &Path,
    root: &Path,
    route: &RoutePlan,
    plan: &Plan,
    requirement: &str,
    events: &Arc<dyn EventSink>,
) {
    // Keep this check in the dir-explicit core as well as the production wrapper:
    // tests and alternate in-crate callers exercise it directly, and capture-off
    // must return before the optional enrichment fork/model call.
    if !capture_enabled(root, MemoryScope::Project, MemoryStore::Recipes) {
        return;
    }
    // Mechanical skeleton from the steps that actually reached Done — the proven order.
    let skeleton = skeleton_from_plan(plan);
    if skeleton.is_empty() {
        return; // no proven build steps → nothing worth remembering
    }
    let fingerprint = fingerprint_for(root, route, requirement);
    let mut scaffold = scaffold_from_plan(plan);
    let mut patterns: Vec<String> = Vec::new();

    // OPTIONAL enrichment: ONE read-only fork consult for short pattern notes + any
    // extra notable scaffold the mechanical scan missed. Fail-open — a `None` reply
    // leaves the mechanical recipe intact (patterns empty, scaffold as scanned).
    if let Some(distilled) = distill_enrichment(session, &skeleton).await {
        patterns = distilled.patterns;
        scaffold = bound_lines(
            [scaffold, distilled.key_scaffold].concat(),
            MAX_SCAFFOLD,
            MAX_LINE_CHARS,
        );
    }

    let recipe = Recipe {
        fingerprint,
        plan_skeleton: skeleton,
        key_scaffold: scaffold,
        patterns,
        stats: OutcomeStats::default(),
    };
    if capture_recipe(dir, recipe) {
        events.emit(EngineEvent::Note(
            "[learned] 交付通过:已把这次干净构建的成功计划形态记入当前项目的私有配方库；下次相似任务会作为可选先验。"
                .to_string(),
        ));
    }
}

/// The proven plan skeleton — every step that reached [`StepStatus::Done`], in plan
/// order, as `seat · title`. A build with no Done step yields an empty skeleton (the
/// caller then skips capture).
fn skeleton_from_plan(plan: &Plan) -> Vec<String> {
    let lines: Vec<String> = plan
        .steps
        .iter()
        .filter(|s| s.status == StepStatus::Done)
        .map(|s| format!("{} · {}", s.seat.role_id(), s.title))
        .collect();
    // A recipe is only worth storing if a real DOER step (not just a review) worked.
    let has_build = plan
        .steps
        .iter()
        .any(|s| s.kind == StepKind::Build && s.status == StepStatus::Done);
    if has_build {
        bound_lines(lines, MAX_SKELETON_STEPS, MAX_LINE_CHARS)
    } else {
        Vec::new()
    }
}

/// The concrete scaffold the clean build produced, scanned MECHANICALLY from the
/// evidence contracts of the Done build steps (`file-exists` / `file-contains` paths).
/// Deterministic; no consult.
fn scaffold_from_plan(plan: &Plan) -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();
    for step in plan
        .steps
        .iter()
        .filter(|s| s.kind == StepKind::Build && s.status == StepStatus::Done)
    {
        for ev in &step.evidence {
            match ev {
                EvidenceContract::FileExists { path }
                | EvidenceContract::FileContains { path, .. } => paths.push(path.clone()),
                _ => {}
            }
        }
    }
    bound_lines(paths, MAX_SCAFFOLD, MAX_LINE_CHARS)
}

/// What the optional enrichment consult returns.
struct Distilled {
    patterns: Vec<String>,
    key_scaffold: Vec<String>,
}

/// Ask the borrowed brain — on ONE read-only fork — to name the reusable PATTERNS the
/// just-finished clean build leaned on plus any extra notable scaffold, as a small
/// JSON object. Fail-open: a missing fork, an offline brain, a timeout, or an
/// unparseable reply yields `None` (the caller keeps the mechanical recipe).
async fn distill_enrichment(
    session: &mut dyn BaseSession,
    skeleton: &[String],
) -> Option<Distilled> {
    let system = "You are wrapping up a CLEAN software build and recording a reusable \
         'success recipe' for next time. Given the plan steps that worked, name the \
         reusable PATTERNS this kind of build should lean on (architecture/testing/data \
         choices — e.g. \"repository pattern for data access\", \"vitest + msw for API \
         tests\") and any NOTABLE scaffold files/dirs a teammate should expect. Keep each \
         entry SHORT and reusable — no project-specific narration, no todo items. \
         JSON shape: {\"patterns\":[\"…\"],\"key_scaffold\":[\"src/…\"]}. Empty arrays are \
         fine if nothing is genuinely reusable.";
    let user = format!("Plan steps that worked:\n{}", skeleton.join("\n"));

    let fork = crate::continuous::fork_with_timeout(session).await;
    let consult = crate::continuous::ForkConsult::new(fork);
    let reply = consult.judge_json("recipe-distill", system, user).await;
    consult.end().await;

    let json = reply?;
    let raw: DistillReply = serde_json::from_str(&json).ok()?;
    Some(Distilled {
        patterns: bound_lines(raw.patterns, MAX_PATTERNS, MAX_LINE_CHARS),
        key_scaffold: bound_lines(raw.key_scaffold, MAX_SCAFFOLD, MAX_LINE_CHARS),
    })
}

/// The strict-JSON shape the enrichment consult returns (both fields optional).
#[derive(Debug, Default, Deserialize)]
struct DistillReply {
    #[serde(default)]
    patterns: Vec<String>,
    #[serde(default)]
    key_scaffold: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::critics::Seat;
    use crate::events::NullSink;
    use crate::plan_state::{AcceptanceSpec, PlanStep};
    use crate::planner::TaskKind;
    use crate::router::{Budget, Depth, RouteClass};
    use std::collections::VecDeque;
    use umadev_runtime::{ApprovalDecision, SessionError, SessionEvent, TurnStatus};

    // ── fixtures ────────────────────────────────────────────────────────────────

    fn sink() -> Arc<dyn EventSink> {
        Arc::new(NullSink)
    }

    fn route(kind: TaskKind) -> RoutePlan {
        RoutePlan {
            class: RouteClass::Build,
            kind,
            depth: Depth::Standard,
            team: vec![Seat::FrontendEngineer],
            scope: Vec::new(),
            needs_clarify: None,
            est_budget: Budget::for_route(RouteClass::Build, Depth::Standard),
            confidence: 0.7,
        }
    }

    fn fp(stack: &str, kind: &str, shape: &[&str]) -> Fingerprint {
        let mut shape: Vec<String> = shape.iter().map(|s| (*s).to_string()).collect();
        shape.sort();
        shape.dedup();
        Fingerprint {
            stack: stack.to_string(),
            kind: kind.to_string(),
            shape,
        }
    }

    fn recipe(stack: &str, kind: &str, shape: &[&str], skeleton: &[&str]) -> Recipe {
        Recipe {
            fingerprint: fp(stack, kind, shape),
            plan_skeleton: skeleton.iter().map(|s| (*s).to_string()).collect(),
            key_scaffold: vec!["src/App.tsx".to_string()],
            patterns: vec!["used repository pattern".to_string()],
            stats: OutcomeStats::default(),
        }
    }

    fn sent_receipt(dir: &Path, query: &Fingerprint) -> RecipeReceipt {
        let prepared = prepare_recipe_prior(dir, query, RECIPE_PRIOR_BUDGET)
            .expect("matching recipe prepares");
        let directive = format!("plan\n{}\nrequirement", prepared.block());
        commit_recipe_prior_sent(dir, prepared, &directive).expect("sent receipt commits")
    }

    fn done_build_step(id: &str, seat: Seat, title: &str, files: &[&str]) -> PlanStep {
        PlanStep {
            files: crate::plan_state::StepFiles::default(),
            id: id.to_string(),
            title: title.to_string(),
            seat,
            kind: StepKind::Build,
            depends_on: Vec::new(),
            acceptance: AcceptanceSpec::SourcePresent,
            evidence: files
                .iter()
                .map(|p| EvidenceContract::FileExists {
                    path: (*p).to_string(),
                })
                .collect(),
            status: StepStatus::Done,
        }
    }

    // A scripted fake base whose read-only fork answers with a fixed reply. Used to
    // drive the distillation consult; `can_fork=false` exercises the fail-open path.
    struct Fork {
        reply: String,
        pending: VecDeque<SessionEvent>,
    }
    #[async_trait::async_trait]
    impl BaseSession for Fork {
        async fn send_turn(&mut self, _d: String) -> Result<(), SessionError> {
            self.pending = [
                SessionEvent::TextDelta(self.reply.clone()),
                SessionEvent::TurnDone {
                    status: TurnStatus::Completed,
                    usage: None,
                },
            ]
            .into_iter()
            .collect();
            Ok(())
        }
        async fn next_event(&mut self) -> Option<SessionEvent> {
            self.pending.pop_front()
        }
        async fn respond(&mut self, _r: &str, _d: ApprovalDecision) -> Result<(), SessionError> {
            Ok(())
        }
        async fn interrupt(&mut self) -> Result<(), SessionError> {
            Ok(())
        }
        async fn end(&mut self) -> Result<(), SessionError> {
            Ok(())
        }
    }
    struct Brain {
        reply: String,
        can_fork: bool,
        forks: usize,
    }
    #[async_trait::async_trait]
    impl BaseSession for Brain {
        async fn fork(&mut self) -> Result<Box<dyn BaseSession>, SessionError> {
            self.forks = self.forks.saturating_add(1);
            if !self.can_fork {
                return Err(SessionError::ForkUnsupported("test".into()));
            }
            Ok(Box::new(Fork {
                reply: self.reply.clone(),
                pending: VecDeque::new(),
            }))
        }
        async fn send_turn(&mut self, _d: String) -> Result<(), SessionError> {
            Ok(())
        }
        async fn next_event(&mut self) -> Option<SessionEvent> {
            None
        }
        async fn respond(&mut self, _r: &str, _d: ApprovalDecision) -> Result<(), SessionError> {
            Ok(())
        }
        async fn interrupt(&mut self) -> Result<(), SessionError> {
            Ok(())
        }
        async fn end(&mut self) -> Result<(), SessionError> {
            Ok(())
        }
    }

    fn clean_plan() -> Plan {
        Plan {
            steps: vec![
                done_build_step(
                    "scaffold",
                    Seat::FrontendEngineer,
                    "Scaffold the app shell",
                    &["src/App.tsx"],
                ),
                done_build_step(
                    "api",
                    Seat::BackendEngineer,
                    "Wire the login API",
                    &["src/api.ts"],
                ),
                PlanStep {
                    status: StepStatus::Done,
                    kind: StepKind::Review,
                    ..done_build_step("qa", Seat::QaEngineer, "Review the build", &[])
                },
            ],
            risks: Vec::new(),
            open_questions: Vec::new(),
        }
    }

    // ── shape / fingerprint ───────────────────────────────────────────────────────

    #[test]
    fn shape_tokens_are_normalised_sorted_and_stopword_filtered() {
        let toks = shape_tokens("Build a Todo LIST app with login");
        // "build"/"a"/"app"/"with" are stopwords; the rest lowercased, sorted, unique.
        assert_eq!(toks, vec!["list", "login", "todo"]);
    }

    #[test]
    fn cjk_shape_tokens_support_overlap_without_broad_route_fallback() {
        let stored = Fingerprint {
            stack: "node".into(),
            kind: "greenfield".into(),
            shape: shape_tokens("构建待办列表并支持登录"),
        };
        let related = Fingerprint {
            stack: "node".into(),
            kind: "greenfield".into(),
            shape: shape_tokens("做一个待办列表登录功能"),
        };
        let unrelated = Fingerprint {
            stack: "node".into(),
            kind: "greenfield".into(),
            shape: shape_tokens("实现支付订单退款"),
        };
        let recipe = Recipe {
            fingerprint: stored,
            plan_skeleton: vec!["frontend-engineer · scaffold".into()],
            key_scaffold: Vec::new(),
            patterns: Vec::new(),
            stats: OutcomeStats::default(),
        };
        assert!(eligible_match(&recipe, &related));
        assert!(!eligible_match(&recipe, &unrelated));
    }

    #[test]
    fn similarity_ranks_stack_then_kind_then_shape() {
        let a = fp("node", "greenfield", &["todo", "login"]);
        let exact = similarity(&a, &fp("node", "greenfield", &["todo", "login"]));
        let stack_only = similarity(&a, &fp("node", "bugfix", &["x"]));
        let kind_only = similarity(&a, &fp("rust", "greenfield", &["x"]));
        let none = similarity(&a, &fp("rust", "bugfix", &["x"]));
        assert!(
            exact > 0.9,
            "exact stack+kind+shape scores highest: {exact}"
        );
        assert!(
            stack_only < MIN_RECALL_SIMILARITY,
            "same stack is insufficient"
        );
        assert!((kind_only - 0.35).abs() < 1e-6, "kind-only ~0.35");
        assert!(none < MIN_RECALL_SIMILARITY, "unrelated is below the floor");
    }

    // ── capture: a clean delivery writes a recipe (fingerprint + skeleton) ──────────

    #[test]
    fn capture_writes_a_recipe_with_fingerprint_and_skeleton() {
        let tmp = tempfile::TempDir::new().unwrap();
        let r = recipe(
            "node",
            "greenfield",
            &["todo"],
            &["frontend-engineer · scaffold"],
        );
        assert!(capture_recipe(tmp.path(), r));
        let store = load_recipes(tmp.path());
        assert_eq!(store.len(), 1);
        assert_eq!(store[0].fingerprint.stack, "node");
        assert_eq!(store[0].plan_skeleton, vec!["frontend-engineer · scaffold"]);
        assert_eq!(
            store[0].stats.clean_builds, 1,
            "first capture = 1 clean build"
        );
        assert!(tmp.path().join(RECIPES_FILE).exists());
    }

    #[test]
    fn recipe_policy_controls_capture_and_recall_without_hiding_the_store() {
        let tmp = tempfile::TempDir::new().unwrap();
        let first = recipe(
            "node",
            "greenfield",
            &["todo"],
            &["frontend-engineer · scaffold"],
        );
        assert!(capture_recipe(tmp.path(), first));

        crate::memory_control::update_capture(
            tmp.path(),
            MemoryScope::Project,
            Some(MemoryStore::Recipes),
            false,
        )
        .unwrap();
        assert!(!capture_recipe(
            tmp.path(),
            recipe(
                "node",
                "greenfield",
                &["payments"],
                &["backend-engineer · payments"]
            )
        ));
        assert_eq!(load_recipes(tmp.path()).len(), 1);

        let query = fp("node", "greenfield", &["todo"]);
        crate::memory_control::update_recall(
            tmp.path(),
            MemoryScope::Project,
            Some(MemoryStore::Recipes),
            false,
        )
        .unwrap();
        assert!(recall_best(tmp.path(), &query).is_none());
        assert!(prepare_recipe_prior(tmp.path(), &query, RECIPE_PRIOR_BUDGET).is_none());
        assert_eq!(
            load_recipes(tmp.path()).len(),
            1,
            "report reads remain visible"
        );

        crate::memory_control::update_recall(
            tmp.path(),
            MemoryScope::Project,
            Some(MemoryStore::Recipes),
            true,
        )
        .unwrap();
        assert!(recall_best(tmp.path(), &query).is_some());

        std::fs::write(
            tmp.path().join(".umadev/memory/policy.toml"),
            "invalid = [toml",
        )
        .unwrap();
        assert!(!capture_recipe(
            tmp.path(),
            recipe(
                "node",
                "greenfield",
                &["chat"],
                &["frontend-engineer · chat"]
            )
        ));
        assert!(recall_best(tmp.path(), &query).is_none());
        assert_eq!(
            load_recipes(tmp.path()).len(),
            1,
            "corruption hides no audit data"
        );
    }

    #[test]
    fn an_existing_recipe_receipt_still_settles_when_recall_is_off() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(capture_recipe(
            tmp.path(),
            recipe(
                "node",
                "greenfield",
                &["todo"],
                &["frontend-engineer · scaffold"]
            )
        ));
        let query = fp("node", "greenfield", &["todo"]);
        let receipt = sent_receipt(tmp.path(), &query);
        crate::memory_control::update_recall(
            tmp.path(),
            MemoryScope::Project,
            Some(MemoryStore::Recipes),
            false,
        )
        .unwrap();

        assert!(settle_recipe_receipt(
            tmp.path(),
            &receipt,
            RecipeOutcome::Pass
        ));
        let store = load_recipes(tmp.path());
        assert_eq!(store[0].stats.reuse_wins, 1);
        assert_eq!(store[0].stats.pending_reuses, 0);
        assert!(prepare_recipe_prior(tmp.path(), &query, RECIPE_PRIOR_BUDGET).is_none());
    }

    #[test]
    fn empty_skeleton_captures_nothing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut r = recipe("node", "greenfield", &["x"], &[]);
        r.plan_skeleton.clear();
        assert!(!capture_recipe(tmp.path(), r));
        assert!(load_recipes(tmp.path()).is_empty());
    }

    // ── exact sent receipt attribution across adapted recipe versions ───────────────

    #[test]
    fn exact_sent_receipt_attributes_only_the_sent_recipe_version() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Run 1: a clean build captures the first recipe.
        assert!(capture_recipe(
            tmp.path(),
            recipe(
                "node",
                "frontend_only",
                &["todo", "list"],
                &["frontend-engineer · scaffold"]
            ),
        ));

        // A compatibility lookup/render is read-only: retrieval is not use.
        let query = fp("node", "frontend_only", &["todo", "tracker"]);
        let block = recall_prior_block(tmp.path(), &query, RECIPE_PRIOR_BUDGET);
        assert!(block.is_some(), "a similar fingerprint recalls the prior");
        let block = block.unwrap();
        assert!(
            block.contains("PRIOR"),
            "the block frames it as a prior: {block}"
        );
        assert!(
            block.contains("frontend-engineer · scaffold"),
            "the block carries the proven skeleton: {block}"
        );
        assert!(
            block.to_lowercase().contains("adapt"),
            "the block invites adaptation, not a template: {block}"
        );
        assert_eq!(
            load_recipes(tmp.path())[0].stats.times_reused,
            0,
            "lookup/render alone must not count as use"
        );

        // Preparing is still read-only. Only the transport-write seam commits the
        // opaque receipt and creates one pending denominator.
        let prepared = prepare_recipe_prior(tmp.path(), &query, RECIPE_PRIOR_BUDGET).unwrap();
        assert_eq!(load_recipes(tmp.path())[0].stats.times_reused, 0);
        let directive = format!("{}\nfinal plan prompt", prepared.block());
        let receipt = commit_recipe_prior_sent(tmp.path(), prepared, &directive).unwrap();
        let pending = load_recipes(tmp.path());
        assert_eq!(pending[0].stats.times_reused, 1);
        assert_eq!(pending[0].stats.pending_reuses, 1);
        assert_eq!(pending[0].stats.reuse_wins, 0);

        // An adapted recipe with the same fingerprint is a separate version. It
        // cannot inherit the old prior's pending receipt or eventual outcome.
        assert!(capture_recipe(
            tmp.path(),
            recipe(
                "node",
                "frontend_only",
                &["list", "todo"],
                &["frontend-engineer · scaffold", "qa-engineer · tests"]
            ),
        ));
        let store = load_recipes(tmp.path());
        assert_eq!(store.len(), 2, "adapted content keeps a distinct identity");
        let original = store
            .iter()
            .find(|recipe| recipe.plan_skeleton.len() == 1)
            .unwrap();
        let adapted = store
            .iter()
            .find(|recipe| recipe.plan_skeleton.len() == 2)
            .unwrap();
        assert_eq!(original.stats.times_reused, 1);
        assert_eq!(original.stats.pending_reuses, 1);
        assert_eq!(adapted.stats.times_reused, 0);
        assert_eq!(adapted.stats.reuse_wins, 0);
        assert!(settle_recipe_receipt(
            tmp.path(),
            &receipt,
            RecipeOutcome::Pass
        ));
        let store = load_recipes(tmp.path());
        let original = store
            .iter()
            .find(|recipe| recipe.plan_skeleton.len() == 1)
            .unwrap();
        let adapted = store
            .iter()
            .find(|recipe| recipe.plan_skeleton.len() == 2)
            .unwrap();
        assert_eq!(original.stats.reuse_wins, 1);
        assert_eq!(original.stats.pending_reuses, 0);
        assert_eq!(original.stats.clean_pass_rate(), Some(1.0));
        assert_eq!(adapted.stats.reuse_wins, 0);
    }

    #[test]
    fn commit_rejects_a_directive_that_did_not_contain_the_prepared_prior() {
        let tmp = tempfile::TempDir::new().unwrap();
        let query = fp("node", "greenfield", &["todo"]);
        assert!(capture_recipe(
            tmp.path(),
            recipe(
                "node",
                "greenfield",
                &["todo"],
                &["frontend-engineer · scaffold"]
            )
        ));
        let prepared = prepare_recipe_prior(tmp.path(), &query, RECIPE_PRIOR_BUDGET).unwrap();
        assert!(commit_recipe_prior_sent(tmp.path(), prepared, "unrelated plan").is_none());
        assert_eq!(load_recipes(tmp.path())[0].stats.times_reused, 0);
    }

    #[test]
    fn repeated_failed_reuse_suppresses_the_recipe() {
        let tmp = tempfile::TempDir::new().unwrap();
        let query = fp("node", "greenfield", &["todo"]);
        assert!(capture_recipe(
            tmp.path(),
            recipe(
                "node",
                "greenfield",
                &["todo"],
                &["frontend-engineer · scaffold"]
            )
        ));
        for _ in 0..2 {
            let receipt = sent_receipt(tmp.path(), &query);
            assert!(settle_recipe_receipt(
                tmp.path(),
                &receipt,
                RecipeOutcome::Fail
            ));
        }
        assert!(
            prepare_recipe_prior(tmp.path(), &query, RECIPE_PRIOR_BUDGET).is_none(),
            "two known failures and no win must make retrieval abstain"
        );
    }

    // ── a non-matching fingerprint recalls nothing (no-op) ──────────────────────────

    #[test]
    fn a_non_matching_fingerprint_recalls_nothing() {
        let tmp = tempfile::TempDir::new().unwrap();
        capture_recipe(
            tmp.path(),
            recipe(
                "node",
                "greenfield",
                &["todo"],
                &["frontend-engineer · scaffold"],
            ),
        );
        // A totally unrelated stack + kind + shape → below the floor → no prior.
        let query = fp("rust", "bugfix", &["kernel", "driver"]);
        assert!(recall_prior_block(tmp.path(), &query, RECIPE_PRIOR_BUDGET).is_none());
        // A no-op recall never bumps a stat.
        assert_eq!(load_recipes(tmp.path())[0].stats.times_reused, 0);
    }

    #[test]
    fn recall_is_fail_open_on_an_empty_store() {
        let tmp = tempfile::TempDir::new().unwrap();
        let query = fp("node", "greenfield", &["todo"]);
        assert!(recall_prior_block(tmp.path(), &query, RECIPE_PRIOR_BUDGET).is_none());
        assert!(load_recipes(tmp.path()).is_empty());
    }

    #[test]
    fn pass_fail_unknown_are_settled_exactly_once() {
        let tmp = tempfile::TempDir::new().unwrap();
        let query = fp("node", "greenfield", &["todo"]);
        assert!(capture_recipe(
            tmp.path(),
            recipe(
                "node",
                "greenfield",
                &["todo"],
                &["frontend-engineer · scaffold"]
            )
        ));

        let pass = sent_receipt(tmp.path(), &query);
        assert!(settle_recipe_receipt(
            tmp.path(),
            &pass,
            RecipeOutcome::Pass
        ));
        assert!(
            !settle_recipe_receipt(tmp.path(), &pass, RecipeOutcome::Fail),
            "a contradictory duplicate cannot overwrite first settlement"
        );

        let fail = sent_receipt(tmp.path(), &query);
        assert!(settle_recipe_receipt(
            tmp.path(),
            &fail,
            RecipeOutcome::Fail
        ));
        let unknown = sent_receipt(tmp.path(), &query);
        assert!(settle_recipe_receipt(
            tmp.path(),
            &unknown,
            RecipeOutcome::Unknown
        ));

        let stats = &load_recipes(tmp.path())[0].stats;
        assert_eq!(stats.times_reused, 3);
        assert_eq!(stats.reuse_wins, 1);
        assert_eq!(stats.reuse_failures, 1);
        assert_eq!(stats.reuse_unknown, 1);
        assert_eq!(stats.pending_reuses, 0);
        assert_eq!(stats.clean_pass_rate(), Some(0.5));
    }

    #[test]
    fn out_of_order_and_cross_project_receipts_are_rejected() {
        let a = tempfile::TempDir::new().unwrap();
        let b = tempfile::TempDir::new().unwrap();
        let query = fp("node", "greenfield", &["todo"]);
        assert!(capture_recipe(
            a.path(),
            recipe(
                "node",
                "greenfield",
                &["todo"],
                &["frontend-engineer · scaffold"]
            )
        ));
        assert!(capture_recipe(
            b.path(),
            recipe(
                "node",
                "greenfield",
                &["todo"],
                &["frontend-engineer · other"]
            )
        ));

        let receipt = sent_receipt(a.path(), &query);
        assert!(
            !settle_recipe_receipt(b.path(), &receipt, RecipeOutcome::Pass),
            "a receipt is scoped to its exact project store"
        );
        let forged = RecipeReceipt {
            receipt_id: unique_nonce("forged"),
            recipe_key: receipt.recipe_key.clone(),
            scope: receipt.scope.clone(),
        };
        assert!(
            !settle_recipe_receipt(a.path(), &forged, RecipeOutcome::Pass),
            "settlement before a sent journal row is out-of-order"
        );
        assert_eq!(load_recipes(a.path())[0].stats.pending_reuses, 1);
    }

    #[test]
    fn a_new_plan_recovers_crashed_pending_receipt_as_unknown() {
        let tmp = tempfile::TempDir::new().unwrap();
        let query = fp("node", "greenfield", &["todo"]);
        assert!(capture_recipe(
            tmp.path(),
            recipe(
                "node",
                "greenfield",
                &["todo"],
                &["frontend-engineer · scaffold"]
            )
        ));
        let _crashed = sent_receipt(tmp.path(), &query);
        assert_eq!(load_recipes(tmp.path())[0].stats.pending_reuses, 1);

        // Preparing a genuinely new plan supersedes an unresumed crashed run.
        let _next = prepare_recipe_prior(tmp.path(), &query, RECIPE_PRIOR_BUDGET).unwrap();
        let stats = &load_recipes(tmp.path())[0].stats;
        assert_eq!(stats.pending_reuses, 0);
        assert_eq!(stats.reuse_unknown, 1);
    }

    #[test]
    fn active_receipt_survives_status_changes_for_resume() {
        let tmp = tempfile::TempDir::new().unwrap();
        let query = fp("node", "greenfield", &["todo"]);
        assert!(capture_recipe(
            tmp.path(),
            recipe(
                "node",
                "greenfield",
                &["todo"],
                &["frontend-engineer · scaffold"]
            )
        ));
        let receipt = sent_receipt(tmp.path(), &query);
        let mut plan = clean_plan();
        assert!(bind_recipe_receipt_to_plan(tmp.path(), &receipt, &plan));
        plan.steps[0].status = StepStatus::Blocked;
        assert_eq!(
            active_recipe_receipt_for_plan(tmp.path(), &plan),
            Some(receipt.clone()),
            "step lifecycle status is excluded from the stable plan identity"
        );
        plan.steps[0].title.push_str(" changed");
        assert!(active_recipe_receipt_for_plan(tmp.path(), &plan).is_none());
    }

    #[test]
    fn rich_recipes_are_project_local_and_cross_project_isolated() {
        let first = tempfile::TempDir::new().unwrap();
        let second = tempfile::TempDir::new().unwrap();
        let first_dir = project_recipes_dir(first.path()).unwrap();
        let second_dir = project_recipes_dir(second.path()).unwrap();
        assert_ne!(first_dir, second_dir);
        assert!(first_dir.ends_with(RECIPES_DIRNAME));
        assert!(second_dir.ends_with(RECIPES_DIRNAME));
        assert!(capture_recipe(
            &first_dir,
            recipe(
                "node",
                "greenfield",
                &["private-feature"],
                &["frontend-engineer · private title"]
            )
        ));
        let query = fp("node", "greenfield", &["private-feature"]);
        assert!(recall_best(&first_dir, &query).is_some());
        assert!(
            recall_best(&second_dir, &query).is_none(),
            "rich recipe material never crosses project stores"
        );
    }

    #[test]
    fn shared_override_parent_still_uses_distinct_project_scopes() {
        let first = tempfile::TempDir::new().unwrap();
        let second = tempfile::TempDir::new().unwrap();
        let shared = tempfile::TempDir::new().unwrap();
        let first_dir =
            project_recipes_dir_with_override(first.path(), Some(shared.path())).unwrap();
        let second_dir =
            project_recipes_dir_with_override(second.path(), Some(shared.path())).unwrap();
        assert_ne!(first_dir, second_dir);
        let canonical_shared = std::fs::canonicalize(shared.path()).unwrap();
        assert!(first_dir.starts_with(canonical_shared.join("projects")));
        assert!(second_dir.starts_with(canonical_shared.join("projects")));
        assert!(capture_recipe(
            &first_dir,
            recipe(
                "node",
                "greenfield",
                &["private-feature"],
                &["frontend-engineer · private title"]
            )
        ));
        let query = fp("node", "greenfield", &["private-feature"]);
        assert!(recall_best(&first_dir, &query).is_some());
        assert!(recall_best(&second_dir, &query).is_none());
    }

    #[test]
    fn legacy_global_rich_rows_are_quarantined_not_recalled() {
        let home = tempfile::TempDir::new().unwrap();
        let old_dir = home.path().join(".umadev/recipes");
        std::fs::create_dir_all(&old_dir).unwrap();
        let legacy = serde_json::to_string(&recipe(
            "node",
            "greenfield",
            &["customer-secret-shape"],
            &["frontend-engineer · private legacy title"],
        ))
        .unwrap();
        std::fs::write(old_dir.join(RECIPES_FILE), legacy).unwrap();
        quarantine_legacy_global_store_in(home.path());
        assert!(!old_dir.join(RECIPES_FILE).exists());
        assert!(old_dir
            .join("recipes.legacy-private-quarantined.jsonl")
            .exists());
    }

    #[test]
    fn secret_bearing_recipe_is_rejected_at_write_and_legacy_read() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut secret = recipe(
            "node",
            "greenfield",
            &["todo"],
            &["frontend-engineer · scaffold"],
        );
        secret.patterns = vec![concat!("api_key=sk-", "proj-1234567890abcdef").to_string()];
        assert!(
            !safe_text(&secret.patterns[0]),
            "fixture must trigger redaction"
        );
        assert!(!capture_recipe(tmp.path(), secret.clone()));
        assert!(load_recipes(tmp.path()).is_empty());

        std::fs::write(
            tmp.path().join(RECIPES_FILE),
            format!("{}\n", serde_json::to_string(&secret).unwrap()),
        )
        .unwrap();
        assert!(
            load_recipes(tmp.path()).is_empty(),
            "legacy sensitive rows are filtered from recall"
        );
    }

    #[test]
    fn concurrent_capture_does_not_lose_clean_builds() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = std::sync::Arc::new(tmp.path().to_path_buf());
        let mut threads = Vec::new();
        for _ in 0..24 {
            let dir = std::sync::Arc::clone(&dir);
            threads.push(std::thread::spawn(move || {
                capture_recipe(
                    &dir,
                    recipe(
                        "node",
                        "greenfield",
                        &["todo"],
                        &["frontend-engineer · scaffold"],
                    ),
                )
            }));
        }
        for thread in threads {
            assert!(thread.join().unwrap());
        }
        assert_eq!(load_recipes(&dir)[0].stats.clean_builds, 24);
    }

    #[test]
    fn cross_process_capture_child() {
        let Some(dir) = std::env::var_os("UMADEV_RECIPE_CHILD_DIR") else {
            return;
        };
        assert!(capture_recipe(
            Path::new(&dir),
            recipe(
                "node",
                "greenfield",
                &["todo"],
                &["frontend-engineer · scaffold"]
            )
        ));
    }

    #[test]
    fn concurrent_processes_do_not_lose_clean_builds() {
        let tmp = tempfile::TempDir::new().unwrap();
        let executable = std::env::current_exe().unwrap();
        let mut children = Vec::new();
        for _ in 0..6 {
            children.push(
                std::process::Command::new(&executable)
                    .args([
                        "--exact",
                        "recipes::tests::cross_process_capture_child",
                        "--nocapture",
                    ])
                    .env("UMADEV_RECIPE_CHILD_DIR", tmp.path())
                    .spawn()
                    .unwrap(),
            );
        }
        for mut child in children {
            assert!(child.wait().unwrap().success());
        }
        assert_eq!(load_recipes(tmp.path())[0].stats.clean_builds, 6);
    }

    #[test]
    fn reclaimed_lock_owner_cannot_delete_its_successor() {
        let tmp = tempfile::TempDir::new().unwrap();
        let first = acquire_cross_process_lock(tmp.path()).unwrap();
        let stale = LockOwner {
            created_at_ms: now_ms().saturating_sub(STALE_LOCK_MS + 1),
            nonce: first.nonce.clone(),
        };
        write_atomic(
            &first.path.join(LOCK_OWNER_FILE),
            &serde_json::to_string(&stale).unwrap(),
        )
        .unwrap();
        reclaim_stale_lock(&first.path);
        let second = acquire_cross_process_lock(tmp.path()).unwrap();

        drop(first);
        assert!(real_dir(&tmp.path().join(STORE_LOCK_DIR)));
        drop(second);
        assert!(!tmp.path().join(STORE_LOCK_DIR).exists());
    }

    #[test]
    fn full_journal_never_accepts_a_sent_receipt_without_a_settlement_slot() {
        let tmp = tempfile::TempDir::new().unwrap();
        let query = fp("node", "greenfield", &["todo"]);
        assert!(capture_recipe(
            tmp.path(),
            recipe(
                "node",
                "greenfield",
                &["todo"],
                &["frontend-engineer · scaffold"]
            )
        ));
        std::fs::write(
            tmp.path().join(RECIPE_OUTCOMES_FILE),
            "{}\n".repeat(MAX_JOURNAL_EVENTS - 1),
        )
        .unwrap();
        let prepared = prepare_recipe_prior(tmp.path(), &query, RECIPE_PRIOR_BUDGET).unwrap();
        let directive = prepared.block().to_string();
        assert!(commit_recipe_prior_sent(tmp.path(), prepared, &directive).is_none());
        assert_eq!(load_recipes(tmp.path())[0].stats.times_reused, 0);
    }

    #[cfg(windows)]
    #[test]
    fn windows_atomic_backup_is_readable_and_recovered() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join(RECIPES_FILE);
        std::fs::write(&target, "old\n").unwrap();
        let backup = atomic_backup_path(&target);
        std::fs::rename(&target, &backup).unwrap();
        assert_eq!(read_bounded(&target, 1024).as_deref(), Some("old\n"));

        write_atomic(&target, "new\n").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new\n");
        assert!(!backup.exists());
    }

    #[cfg(unix)]
    #[test]
    fn linked_store_file_is_rejected_without_touching_target() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::TempDir::new().unwrap();
        let victim = tmp.path().join("victim");
        std::fs::write(&victim, "do-not-touch").unwrap();
        symlink(&victim, tmp.path().join(RECIPES_FILE)).unwrap();
        assert!(!capture_recipe(
            tmp.path(),
            recipe(
                "node",
                "greenfield",
                &["todo"],
                &["frontend-engineer · scaffold"]
            )
        ));
        assert_eq!(std::fs::read_to_string(victim).unwrap(), "do-not-touch");
    }

    #[cfg(unix)]
    #[test]
    fn managed_directories_journal_and_active_marker_never_follow_links() {
        use std::os::unix::fs::symlink;

        let project = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        symlink(outside.path(), project.path().join(".umadev")).unwrap();
        assert!(project_recipes_dir(project.path()).is_none());

        let store = tempfile::TempDir::new().unwrap();
        let query = fp("node", "greenfield", &["todo"]);
        assert!(capture_recipe(
            store.path(),
            recipe(
                "node",
                "greenfield",
                &["todo"],
                &["frontend-engineer · scaffold"]
            )
        ));
        let victim = store.path().join("victim");
        std::fs::write(&victim, "unchanged").unwrap();
        symlink(&victim, store.path().join(RECIPE_OUTCOMES_FILE)).unwrap();
        let prepared = prepare_recipe_prior(store.path(), &query, RECIPE_PRIOR_BUDGET).unwrap();
        let directive = prepared.block().to_string();
        assert!(commit_recipe_prior_sent(store.path(), prepared, &directive).is_none());
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "unchanged");

        std::fs::remove_file(store.path().join(RECIPE_OUTCOMES_FILE)).unwrap();
        let receipt = sent_receipt(store.path(), &query);
        symlink(&victim, store.path().join(ACTIVE_RECEIPT_FILE)).unwrap();
        assert!(!bind_recipe_receipt_to_plan(
            store.path(),
            &receipt,
            &clean_plan()
        ));
        assert_eq!(std::fs::read_to_string(victim).unwrap(), "unchanged");
    }

    #[test]
    fn load_is_forgiving_of_corrupt_lines() {
        let tmp = tempfile::TempDir::new().unwrap();
        let good = serde_json::to_string(&recipe(
            "node",
            "greenfield",
            &["todo"],
            &["frontend-engineer · scaffold"],
        ))
        .unwrap();
        std::fs::write(
            tmp.path().join(RECIPES_FILE),
            format!("{good}\nthis is not json {{{{\n"),
        )
        .unwrap();
        let store = load_recipes(tmp.path());
        assert_eq!(store.len(), 1, "good line kept, garbage skipped: {store:?}");
    }

    // ── capture is fail-open (a store error never fails delivery) ────────────────────

    #[tokio::test]
    async fn capture_at_delivery_is_fail_open_on_an_unwritable_store() {
        // Point the store at a path whose parent is a regular file — the dir can never
        // be created, so every write silently fails. capture_at_delivery must not panic
        // and must record nothing, never affecting the (already-clean) delivery.
        let tmp = tempfile::TempDir::new().unwrap();
        let blocker = tmp.path().join("not-a-dir");
        std::fs::write(&blocker, b"x").unwrap();
        let dead_dir = blocker.join("recipes");

        let mut brain = Brain {
            reply: String::new(),
            can_fork: false,
            forks: 0,
        };
        // Drive capture with an explicit dead dir via the core write path — a write
        // failure is swallowed and load stays empty.
        let skeleton = skeleton_from_plan(&clean_plan());
        assert!(!skeleton.is_empty());
        let r = Recipe {
            fingerprint: fp("node", "greenfield", &["x"]),
            plan_skeleton: skeleton,
            key_scaffold: Vec::new(),
            patterns: Vec::new(),
            stats: OutcomeStats::default(),
        };
        // No panic; the unwritable store simply holds nothing.
        assert!(
            !capture_recipe(&dead_dir, r),
            "write failure must not report learned success"
        );
        assert!(load_recipes(&dead_dir).is_empty());
        // And the async orchestrator's distill path is itself fail-open on no fork.
        let d = distill_enrichment(&mut brain, &["frontend-engineer · scaffold".to_string()]).await;
        assert!(
            d.is_none(),
            "no fork → no enrichment, mechanical recipe stands"
        );
    }

    // ── the distillation consult is fail-open (brain down → mechanical recipe) ───────

    #[tokio::test]
    async fn capture_at_delivery_stores_a_mechanical_recipe_when_the_brain_is_down() {
        // Two dirs: the recipes store dir (dir-explicit core, no env) and the project
        // root (only used for stack detection — an empty dir detects `none`).
        let store_dir = tempfile::TempDir::new().unwrap();
        let proj = tempfile::TempDir::new().unwrap();
        let mut brain = Brain {
            reply: String::new(),
            can_fork: false, // offline / no fork → distillation returns None
            forks: 0,
        };
        capture_at_delivery_in(
            &mut brain,
            store_dir.path(),
            proj.path(),
            &route(TaskKind::Greenfield),
            &clean_plan(),
            "build a todo list",
            &sink(),
        )
        .await;

        let store = load_recipes(store_dir.path());
        assert_eq!(
            store.len(),
            1,
            "a mechanical recipe is still stored: {store:?}"
        );
        // The skeleton came from the Done steps; the scaffold from their evidence paths.
        assert!(store[0]
            .plan_skeleton
            .iter()
            .any(|s| s.contains("Scaffold the app shell")));
        assert!(store[0].key_scaffold.contains(&"src/App.tsx".to_string()));
        assert!(store[0].key_scaffold.contains(&"src/api.ts".to_string()));
        // Brain was down → no distilled patterns, but the recipe is otherwise complete.
        assert!(store[0].patterns.is_empty(), "no consult → no patterns");
    }

    #[tokio::test]
    async fn recipe_capture_policy_off_and_corrupt_never_fork_the_base() {
        let store_dir = tempfile::TempDir::new().unwrap();
        let project = tempfile::TempDir::new().unwrap();
        crate::memory_control::update_capture(
            project.path(),
            MemoryScope::Project,
            Some(MemoryStore::Recipes),
            false,
        )
        .unwrap();
        let mut disabled = Brain {
            reply: "{\"patterns\":[\"unused\"]}".to_string(),
            can_fork: true,
            forks: 0,
        };
        capture_at_delivery_in(
            &mut disabled,
            store_dir.path(),
            project.path(),
            &route(TaskKind::Greenfield),
            &clean_plan(),
            "build a todo list",
            &sink(),
        )
        .await;
        assert_eq!(disabled.forks, 0);
        assert!(load_recipes(store_dir.path()).is_empty());

        crate::memory_control::update_capture(
            project.path(),
            MemoryScope::Project,
            Some(MemoryStore::Recipes),
            true,
        )
        .unwrap();
        let mut enabled = Brain {
            reply: "{\"patterns\":[\"repository pattern\"]}".to_string(),
            can_fork: true,
            forks: 0,
        };
        capture_at_delivery_in(
            &mut enabled,
            store_dir.path(),
            project.path(),
            &route(TaskKind::Greenfield),
            &clean_plan(),
            "build a todo list",
            &sink(),
        )
        .await;
        assert_eq!(enabled.forks, 1);
        assert_eq!(load_recipes(store_dir.path()).len(), 1);

        std::fs::write(
            project.path().join(".umadev/memory/policy.toml"),
            "invalid = [toml",
        )
        .unwrap();
        let mut corrupt = Brain {
            reply: "{\"patterns\":[\"unused\"]}".to_string(),
            can_fork: true,
            forks: 0,
        };
        capture_at_delivery_in(
            &mut corrupt,
            store_dir.path(),
            project.path(),
            &route(TaskKind::Greenfield),
            &clean_plan(),
            "build another todo list",
            &sink(),
        )
        .await;
        assert_eq!(corrupt.forks, 0);
        assert_eq!(load_recipes(store_dir.path()).len(), 1);
    }

    #[tokio::test]
    async fn distillation_consult_enriches_patterns_when_the_brain_answers() {
        let mut brain = Brain {
            reply: "{\"patterns\":[\"repository pattern for data access\"],\
                    \"key_scaffold\":[\"src/db/repo.ts\"]}"
                .to_string(),
            can_fork: true,
            forks: 0,
        };
        let d = distill_enrichment(&mut brain, &["backend-engineer · wire the API".to_string()])
            .await
            .expect("a forking brain enriches");
        assert_eq!(d.patterns, vec!["repository pattern for data access"]);
        assert_eq!(d.key_scaffold, vec!["src/db/repo.ts"]);
    }

    // ── bounded under many recipes ──────────────────────────────────────────────────

    #[test]
    fn store_is_bounded_under_many_recipes() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Capture well over the cap, each a DISTINCT fingerprint so none merge.
        for i in 0..(MAX_RECIPES + 40) {
            let r = Recipe {
                fingerprint: Fingerprint {
                    stack: "node".to_string(),
                    kind: "greenfield".to_string(),
                    shape: vec![format!("feat{i}")],
                },
                plan_skeleton: vec!["frontend-engineer · scaffold".to_string()],
                key_scaffold: Vec::new(),
                patterns: Vec::new(),
                stats: OutcomeStats::default(),
            };
            capture_recipe(tmp.path(), r);
        }
        let store = load_recipes(tmp.path());
        assert!(
            store.len() <= MAX_RECIPES,
            "store capped at MAX_RECIPES ({} > {MAX_RECIPES})",
            store.len()
        );
    }

    #[test]
    fn per_recipe_fields_are_bounded() {
        let tmp = tempfile::TempDir::new().unwrap();
        let big_skeleton: Vec<String> = (0..100)
            .map(|i| format!("frontend-engineer · step {i}"))
            .collect();
        let r = Recipe {
            fingerprint: fp("node", "greenfield", &["x"]),
            plan_skeleton: big_skeleton,
            key_scaffold: (0..100).map(|i| format!("src/f{i}.ts")).collect(),
            patterns: (0..100).map(|i| format!("pattern {i}")).collect(),
            stats: OutcomeStats::default(),
        };
        capture_recipe(tmp.path(), r);
        let s = &load_recipes(tmp.path())[0];
        assert!(s.plan_skeleton.len() <= MAX_SKELETON_STEPS);
        assert!(s.key_scaffold.len() <= MAX_SCAFFOLD);
        assert!(s.patterns.len() <= MAX_PATTERNS);
    }

    #[test]
    fn prior_block_is_budget_bounded() {
        let big: Vec<String> = (0..MAX_SKELETON_STEPS)
            .map(|i| {
                format!(
                    "frontend-engineer · a long proven step title number {i} {}",
                    "x".repeat(80)
                )
            })
            .collect();
        let r = Recipe {
            fingerprint: fp("node", "greenfield", &["todo"]),
            plan_skeleton: big,
            key_scaffold: vec!["src/App.tsx".to_string()],
            patterns: vec!["used repository pattern".to_string()],
            stats: OutcomeStats {
                clean_builds: 3,
                times_reused: 4,
                reuse_wins: 2,
                reuse_failures: 2,
                reuse_unknown: 0,
                pending_reuses: 0,
            },
        };
        let block = recipe_prior_block(&r, RECIPE_PRIOR_BUDGET);
        assert!(
            block.chars().count() <= RECIPE_PRIOR_BUDGET,
            "prior block within budget ({} > {RECIPE_PRIOR_BUDGET})",
            block.chars().count()
        );
        assert!(
            block.contains("clean-pass rate 50%"),
            "renders the clean-pass rate: {block}"
        );
    }
}
