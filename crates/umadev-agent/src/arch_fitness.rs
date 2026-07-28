//! Architecture-fitness floor — the deterministic anti-spaghetti gate
//! (`UD-CODE-006` clause family; normative prose in
//! `spec/UMADEV_HOST_SPEC_V1.md` §3.6).
//!
//! The L0 firmware *preaches* layering, small focused modules, and no
//! copy-paste — but preaching is a prompt, not a floor. A borrowed brain under
//! pressure ships one giant file, imports the database from the UI, and pastes
//! the same block into three places, and every prior deterministic check
//! (build/test, coverage, contract, test-integrity) still reads green. This
//! module makes UmaDev's own deterministic floor *verify* architecture
//! fitness, with four rules:
//!
//! 1. **God-file gate** (`UD-CODE-006a`, blocking) — a NEW source file over
//!    500 lines, or a touched file that GREW PAST 800 lines this step, blocks
//!    with a split directive ("split by feature/domain; real teams don't ship
//!    one giant file"). The grown ceiling is overridable via
//!    `UMADEV_ARCH_MAX_FILE_LINES`; the new-file ceiling is
//!    `min(500, that ceiling)`. Generated/vendored/lock files and tests are
//!    exempt. Without a before-baseline (the plain [`arch_fitness_findings`]
//!    entry) newness cannot be known, so only the hard grown ceiling fires on
//!    a touched file — never a false block on a merely-touched legacy file.
//! 2. **Layer-dependency rules** (`UD-CODE-006b`, blocking) — the architecture
//!    doc (`output/<slug>-architecture.md`) may DECLARE a layering contract
//!    (convention below); every resolved import edge from the repo map
//!    ([`umadev_knowledge::repomap::symbol_index`], the same
//!    confidence-disciplined edges the L0 repo-map slice ranks with) is
//!    checked against it. An edge that goes AGAINST the declared one-way order
//!    or crosses a banned pair blocks, naming both files and the violated
//!    rule. No declaration in the doc → this check silently no-ops.
//! 3. **Clone gate** (`UD-CODE-006c`, ADVISORY) — normalized
//!    (whitespace-squeezed, comment-stripped) 5-line windows of code ADDED in
//!    touched files are hashed against the rest of the repo; a duplicated
//!    block ≥ 5 lines yields an advisory naming the sibling location ("reuse
//!    X:line instead"). Advisory, not blocking — deduplication judgment needs
//!    a human/critic; the floor only surfaces the evidence.
//! 4. **Comment hygiene** (`UD-CODE-006d`, ADVISORY) — a touched source file
//!    that newly gains an 8-line ordinary-comment run, or at least 12 ordinary
//!    comment-only lines exceeding its code lines, gets a concise advisory.
//!    Language-specific documentation, licenses, generated/vendored files,
//!    directives, and tests are exempt.
//!
//! # The architecture-doc layering convention
//!
//! The layer contract is declared in `output/<slug>-architecture.md` in either
//! (or both) of two forms:
//!
//! **A `## Layering` section** (any heading level; the heading text contains
//! `layering` or `分层`, case-insensitive) holding
//!
//! - an optional markdown table mapping directory prefixes to layer names
//!   (first column = dir, second column = layer; header/separator rows are
//!   skipped):
//!
//!   ```markdown
//!   ## Layering
//!   | dir             | layer      |
//!   | --------------- | ---------- |
//!   | src/controllers | controller |
//!   | src/services    | service    |
//!   | src/db          | repository |
//!   ```
//!
//! - and/or a ONE-WAY order chain line (the first ` -> ` chain in the
//!   section; an optional `Order:` label is allowed):
//!
//!   ```markdown
//!   Order: controller -> service -> repository
//!   ```
//!
//!   meaning dependencies may only flow left→right: `controller` may import
//!   `service`/`repository`, `service` may import `repository`, and an import
//!   in the opposite direction (e.g. `repository` → `controller`) is a
//!   violation. Same-layer imports are always fine.
//!
//! **Explicit ban lines** anywhere in the doc:
//!
//! ```markdown
//! LAYER-RULE: ui !-> db
//! ```
//!
//! meaning files in layer `ui` must never import files in layer `db`.
//!
//! When no dir table maps a file, a file belongs to layer `L` when one of its
//! path segments equals `L` or `L` + `"s"` (case-insensitive) — so
//! `src/controllers/user.ts` matches layer `controller` without any table.
//!
//! # Fail-open by contract
//!
//! Every path that cannot be determined yields NO findings: an unreadable /
//! absent architecture doc, a doc with no layering declaration, an empty or
//! unresolvable import-edge set, an unreadable tree, and a huge repo (more
//! than the bounded source-file limit, or a blown read budget) all degrade
//! to a silent skip. The gate can never fabricate a block, never error, and
//! its rework is bounded by the caller's existing fix-round counters.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::fswalk::{classify_no_follow, EntryKind};

// ---------------------------------------------------------------------------
// Rule ids + thresholds
// ---------------------------------------------------------------------------

/// Clause id of the god-file gate (rule 1, spec §3.6).
pub const RULE_GOD_FILE: &str = "UD-CODE-006a";
/// Clause id of the layer-dependency gate (rule 2, spec §3.6).
pub const RULE_LAYER: &str = "UD-CODE-006b";
/// Clause id of the clone gate (rule 3, advisory, spec §3.6).
pub const RULE_CLONE: &str = "UD-CODE-006c";
/// Clause id of the comment-hygiene gate (rule 4, advisory, spec §3.6).
pub const RULE_COMMENT_HYGIENE: &str = "UD-CODE-006d";

/// Default line ceiling for a NEW source file (`UD-CODE-006a`).
const NEW_FILE_MAX_LINES: usize = 500;
/// Default line ceiling a touched file must not GROW PAST (`UD-CODE-006a`).
/// Overridable via `UMADEV_ARCH_MAX_FILE_LINES`.
const GROWN_FILE_MAX_LINES: usize = 800;

/// Hard repo-size skip: more source files than this → the god-file and clone
/// scans silently no-op (fail-open; the pass must stay fast on any repo).
const MAX_SCAN_FILES: usize = 5_000;
/// Hard cap across all directory entries, including irrelevant extensions.
const MAX_SCAN_ENTRIES: usize = 50_000;
/// Max bytes read per file. A larger file disables the whole comparative scan;
/// its prefix is never treated as complete architecture evidence.
const MAX_FILE_BYTES: usize = 512 * 1024;
/// Total read budget across one scan — blown → the whole scan is discarded
/// (partial data could misclassify a pre-existing file as "new").
const MAX_TOTAL_BYTES: usize = 32 * 1024 * 1024;
/// Architecture declaration input is small prose; reject pathological files
/// instead of parsing an arbitrary prefix as the complete layering contract.
const MAX_LAYER_DOC_BYTES: usize = 2 * 1024 * 1024;
/// Max directory recursion depth (mirrors the acceptance source walk).
const MAX_SCAN_DEPTH: usize = 16;

/// Clone-window shape: 5 consecutive normalized lines.
const CLONE_WINDOW: usize = 5;
/// A window's joined normalized text must be at least this long to count as
/// distinctive (filters brace/boilerplate runs).
const MIN_WINDOW_CHARS: usize = 40;
/// A normalized line shorter than this (after whitespace squeeze) is dropped —
/// `}`/`);` runs must not manufacture matches.
const MIN_LINE_CHARS: usize = 3;
/// Max windows recorded per file.
const MAX_WINDOWS_PER_FILE: usize = 2_000;
/// Max windows recorded across the whole scan — blown → the clone gate is
/// disabled for this pass (god-file/layer results are unaffected).
const MAX_TOTAL_WINDOWS: usize = 200_000;
/// Max touched files the clone gate examines.
const MAX_CLONE_TOUCHED: usize = 20;
/// Max clone advisories per touched file / in total.
const MAX_CLONES_PER_FILE: usize = 3;
const MAX_CLONE_FINDINGS: usize = 12;
/// Comment-hygiene thresholds. These are deliberately high enough to catch
/// narration blocks without imposing a comment quota on ordinary code.
const LONG_COMMENT_RUN: usize = 8;
const COMMENT_RATIO_MIN: usize = 12;
const MAX_COMMENT_FINDINGS: usize = 12;
/// Max layer-violation findings reported per pass.
const MAX_LAYER_FINDINGS: usize = 10;

/// Directories never worth scanning (mirrors `acceptance::SKIP_DIRS`).
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "dist",
    "build",
    ".git",
    "vendor",
    "vendors",
    "third_party",
    "__pycache__",
    ".pytest_cache",
    ".next",
    "out",
    "coverage",
    "output",
];

/// Code extensions the fitness scan covers. Deliberately code-only — styles,
/// markup and data files are out of scope for layering / god-file / clone
/// judgments.
const SRC_EXT: &[&str] = &[
    "ts", "tsx", "js", "jsx", "mjs", "cjs", "vue", "svelte", "astro", "py", "rs", "go", "java",
    "kt", "kts", "rb", "php", "cs", "ex", "exs", "dart", "swift", "scala", "c", "cc", "cpp", "h",
    "hpp", "m", "mm", "lua",
];

/// The god-file line ceilings, honoring `UMADEV_ARCH_MAX_FILE_LINES` for the
/// grown ceiling. The new-file ceiling is `min(500, grown)` so raising the
/// grown ceiling never silently raises the new-file bar, while lowering it
/// below 500 tightens both. A non-numeric / non-positive env value falls back
/// to the default (fail-open).
fn line_ceilings() -> (usize, usize) {
    let grown = std::env::var("UMADEV_ARCH_MAX_FILE_LINES")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(GROWN_FILE_MAX_LINES);
    (NEW_FILE_MAX_LINES.min(grown), grown)
}

// ---------------------------------------------------------------------------
// Finding
// ---------------------------------------------------------------------------

/// One architecture-fitness finding. `blocking == true` folds into the
/// deterministic floor's blocking list (god-file / layer violation);
/// `blocking == false` is advisory (clone gate) and surfaces as a note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// Whether this finding blocks the step (true) or is advisory (false).
    pub blocking: bool,
    /// Human-readable, evidence-bearing message (self-prefixed
    /// `arch-fitness:`) suitable for folding into a rework directive.
    pub message: String,
    /// Workspace-relative, `/`-separated path of the offending file (the
    /// importer, for a layer violation).
    pub file: String,
    /// The violated rule's clause id ([`RULE_GOD_FILE`] / [`RULE_LAYER`] /
    /// [`RULE_CLONE`] / [`RULE_COMMENT_HYGIENE`]).
    pub rule_id: &'static str,
}

// ---------------------------------------------------------------------------
// Baseline + scan (the changed-file-set source, mirroring test_integrity)
// ---------------------------------------------------------------------------

/// Per-file scan record used by the diff-aware fitness rules.
#[derive(Debug, Clone)]
struct FileScan {
    lines: usize,
    hash: u64,
    windows: HashMap<u64, u32>,
    comments: CommentStats,
}

