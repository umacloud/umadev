//! Architecture-fitness floor — the deterministic anti-spaghetti gate
//! (`UD-CODE-005` clause family; the spec-prose addition to
//! `spec/UMADEV_HOST_SPEC_V1.md` is pending).
//!
//! The L0 firmware *preaches* layering, small focused modules, and no
//! copy-paste — but preaching is a prompt, not a floor. A borrowed brain under
//! pressure ships one giant file, imports the database from the UI, and pastes
//! the same block into three places, and every prior deterministic check
//! (build/test, coverage, contract, test-integrity) still reads green. This
//! module makes UmaDev's own deterministic floor *verify* architecture
//! fitness, with three rules:
//!
//! 1. **God-file gate** (`UD-CODE-005a`, blocking) — a NEW source file over
//!    500 lines, or a touched file that GREW PAST 800 lines this step, blocks
//!    with a split directive ("split by feature/domain; real teams don't ship
//!    one giant file"). The grown ceiling is overridable via
//!    `UMADEV_ARCH_MAX_FILE_LINES`; the new-file ceiling is
//!    `min(500, that ceiling)`. Generated/vendored/lock files and tests are
//!    exempt. Without a before-baseline (the plain [`arch_fitness_findings`]
//!    entry) newness cannot be known, so only the hard grown ceiling fires on
//!    a touched file — never a false block on a merely-touched legacy file.
//! 2. **Layer-dependency rules** (`UD-CODE-005b`, blocking) — the architecture
//!    doc (`output/<slug>-architecture.md`) may DECLARE a layering contract
//!    (convention below); every resolved import edge from the repo map
//!    ([`umadev_knowledge::repomap::symbol_index`], the same
//!    confidence-disciplined edges the L0 repo-map slice ranks with) is
//!    checked against it. An edge that goes AGAINST the declared one-way order
//!    or crosses a banned pair blocks, naming both files and the violated
//!    rule. No declaration in the doc → this check silently no-ops.
//! 3. **Clone gate** (`UD-CODE-005c`, ADVISORY) — normalized
//!    (whitespace-squeezed, comment-stripped) 5-line windows of code ADDED in
//!    touched files are hashed against the rest of the repo; a duplicated
//!    block ≥ 5 lines yields an advisory naming the sibling location ("reuse
//!    X:line instead"). Advisory, not blocking — deduplication judgment needs
//!    a human/critic; the floor only surfaces the evidence.
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
//! than [`MAX_SCAN_FILES`] source files, or a blown read budget) all degrade
//! to a silent skip. The gate can never fabricate a block, never error, and
//! its rework is bounded by the caller's existing fix-round counters.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::fswalk::{classify_no_follow, EntryKind};

// ---------------------------------------------------------------------------
// Rule ids + thresholds
// ---------------------------------------------------------------------------

/// Clause id of the god-file gate (rule 1). Spec prose addition pending.
pub const RULE_GOD_FILE: &str = "UD-CODE-005a";
/// Clause id of the layer-dependency gate (rule 2). Spec prose addition pending.
pub const RULE_LAYER: &str = "UD-CODE-005b";
/// Clause id of the clone gate (rule 3, advisory). Spec prose addition pending.
pub const RULE_CLONE: &str = "UD-CODE-005c";

/// Default line ceiling for a NEW source file (`UD-CODE-005a`).
const NEW_FILE_MAX_LINES: usize = 500;
/// Default line ceiling a touched file must not GROW PAST (`UD-CODE-005a`).
/// Overridable via `UMADEV_ARCH_MAX_FILE_LINES`.
const GROWN_FILE_MAX_LINES: usize = 800;

/// Hard repo-size skip: more source files than this → the god-file and clone
/// scans silently no-op (fail-open; the pass must stay fast on any repo).
const MAX_SCAN_FILES: usize = 5_000;
/// Max bytes read per file (a 512 KiB cap still counts far beyond any line
/// ceiling, so god-file detection is unaffected).
const MAX_FILE_BYTES: usize = 512 * 1024;
/// Total read budget across one scan — blown → the whole scan is discarded
/// (partial data could misclassify a pre-existing file as "new").
const MAX_TOTAL_BYTES: usize = 32 * 1024 * 1024;
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
    "hpp",
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
    /// [`RULE_CLONE`]).
    pub rule_id: &'static str,
}

// ---------------------------------------------------------------------------
// Baseline + scan (the changed-file-set source, mirroring test_integrity)
// ---------------------------------------------------------------------------