#[derive(Debug, Clone, Default)]
struct CommentStats {
    ordinary: usize,
    code: usize,
    max_run: usize,
    max_run_start: u32,
    runs: Vec<CommentRun>,
}

#[derive(Debug, Clone)]
struct CommentRun {
    start: u32,
    lines: Vec<u64>,
}

/// A point-in-time snapshot of the project's fitness-relevant source surface,
/// captured BEFORE a build step's doer turn (exactly like
/// [`crate::test_integrity::snapshot`]) and compared to the after-state by
/// [`arch_fitness_findings_since`]. `disabled == true` (huge repo / blown
/// read budget / unreadable tree at capture time) makes every later
/// comparison a silent no-op — fail-open (`UD-CODE-006`).
#[derive(Debug, Clone, Default)]
pub struct ArchBaseline {
    files: BTreeMap<String, FileScan>,
    clone_ok: bool,
    disabled: bool,
}

/// The full-scan result (same shape as the baseline, plus whether the clone
/// window budget survived).
struct ArchScan {
    files: BTreeMap<String, FileScan>,
    clone_ok: bool,
}

/// Capture the pre-step architecture baseline. Bounded and fail-open: a repo
/// over the bounded source-file limit (or a blown read budget) yields a
/// `disabled` baseline against which [`arch_fitness_findings_since`] reports
/// nothing.
#[must_use]
pub fn baseline(root: &Path) -> ArchBaseline {
    match scan(root) {
        Some(s) => ArchBaseline {
            files: s.files,
            clone_ok: s.clone_ok,
            disabled: false,
        },
        None => ArchBaseline {
            files: BTreeMap::new(),
            clone_ok: false,
            disabled: true,
        },
    }
}

/// The files touched since `before` was captured — new files plus files whose
/// content hash changed — as absolute paths. Empty when the baseline is
/// disabled or the current tree cannot be scanned (fail-open).
#[must_use]
pub fn touched_since(root: &Path, before: &ArchBaseline) -> Vec<PathBuf> {
    if before.disabled {
        return Vec::new();
    }
    let Some(now) = scan(root) else {
        return Vec::new();
    };
    touched_rels(&now, before)
        .into_iter()
        .map(|rel| root.join(rel))
        .collect()
}

/// The workspace-relative touched set: files in `now` that are new or whose
/// hash differs from the baseline.
fn touched_rels(now: &ArchScan, before: &ArchBaseline) -> Vec<String> {
    now.files
        .iter()
        .filter(|(rel, f)| before.files.get(*rel).is_none_or(|b| b.hash != f.hash))
        .map(|(rel, _)| rel.clone())
        .collect()
}

/// Bounded, no-follow scan of the fitness-relevant source surface. `None`
/// when the repo exceeds [`MAX_SCAN_FILES`] source files or the read budget —
/// the explicit fail-open "skip silently on huge repos" path (partial data
/// could misclassify a pre-existing file as new, so a blown budget discards
/// the whole scan rather than returning a half-truth).
fn scan(root: &Path) -> Option<ArchScan> {
    let mut paths: Vec<(String, PathBuf)> = Vec::new();
    let mut entries_seen = 0usize;
    if !collect(root, root, &mut paths, &mut entries_seen, 0) || paths.len() > MAX_SCAN_FILES {
        return None;
    }
    let mut files = BTreeMap::new();
    let mut clone_ok = true;
    let mut total_windows = 0usize;
    let mut read_budget = crate::bounded_fs::Utf8ReadBudget::new(MAX_TOTAL_BYTES, MAX_FILE_BYTES);
    for (rel, abs) in paths {
        let Ok(bytes) = read_budget.read_bytes_beneath(root, &abs) else {
            // A partial tree can misclassify an existing file as newly added.
            // Discard the whole scan rather than silently trusting a prefix.
            return None;
        };
        let content = String::from_utf8_lossy(&bytes);
        let windows = if clone_ok {
            let w = windows_of(&content);
            total_windows += w.len();
            if total_windows > MAX_TOTAL_WINDOWS {
                clone_ok = false; // clone gate off; god-file/layer still fine
            }
            w
        } else {
            HashMap::new()
        };
        let comments = std::str::from_utf8(&bytes)
            .ok()
            .map(|text| comment_stats(text, &rel))
            .unwrap_or_default();
        files.insert(
            rel,
            FileScan {
                lines: content.lines().count(),
                hash: fnv(content.as_bytes()),
                windows,
                comments,
            },
        );
    }
    Some(ArchScan { files, clone_ok })
}