/// Per-file scan record: raw line count, content hash, and the normalized
/// 5-line clone windows (window hash → 1-based start line of its first
/// occurrence).
#[derive(Debug, Clone)]
struct FileScan {
    lines: usize,
    hash: u64,
    windows: HashMap<u64, u32>,
}

/// A point-in-time snapshot of the project's fitness-relevant source surface,
/// captured BEFORE a build step's doer turn (exactly like
/// [`crate::test_integrity::snapshot`]) and compared to the after-state by
/// [`arch_fitness_findings_since`]. `disabled == true` (huge repo / blown
/// read budget / unreadable tree at capture time) makes every later
/// comparison a silent no-op — fail-open (`UD-CODE-005`).
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
/// over [`MAX_SCAN_FILES`] source files (or a blown read budget) yields a
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
    collect(root, root, &mut paths, 0);
    if paths.len() > MAX_SCAN_FILES {
        return None;
    }
    let mut files = BTreeMap::new();
    let mut clone_ok = true;
    let mut total_bytes = 0usize;
    let mut total_windows = 0usize;
    for (rel, abs) in paths {
        let Ok(bytes) = std::fs::read(&abs) else {
            continue; // unreadable file → skip (fail-open)
        };
        total_bytes = total_bytes.saturating_add(bytes.len().min(MAX_FILE_BYTES));
        if total_bytes > MAX_TOTAL_BYTES {
            return None; // blown budget → discard (never a half-truth)
        }
        let capped = if bytes.len() > MAX_FILE_BYTES {
            &bytes[..MAX_FILE_BYTES]
        } else {
            &bytes[..]
        };
        let content = String::from_utf8_lossy(capped);
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
        files.insert(
            rel,
            FileScan {
                lines: content.lines().count(),
                hash: fnv(content.as_bytes()),
                windows,
            },
        );
    }
    Some(ArchScan { files, clone_ok })
}

/// Recursively collect fitness-relevant source files as `(rel, abs)` pairs —
/// code extensions only, skipping vendored/build dirs, dot-dirs, symlinks
/// (no-follow), and exempt (test / generated / lock) files. Collects at most
/// [`MAX_SCAN_FILES`] + 1 entries so the caller can detect the overflow.
fn collect(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>, depth: usize) {
    if depth > MAX_SCAN_DEPTH || out.len() > MAX_SCAN_FILES {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return; // unreadable dir → skip (fail-open)
    };
    for e in rd.flatten() {
        if out.len() > MAX_SCAN_FILES {
            return;
        }
        let p = e.path();
        match classify_no_follow(&p) {
            EntryKind::Dir => {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name.starts_with('.') || SKIP_DIRS.contains(&name) {
                    continue;
                }
                collect(root, &p, out, depth + 1);
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
        || name.ends_with("_test.py")
        || name.ends_with("_test.go")
        || name.ends_with("_test.rs")
        || name.ends_with("_test.ts")
        || name.ends_with("_test.js")
        || name.ends_with("_spec.rb")
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
/// (`UD-CODE-005`; spec prose pending). `touched` is the changed-file set the
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
    out
}

/// The step-level architecture-fitness check: compare the current tree to the
/// pre-step [`baseline`], derive the touched set, and run all three rules
/// with full semantics — a NEW file over the new-file ceiling or a file that
/// GREW PAST the grown ceiling blocks; a duplicated block of ADDED code is
/// advisory. Empty when the baseline is disabled or the tree cannot be
/// scanned (fail-open, `UD-CODE-005`).
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
    out
}

/// Build one god-file finding (`UD-CODE-005a`). `before_lines` is `Some` for
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
/// declaration (`UD-CODE-005b`). No doc / no declaration / no resolved edges
/// → empty (fail-open). Bounded by the repo map's own scan caps (and its
/// mtime cache keeps repeat calls cheap).
fn layer_findings(root: &Path, slug: &str) -> Vec<Finding> {
    let doc_rel = format!("output/{slug}-architecture.md");
    let Ok(doc) = std::fs::read_to_string(root.join(&doc_rel)) else {
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
        if line.len() >= 11 && line[..11].eq_ignore_ascii_case("layer-rule:") {
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

    // ---------------- god-file gate (UD-CODE-005a) ----------------

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

    // ---------------- layer rules (UD-CODE-005b) ----------------

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

    // ---------------- clone gate (UD-CODE-005c, advisory) ----------------

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
}