/// Recursively collect fitness-relevant source files as `(rel, abs)` pairs —
/// code extensions only, skipping vendored/build dirs, dot-dirs, symlinks
/// (no-follow), and exempt (test / generated / lock) files. Collects at most
/// [`MAX_SCAN_FILES`] + 1 entries so the caller can detect the overflow.
fn collect(
    root: &Path,
    dir: &Path,
    out: &mut Vec<(String, PathBuf)>,
    entries_seen: &mut usize,
    depth: usize,
) -> bool {
    if depth > MAX_SCAN_DEPTH || out.len() > MAX_SCAN_FILES {
        return false;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in rd {
        *entries_seen = entries_seen.saturating_add(1);
        if *entries_seen > MAX_SCAN_ENTRIES {
            return false;
        }
        let Ok(e) = entry else {
            return false;
        };
        if out.len() > MAX_SCAN_FILES {
            return false;
        }
        let p = e.path();
        match classify_no_follow(&p) {
            EntryKind::Dir => {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                let lower_name = name.to_ascii_lowercase();
                if name.starts_with('.') || SKIP_DIRS.contains(&lower_name.as_str()) {
                    continue;
                }
                if !collect(root, &p, out, entries_seen, depth + 1) {
                    return false;
                }
            }
            EntryKind::File => {
                let ext = p
                    .extension()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if !SRC_EXT.contains(&ext.as_str()) {
                    continue;
                }
                let rel = rel_of(root, &p);
                if is_exempt(&rel) {
                    continue;
                }
                out.push((rel, p));
            }
            EntryKind::Skip => {}
        }
    }
    true
}

/// Workspace-relative, `/`-separated form of `p`.
fn rel_of(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/")
}

/// Whether a (relative, `/`-separated) path is exempt from every fitness rule:
/// test files (a long test file / a shared fixture block is normal), and
/// generated / minified / lockfile artifacts (nobody hand-splits those).
/// Vendored dirs are already excluded by the walk.
// `name` is lowercased right below, so every `ends_with` here IS
// case-insensitive already.
#[allow(clippy::case_sensitive_file_extension_comparisons)]
fn is_exempt(rel: &str) -> bool {
    let lower = rel.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    // Tests, by the universal path/name conventions.
    let test_by_dir = lower.starts_with("tests/")
        || lower.starts_with("test/")
        || lower.starts_with("spec/")
        || lower.starts_with("__tests__/")
        || lower.contains("/tests/")
        || lower.contains("/test/")
        || lower.contains("/spec/")
        || lower.contains("/__tests__/")
        || lower.contains("/fixtures/");
    let test_by_name = name.contains(".test.")
        || name.contains(".spec.")
        || name.starts_with("test_")
        || name.rsplit_once('.').is_some_and(|(stem, _)| {
            stem.ends_with("_test")
                || stem.ends_with("_tests")
                || stem.ends_with("_spec")
                || stem.ends_with("_specs")
        })
        || name.ends_with("test.java")
        || name.ends_with("tests.java")
        || name.ends_with("test.kt");
    // Generated / minified / lock artifacts.
    let generated = name.contains(".min.")
        || name.contains(".generated.")
        || name.contains(".gen.")
        || name.ends_with(".g.dart")
        || name.ends_with("_pb2.py")
        || name.ends_with(".pb.go")
        || name.ends_with(".d.ts")
        || name.ends_with(".lock")
        || name.contains("generated")
        || lower.contains("/generated/");
    test_by_dir || test_by_name || generated
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Architecture-fitness findings for the given touched-file set
/// (`UD-CODE-006`, spec §3.6). `touched` is the changed-file set the
/// caller knows (absolute or workspace-relative paths). Deterministic and
/// fail-open: any error path yields no findings.
///
/// Without a before-baseline this entry cannot tell a NEW file from a
/// pre-existing one, so the god-file gate fires only on the hard grown
/// ceiling (a touched file over it), and the clone gate considers every
/// window of a touched file (not just added ones). The step-level wiring uses
/// [`arch_fitness_findings_since`], which has the full new-vs-grown and
/// added-only semantics. The layer-dependency check (rule 2) is a repo-global
/// property and runs regardless of `touched` — so calling this with an empty
/// set is the cheap "layer rules only" form the acceptance floor uses.
#[must_use]
pub fn arch_fitness_findings(root: &Path, slug: &str, touched: &[PathBuf]) -> Vec<Finding> {
    let mut out = layer_findings(root, slug);
    if touched.is_empty() {
        return out; // no touched set → god-file + clone have nothing to judge
    }
    let Some(now) = scan(root) else {
        return out; // huge repo / blown budget → touched-file rules skip silently
    };
    let rels: Vec<String> = touched
        .iter()
        .map(|p| rel_of(root, p))
        .filter(|r| !is_exempt(r))
        .collect();
    let (_, grown_max) = line_ceilings();
    for rel in &rels {
        let Some(f) = now.files.get(rel) else {
            continue; // deleted / not a scanned source file → nothing to judge
        };
        if f.lines > grown_max {
            out.push(god_file_finding(rel, None, f.lines, grown_max));
        }
    }
    out.extend(clone_findings(&now, &rels, None));
    // Comment hygiene is intentionally baseline-only. A touched path says the
    // file changed, but without its previous comment shape we cannot prove the
    // narration was added by this change; surfacing legacy debt would violate
    // the rule's "newly gains" boundary.
    out
}

/// The step-level architecture-fitness check: compare the current tree to the
/// pre-step [`baseline`], derive the touched set, and run all four rules
/// with full semantics — a NEW file over the new-file ceiling or a file that
/// GREW PAST the grown ceiling blocks; a duplicated block of ADDED code is
/// advisory; newly added comment narration is advisory. Empty when the
/// baseline is disabled or the tree cannot be scanned (fail-open,
/// `UD-CODE-006`).
#[must_use]
pub fn arch_fitness_findings_since(root: &Path, slug: &str, before: &ArchBaseline) -> Vec<Finding> {
    if before.disabled {
        return Vec::new();
    }
    let Some(now) = scan(root) else {
        return Vec::new();
    };
    let touched = touched_rels(&now, before);
    let mut out = layer_findings(root, slug);
    let (new_max, grown_max) = line_ceilings();
    for rel in &touched {
        let Some(f) = now.files.get(rel) else {
            continue;
        };
        match before.files.get(rel) {
            None if f.lines > new_max => {
                out.push(god_file_finding(rel, None, f.lines, new_max));
            }
            Some(b) if b.lines <= grown_max && f.lines > grown_max => {
                out.push(god_file_finding(rel, Some(b.lines), f.lines, grown_max));
            }
            _ => {}
        }
    }
    if before.clone_ok {
        out.extend(clone_findings(&now, &touched, Some(before)));
    }
    out.extend(comment_hygiene_findings(&now, &touched, Some(before)));
    out
}

/// Build one god-file finding (`UD-CODE-006a`). `before_lines` is `Some` for
/// the grew-past form, `None` for the new-file / ceiling form.
fn god_file_finding(rel: &str, before_lines: Option<usize>, lines: usize, max: usize) -> Finding {
    let message = match before_lines {
        Some(b) => format!(
            "arch-fitness: {rel} grew past {max} lines this step ({b} -> {lines}) — split it \
             by feature/domain into focused modules instead of letting it keep growing; real \
             teams don't ship one giant file"
        ),
        None => format!(
            "arch-fitness: {rel} is {lines} lines (over the {max}-line ceiling) — split it by \
             feature/domain into focused modules; real teams don't ship one giant file"
        ),
    };
    Finding {
        blocking: true,
        message,
        file: rel.to_string(),
        rule_id: RULE_GOD_FILE,
    }
}

// ---------------------------------------------------------------------------
// Rule 2 — layer-dependency rules from the architecture doc
// ---------------------------------------------------------------------------

/// The parsed layering declaration (see the module docs for the convention).
#[derive(Debug, Default, PartialEq, Eq)]
struct LayerSpec {
    /// One-way order, upstream → downstream (lowercased layer names).
    order: Vec<String>,
    /// Dir-prefix → layer mappings from the `## Layering` table (lowercased).
    dirs: Vec<(String, String)>,
    /// Banned `(from, to)` pairs from `LAYER-RULE: from !-> to` lines.
    banned: Vec<(String, String)>,
}

impl LayerSpec {
    /// Whether the doc declared anything checkable.
    fn is_empty(&self) -> bool {
        self.order.len() < 2 && self.banned.is_empty()
    }
}

/// Verify the repo-map import edges against the architecture doc's layering
/// declaration (`UD-CODE-006b`). No doc / no declaration / no resolved edges
/// → empty (fail-open). Bounded by the repo map's own scan caps (and its
/// mtime cache keeps repeat calls cheap).
fn layer_findings(root: &Path, slug: &str) -> Vec<Finding> {
    let doc_rel = format!("output/{slug}-architecture.md");
    let Ok(doc) =
        crate::bounded_fs::read_utf8_beneath(root, &root.join(&doc_rel), MAX_LAYER_DOC_BYTES)
    else {
        return Vec::new();
    };
    let spec = parse_layer_spec(&doc);
    if spec.is_empty() {
        return Vec::new();
    }
    let index = umadev_knowledge::repomap::symbol_index(root);
    if index.edges.is_empty() {
        return Vec::new();
    }
    let names = spec_layer_names(&spec);
    let mut out = Vec::new();
    let mut seen: HashSet<(usize, usize)> = HashSet::new();
    for &(from, to) in &index.edges {
        if out.len() >= MAX_LAYER_FINDINGS {
            break;
        }
        let (Some(fa), Some(fb)) = (index.files.get(from), index.files.get(to)) else {
            continue; // out-of-range edge → skip (fail-open)
        };
        if !seen.insert((from, to)) {
            continue;
        }
        let (ra, rb) = (&fa.rel_path, &fb.rel_path);
        if is_exempt(ra) || is_exempt(rb) {
            continue; // a test importing anything is not an architecture fact
        }
        let (Some(la), Some(lb)) = (
            layer_of(&ra.to_ascii_lowercase(), &spec, &names),
            layer_of(&rb.to_ascii_lowercase(), &spec, &names),
        ) else {
            continue; // unlayered file → no rule applies
        };
        if la == lb {
            continue;
        }
        if spec.banned.iter().any(|(a, b)| *a == la && *b == lb) {
            out.push(Finding {
                blocking: true,
                message: format!(
                    "arch-fitness: banned dependency — {ra} (layer '{la}') imports {rb} \
                     (layer '{lb}'), but {doc_rel} declares 'LAYER-RULE: {la} !-> {lb}'; \
                     remove or invert this import"
                ),
                file: ra.clone(),
                rule_id: RULE_LAYER,
            });
            continue;
        }
        if let (Some(ia), Some(ib)) = (
            spec.order.iter().position(|l| *l == la),
            spec.order.iter().position(|l| *l == lb),
        ) {
            if ia > ib {
                let chain = spec.order.join(" -> ");
                out.push(Finding {
                    blocking: true,
                    message: format!(
                        "arch-fitness: layer violation — {ra} (layer '{la}') imports {rb} \
                         (layer '{lb}') AGAINST the one-way order '{chain}' declared in \
                         {doc_rel}; dependencies must flow {chain} — invert this import \
                         (move the shared piece downstream or depend on an interface)"
                    ),
                    file: ra.clone(),
                    rule_id: RULE_LAYER,
                });
            }
        }
    }
    out
}

/// Every layer name the spec mentions (order + banned pairs + table layers).
fn spec_layer_names(spec: &LayerSpec) -> HashSet<String> {
    let mut names: HashSet<String> = spec.order.iter().cloned().collect();
    for (a, b) in &spec.banned {
        names.insert(a.clone());
        names.insert(b.clone());
    }
    for (_, l) in &spec.dirs {
        names.insert(l.clone());
    }
    names
}

/// Parse the layering declaration out of the architecture doc (see the module
/// docs for the convention). Tolerant + fail-open: an unrecognized line is
/// simply skipped.
fn parse_layer_spec(doc: &str) -> LayerSpec {
    let mut spec = LayerSpec::default();
    let mut in_layering = false;
    for raw in doc.lines() {
        let line = raw.trim();
        // `LAYER-RULE: a !-> b` — anywhere in the doc, case-insensitive prefix.
        // `line.get(..11)` (not `line[..11]`) because a >=11-BYTE line is not a
        // >=11-CHAR one: byte 11 lands mid-character on CJK text (a 3-byte Chinese
        // glyph), and a raw byte-slice there panics. Architecture docs for a
        // Chinese-first product are overwhelmingly Chinese, so this is the common
        // input, not an edge case — and this floor runs OUTSIDE the governance
        // catch_unwind, so a panic here is a fail-open violation that can abort the
        // host run. `get` yields None on a non-boundary and the rule simply skips.
        if line
            .get(..11)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("layer-rule:"))
        {
            if let Some((a, b)) = line[11..].split_once("!->") {
                let (a, b) = (clean_layer_name(a), clean_layer_name(b));
                if !a.is_empty() && !b.is_empty() {
                    spec.banned.push((a, b));
                }
            }
            continue;
        }
        if line.starts_with('#') {
            let h = line.trim_start_matches('#').trim().to_ascii_lowercase();
            in_layering = h.contains("layering") || h.contains("分层");
            continue;
        }
        if !in_layering {
            continue;
        }
        // Table row: `| dir | layer |`.
        if line.starts_with('|') {
            let cells: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
            if cells.len() < 2 {
                continue;
            }
            let dir_raw = cells[0].trim_matches('`').trim();
            let layer = clean_layer_name(cells[1]);
            let is_separator =
                !dir_raw.is_empty() && dir_raw.chars().all(|c| matches!(c, '-' | ':' | ' '));
            let dl = dir_raw.to_ascii_lowercase();
            let is_header = matches!(dl.as_str(), "dir" | "directory" | "path" | "目录")
                || matches!(layer.as_str(), "layer" | "层");
            if is_separator || is_header || dir_raw.is_empty() || layer.is_empty() {
                continue;
            }
            let mut dir = dl.replace('\\', "/");
            while dir.ends_with('/') {
                dir.pop();
            }
            let dir = dir.trim_start_matches("./").to_string();
            if !dir.is_empty() {
                spec.dirs.push((dir, layer));
            }
            continue;
        }
        // Order chain (the FIRST ` -> ` chain in the section wins).
        if spec.order.len() < 2 && line.contains("->") && !line.contains("!->") {
            let mut body = line;
            // Strip an optional label (`Order:` / `顺序：`) before the chain.
            for colon in [":", "："] {
                if let (Some(c), Some(a)) = (body.find(colon), body.find("->")) {
                    if c < a {
                        body = &body[c + colon.len()..];
                    }
                }
            }
            let parts: Vec<String> = body.split("->").map(clean_layer_name).collect();
            let plausible = parts.len() >= 2
                && parts.iter().all(|p| {
                    !p.is_empty()
                        && p.len() <= 40
                        && p.chars()
                            .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
                });
            if plausible {
                spec.order = parts;
            }
        }
    }
    spec
}

/// Normalize one layer-name token: strip bullets/backticks/emphasis, trim,
/// lowercase.
fn clean_layer_name(s: &str) -> String {
    s.trim()
        .trim_start_matches(['-', '*', '>'])
        .trim_matches('`')
        .trim_matches('*')
        .trim()
        .to_ascii_lowercase()
}

/// The layer a (lowercased, `/`-separated, workspace-relative) file belongs
/// to: the LONGEST matching dir prefix from the table wins; otherwise a path
/// segment equal to a declared layer name (or its `+"s"` plural) matches.
fn layer_of(rel_lower: &str, spec: &LayerSpec, names: &HashSet<String>) -> Option<String> {
    let mut best: Option<(usize, &str)> = None;
    for (dir, layer) in &spec.dirs {
        let matches = rel_lower == dir
            || (rel_lower.len() > dir.len()
                && rel_lower.starts_with(dir.as_str())
                && rel_lower.as_bytes()[dir.len()] == b'/');
        if matches && best.is_none_or(|(l, _)| dir.len() > l) {
            best = Some((dir.len(), layer));
        }
    }
    if let Some((_, layer)) = best {
        return Some(layer.to_string());
    }
    for seg in rel_lower.split('/') {
        if names.contains(seg) {
            return Some(seg.to_string());
        }
        if let Some(singular) = seg.strip_suffix('s') {
            if names.contains(singular) {
                return Some(singular.to_string());
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Rule 3 — clone gate (advisory)
// ---------------------------------------------------------------------------

/// Advisory clone findings for the touched files: a normalized 5-line window
/// of (added, when a baseline is given) code that also appears in ANOTHER
/// file yields one advisory per (touched, sibling) pair, naming the sibling
/// location. Bounded ([`MAX_CLONE_TOUCHED`] / [`MAX_CLONES_PER_FILE`] /
/// [`MAX_CLONE_FINDINGS`]) and skipped entirely when the window budget blew
/// (`clone_ok == false`).
fn clone_findings(
    now: &ArchScan,
    touched: &[String],
    before: Option<&ArchBaseline>,
) -> Vec<Finding> {
    if !now.clone_ok || touched.is_empty() {
        return Vec::new();
    }
    // Sibling map: window hash → up to 2 distinct locations (so a self-match
    // can still fall through to the other file).
    let mut sibling: HashMap<u64, Vec<(&str, u32)>> = HashMap::new();
    for (rel, f) in &now.files {
        for (h, line) in &f.windows {
            let entry = sibling.entry(*h).or_default();
            if entry.len() < 2 && !entry.iter().any(|(r, _)| *r == rel.as_str()) {
                entry.push((rel.as_str(), *line));
            }
        }
    }
    let mut out = Vec::new();
    let mut reported: HashSet<(String, String)> = HashSet::new();
    for rel in touched.iter().take(MAX_CLONE_TOUCHED) {
        if out.len() >= MAX_CLONE_FINDINGS {
            break;
        }
        let Some(f) = now.files.get(rel) else {
            continue;
        };
        let before_windows = before.and_then(|b| b.files.get(rel)).map(|b| &b.windows);
        let mut per_file = 0usize;
        // Deterministic order: report the earliest-added window first.
        let mut added: Vec<(u32, u64)> = f
            .windows
            .iter()
            .filter(|(h, _)| before_windows.is_none_or(|bw| !bw.contains_key(*h)))
            .map(|(h, line)| (*line, *h))
            .collect();
        added.sort_unstable();
        for (line, h) in added {
            if per_file >= MAX_CLONES_PER_FILE || out.len() >= MAX_CLONE_FINDINGS {
                break;
            }
            let Some(locs) = sibling.get(&h) else {
                continue;
            };
            let Some((sib, sib_line)) = locs.iter().find(|(r, _)| *r != rel.as_str()) else {
                continue;
            };
            if !reported.insert((rel.clone(), (*sib).to_string())) {
                continue;
            }
            per_file += 1;
            out.push(Finding {
                blocking: false,
                message: format!(
                    "arch-fitness: {rel}:{line} duplicates a block (>= {CLONE_WINDOW} \
                     normalized lines) that also lives at {sib}:{sib_line} — reuse \
                     {sib}:{sib_line} (extract a shared helper) instead of copying it"
                ),
                file: rel.clone(),
                rule_id: RULE_CLONE,
            });
        }
    }
    out
}

/// Normalize a file body into clone-comparable lines: strip full-line and
/// `/* … */` block comments (crude state machine — a miss only costs advisory
/// precision), squeeze ALL whitespace out of each line, and drop lines
/// shorter than [`MIN_LINE_CHARS`] (brace/paren runs). Returns
/// `(1-based original line, normalized text)` pairs.
fn normalized_lines(content: &str) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    let mut in_block = false;
    for (i, raw) in content.lines().enumerate() {
        let mut line = raw.trim().to_string();
        if in_block {
            let Some(e) = line.find("*/") else {
                continue; // still inside the block comment
            };
            line = line[e + 2..].trim().to_string();
            in_block = false;
        }
        // Inline `/* … */` (possibly repeated); an unclosed opener spills to
        // the following lines.
        while let Some(s) = line.find("/*") {
            if let Some(rel_e) = line[s..].find("*/") {
                let e = s + rel_e + 2;
                line = format!("{}{}", &line[..s], &line[e..]);
            } else {
                line.truncate(s);
                in_block = true;
            }
        }
        let trimmed = line.trim();
        if trimmed.is_empty()
            || trimmed.starts_with("//")
            || trimmed.starts_with('#')
            || trimmed.starts_with("--")
            || trimmed.starts_with('*')
        {
            continue;
        }
        let squeezed: String = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
        if squeezed.chars().count() < MIN_LINE_CHARS {
            continue;
        }
        let line_no = u32::try_from(i + 1).unwrap_or(u32::MAX);
        out.push((line_no, squeezed));
    }
    out
}

/// Hash the normalized [`CLONE_WINDOW`]-line windows of `content` (window
/// hash → 1-based start line of the first occurrence), keeping only windows
/// whose joined text is distinctive enough ([`MIN_WINDOW_CHARS`]). Capped at
/// [`MAX_WINDOWS_PER_FILE`].
fn windows_of(content: &str) -> HashMap<u64, u32> {
    let lines = normalized_lines(content);
    let mut out = HashMap::new();
    if lines.len() < CLONE_WINDOW {
        return out;
    }
    for w in lines.windows(CLONE_WINDOW) {
        if out.len() >= MAX_WINDOWS_PER_FILE {
            break;
        }
        let joined: usize = w.iter().map(|(_, s)| s.len() + 1).sum();
        if joined < MIN_WINDOW_CHARS {
            continue;
        }
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for (_, s) in w {
            for b in s.as_bytes() {
                h ^= u64::from(*b);
                h = h.wrapping_mul(0x0100_0000_01b3);
            }
            h ^= u64::from(b'\n');
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        out.entry(h).or_insert(w[0].0);
    }
    out
}

// ---------------------------------------------------------------------------
// Rule 4 — comment hygiene (advisory)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommentStyle {
    Slash,
    Hash,
    Other,
}

#[derive(Debug, Clone)]
enum ClassifiedLine {
    Ordinary { hash: u64, style: CommentStyle },
    Exempt,
    Blank,
    Code,
}

#[derive(Debug, Clone, Copy)]
enum LineCommentSyntax {
    Slash,
    Hash,
    SlashAndHash,
    Dash,
}

#[derive(Debug, Clone, Copy)]
enum BlockCommentSyntax {
    None,
    C,
    CAndHtml,
    Ruby,
    Lua,
}

#[derive(Debug, Clone, Copy)]
struct CommentSyntax {
    line: LineCommentSyntax,
    block: BlockCommentSyntax,
}

impl CommentSyntax {
    fn slash(self) -> bool {
        matches!(
            self.line,
            LineCommentSyntax::Slash | LineCommentSyntax::SlashAndHash
        )
    }

    fn hash(self) -> bool {
        matches!(
            self.line,
            LineCommentSyntax::Hash | LineCommentSyntax::SlashAndHash
        )
    }

    fn dash(self) -> bool {
        matches!(self.line, LineCommentSyntax::Dash)
    }

    fn c_block(self) -> bool {
        matches!(
            self.block,
            BlockCommentSyntax::C | BlockCommentSyntax::CAndHtml
        )
    }

    fn html_block(self) -> bool {
        matches!(self.block, BlockCommentSyntax::CAndHtml)
    }

    fn ruby_block(self) -> bool {
        matches!(self.block, BlockCommentSyntax::Ruby)
    }
}

#[derive(Debug, Clone, Copy)]
struct BlockState {
    end: &'static str,
    ordinary: bool,
}

fn comment_syntax(rel: &str) -> Option<CommentSyntax> {
    let ext = rel.rsplit_once('.')?.1.to_ascii_lowercase();
    let syntax = match ext.as_str() {
        "py" | "ex" | "exs" => CommentSyntax {
            line: LineCommentSyntax::Hash,
            block: BlockCommentSyntax::None,
        },
        "rb" => CommentSyntax {
            line: LineCommentSyntax::Hash,
            block: BlockCommentSyntax::Ruby,
        },
        "lua" => CommentSyntax {
            line: LineCommentSyntax::Dash,
            block: BlockCommentSyntax::Lua,
        },
        "php" => CommentSyntax {
            line: LineCommentSyntax::SlashAndHash,
            block: BlockCommentSyntax::C,
        },
        "vue" | "svelte" | "astro" => CommentSyntax {
            line: LineCommentSyntax::Slash,
            block: BlockCommentSyntax::CAndHtml,
        },
        ext if SRC_EXT.contains(&ext) => CommentSyntax {
            line: LineCommentSyntax::Slash,
            block: BlockCommentSyntax::C,
        },
        _ => return None,
    };
    Some(syntax)
}

fn generated_header(content: &str) -> bool {
    content.lines().take(20).any(|raw| {
        let line = raw.trim().to_ascii_lowercase();
        let comment_header = line.starts_with("//")
            || line.starts_with('#')
            || line.starts_with("/*")
            || line.starts_with('*')
            || line.starts_with("<!--")
            || line.starts_with("--");
        comment_header
            && (line.contains("@generated")
                || line.contains("<auto-generated")
                || (line.contains("code generated") && line.contains("do not edit"))
                || (line.contains("generated by") && line.contains("do not edit"))
                || (line.contains("auto-generated") && line.contains("do not edit")))
    })
}

fn license_header(content: &str) -> bool {
    content.lines().take(128).any(|raw| {
        let line = raw.trim().to_ascii_lowercase();
        let comment_header = line.starts_with("//")
            || line.starts_with('#')
            || line.starts_with("/*")
            || line.starts_with('*')
            || line.starts_with("<!--")
            || line.starts_with("--");
        comment_header
            && (line.contains("spdx-license-identifier")
                || line.contains("copyright")
                || line.contains("licensed under")
                || line.contains("permission is hereby granted")
                || line.contains("apache license")
                || line.contains("gnu general public license")
                || line.contains("mozilla public license"))
    })
}

fn comment_directive(trimmed: &str) -> bool {
    let body = trimmed
        .trim_start_matches(['/', '#', '-', '*', '!', ':'])
        .trim()
        .to_ascii_lowercase();
    [
        "@ts-",
        "+build",
        "biome-ignore",
        "clang-format",
        "eslint-",
        "fmt:",
        "go:",
        "ktlint-",
        "mypy:",
        "noqa",
        "nolint",
        "nosec",
        "noinspection",
        "pylint:",
        "pyright:",
        "region",
        "endregion",
        "ruff:",
        "swiftlint:",
        "type:",
    ]
    .iter()
    .any(|prefix| body.starts_with(prefix))
}

fn comment_hash(trimmed: &str) -> u64 {
    let mut normalized = String::with_capacity(trimmed.len());
    let mut whitespace = false;
    for ch in trimmed.chars() {
        if ch.is_whitespace() {
            whitespace = true;
        } else {
            if whitespace && !normalized.is_empty() {
                normalized.push(' ');
            }
            whitespace = false;
            normalized.extend(ch.to_lowercase());
        }
    }
    fnv(normalized.as_bytes())
}

fn block_opener(
    trimmed: &str,
    syntax: CommentSyntax,
    header_exempt: bool,
) -> Option<(&'static str, &'static str, bool)> {
    if syntax.c_block() && trimmed.starts_with("/*") {
        let doc = trimmed.starts_with("/**") || trimmed.starts_with("/*!");
        return Some((
            "/*",
            "*/",
            !doc && !header_exempt && !comment_directive(trimmed),
        ));
    }
    if syntax.html_block() && trimmed.starts_with("<!--") {
        return Some(("<!--", "-->", !header_exempt));
    }
    if matches!(syntax.block, BlockCommentSyntax::Lua) && trimmed.starts_with("--[[") {
        return Some(("--[[", "]]", !header_exempt));
    }
    None
}

fn api_declaration(rel: &str, trimmed: &str) -> bool {
    let ext = rel
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "go" => ["package ", "func ", "type ", "var ", "const "]
            .iter()
            .any(|prefix| trimmed.starts_with(prefix)),
        "rb" => ["class ", "module ", "def ", "attr_"]
            .iter()
            .any(|prefix| trimmed.starts_with(prefix)),
        _ => false,
    }
}

fn exclude_implicit_api_docs(rel: &str, raw: &[&str], lines: &mut [ClassifiedLine]) {
    let mut index = 0usize;
    while index < lines.len() {
        let style = if let Some(ClassifiedLine::Ordinary { style, .. }) = lines.get(index) {
            *style
        } else {
            index += 1;
            continue;
        };
        let start = index;
        while matches!(
            lines.get(index),
            Some(ClassifiedLine::Ordinary { style: current, .. }) if *current == style
        ) {
            index += 1;
        }
        let mut next = index;
        while matches!(lines.get(next), Some(ClassifiedLine::Exempt)) {
            next += 1;
        }
        let eligible_style = match rel
            .rsplit_once('.')
            .map(|(_, ext)| ext.to_ascii_lowercase())
            .as_deref()
        {
            Some("go") => style == CommentStyle::Slash,
            Some("rb") => style == CommentStyle::Hash,
            _ => false,
        };
        if eligible_style
            && raw
                .get(next)
                .is_some_and(|line| api_declaration(rel, line.trim()))
        {
            lines[start..index].fill(ClassifiedLine::Exempt);
        }
    }
}

fn finish_comment_run(stats: &mut CommentStats, start: u32, run: &mut Vec<u64>) {
    if run.is_empty() {
        return;
    }
    if run.len() > stats.max_run {
        stats.max_run = run.len();
        stats.max_run_start = start;
    }
    stats.runs.push(CommentRun {
        start,
        lines: std::mem::take(run),
    });
}

/// Language-aware, comment-only-line classifier. Unknown, generated, oversized,
/// and non-UTF-8 inputs produce the default record and therefore no advisory.
fn comment_stats(content: &str, rel: &str) -> CommentStats {
    let Some(syntax) = comment_syntax(rel) else {
        return CommentStats::default();
    };
    if generated_header(content) {
        return CommentStats::default();
    }
    let raw: Vec<&str> = content.lines().collect();
    let has_license = license_header(content);
    let mut block: Option<BlockState> = None;
    let mut code_seen = false;
    let mut classified = Vec::with_capacity(raw.len());

    for (index, line) in raw.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            classified.push(ClassifiedLine::Blank);
            continue;
        }
        if index == 0 && trimmed.starts_with("#!") {
            classified.push(ClassifiedLine::Exempt);
            continue;
        }
        let header_exempt = has_license && !code_seen;
        if let Some(state) = block {
            if let Some(end) = trimmed.find(state.end) {
                block = None;
                if trimmed[end + state.end.len()..].trim().is_empty() {
                    if state.ordinary {
                        classified.push(ClassifiedLine::Ordinary {
                            hash: comment_hash(trimmed),
                            style: CommentStyle::Other,
                        });
                    } else {
                        classified.push(ClassifiedLine::Exempt);
                    }
                } else {
                    code_seen = true;
                    classified.push(ClassifiedLine::Code);
                }
            } else if state.ordinary {
                classified.push(ClassifiedLine::Ordinary {
                    hash: comment_hash(trimmed),
                    style: CommentStyle::Other,
                });
            } else {
                classified.push(ClassifiedLine::Exempt);
            }
            continue;
        }

        if syntax.ruby_block() && line.starts_with("=begin") {
            block = Some(BlockState {
                end: "=end",
                ordinary: !header_exempt,
            });
            classified.push(if header_exempt {
                ClassifiedLine::Exempt
            } else {
                ClassifiedLine::Ordinary {
                    hash: comment_hash(trimmed),
                    style: CommentStyle::Other,
                }
            });
            continue;
        }

        if let Some((open, end, ordinary)) = block_opener(trimmed, syntax, header_exempt) {
            let tail = &trimmed[open.len()..];
            if let Some(end_at) = tail.find(end) {
                let suffix = tail[end_at + end.len()..].trim();
                if !suffix.is_empty() {
                    code_seen = true;
                    classified.push(ClassifiedLine::Code);
                } else if ordinary {
                    classified.push(ClassifiedLine::Ordinary {
                        hash: comment_hash(trimmed),
                        style: CommentStyle::Other,
                    });
                } else {
                    classified.push(ClassifiedLine::Exempt);
                }
            } else {
                block = Some(BlockState { end, ordinary });
                classified.push(if ordinary {
                    ClassifiedLine::Ordinary {
                        hash: comment_hash(trimmed),
                        style: CommentStyle::Other,
                    }
                } else {
                    ClassifiedLine::Exempt
                });
            }
            continue;
        }

        let line_comment = if syntax.slash() && trimmed.starts_with("//") {
            let exempt = header_exempt
                || trimmed.starts_with("///")
                || trimmed.starts_with("//!")
                || comment_directive(trimmed);
            Some((CommentStyle::Slash, exempt))
        } else if syntax.hash() && trimmed.starts_with('#') {
            let exempt = header_exempt
                || trimmed.starts_with("#!")
                || trimmed.starts_with("#[")
                || trimmed.starts_with("#:")
                || (index < 2 && (trimmed.contains("coding:") || trimmed.contains("coding=")))
                || comment_directive(trimmed);
            Some((CommentStyle::Hash, exempt))
        } else if syntax.dash() && trimmed.starts_with("--") {
            let exempt = header_exempt || trimmed.starts_with("---") || comment_directive(trimmed);
            Some((CommentStyle::Other, exempt))
        } else {
            None
        };
        match line_comment {
            Some((_, true)) => classified.push(ClassifiedLine::Exempt),
            Some((style, false)) => classified.push(ClassifiedLine::Ordinary {
                hash: comment_hash(trimmed),
                style,
            }),
            None => {
                code_seen = true;
                classified.push(ClassifiedLine::Code);
            }
        }
    }

    if matches!(
        rel.rsplit_once('.')
            .map(|(_, ext)| ext.to_ascii_lowercase())
            .as_deref(),
        Some("go" | "rb")
    ) {
        exclude_implicit_api_docs(rel, &raw, &mut classified);
    }
    let mut stats = CommentStats::default();
    let mut run = Vec::new();
    let mut run_start = 0u32;
    for (index, line) in classified.into_iter().enumerate() {
        match line {
            ClassifiedLine::Ordinary { hash, .. } => {
                if run.is_empty() {
                    run_start = u32::try_from(index + 1).unwrap_or(u32::MAX);
                }
                stats.ordinary += 1;
                run.push(hash);
            }
            ClassifiedLine::Code => {
                finish_comment_run(&mut stats, run_start, &mut run);
                stats.code += 1;
            }
            ClassifiedLine::Exempt | ClassifiedLine::Blank => {
                finish_comment_run(&mut stats, run_start, &mut run);
            }
        }
    }
    finish_comment_run(&mut stats, run_start, &mut run);
    stats
}

fn added_comment_evidence(
    after: &CommentStats,
    prior: Option<&CommentStats>,
) -> (usize, Option<(u32, usize)>) {
    let Some(prior) = prior else {
        let run = after
            .runs
            .iter()
            .find(|run| run.lines.len() >= LONG_COMMENT_RUN)
            .map(|run| (run.start, run.lines.len()));
        return (after.ordinary, run);
    };
    let mut remaining = HashMap::new();
    for hash in prior.runs.iter().flat_map(|run| &run.lines) {
        *remaining.entry(*hash).or_insert(0usize) += 1;
    }
    let mut added = 0usize;
    let mut first_long_added = None;
    for run in &after.runs {
        let mut consecutive = 0usize;
        let mut consecutive_start = run.start;
        for (offset, hash) in run.lines.iter().enumerate() {
            let reused = remaining.get_mut(hash).is_some_and(|count| {
                if *count == 0 {
                    false
                } else {
                    *count -= 1;
                    true
                }
            });
            if reused {
                consecutive = 0;
            } else {
                added += 1;
                if consecutive == 0 {
                    consecutive_start = run
                        .start
                        .saturating_add(u32::try_from(offset).unwrap_or(u32::MAX));
                }
                consecutive += 1;
                if consecutive >= LONG_COMMENT_RUN && first_long_added.is_none() {
                    first_long_added = Some((consecutive_start, consecutive));
                }
            }
        }
    }
    (added, first_long_added)
}

fn comment_hygiene_findings(
    now: &ArchScan,
    touched: &[String],
    before: Option<&ArchBaseline>,
) -> Vec<Finding> {
    let mut out = Vec::new();
    for rel in touched {
        if out.len() >= MAX_COMMENT_FINDINGS {
            break;
        }
        let Some(after) = now.files.get(rel).map(|f| &f.comments) else {
            continue;
        };
        let prior = before.and_then(|b| b.files.get(rel)).map(|f| &f.comments);
        let (added, added_long_run) = added_comment_evidence(after, prior);
        let grew = prior.is_none_or(|p| after.ordinary > p.ordinary);
        let crossed_long_run =
            after.max_run >= LONG_COMMENT_RUN && prior.is_none_or(|p| p.max_run < LONG_COMMENT_RUN);
        let new_long_run = if grew && added_long_run.is_some() {
            added_long_run
        } else if crossed_long_run {
            after
                .runs
                .iter()
                .find(|run| run.lines.len() >= LONG_COMMENT_RUN)
                .map(|run| (run.start, run.lines.len()))
        } else {
            None
        };
        let ratio = after.ordinary >= COMMENT_RATIO_MIN && after.ordinary > after.code;
        let prior_ratio =
            prior.is_some_and(|p| p.ordinary >= COMMENT_RATIO_MIN && p.ordinary > p.code);
        let newly_worse_ratio =
            ratio && grew && added > 0 && (!prior_ratio || added >= COMMENT_RATIO_MIN);
        if new_long_run.is_none() && !newly_worse_ratio {
            continue;
        }
        let (line, reported_run) =
            new_long_run.unwrap_or((after.max_run_start.max(1), after.max_run));
        out.push(Finding {
            blocking: false,
            message: format!(
                "arch-fitness: {rel}:{line} has comment narration heavier than the code \
                 ({} ordinary comment lines, {} newly added, {} code lines, longest run {}) — keep comments \
                 for why/invariants and move history or repair narration to the change report",
                after.ordinary, added, after.code, reported_run
            ),
            file: rel.clone(),
            rule_id: RULE_COMMENT_HYGIENE,
        });
    }
    out
}

/// FNV-1a content hash (equality-only, non-cryptographic).
fn fnv(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Serializes the tests that READ or MUTATE `UMADEV_ARCH_MAX_FILE_LINES`
    /// (process env is global, tests run multi-threaded). Every god-file test
    /// takes this, so the env-override test can never race a ceiling read.
    /// Poison-tolerant.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn ceiling_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, body).unwrap();
    }

    /// A file of `n` distinct, real-looking code lines.
    fn code_lines(n: usize) -> String {
        (0..n)
            .map(|i| format!("pub fn generated_symbol_{i}(x: u32) -> u32 {{ x + {i} }}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    // ---------------- god-file gate (UD-CODE-006a) ----------------

    #[test]
    fn new_600_line_file_blocks_with_a_split_directive() {
        let _guard = ceiling_lock();
        let tmp = TempDir::new().unwrap();
        let before = baseline(tmp.path()); // empty tree
        write(tmp.path(), "src/huge.rs", &code_lines(600));
        let findings = arch_fitness_findings_since(tmp.path(), "demo", &before);
        let god: Vec<_> = findings
            .iter()
            .filter(|f| f.rule_id == RULE_GOD_FILE)
            .collect();
        assert_eq!(god.len(), 1, "{findings:?}");
        assert!(god[0].blocking, "a god file is a blocking finding");
        assert!(god[0].file.contains("src/huge.rs"));
        assert!(
            god[0].message.contains("split it by feature/domain")
                && god[0].message.contains("one giant file"),
            "the finding carries the split directive: {}",
            god[0].message
        );
    }

    #[test]
    fn new_300_line_file_is_fine() {
        let _guard = ceiling_lock();
        let tmp = TempDir::new().unwrap();
        let before = baseline(tmp.path());
        write(tmp.path(), "src/ok.rs", &code_lines(300));
        let findings = arch_fitness_findings_since(tmp.path(), "demo", &before);
        assert!(
            !findings.iter().any(|f| f.rule_id == RULE_GOD_FILE),
            "300 lines is under the new-file ceiling: {findings:?}"
        );
    }

    #[test]
    fn a_600_line_test_file_is_exempt() {
        let _guard = ceiling_lock();
        let tmp = TempDir::new().unwrap();
        let before = baseline(tmp.path());
        write(tmp.path(), "src/app.test.ts", &code_lines(600));
        write(tmp.path(), "tests/big_suite.rs", &code_lines(900));
        let findings = arch_fitness_findings_since(tmp.path(), "demo", &before);
        assert!(
            findings.is_empty(),
            "test files never trip the god-file gate: {findings:?}"
        );
    }

    #[test]
    fn a_file_that_grows_past_800_blocks_but_an_edit_to_an_already_big_file_does_not() {
        let _guard = ceiling_lock();
        let tmp = TempDir::new().unwrap();
        // Pre-existing: one file just under the ceiling, one already far over.
        write(tmp.path(), "src/growing.rs", &code_lines(780));
        write(tmp.path(), "src/legacy.rs", &code_lines(900));
        let before = baseline(tmp.path());
        // The step grows one past the ceiling and lightly edits the legacy giant.
        write(tmp.path(), "src/growing.rs", &code_lines(820));
        write(
            tmp.path(),
            "src/legacy.rs",
            &format!("{}\n// touched\n", code_lines(900)),
        );
        let findings = arch_fitness_findings_since(tmp.path(), "demo", &before);
        let god: Vec<_> = findings
            .iter()
            .filter(|f| f.rule_id == RULE_GOD_FILE)
            .collect();
        assert_eq!(god.len(), 1, "{findings:?}");
        assert!(
            god[0].file.contains("growing.rs") && god[0].message.contains("grew past"),
            "only the file that CROSSED the ceiling blocks (a light edit to a \
             pre-existing giant is not this step's doing): {}",
            god[0].message
        );
    }

    #[test]
    fn baseline_less_entry_fires_only_on_the_hard_ceiling() {
        let _guard = ceiling_lock();
        // Without a baseline, newness is unknowable — a touched 600-line file
        // must NOT block (it could be a pre-existing file with a 1-line fix),
        // but a touched file over the grown ceiling still does.
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "src/mid.rs", &code_lines(600));
        write(tmp.path(), "src/huge.rs", &code_lines(900));
        let touched = vec![
            tmp.path().join("src/mid.rs"),
            tmp.path().join("src/huge.rs"),
        ];
        let findings = arch_fitness_findings(tmp.path(), "demo", &touched);
        let god: Vec<_> = findings
            .iter()
            .filter(|f| f.rule_id == RULE_GOD_FILE)
            .collect();
        assert_eq!(god.len(), 1, "{findings:?}");
        assert!(god[0].file.contains("huge.rs"));
    }

    #[test]
    fn env_ceiling_override_is_honored_and_bad_values_fall_back() {
        let _guard = ceiling_lock();
        std::env::set_var("UMADEV_ARCH_MAX_FILE_LINES", "100");
        // Lowering the grown ceiling below 500 tightens the new-file bar too.
        assert_eq!(line_ceilings(), (100, 100));
        let tmp = TempDir::new().unwrap();
        let before = baseline(tmp.path());
        write(tmp.path(), "src/small_god.rs", &code_lines(120));
        let findings = arch_fitness_findings_since(tmp.path(), "demo", &before);
        assert!(
            findings.iter().any(|f| f.rule_id == RULE_GOD_FILE),
            "a 120-line new file blocks under a 100-line env ceiling: {findings:?}"
        );
        std::env::set_var("UMADEV_ARCH_MAX_FILE_LINES", "nonsense");
        assert_eq!(line_ceilings(), (NEW_FILE_MAX_LINES, GROWN_FILE_MAX_LINES));
        std::env::set_var("UMADEV_ARCH_MAX_FILE_LINES", "0");
        assert_eq!(line_ceilings(), (NEW_FILE_MAX_LINES, GROWN_FILE_MAX_LINES));
        std::env::remove_var("UMADEV_ARCH_MAX_FILE_LINES");
        assert_eq!(line_ceilings(), (NEW_FILE_MAX_LINES, GROWN_FILE_MAX_LINES));
    }

    // ---------------- layer rules (UD-CODE-006b) ----------------

    /// Seed a three-layer TS project whose repository file imports the
    /// controller — an edge AGAINST the declared order.
    fn seed_layered_violation(root: &Path) {
        write(
            root,
            "src/controller/user.ts",
            "export function userController() {}\n",
        );
        write(
            root,
            "src/service/user.ts",
            "import { userRepo } from '../repository/user';\nexport function userService() {}\n",
        );
        write(
            root,
            "src/repository/user.ts",
            "import { userController } from '../controller/user';\nexport function userRepo() {}\n",
        );
    }

    const LAYERED_DOC: &str = "# Architecture\n\n## Layering\n\n\
        | dir | layer |\n| --- | --- |\n\
        | src/controller | controller |\n\
        | src/service | service |\n\
        | src/repository | repository |\n\n\
        Order: controller -> service -> repository\n";

    #[test]
    fn an_import_edge_against_the_declared_order_blocks() {
        let tmp = TempDir::new().unwrap();
        seed_layered_violation(tmp.path());
        write(tmp.path(), "output/demo-architecture.md", LAYERED_DOC);
        let findings = arch_fitness_findings(tmp.path(), "demo", &[]);
        let layer: Vec<_> = findings
            .iter()
            .filter(|f| f.rule_id == RULE_LAYER)
            .collect();
        assert_eq!(layer.len(), 1, "{findings:?}");
        assert!(layer[0].blocking);
        assert!(
            layer[0].message.contains("src/repository/user.ts")
                && layer[0].message.contains("src/controller/user.ts")
                && layer[0]
                    .message
                    .contains("controller -> service -> repository"),
            "the finding names both files and the violated order: {}",
            layer[0].message
        );
        // The compliant edge (service → repository) raised nothing extra.
    }

    #[test]
    fn no_architecture_doc_or_no_declaration_is_a_silent_noop() {
        let tmp = TempDir::new().unwrap();
        seed_layered_violation(tmp.path());
        // No doc at all.
        assert!(arch_fitness_findings(tmp.path(), "demo", &[]).is_empty());
        // A doc WITHOUT any layering declaration.
        write(
            tmp.path(),
            "output/demo-architecture.md",
            "# Architecture\n\nJust prose, no layering contract.\n",
        );
        assert!(
            arch_fitness_findings(tmp.path(), "demo", &[]).is_empty(),
            "no declaration → the layer check silently no-ops"
        );
    }

    #[test]
    fn a_banned_pair_blocks_via_segment_name_fallback() {
        // `LAYER-RULE: ui !-> db` with NO dir table: files match by path
        // segment (`src/ui/…`, `src/db/…`).
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "src/ui/panel.ts",
            "import { query } from '../db/query';\nexport function panel() {}\n",
        );
        write(
            tmp.path(),
            "src/db/query.ts",
            "export function query() {}\n",
        );
        write(
            tmp.path(),
            "output/demo-architecture.md",
            "# Architecture\n\nLAYER-RULE: ui !-> db\n",
        );
        let findings = arch_fitness_findings(tmp.path(), "demo", &[]);
        let layer: Vec<_> = findings
            .iter()
            .filter(|f| f.rule_id == RULE_LAYER)
            .collect();
        assert_eq!(layer.len(), 1, "{findings:?}");
        assert!(
            layer[0].message.contains("LAYER-RULE: ui !-> db")
                && layer[0].message.contains("src/ui/panel.ts")
                && layer[0].message.contains("src/db/query.ts"),
            "{}",
            layer[0].message
        );
    }

    #[test]
    fn parse_layer_spec_reads_table_order_and_bans() {
        let spec = parse_layer_spec(LAYERED_DOC);
        assert_eq!(
            spec.order,
            vec!["controller", "service", "repository"],
            "the order chain is parsed"
        );
        assert_eq!(spec.dirs.len(), 3, "{:?}", spec.dirs);
        assert!(spec.banned.is_empty());
        let spec2 = parse_layer_spec("LAYER-RULE: ui !-> db\n");
        assert_eq!(spec2.banned, vec![("ui".to_string(), "db".to_string())]);
        assert!(parse_layer_spec("# nothing here\n").is_empty());
    }

    // ---------------- clone gate (UD-CODE-006c, advisory) ----------------

    /// An 8-line, distinctly non-boilerplate block.
    const SHARED_BLOCK: &str = "\
        const payload = buildRequestPayload(user, session);\n\
        const response = await client.submitOrder(payload);\n\
        if (!response.ok) { throw new OrderError(response.status); }\n\
        const parsed = OrderSchema.parse(await response.json());\n\
        recordAuditTrail('order.submitted', parsed.orderId);\n\
        metrics.increment('orders.submitted.total');\n\
        cache.invalidate(['orders', user.id]);\n\
        return parsed;\n";

    #[test]
    fn a_duplicated_block_yields_an_advisory_naming_the_sibling() {
        let tmp = TempDir::new().unwrap();
        let before = baseline(tmp.path());
        write(
            tmp.path(),
            "src/checkout.ts",
            &format!("export async function checkout(user, session) {{\n{SHARED_BLOCK}}}\n"),
        );
        write(
            tmp.path(),
            "src/reorder.ts",
            &format!("export async function reorder(user, session) {{\n{SHARED_BLOCK}}}\n"),
        );
        let findings = arch_fitness_findings_since(tmp.path(), "demo", &before);
        let clones: Vec<_> = findings
            .iter()
            .filter(|f| f.rule_id == RULE_CLONE)
            .collect();
        assert!(!clones.is_empty(), "{findings:?}");
        assert!(
            clones.iter().all(|f| !f.blocking),
            "the clone gate is ADVISORY, never blocking: {clones:?}"
        );
        assert!(
            clones.iter().any(|f| f.message.contains("reuse")
                && (f.message.contains("src/checkout.ts") || f.message.contains("src/reorder.ts"))),
            "the advisory names the sibling location: {clones:?}"
        );
    }

    #[test]
    fn unique_code_yields_no_clone_findings() {
        let tmp = TempDir::new().unwrap();
        let before = baseline(tmp.path());
        write(tmp.path(), "src/a.ts", &code_lines(60));
        write(
            tmp.path(),
            "src/b.ts",
            &(60..120)
                .map(|i| format!("export const unique_binding_{i} = compute({i});"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let findings = arch_fitness_findings_since(tmp.path(), "demo", &before);
        assert!(
            !findings.iter().any(|f| f.rule_id == RULE_CLONE),
            "unique code raises no clone advisory: {findings:?}"
        );
    }

    #[test]
    fn a_pre_existing_duplicate_is_not_re_flagged_only_added_code_is() {
        // The duplication existed BEFORE the step; the step's edit adds only
        // unique code → the added-only subtraction keeps the gate quiet.
        let tmp = TempDir::new().unwrap();
        let dup_a = format!("export async function a(user, session) {{\n{SHARED_BLOCK}}}\n");
        let dup_b = format!("export async function b(user, session) {{\n{SHARED_BLOCK}}}\n");
        write(tmp.path(), "src/a.ts", &dup_a);
        write(tmp.path(), "src/b.ts", &dup_b);
        let before = baseline(tmp.path());
        write(
            tmp.path(),
            "src/a.ts",
            &format!("{dup_a}export const freshlyAddedUniqueBinding = 42;\n"),
        );
        let findings = arch_fitness_findings_since(tmp.path(), "demo", &before);
        assert!(
            !findings.iter().any(|f| f.rule_id == RULE_CLONE),
            "pre-existing duplication is not this step's doing: {findings:?}"
        );
    }

    #[test]
    fn normalization_ignores_whitespace_and_comments() {
        let a = normalized_lines(
            "  const x = compute(1);   // trailing note\n\n/* block */ const y = x + 2;\n",
        );
        let b = normalized_lines("const x=compute(1);\nconst y=x+2;\n");
        // Hmm: `// trailing note` — full-line comments are stripped, trailing
        // line comments are KEPT (a URL in a string must not be mangled), so
        // these two only agree on the second line.
        assert_eq!(a[1].1, b[1].1, "whitespace + block comments are ignored");
        let c = normalized_lines("// only a comment\n# hash comment\n}\n);\n");
        assert!(c.is_empty(), "comments and brace runs are dropped: {c:?}");
    }

    // ---------------- comment hygiene (UD-CODE-006d, advisory) ----------------

    #[test]
    fn a_new_narration_block_is_advisory() {
        let tmp = TempDir::new().unwrap();
        let before = baseline(tmp.path());
        let narration = (1..=10)
            .map(|n| format!("// repair history line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        write(
            tmp.path(),
            "src/repair.rs",
            &format!("{narration}\npub fn repair() {{}}\npub fn verify() {{}}\n"),
        );
        let findings = arch_fitness_findings_since(tmp.path(), "demo", &before);
        let comments: Vec<_> = findings
            .iter()
            .filter(|f| f.rule_id == RULE_COMMENT_HYGIENE)
            .collect();
        assert_eq!(comments.len(), 1, "{findings:?}");
        assert!(!comments[0].blocking);
        assert!(comments[0].message.contains("why/invariants"));
    }

    #[test]
    fn comment_hygiene_never_requires_comments() {
        let tmp = TempDir::new().unwrap();
        let before = baseline(tmp.path());
        write(
            tmp.path(),
            "src/clear.rs",
            "pub fn clear() {}\npub fn concise() {}\n",
        );
        let findings = arch_fitness_findings_since(tmp.path(), "demo", &before);
        assert!(
            !findings.iter().any(|f| f.rule_id == RULE_COMMENT_HYGIENE),
            "comment-free code is valid: {findings:?}"
        );
    }

    #[test]
    fn docs_and_license_headers_are_exempt_from_comment_hygiene() {
        let tmp = TempDir::new().unwrap();
        let before = baseline(tmp.path());
        let docs = (1..=14)
            .map(|n| format!("/// Public API detail {n}."))
            .collect::<Vec<_>>()
            .join("\n");
        write(
            tmp.path(),
            "src/api.rs",
            &format!(
                "// SPDX-License-Identifier: MIT\n// Copyright 2026 Example\n{docs}\npub fn api() {{}}\n"
            ),
        );
        let findings = arch_fitness_findings_since(tmp.path(), "demo", &before);
        assert!(
            !findings.iter().any(|f| f.rule_id == RULE_COMMENT_HYGIENE),
            "API docs and license headers are not narration: {findings:?}"
        );
    }

    #[test]
    fn ordinary_block_comments_are_governed_but_doc_blocks_are_exempt() {
        let tmp = TempDir::new().unwrap();
        let before = baseline(tmp.path());
        write(
            tmp.path(),
            "src/repair.js",
            "/*\n * repair one\n * repair two\n * repair three\n * repair four\n * repair five\n * repair six\n * repair seven\n */\nfunction repair() {}\n",
        );
        let findings = arch_fitness_findings_since(tmp.path(), "demo", &before);
        assert!(findings.iter().any(|f| f.rule_id == RULE_COMMENT_HYGIENE));

        let clean = TempDir::new().unwrap();
        let clean_before = baseline(clean.path());
        write(
            clean.path(),
            "src/api.js",
            "/**\n * Public API documentation.\n * Parameters and return value.\n * More public contract details.\n * More public contract details.\n * More public contract details.\n * More public contract details.\n * More public contract details.\n */\nexport function api() {}\n",
        );
        let findings = arch_fitness_findings_since(clean.path(), "demo", &clean_before);
        assert!(!findings.iter().any(|f| f.rule_id == RULE_COMMENT_HYGIENE));
    }

    #[test]
    fn pre_existing_comment_debt_is_not_reflagged() {
        let tmp = TempDir::new().unwrap();
        let narration = (1..=10)
            .map(|n| format!("// legacy explanation {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        write(
            tmp.path(),
            "src/legacy.rs",
            &format!("{narration}\npub fn before() {{}}\n"),
        );
        let before = baseline(tmp.path());
        write(
            tmp.path(),
            "src/legacy.rs",
            &format!("{narration}\npub fn before() {{}}\npub fn after() {{}}\n"),
        );
        let findings = arch_fitness_findings_since(tmp.path(), "demo", &before);
        assert!(
            !findings.iter().any(|f| f.rule_id == RULE_COMMENT_HYGIENE),
            "unchanged legacy comment debt is not this step's regression: {findings:?}"
        );
    }

    #[test]
    fn supported_languages_use_their_own_comment_syntax() {
        for ext in SRC_EXT {
            let rel = format!("src/sample.{ext}");
            let syntax = comment_syntax(&rel).unwrap_or_else(|| panic!("missing syntax for {ext}"));
            let prefix = if syntax.hash() {
                "#"
            } else if syntax.dash() {
                "--"
            } else {
                assert!(syntax.slash(), "{ext} needs a reliable line-comment form");
                "//"
            };
            let body = (0..LONG_COMMENT_RUN)
                .map(|n| format!("{prefix} repair narration {n}"))
                .collect::<Vec<_>>()
                .join("\n");
            let stats = comment_stats(&body, &rel);
            assert_eq!(stats.max_run, LONG_COMMENT_RUN, "wrong syntax for {ext}");
        }

        let preprocessor = (0..14)
            .map(|n| format!("#include <header_{n}.h>"))
            .collect::<Vec<_>>()
            .join("\n");
        let stats = comment_stats(&preprocessor, "src/native.cpp");
        assert_eq!(stats.ordinary, 0, "C/C++ preprocessor lines are code");
        assert_eq!(stats.code, 14);
    }

    #[test]
    fn generated_license_and_api_documentation_are_exempt() {
        for (rel, body) in [
            (
                "src/schema_runtime.go",
                format!(
                    "// Code generated by schema-tool. DO NOT EDIT.\n{}\npackage generated\n",
                    (0..12)
                        .map(|n| format!("// generated explanation {n}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                ),
            ),
            (
                "src/licensed.ts",
                format!(
                    "/*\n * Copyright 2026 Example\n * Licensed under the Apache License, Version 2.0\n{}\n */\nexport const value = 1;\n",
                    (0..12)
                        .map(|n| format!(" * license term {n}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                ),
            ),
            (
                "src/cli.js",
                format!(
                    "#!/usr/bin/env node\n// Copyright 2026 Example\n{}\nexport const value = 1;\n",
                    (0..12)
                        .map(|n| format!("// license term {n}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                ),
            ),
            (
                "src/widget.go",
                format!(
                    "{}\ntype Widget struct {{}}\n",
                    (0..10)
                        .map(|n| format!("// Widget public contract detail {n}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                ),
            ),
            (
                "src/widget.rb",
                format!(
                    "{}\nclass Widget\nend\n",
                    (0..10)
                        .map(|n| format!("# Widget public contract detail {n}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                ),
            ),
        ] {
            let tmp = TempDir::new().unwrap();
            let before = baseline(tmp.path());
            write(tmp.path(), rel, &body);
            let findings = arch_fitness_findings_since(tmp.path(), "demo", &before);
            assert!(
                !findings.iter().any(|f| f.rule_id == RULE_COMMENT_HYGIENE),
                "{rel} is exempt: {findings:?}"
            );
        }

        let detached = TempDir::new().unwrap();
        let detached_before = baseline(detached.path());
        let narration = (0..10)
            .map(|n| format!("// repair history {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        write(
            detached.path(),
            "src/widget.go",
            &format!("{narration}\n\ntype Widget struct {{}}\n"),
        );
        let findings = arch_fitness_findings_since(detached.path(), "demo", &detached_before);
        assert!(
            findings.iter().any(|f| f.rule_id == RULE_COMMENT_HYGIENE),
            "a detached Go comment block is not API documentation: {findings:?}"
        );
    }

    #[test]
    fn only_comment_only_lines_count() {
        let tmp = TempDir::new().unwrap();
        let before = baseline(tmp.path());
        let body = (0..12)
            .map(|n| format!("/* invariant {n} */ const value_{n} = {n};"))
            .collect::<Vec<_>>()
            .join("\n");
        write(tmp.path(), "src/inline.ts", &body);
        let findings = arch_fitness_findings_since(tmp.path(), "demo", &before);
        assert!(
            !findings.iter().any(|f| f.rule_id == RULE_COMMENT_HYGIENE),
            "inline comments are code lines, not comment-only narration: {findings:?}"
        );
    }

    #[test]
    fn editing_an_existing_long_block_without_growth_is_not_new_debt() {
        let tmp = TempDir::new().unwrap();
        let before_text = (0..10)
            .map(|n| format!("// legacy explanation {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        write(
            tmp.path(),
            "src/legacy.ts",
            &format!("{before_text}\nexport const value = 1;\n"),
        );
        let before = baseline(tmp.path());
        let after_text = before_text.replace("explanation 4", "clarification four");
        write(
            tmp.path(),
            "src/legacy.ts",
            &format!("{after_text}\nexport const value = 2;\n"),
        );
        let findings = arch_fitness_findings_since(tmp.path(), "demo", &before);
        assert!(
            !findings.iter().any(|f| f.rule_id == RULE_COMMENT_HYGIENE),
            "a wording edit with no comment growth is not newly gained debt: {findings:?}"
        );
    }

    #[test]
    fn crossing_the_long_run_threshold_is_advisory() {
        let tmp = TempDir::new().unwrap();
        let seven = (0..7)
            .map(|n| format!("// repair narration {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        write(
            tmp.path(),
            "src/crossing.ts",
            &format!("{seven}\nexport const value = 1;\n"),
        );
        let before = baseline(tmp.path());
        write(
            tmp.path(),
            "src/crossing.ts",
            &format!("{seven}\n// repair narration 7\nexport const value = 1;\n"),
        );
        let findings = arch_fitness_findings_since(tmp.path(), "demo", &before);
        assert!(findings
            .iter()
            .any(|f| f.rule_id == RULE_COMMENT_HYGIENE && !f.blocking));
    }

    #[test]
    fn dispersed_comment_ratio_is_governed_without_a_comment_quota() {
        let tmp = TempDir::new().unwrap();
        let comments = (0..COMMENT_RATIO_MIN)
            .map(|n| format!("// repair note {n}\n"))
            .collect::<Vec<_>>()
            .join("\n");
        let before = baseline(tmp.path());
        write(
            tmp.path(),
            "src/ratio.ts",
            &format!("{comments}\nexport const value = 1;\n"),
        );
        let findings = arch_fitness_findings_since(tmp.path(), "demo", &before);
        assert!(findings.iter().any(|f| {
            f.rule_id == RULE_COMMENT_HYGIENE
                && f.message
                    .contains(&format!("{COMMENT_RATIO_MIN} newly added"))
        }));

        let clean = TempDir::new().unwrap();
        let clean_before = baseline(clean.path());
        write(
            clean.path(),
            "src/no-comments.ts",
            "export const value = 1;\n",
        );
        assert!(
            !arch_fitness_findings_since(clean.path(), "demo", &clean_before)
                .iter()
                .any(|f| f.rule_id == RULE_COMMENT_HYGIENE)
        );
    }

    #[test]
    fn a_small_addition_to_pre_existing_ratio_debt_is_not_reflagged() {
        let tmp = TempDir::new().unwrap();
        let comments = (0..COMMENT_RATIO_MIN)
            .map(|n| format!("// legacy note {n}\n"))
            .collect::<Vec<_>>()
            .join("\n");
        write(
            tmp.path(),
            "src/ratio.ts",
            &format!("{comments}\nexport const value = 1;\n"),
        );
        let before = baseline(tmp.path());
        write(
            tmp.path(),
            "src/ratio.ts",
            &format!("{comments}\n// one new invariant\n\nexport const value = 1;\n"),
        );
        let findings = arch_fitness_findings_since(tmp.path(), "demo", &before);
        assert!(
            !findings.iter().any(|f| f.rule_id == RULE_COMMENT_HYGIENE),
            "legacy ratio debt needs a material new regression: {findings:?}"
        );
    }

    #[test]
    fn baseline_less_calls_never_assign_legacy_comment_debt() {
        let tmp = TempDir::new().unwrap();
        let narration = (0..10)
            .map(|n| format!("// legacy narration {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        write(
            tmp.path(),
            "src/legacy.ts",
            &format!("{narration}\nexport const value = 1;\n"),
        );
        let findings =
            arch_fitness_findings(tmp.path(), "demo", &[tmp.path().join("src/legacy.ts")]);
        assert!(!findings.iter().any(|f| f.rule_id == RULE_COMMENT_HYGIENE));
    }

    #[test]
    fn vendored_directories_are_exempt_case_insensitively() {
        let tmp = TempDir::new().unwrap();
        let before = baseline(tmp.path());
        let narration = (0..12)
            .map(|n| format!("// third-party narration {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        write(
            tmp.path(),
            "Vendor/dependency.ts",
            &format!("{narration}\nexport const dependency = 1;\n"),
        );
        let findings = arch_fitness_findings_since(tmp.path(), "demo", &before);
        assert!(
            findings.is_empty(),
            "vendored files are not governed: {findings:?}"
        );
    }

    #[test]
    fn uncertain_text_is_fail_open_and_results_are_deterministic() {
        let tmp = TempDir::new().unwrap();
        let before = baseline(tmp.path());
        let mut invalid = b"// narration one\n// narration two\n".to_vec();
        invalid.push(0xff);
        invalid.extend_from_slice(b"\n// narration three\n// narration four\n// narration five\n// narration six\n// narration seven\n// narration eight\n");
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/invalid.ts"), invalid).unwrap();
        let findings = arch_fitness_findings_since(tmp.path(), "demo", &before);
        assert!(!findings.iter().any(|f| f.rule_id == RULE_COMMENT_HYGIENE));

        let oversized = TempDir::new().unwrap();
        let oversized_before = baseline(oversized.path());
        write(
            oversized.path(),
            "src/oversized.ts",
            &"// uncertain oversized narration\n".repeat(MAX_FILE_BYTES / 16),
        );
        let oversized_findings =
            arch_fitness_findings_since(oversized.path(), "demo", &oversized_before);
        assert!(!oversized_findings
            .iter()
            .any(|f| f.rule_id == RULE_COMMENT_HYGIENE));

        let stable = TempDir::new().unwrap();
        let stable_before = baseline(stable.path());
        let narration = (0..10)
            .map(|n| format!("// new narration {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        write(
            stable.path(),
            "src/stable.ts",
            &format!("{narration}\nexport const value = 1;\n"),
        );
        let first = arch_fitness_findings_since(stable.path(), "demo", &stable_before);
        let second = arch_fitness_findings_since(stable.path(), "demo", &stable_before);
        assert_eq!(first, second);
    }

    // ---------------- fail-open ----------------

    #[test]
    fn fail_open_paths_yield_no_findings_and_never_panic() {
        let tmp = TempDir::new().unwrap();
        // Nonexistent root.
        let ghost = tmp.path().join("does-not-exist");
        assert!(arch_fitness_findings(&ghost, "demo", &[]).is_empty());
        assert!(arch_fitness_findings_since(&ghost, "demo", &baseline(&ghost)).is_empty());
        // The architecture doc is unreadable (it's a DIRECTORY).
        fs::create_dir_all(tmp.path().join("output/demo-architecture.md")).unwrap();
        write(tmp.path(), "src/app.ts", "export const app = 1;\n");
        assert!(arch_fitness_findings(tmp.path(), "demo", &[]).is_empty());
        // Touched paths that don't exist on disk.
        let findings = arch_fitness_findings(
            tmp.path(),
            "demo",
            &[tmp.path().join("src/never-written.ts")],
        );
        assert!(findings.is_empty(), "{findings:?}");
        // A disabled baseline reports nothing.
        let disabled = ArchBaseline {
            files: BTreeMap::new(),
            clone_ok: false,
            disabled: true,
        };
        write(tmp.path(), "src/huge.rs", &code_lines(900));
        assert!(
            arch_fitness_findings_since(tmp.path(), "demo", &disabled).is_empty(),
            "a disabled (huge-repo) baseline is a silent no-op"
        );
    }

    #[test]
    fn oversized_source_or_exhausted_aggregate_disables_the_whole_scan() {
        let oversized = TempDir::new().unwrap();
        fs::create_dir_all(oversized.path().join("src")).unwrap();
        let file = fs::File::create(oversized.path().join("src/huge.rs")).unwrap();
        file.set_len((MAX_FILE_BYTES + 1) as u64).unwrap();
        drop(file);
        assert!(baseline(oversized.path()).disabled);

        let exhausted = TempDir::new().unwrap();
        let src = exhausted.path().join("src");
        fs::create_dir_all(&src).unwrap();
        let file_count = MAX_TOTAL_BYTES / MAX_FILE_BYTES + 1;
        for index in 0..file_count {
            let file = fs::File::create(src.join(format!("part-{index}.rs"))).unwrap();
            file.set_len(MAX_FILE_BYTES as u64).unwrap();
        }
        assert!(
            baseline(exhausted.path()).disabled,
            "aggregate exhaustion must discard the scan rather than trust a partial tree"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_source_file_is_not_added_to_the_architecture_baseline() {
        use std::os::unix::fs::symlink;

        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::create_dir_all(workspace.path().join("src")).unwrap();
        let external = outside.path().join("secret.rs");
        fs::write(&external, "pub const SECRET: &str = \"outside\";\n").unwrap();
        symlink(&external, workspace.path().join("src/secret.rs")).unwrap();

        let before = baseline(workspace.path());
        assert!(!before.disabled);
        assert!(before.files.is_empty());
    }

    #[test]
    fn touched_since_reports_new_and_modified_files_only() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "src/stable.rs", "pub fn stable() {}\n");
        write(tmp.path(), "src/edited.rs", "pub fn v1() {}\n");
        let before = baseline(tmp.path());
        write(tmp.path(), "src/edited.rs", "pub fn v2() {}\n");
        write(tmp.path(), "src/fresh.rs", "pub fn fresh() {}\n");
        let touched = touched_since(tmp.path(), &before);
        let rels: Vec<String> = touched.iter().map(|p| rel_of(tmp.path(), p)).collect();
        assert!(rels.contains(&"src/edited.rs".to_string()), "{rels:?}");
        assert!(rels.contains(&"src/fresh.rs".to_string()), "{rels:?}");
        assert!(
            !rels.contains(&"src/stable.rs".to_string()),
            "an untouched file is not in the changed set: {rels:?}"
        );
    }

    #[test]
    fn exempt_classification_covers_tests_generated_and_locks() {
        for exempt in [
            "tests/suite.rs",
            "src/app.test.ts",
            "src/app.spec.js",
            "src/__tests__/x.ts",
            "src/widget_test.dart",
            "src/widget_spec.exs",
            "src/widget_tests.cs",
            "bundle.min.js",
            "proto/api_pb2.py",
            "src/schema.generated.ts",
            "types/global.d.ts",
        ] {
            assert!(is_exempt(exempt), "{exempt} should be exempt");
        }
        for real in ["src/app.ts", "src/controller/user.ts", "lib.rs"] {
            assert!(!is_exempt(real), "{real} should NOT be exempt");
        }
    }

    #[test]
    fn parse_layer_spec_never_panics_on_cjk_architecture_docs() {
        // Regression: `line[..11]` panicked whenever byte 11 landed mid-character —
        // the norm for a Chinese architecture doc (3-byte glyphs). This floor runs
        // outside the governance catch_unwind, so the panic could abort the run.
        // Every line here has >=11 bytes with byte 11 mid-glyph; parsing must be
        // panic-free and simply find no layer rules.
        let doc = "## 架构分层说明文档标题\n\
                   前端层不允许直接访问数据库连接池对象实例\n\
                   ## 领域层与基础设施层的依赖方向约束\n\
                   这一段是纯中文段落用来触发字节十一落在字符中间的情况\n";
        let spec = parse_layer_spec(doc);
        assert!(spec.banned.is_empty());

        // The real rule still parses when present (ASCII prefix, CJK layer names).
        let with_rule = "LAYER-RULE: 前端 !-> 数据库\n## 其他\n";
        let spec = parse_layer_spec(with_rule);
        assert_eq!(spec.banned.len(), 1);
    }
}
