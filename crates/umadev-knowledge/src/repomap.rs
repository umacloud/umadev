//! `repomap` — a dependency-light **repo-map / symbol graph** over the
//! *user's* code (NOT the knowledge corpus).
//!
//! ## Why this exists (Wave 3, Gap G4)
//! On a real repository UmaDev currently adds nothing over the bare base: the
//! base sees only the files it happens to open. To "understand the codebase"
//! (brownfield feature-add / "explain this code" / "navigate to X") the base
//! needs a compact, whole-repo *map* — a signature-level outline of every
//! top-level symbol (functions, types, exports) keyed by `path:line`. This
//! module produces that map within a caller-supplied **token budget**, ranked
//! by a lightweight importance heuristic, and **personalised** to the current
//! task via a `scope` of path hints (the files the router thinks are relevant).
//!
//! ## Design constraints (honoured deliberately)
//! - **Dependency-light.** No `tree-sitter`, no language server, no ICU tree.
//!   Symbols are extracted with **per-language `regex`** (already a workspace
//!   dep used by the equally dep-light `governance` / `contract` crates) plus
//!   hand-written line scanning. This is a *signature* scan, NOT a semantic
//!   parse: we capture the declaration line of each top-level symbol, not its
//!   body, types, or call graph. That is exactly enough for a map and keeps
//!   the build lean (the anti-rule against heavy parser trees).
//!   On top of the symbol scan, a per-language **import scan** resolves each
//!   file's imports against the scanned file set (relative paths by
//!   extension resolution, module paths by unique suffix match; an ambiguous
//!   resolution is *declined*, never guessed) into [`SymbolIndex::edges`] —
//!   real fan-in centrality for ranking, and a scope-seeded
//!   random-walk-with-restart that orders the map by structural relatedness
//!   instead of a bare path-substring partition.
//! - **Fail-open, never blocks.** Every error path (unreadable file, no source,
//!   empty repo, pathological input) returns an empty `String` / empty index —
//!   never an `Err`, never a panic. The map is an *enhancement*; a bug here
//!   must not break a run.
//! - **Bounded.** File count, per-file bytes, and total symbols are all capped
//!   so a vendored mega-file or a hostile repo can't blow up time or memory.
//! - **Cross-platform.** Rendered paths are always `/`-separated; the mtime
//!   cache key is derived from relative paths so it is stable per-machine.
//!
//! ## Public surface (the next batch wires these into firmware)
//! - [`repo_map`] — `(root, scope, budget_chars) -> String`: the rendered,
//!   token-budgeted signature outline, scope-personalised. The headline entry
//!   point.
//! - [`symbol_index`] — `(root) -> SymbolIndex`: the structured form (every
//!   file → its ranked symbols), for callers that want to retrieve / filter
//!   rather than render.
//! - [`Symbol`], [`SymbolKind`], [`FileSymbols`], [`SymbolIndex`] — the data.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use sha2::{Digest as _, Sha256};
use umadev_state::fs::{RootedDir, RootedEntryKind};

/// Source extensions we extract symbols from. A subset of the acceptance
/// crate's `SRC_EXT` (we only map *code*, not styles/markup), kept local so
/// `umadev-knowledge` doesn't depend on `umadev-agent`. Order is irrelevant.
const CODE_EXT: &[&str] = &[
    "ts", "tsx", "js", "jsx", "mjs", "cjs", // JS / TS family
    "py",  // Python
    "rs",  // Rust
    "go",  // Go
    "java", "kt",    // JVM
    "rb",    // Ruby
    "php",   // PHP
    "cs",    // C#
    "swift", // Swift
    "dart",  // Dart
];

/// Directories never worth scanning — build output / vendored deps / VCS /
/// UmaDev's own artifact dirs. Mirrors `acceptance::SKIP_DIRS` (kept local to
/// avoid the cross-crate dependency). Leading-dot dirs are skipped separately.
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "dist",
    "build",
    ".git",
    "vendor",
    "__pycache__",
    ".pytest_cache",
    ".next",
    "out",
    "coverage",
    "output",
];

/// Filenames (stem, lowercased) that mark an **entry point** — symbols defined
/// in these files get an importance boost (they are the repo's public face).
const ENTRY_STEMS: &[&str] = &[
    "main", "index", "app", "lib", "mod", "server", "cli", "__init__", "__main__", "root",
];

/// Hard bounds (a hostile / vendored mega-repo must not blow up time/memory).
mod limits {
    /// Max directory recursion depth.
    pub const MAX_DEPTH: usize = 12;
    /// Max source files scanned.
    pub const MAX_FILES: usize = 4000;
    /// Max directory entries inspected, including ignored and non-code files.
    pub const MAX_NODES: usize = 32_000;
    /// Max bytes read per file (a generated bundle can be megabytes — we only
    /// need the top declarations, and a giant minified line is useless anyway).
    pub const MAX_FILE_BYTES: usize = 512 * 1024;
    /// Max symbols kept per file (defends against a machine-generated file with
    /// tens of thousands of declarations).
    pub const MAX_SYMBOLS_PER_FILE: usize = 400;
    /// Max total symbols across the whole index.
    pub const MAX_TOTAL_SYMBOLS: usize = 40_000;
    /// A single physical line longer than this is treated as minified/generated
    /// and skipped for symbol matching (regex on a 2 MB line is pathological).
    pub const MAX_LINE_BYTES: usize = 2_000;
}

/// What a captured symbol *is*. Coarse on purpose — a signature scan can't
/// always tell a method from a free function, and the map doesn't need it to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    /// A function or method.
    Function,
    /// A class / struct / record.
    Class,
    /// An enum.
    Enum,
    /// An interface / trait / protocol.
    Interface,
    /// A type alias / typedef.
    TypeAlias,
    /// A top-level constant / static / exported variable binding.
    Const,
    /// Anything declaration-like we recognised but can't classify finer.
    Other,
}

impl SymbolKind {
    /// A short, stable, language-neutral label for rendering (`fn`, `class`, …).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            SymbolKind::Function => "fn",
            SymbolKind::Class => "class",
            SymbolKind::Enum => "enum",
            SymbolKind::Interface => "interface",
            SymbolKind::TypeAlias => "type",
            SymbolKind::Const => "const",
            SymbolKind::Other => "def",
        }
    }
}

/// One extracted top-level symbol: its name, kind, the verbatim declaration
/// line (trimmed), and its `1`-based line number within the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    /// Identifier (best-effort; the capture group from the language pattern).
    pub name: String,
    /// Coarse classification.
    pub kind: SymbolKind,
    /// The declaration line, trimmed — the "signature" we show in the outline.
    pub signature: String,
    /// 1-based line number of the declaration within its file.
    pub line: usize,
    /// Whether the declaration is exported / public (boosts importance).
    pub exported: bool,
}

/// All symbols extracted from one file, plus the file's importance score.
#[derive(Debug, Clone, PartialEq)]
pub struct FileSymbols {
    /// Path relative to the repo root, always `/`-separated (cross-platform).
    pub rel_path: String,
    /// Symbols in source order.
    pub symbols: Vec<Symbol>,
    /// Importance score assigned by the internal file ranker.
    pub score: f64,
}

/// The structured repo map: every scanned file with its symbols, ranked.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SymbolIndex {
    /// Per-file symbol sets, sorted by descending importance after ranking.
    pub files: Vec<FileSymbols>,
    /// Resolved import edges as `(importer, imported)` index pairs into
    /// [`SymbolIndex::files`] (kept consistent through the rank reorder).
    /// Extracted by per-language import patterns and resolved with a
    /// confidence discipline: relative paths by extension resolution, module
    /// paths by unique suffix match against the scanned set — a resolution
    /// matching more than one file is DECLINED rather than guessed. Sorted +
    /// deduped. Empty when nothing resolves (fail-open: every consumer must
    /// degrade to its edge-less behaviour).
    pub edges: Vec<(usize, usize)>,
}

impl SymbolIndex {
    /// Total number of symbols across all files.
    #[must_use]
    pub fn symbol_count(&self) -> usize {
        self.files.iter().map(|f| f.symbols.len()).sum()
    }

    /// Whether the index found nothing (empty repo / no recognised source).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

// ---------------------------------------------------------------------------
// File walk (bounded, fail-open) — mirrors acceptance::source_files locally.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct RepoFile {
    relative: PathBuf,
    len: u64,
    modified: Option<std::time::SystemTime>,
}

fn is_code_path(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .is_some_and(|extension| CODE_EXT.contains(&extension.as_str()))
}

fn collect_bounded(
    root: &RootedDir,
    relative_dir: &Path,
    out: &mut Vec<RepoFile>,
    depth: usize,
    visited: &mut usize,
    max_files: usize,
    max_nodes: usize,
) -> std::io::Result<()> {
    if depth > limits::MAX_DEPTH || out.len() >= max_files {
        return Ok(());
    }
    let remaining = max_nodes.saturating_sub(*visited);
    if remaining == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "repo map exceeds its directory-entry bound",
        ));
    }
    let entries = root.list_entries(relative_dir, remaining)?;
    *visited = visited.checked_add(entries.len()).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "repo map directory-entry count overflow",
        )
    })?;
    for entry in entries {
        if out.len() >= max_files {
            return Ok(());
        }
        let path = relative_dir.join(&entry.name);
        match entry.kind {
            RootedEntryKind::Directory => {
                let name = entry.name.to_str().unwrap_or("");
                if name.starts_with('.') || SKIP_DIRS.contains(&name) {
                    continue;
                }
                collect_bounded(root, &path, out, depth + 1, visited, max_files, max_nodes)?;
            }
            RootedEntryKind::RegularFile if is_code_path(&path) => out.push(RepoFile {
                relative: path,
                len: entry.len,
                modified: entry.modified,
            }),
            RootedEntryKind::RegularFile | RootedEntryKind::Unsafe => {
                continue;
            }
        }
    }
    Ok(())
}

/// Collect the repo's code files beneath one pinned, no-follow root.
fn code_files(root: &RootedDir, max_files: usize, max_nodes: usize) -> Option<Vec<RepoFile>> {
    let mut files = Vec::new();
    let mut visited = 0usize;
    collect_bounded(
        root,
        Path::new(""),
        &mut files,
        0,
        &mut visited,
        max_files,
        max_nodes,
    )
    .ok()?;
    files.sort_by(|left, right| left.relative.cmp(&right.relative));
    Some(files)
}

fn rel_display(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

// ---------------------------------------------------------------------------
// Language patterns — the heart of the signature scan.
// ---------------------------------------------------------------------------

/// A compiled extraction rule: a regex whose capture group 1 is the symbol
/// name, plus the kind to assign on a match.
struct Pattern {
    re: Regex,
    kind: SymbolKind,
}

/// Which language family an extension belongs to (selects the pattern set).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Lang {
    JsTs,
    Python,
    Rust,
    Go,
    Java, // also Kotlin (close enough at signature level)
    Ruby,
    Php,
    CSharp,
    Swift,
    Dart,
}

impl Lang {
    fn from_ext(ext: &str) -> Option<Lang> {
        Some(match ext {
            "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" => Lang::JsTs,
            "py" => Lang::Python,
            "rs" => Lang::Rust,
            "go" => Lang::Go,
            "java" | "kt" => Lang::Java,
            "rb" => Lang::Ruby,
            "php" => Lang::Php,
            "cs" => Lang::CSharp,
            "swift" => Lang::Swift,
            "dart" => Lang::Dart,
            _ => return None,
        })
    }
}

/// Build the (cached) pattern set for a language. Patterns are compiled once
/// per process via a `OnceLock` keyed by language. A compile failure for any
/// single pattern is skipped (fail-open) rather than panicking — so a typo in a
/// future pattern can never break the whole scan.
fn patterns_for(lang: Lang) -> &'static [Pattern] {
    static CACHE: OnceLock<HashMap<u8, Vec<Pattern>>> = OnceLock::new();
    let map = CACHE.get_or_init(build_all_patterns);
    map.get(&(lang as u8)).map_or(&[], Vec::as_slice)
}

/// Compile every language's pattern table once.
fn build_all_patterns() -> HashMap<u8, Vec<Pattern>> {
    use SymbolKind::{Class, Const, Enum, Function, Interface, Other, TypeAlias};
    // (pattern, kind) source — each `(&str, SymbolKind)`. Capture group 1 is
    // the identifier. Patterns are intentionally anchored to a leading optional
    // export/visibility keyword so they only match *declarations*, not calls.
    // We compile with `(?m)` so `^` means start-of-line within a multi-line
    // file; we still scan line-by-line for line numbers, so each pattern is
    // applied to a single trimmed line (the `(?m)` is harmless there).
    let mut out: HashMap<u8, Vec<Pattern>> = HashMap::new();

    let mut add = |lang: Lang, defs: &[(&str, SymbolKind)]| {
        let v: Vec<Pattern> = defs
            .iter()
            .filter_map(|(src, kind)| Regex::new(src).ok().map(|re| Pattern { re, kind: *kind }))
            .collect();
        out.insert(lang as u8, v);
    };

    // --- JavaScript / TypeScript -----------------------------------------
    // Handles: function decls, class/interface/enum/type, const arrow fns,
    // exported bindings. Identifier = [A-Za-z_$][\w$]* .
    add(
        Lang::JsTs,
        &[
            (
                r"^\s*(?:export\s+)?(?:default\s+)?(?:async\s+)?function\s*\*?\s*([A-Za-z_$][\w$]*)",
                Function,
            ),
            (
                r"^\s*(?:export\s+)?(?:default\s+)?(?:abstract\s+)?class\s+([A-Za-z_$][\w$]*)",
                Class,
            ),
            (
                r"^\s*(?:export\s+)?(?:declare\s+)?interface\s+([A-Za-z_$][\w$]*)",
                Interface,
            ),
            (
                r"^\s*(?:export\s+)?(?:const\s+)?enum\s+([A-Za-z_$][\w$]*)",
                Enum,
            ),
            (
                r"^\s*(?:export\s+)?type\s+([A-Za-z_$][\w$]*)\s*[=<]",
                TypeAlias,
            ),
            // const/let/var arrow-function or function-expression binding.
            (
                r"^\s*(?:export\s+)?(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*(?::[^=]+)?=\s*(?:async\s*)?(?:\([^)]*\)|[A-Za-z_$][\w$]*)\s*=>",
                Function,
            ),
            // Plain exported const binding (config object, etc.).
            (
                r"^\s*export\s+(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*[:=]",
                Const,
            ),
        ],
    );

    // --- Python -----------------------------------------------------------
    add(
        Lang::Python,
        &[
            (r"^\s*(?:async\s+)?def\s+([A-Za-z_][\w]*)", Function),
            (r"^\s*class\s+([A-Za-z_][\w]*)", Class),
        ],
    );

    // --- Rust -------------------------------------------------------------
    add(
        Lang::Rust,
        &[
            (
                r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?(?:extern\s+(?:\x22[^\x22]*\x22\s+)?)?fn\s+([A-Za-z_][\w]*)",
                Function,
            ),
            (
                r"^\s*(?:pub(?:\([^)]*\))?\s+)?struct\s+([A-Za-z_][\w]*)",
                Class,
            ),
            (
                r"^\s*(?:pub(?:\([^)]*\))?\s+)?enum\s+([A-Za-z_][\w]*)",
                Enum,
            ),
            (
                r"^\s*(?:pub(?:\([^)]*\))?\s+)?trait\s+([A-Za-z_][\w]*)",
                Interface,
            ),
            (
                r"^\s*(?:pub(?:\([^)]*\))?\s+)?type\s+([A-Za-z_][\w]*)",
                TypeAlias,
            ),
            (
                r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:const|static)\s+([A-Za-z_][\w]*)",
                Const,
            ),
        ],
    );

    // --- Go ---------------------------------------------------------------
    add(
        Lang::Go,
        &[
            (r"^\s*func\s+(?:\([^)]*\)\s*)?([A-Za-z_][\w]*)", Function),
            (r"^\s*type\s+([A-Za-z_][\w]*)\s+struct\b", Class),
            (r"^\s*type\s+([A-Za-z_][\w]*)\s+interface\b", Interface),
            (r"^\s*type\s+([A-Za-z_][\w]*)\b", TypeAlias),
        ],
    );

    // --- Java / Kotlin ----------------------------------------------------
    // Modifier prefix: zero-or-more visibility/qualifier keywords, each
    // followed by whitespace. The `\s+` lives OUTSIDE the alternation so every
    // branch consumes its trailing space (a `public|...|internal\s+` form would
    // only attach the space to the last branch and fail on `public class X`).
    add(
        Lang::Java,
        &[
            (
                r"^\s*(?:(?:public|private|protected|internal|static|final|abstract|sealed|open|data)\s+)*class\s+([A-Za-z_][\w]*)",
                Class,
            ),
            (
                r"^\s*(?:(?:public|private|protected|internal)\s+)*interface\s+([A-Za-z_][\w]*)",
                Interface,
            ),
            (
                r"^\s*(?:(?:public|private|protected|internal)\s+)*enum\s+([A-Za-z_][\w]*)",
                Enum,
            ),
            // Kotlin fun.
            (
                r"^\s*(?:(?:public|private|protected|internal|suspend|inline|override|open)\s+)*fun\s+([A-Za-z_][\w]*)",
                Function,
            ),
        ],
    );

    // --- Ruby -------------------------------------------------------------
    add(
        Lang::Ruby,
        &[
            (r"^\s*def\s+(?:self\.)?([A-Za-z_][\w]*[!?=]?)", Function),
            (r"^\s*class\s+([A-Za-z_][\w:]*)", Class),
            (r"^\s*module\s+([A-Za-z_][\w:]*)", Other),
        ],
    );

    // --- PHP --------------------------------------------------------------
    add(
        Lang::Php,
        &[
            (
                r"^\s*(?:public\s+|private\s+|protected\s+|static\s+|final\s+|abstract\s+)*function\s+([A-Za-z_][\w]*)",
                Function,
            ),
            (
                r"^\s*(?:abstract\s+|final\s+)?class\s+([A-Za-z_][\w]*)",
                Class,
            ),
            (r"^\s*interface\s+([A-Za-z_][\w]*)", Interface),
            (r"^\s*trait\s+([A-Za-z_][\w]*)", Interface),
            (r"^\s*enum\s+([A-Za-z_][\w]*)", Enum),
        ],
    );

    // --- C# ---------------------------------------------------------------
    add(
        Lang::CSharp,
        &[
            (
                r"^\s*(?:(?:public|private|protected|internal|static|sealed|abstract|partial)\s+)*class\s+([A-Za-z_][\w]*)",
                Class,
            ),
            (
                r"^\s*(?:(?:public|private|protected|internal)\s+)*interface\s+([A-Za-z_][\w]*)",
                Interface,
            ),
            (
                r"^\s*(?:(?:public|private|protected|internal)\s+)*enum\s+([A-Za-z_][\w]*)",
                Enum,
            ),
            (
                r"^\s*(?:(?:public|private|protected|internal)\s+)*struct\s+([A-Za-z_][\w]*)",
                Class,
            ),
        ],
    );

    // --- Swift ------------------------------------------------------------
    add(
        Lang::Swift,
        &[
            (
                r"^\s*(?:public\s+|private\s+|internal\s+|open\s+|fileprivate\s+|static\s+)*func\s+([A-Za-z_][\w]*)",
                Function,
            ),
            (
                r"^\s*(?:public\s+|private\s+|internal\s+|open\s+|final\s+)*class\s+([A-Za-z_][\w]*)",
                Class,
            ),
            (
                r"^\s*(?:public\s+|private\s+|internal\s+)?struct\s+([A-Za-z_][\w]*)",
                Class,
            ),
            (
                r"^\s*(?:public\s+|private\s+|internal\s+)?enum\s+([A-Za-z_][\w]*)",
                Enum,
            ),
            (
                r"^\s*(?:public\s+|private\s+|internal\s+)?protocol\s+([A-Za-z_][\w]*)",
                Interface,
            ),
        ],
    );

    // --- Dart -------------------------------------------------------------
    add(
        Lang::Dart,
        &[
            (r"^\s*(?:abstract\s+)?class\s+([A-Za-z_][\w]*)", Class),
            (r"^\s*enum\s+([A-Za-z_][\w]*)", Enum),
            (r"^\s*mixin\s+([A-Za-z_][\w]*)", Interface),
        ],
    );

    out
}

/// A loose heuristic for "this line declares something exported/public" — used
/// for the importance boost. Cheap substring checks (no regex): we already
/// matched a symbol, this only refines its score.
fn looks_exported(line: &str, lang: Lang) -> bool {
    let t = line.trim_start();
    match lang {
        Lang::JsTs => t.starts_with("export"),
        Lang::Rust => t.starts_with("pub"),
        Lang::Python => {
            // Public unless name starts with `_` (checked by caller on the name).
            true
        }
        Lang::Go => {
            // Exported Go identifiers start uppercase — checked on the name by
            // the caller; treat the line as a candidate here.
            true
        }
        Lang::Java | Lang::CSharp => t.starts_with("public"),
        Lang::Php => t.contains("public"),
        Lang::Swift => t.starts_with("public") || t.starts_with("open"),
        Lang::Ruby | Lang::Dart => true,
    }
}

/// Extract symbols from one file's text. `lang` selects the pattern set. The
/// scan is line-oriented (cheap, gives line numbers for free) and bounded by
/// [`limits::MAX_SYMBOLS_PER_FILE`].
fn extract_symbols(text: &str, lang: Lang) -> Vec<Symbol> {
    let pats = patterns_for(lang);
    if pats.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    // Multi-line comment / docstring state — a declaration-looking line INSIDE a
    // `/* ... */` block comment or a triple-quoted docstring is NOT a real symbol
    // and must not be captured as a phantom (LOW #6). Line-granular: cheap, and
    // enough to kill the common multi-line-block-comment / docstring false
    // positives (license headers, commented-out code, Python docstrings).
    let mut in_block = false;
    let mut doc_marker: Option<&'static str> = None;
    for (i, raw_line) in text.lines().enumerate() {
        if out.len() >= limits::MAX_SYMBOLS_PER_FILE {
            break;
        }
        // Skip absurdly long lines (minified / generated) — regex on them is
        // both useless and a perf hazard.
        if raw_line.len() > limits::MAX_LINE_BYTES {
            continue;
        }
        let line = raw_line.trim_end();
        let trimmed = line.trim_start();

        // --- comment / docstring state machine (skip symbol matching inside) ---
        if in_block {
            if trimmed.contains("*/") {
                in_block = false;
            }
            continue;
        }
        if let Some(marker) = doc_marker {
            if trimmed.contains(marker) {
                doc_marker = None;
            }
            continue;
        }
        // A block comment opened with a line beginning `/*` (covers `/*`, `/**`
        // JSDoc, `/* commented code`). A single-line `/* ... */` closes on the
        // same line, so it does not enter the multi-line state.
        if trimmed.starts_with("/*") {
            if !trimmed.contains("*/") {
                in_block = true;
            }
            continue; // a comment opener carries no symbol
        }
        // Python triple-quoted docstring (`"""` or `'''`). Enters the docstring
        // state unless the same line also closes it.
        if matches!(lang, Lang::Python)
            && (trimmed.starts_with("\"\"\"") || trimmed.starts_with("'''"))
        {
            let marker = if trimmed.starts_with("\"\"\"") {
                "\"\"\""
            } else {
                "'''"
            };
            if !trimmed[marker.len()..].contains(marker) {
                doc_marker = Some(marker);
            }
            continue;
        }

        if trimmed.is_empty()
            || trimmed.starts_with("//")
            || trimmed.starts_with('#') && lang != Lang::Python && lang != Lang::Ruby
            || trimmed.starts_with('*')
        {
            // Comment / blank — Python & Ruby use `#` for comments too, but
            // `def`/`class` never start with `#`, so the pattern simply won't
            // match; we only fast-skip obvious noise lines.
            if !(lang == Lang::Python || lang == Lang::Ruby) {
                continue;
            }
        }
        for pat in pats {
            if let Some(caps) = pat.re.captures(line) {
                if let Some(name) = caps.get(1) {
                    let name = name.as_str().to_string();
                    let exported = compute_exported(&name, line, lang);
                    out.push(Symbol {
                        name,
                        kind: pat.kind,
                        signature: trimmed.to_string(),
                        line: i + 1,
                        exported,
                    });
                    break; // one symbol per line — first matching pattern wins
                }
            }
        }
    }
    out
}

/// Decide whether a matched symbol is exported, refining [`looks_exported`]
/// with name-based rules for languages where visibility is encoded in the name.
fn compute_exported(name: &str, line: &str, lang: Lang) -> bool {
    match lang {
        Lang::Python | Lang::Ruby => !name.starts_with('_'),
        Lang::Go => name.chars().next().is_some_and(char::is_uppercase),
        _ => looks_exported(line, lang),
    }
}

// ---------------------------------------------------------------------------
// Import edges — per-language import scan + disciplined resolution.
//
// The same regex-not-parser tradeoff as the symbol scan: imports are the most
// regular syntax in every language, so a line-oriented capture is accurate
// enough, and the consumer (ranking / map ordering) is noise-tolerant by
// design. The resolution discipline is the important part: a spec that maps
// to MORE than one scanned file is declined outright — a wrong edge is worse
// than a missing one when the output is an ordering signal.
// ---------------------------------------------------------------------------

/// Bounds for the import scan (same spirit as [`limits`]).
mod import_limits {
    /// Max import specs captured per file.
    pub const MAX_IMPORTS_PER_FILE: usize = 200;
    /// Max resolved edges across the whole index.
    pub const MAX_EDGES: usize = 20_000;
    /// Max byte length of a single captured spec (defends against generated
    /// monster import lines).
    pub const MAX_SPEC_BYTES: usize = 300;
    /// Max path segments considered when suffix-matching a module path.
    pub const MAX_SEGS: usize = 24;
}

/// A compiled import-capture rule: capture group 1 is the import spec.
/// `marker` is prefixed onto the captured spec so the resolver can tell apart
/// declaration forms that resolve differently within one language.
struct ImportPattern {
    re: Regex,
    /// `""` for ordinary imports, `"mod:"` for Rust `mod x;` declarations,
    /// `"rel:"` for Ruby `require_relative`.
    marker: &'static str,
}

/// Build the (cached) import-pattern set for a language. Same fail-open
/// compile policy as [`patterns_for`]. Go is absent on purpose: its import
/// blocks are handled by a tiny state machine in [`extract_imports`].
fn import_patterns_for(lang: Lang) -> &'static [ImportPattern] {
    static CACHE: OnceLock<HashMap<u8, Vec<ImportPattern>>> = OnceLock::new();
    let map = CACHE.get_or_init(build_all_import_patterns);
    map.get(&(lang as u8)).map_or(&[], Vec::as_slice)
}

/// Compile every language's import-pattern table once.
fn build_all_import_patterns() -> HashMap<u8, Vec<ImportPattern>> {
    let mut out: HashMap<u8, Vec<ImportPattern>> = HashMap::new();
    let mut add = |lang: Lang, defs: &[(&str, &'static str)]| {
        let v: Vec<ImportPattern> = defs
            .iter()
            .filter_map(|(src, marker)| Regex::new(src).ok().map(|re| ImportPattern { re, marker }))
            .collect();
        out.insert(lang as u8, v);
    };

    add(
        Lang::JsTs,
        &[
            // import x / {a,b} / * as ns  from '...'
            (r#"^\s*import\s+[^'"]*?\bfrom\s*['"]([^'"]+)['"]"#, ""),
            // side-effect import '...'
            (r#"^\s*import\s*['"]([^'"]+)['"]"#, ""),
            // export { a } from '...' / export * from '...'
            (r#"^\s*export\s+[^'"]*?\bfrom\s*['"]([^'"]+)['"]"#, ""),
            // closing line of a multi-line import: `} from '...'`
            (r#"^\s*\}\s*from\s*['"]([^'"]+)['"]"#, ""),
            // CommonJS require('...')
            (r#"\brequire\s*\(\s*['"]([^'"]+)['"]\s*\)"#, ""),
            // dynamic import('...')
            (r#"\bimport\s*\(\s*['"]([^'"]+)['"]\s*\)"#, ""),
        ],
    );
    add(
        Lang::Python,
        &[
            (r"^\s*from\s+([.\w]+)\s+import\b", ""),
            (r"^\s*import\s+([\w.]+(?:\s*,\s*[\w.]+)*)", ""),
        ],
    );
    add(
        Lang::Rust,
        &[
            (r"^\s*(?:pub(?:\([^)]*\))?\s+)?use\s+([A-Za-z_][\w:]*)", ""),
            (
                r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_]\w*)\s*;",
                "mod:",
            ),
        ],
    );
    add(
        Lang::Java, // also Kotlin
        &[(r"^\s*import\s+(?:static\s+)?([\w.]+\w)\s*;?\s*$", "")],
    );
    add(
        Lang::Ruby,
        &[
            (r#"^\s*require_relative\s+['"]([^'"]+)['"]"#, "rel:"),
            (r#"^\s*require\s+['"]([^'"]+)['"]"#, ""),
        ],
    );
    add(
        Lang::Php,
        &[
            (r"^\s*use\s+(?:function\s+|const\s+)?\\?([\w\\]+)", ""),
            (
                r#"\b(?:require|include)(?:_once)?\s*\(?\s*['"]([^'"]+)['"]"#,
                "",
            ),
        ],
    );
    add(
        Lang::CSharp,
        &[(r"^\s*using\s+(?:static\s+)?([\w.]+\w)\s*;", "")],
    );
    add(
        Lang::Swift,
        &[(r"^\s*(?:@testable\s+)?import\s+([A-Za-z_]\w*)", "")],
    );
    add(
        Lang::Dart,
        &[(r#"^\s*(?:import|export|part)\s+['"]([^'"]+)['"]"#, "")],
    );

    out
}

/// Extract the first double-quoted path on a line (`"pkg/path"` → `pkg/path`).
/// Used for Go import lines, where the path is always double-quoted.
fn quoted_path(line: &str) -> Option<&str> {
    let start = line.find('"')? + 1;
    let end = start + line[start..].find('"')?;
    let s = &line[start..end];
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Extract raw import specs (with per-form markers) from one file's text.
/// Line-oriented and bounded like [`extract_symbols`]; block comments and
/// Python docstrings are skipped so commented-out / documented imports don't
/// produce phantom edges.
fn extract_imports(text: &str, lang: Lang) -> Vec<String> {
    let pats = import_patterns_for(lang);
    let mut out: Vec<String> = Vec::new();
    let mut in_block = false;
    let mut doc_marker: Option<&'static str> = None;
    let mut in_go_import = false;
    for raw_line in text.lines() {
        if out.len() >= import_limits::MAX_IMPORTS_PER_FILE {
            break;
        }
        if raw_line.len() > limits::MAX_LINE_BYTES {
            continue;
        }
        let trimmed = raw_line.trim();
        if in_block {
            if trimmed.contains("*/") {
                in_block = false;
            }
            continue;
        }
        if let Some(marker) = doc_marker {
            if trimmed.contains(marker) {
                doc_marker = None;
            }
            continue;
        }
        if trimmed.starts_with("/*") {
            if !trimmed.contains("*/") {
                in_block = true;
            }
            continue;
        }
        if matches!(lang, Lang::Python)
            && (trimmed.starts_with("\"\"\"") || trimmed.starts_with("'''"))
        {
            let marker = if trimmed.starts_with("\"\"\"") {
                "\"\"\""
            } else {
                "'''"
            };
            if !trimmed[marker.len()..].contains(marker) {
                doc_marker = Some(marker);
            }
            continue;
        }
        if trimmed.starts_with("//") {
            continue;
        }
        // Go: `import "x"` plus the multi-line `import ( ... )` block.
        if matches!(lang, Lang::Go) {
            if in_go_import {
                if trimmed.starts_with(')') {
                    in_go_import = false;
                } else if let Some(q) = quoted_path(trimmed) {
                    if q.len() <= import_limits::MAX_SPEC_BYTES {
                        out.push(q.to_string());
                    }
                }
            } else if let Some(rest) = trimmed.strip_prefix("import") {
                let rest = rest.trim_start();
                if rest.starts_with('(') {
                    in_go_import = true;
                } else if let Some(q) = quoted_path(rest) {
                    if q.len() <= import_limits::MAX_SPEC_BYTES {
                        out.push(q.to_string());
                    }
                }
            }
            continue;
        }
        for pat in pats {
            if let Some(caps) = pat.re.captures(trimmed) {
                if let Some(m) = caps.get(1) {
                    let spec = m.as_str().trim();
                    if spec.is_empty() || spec.len() > import_limits::MAX_SPEC_BYTES {
                        break;
                    }
                    // Python `import a, b, c` — split the comma list.
                    if matches!(lang, Lang::Python) && spec.contains(',') {
                        for part in spec.split(',') {
                            let p = part.trim();
                            if !p.is_empty() {
                                out.push(p.to_string());
                            }
                        }
                    } else {
                        out.push(format!("{}{}", pat.marker, spec));
                    }
                    break; // one import per line — first matching pattern wins
                }
            }
        }
    }
    out
}

// --- Resolution -------------------------------------------------------------

/// Lookup tables over the scanned file set, built once per resolution pass.
struct ResolveCtx<'a> {
    files: &'a [FileSymbols],
    langs: &'a [Lang],
    /// Exact relative path → file index.
    by_path: HashMap<&'a str, usize>,
    /// File basename (`core.ts`) → file indices (candidate prefilter).
    by_name: HashMap<&'a str, Vec<usize>>,
    /// Last parent-dir segment (`tools`) → file indices (package-dir match).
    by_dir_name: HashMap<&'a str, Vec<usize>>,
}

impl<'a> ResolveCtx<'a> {
    fn new(files: &'a [FileSymbols], langs: &'a [Lang]) -> Self {
        let mut by_path = HashMap::new();
        let mut by_name: HashMap<&str, Vec<usize>> = HashMap::new();
        let mut by_dir_name: HashMap<&str, Vec<usize>> = HashMap::new();
        for (i, f) in files.iter().enumerate() {
            let rel = f.rel_path.as_str();
            by_path.insert(rel, i);
            let base = rel.rsplit('/').next().unwrap_or(rel);
            by_name.entry(base).or_default().push(i);
            let dir = parent_dir(rel);
            if !dir.is_empty() {
                let dir_base = dir.rsplit('/').next().unwrap_or(dir);
                by_dir_name.entry(dir_base).or_default().push(i);
            }
        }
        ResolveCtx {
            files,
            langs,
            by_path,
            by_name,
            by_dir_name,
        }
    }
}

/// The `/`-separated parent directory of a relative path (`""` at the root).
fn parent_dir(rel: &str) -> &str {
    rel.rsplit_once('/').map_or("", |(d, _)| d)
}

/// Join a base directory with a relative import spec, resolving `.` / `..`.
/// `None` when the spec escapes the repo root (declined, not guessed).
fn normalize_join(base_dir: &str, spec: &str) -> Option<String> {
    let mut parts: Vec<&str> = if base_dir.is_empty() {
        Vec::new()
    } else {
        base_dir.split('/').collect()
    };
    for seg in spec.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            s => parts.push(s),
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

/// Whether a normalized path already carries a recognised code extension.
fn has_code_ext(p: &str) -> bool {
    p.rsplit('/')
        .next()
        .unwrap_or(p)
        .rsplit_once('.')
        .is_some_and(|(_, ext)| CODE_EXT.contains(&ext.to_ascii_lowercase().as_str()))
}

/// First candidate path that exists in the scanned set (extension-resolution
/// order is a *preference*, not an ambiguity — the first hit wins).
fn first_hit(cands: &[String], ctx: &ResolveCtx) -> Option<usize> {
    cands
        .iter()
        .find_map(|c| ctx.by_path.get(c.as_str()).copied())
}

/// Outcome of a suffix match: exactly one file, ambiguous (declined), or none.
enum ModRes {
    Hit(usize),
    Ambiguous,
    Miss,
}

/// Match `(basename, path_suffix)` candidates against the scanned set.
/// The basename bucket keeps this O(candidates), and the full suffix must
/// align on a `/` boundary. More than one distinct file → [`ModRes::Ambiguous`]
/// (the caller declines — a guessed edge poisons the ranking signal).
fn suffix_match(cands: &[(String, String)], ctx: &ResolveCtx) -> ModRes {
    let mut found: Option<usize> = None;
    for (base, suffix) in cands {
        if let Some(list) = ctx.by_name.get(base.as_str()) {
            for &idx in list {
                let rel = ctx.files[idx].rel_path.as_str();
                if rel == suffix || rel.ends_with(&format!("/{suffix}")) {
                    match found {
                        None => found = Some(idx),
                        Some(prev) if prev != idx => return ModRes::Ambiguous,
                        Some(_) => {}
                    }
                }
            }
        }
    }
    found.map_or(ModRes::Miss, ModRes::Hit)
}

/// Match a package-style import (Go / C# namespaces) against scanned files by
/// their parent directory: drop leading segments until the remaining suffix
/// names a directory holding exactly ONE scanned file of the right language;
/// multiple dirs / multiple files → declined.
fn dir_match(segs: &[&str], lang: Lang, ctx: &ResolveCtx) -> Option<usize> {
    let last = *segs.last()?;
    let bucket = ctx.by_dir_name.get(last)?;
    for start in 0..segs.len() {
        let suffix = segs[start..].join("/");
        let mut found: Option<usize> = None;
        for &idx in bucket {
            if ctx.langs[idx] != lang {
                continue;
            }
            let dir = parent_dir(&ctx.files[idx].rel_path);
            if dir == suffix || dir.ends_with(&format!("/{suffix}")) {
                match found {
                    None => found = Some(idx),
                    Some(prev) if prev != idx => return None, // >1 file → decline
                    Some(_) => {}
                }
            }
        }
        if let Some(idx) = found {
            return Some(idx);
        }
    }
    None
}

/// Resolve one captured import spec to a scanned file index, or `None`
/// (external / unresolvable / ambiguous — all declined the same way).
#[allow(clippy::too_many_lines)]
fn resolve_import(raw: &str, lang: Lang, importer_rel: &str, ctx: &ResolveCtx) -> Option<usize> {
    let dir = parent_dir(importer_rel);
    match lang {
        Lang::JsTs => {
            if raw.starts_with('.') {
                let p = normalize_join(dir, raw)?;
                let mut cands: Vec<String> = Vec::new();
                if has_code_ext(&p) {
                    cands.push(p.clone());
                    // TS source written with NodeNext `.js` specifiers.
                    if let Some(s) = p.strip_suffix(".js") {
                        cands.push(format!("{s}.ts"));
                        cands.push(format!("{s}.tsx"));
                    } else if let Some(s) = p.strip_suffix(".jsx") {
                        cands.push(format!("{s}.tsx"));
                    }
                } else {
                    for ext in ["ts", "tsx", "js", "jsx", "mjs", "cjs"] {
                        cands.push(format!("{p}.{ext}"));
                    }
                    for ix in ["index.ts", "index.tsx", "index.js", "index.jsx"] {
                        cands.push(format!("{p}/{ix}"));
                    }
                }
                first_hit(&cands, ctx)
            } else {
                // Aliased / baseUrl-style path. A bare one-segment specifier
                // (`react`) is an external package → declined.
                let stripped = raw.strip_prefix("~/").unwrap_or(raw);
                let stripped = if stripped.starts_with('@') {
                    stripped.split_once('/').map_or("", |(_, r)| r)
                } else {
                    stripped
                };
                if stripped.is_empty() || !stripped.contains('/') {
                    return None;
                }
                let segs: Vec<&str> = stripped.split('/').filter(|s| !s.is_empty()).collect();
                if segs.is_empty() || segs.len() > import_limits::MAX_SEGS {
                    return None;
                }
                let joined = segs.join("/");
                let last = segs[segs.len() - 1];
                let mut cands: Vec<(String, String)> = Vec::new();
                if has_code_ext(last) {
                    cands.push((last.to_string(), joined.clone()));
                } else {
                    for ext in ["ts", "tsx", "js", "jsx", "mjs", "cjs"] {
                        cands.push((format!("{last}.{ext}"), format!("{joined}.{ext}")));
                    }
                    for ix in ["index.ts", "index.tsx", "index.js", "index.jsx"] {
                        cands.push((ix.to_string(), format!("{joined}/{ix}")));
                    }
                }
                match suffix_match(&cands, ctx) {
                    ModRes::Hit(i) => Some(i),
                    ModRes::Ambiguous | ModRes::Miss => None,
                }
            }
        }
        Lang::Python => {
            if let Some(rest) = raw.strip_prefix('.') {
                // Relative: `.` = importer's package, each extra `.` pops one.
                let extra_dots = rest.chars().take_while(|&c| c == '.').count();
                let rest = &rest[extra_dots..];
                let mut base: Vec<&str> = if dir.is_empty() {
                    Vec::new()
                } else {
                    dir.split('/').collect()
                };
                for _ in 0..extra_dots {
                    base.pop()?;
                }
                let base = base.join("/");
                let p = if rest.is_empty() {
                    if base.is_empty() {
                        return None;
                    }
                    base
                } else {
                    let rel = rest.replace('.', "/");
                    if base.is_empty() {
                        rel
                    } else {
                        format!("{base}/{rel}")
                    }
                };
                first_hit(&[format!("{p}.py"), format!("{p}/__init__.py")], ctx)
            } else {
                let segs: Vec<&str> = raw.split('.').filter(|s| !s.is_empty()).collect();
                if segs.is_empty() || segs.len() > import_limits::MAX_SEGS {
                    return None;
                }
                let joined = segs.join("/");
                let last = segs[segs.len() - 1];
                let cands = [
                    (format!("{last}.py"), format!("{joined}.py")),
                    ("__init__.py".to_string(), format!("{joined}/__init__.py")),
                ];
                match suffix_match(&cands, ctx) {
                    ModRes::Hit(i) => Some(i),
                    ModRes::Ambiguous | ModRes::Miss => None,
                }
            }
        }
        Lang::Rust => {
            if let Some(name) = raw.strip_prefix("mod:") {
                // `mod x;` — a precise sibling/child module declaration.
                let stem = file_stem_lc(importer_rel);
                let mut cands: Vec<String> = Vec::new();
                let child = |rest: &str| -> String {
                    if dir.is_empty() {
                        rest.to_string()
                    } else {
                        format!("{dir}/{rest}")
                    }
                };
                if matches!(stem.as_str(), "mod" | "lib" | "main") {
                    cands.push(child(&format!("{name}.rs")));
                    cands.push(child(&format!("{name}/mod.rs")));
                } else {
                    cands.push(child(&format!("{stem}/{name}.rs")));
                    cands.push(child(&format!("{stem}/{name}/mod.rs")));
                    cands.push(child(&format!("{name}.rs")));
                }
                first_hit(&cands, ctx)
            } else {
                // `use a::b::C` — the tail may be an item, so try the full
                // module path first and shorten from the right; the first
                // AMBIGUOUS level declines the whole spec.
                let path = raw.trim_end_matches(':');
                let segs: Vec<&str> = path
                    .split("::")
                    .filter(|s| !s.is_empty() && !matches!(*s, "crate" | "self" | "super"))
                    .collect();
                if segs.is_empty() || segs.len() > import_limits::MAX_SEGS {
                    return None;
                }
                for k in (1..=segs.len()).rev() {
                    let joined = segs[..k].join("/");
                    let last = segs[k - 1];
                    let cands = [
                        (format!("{last}.rs"), format!("{joined}.rs")),
                        ("mod.rs".to_string(), format!("{joined}/mod.rs")),
                    ];
                    match suffix_match(&cands, ctx) {
                        ModRes::Hit(i) => return Some(i),
                        ModRes::Ambiguous => return None,
                        ModRes::Miss => {}
                    }
                }
                None
            }
        }
        Lang::Go | Lang::CSharp => {
            // Package/namespace path → the directory holding it.
            let sep = if matches!(lang, Lang::Go) { '/' } else { '.' };
            let segs: Vec<&str> = raw.split(sep).filter(|s| !s.is_empty()).collect();
            if segs.is_empty() || segs.len() > import_limits::MAX_SEGS {
                return None;
            }
            dir_match(&segs, lang, ctx)
        }
        Lang::Java => {
            // `com.example.Foo` — the tail is the type; source roots sit above
            // the package dirs, so a full-FQN suffix match lands exactly. One
            // right-shortening step covers `import static com.x.Foo.bar`.
            let segs: Vec<&str> = raw.split('.').filter(|s| !s.is_empty()).collect();
            if segs.is_empty() || segs.len() > import_limits::MAX_SEGS {
                return None;
            }
            let min_k = segs.len().saturating_sub(1).max(1);
            for k in (min_k..=segs.len()).rev() {
                let joined = segs[..k].join("/");
                let last = segs[k - 1];
                let cands = [
                    (format!("{last}.java"), format!("{joined}.java")),
                    (format!("{last}.kt"), format!("{joined}.kt")),
                ];
                match suffix_match(&cands, ctx) {
                    ModRes::Hit(i) => return Some(i),
                    ModRes::Ambiguous => return None,
                    ModRes::Miss => {}
                }
            }
            None
        }
        Lang::Ruby => {
            if let Some(rel) = raw.strip_prefix("rel:") {
                let p = normalize_join(dir, rel)?;
                let cands = if p.ends_with(".rb") {
                    vec![p]
                } else {
                    vec![format!("{p}.rb")]
                };
                first_hit(&cands, ctx)
            } else {
                let segs: Vec<&str> = raw.split('/').filter(|s| !s.is_empty()).collect();
                if segs.is_empty() || segs.len() > import_limits::MAX_SEGS {
                    return None;
                }
                let joined = segs.join("/");
                let last = segs[segs.len() - 1];
                let base = if last.ends_with(".rb") {
                    (last.to_string(), joined)
                } else {
                    (format!("{last}.rb"), format!("{joined}.rb"))
                };
                match suffix_match(&[base], ctx) {
                    ModRes::Hit(i) => Some(i),
                    ModRes::Ambiguous | ModRes::Miss => None,
                }
            }
        }
        Lang::Php => {
            if raw.starts_with('.') {
                let p = normalize_join(dir, raw)?;
                let cands = if p.ends_with(".php") {
                    vec![p]
                } else {
                    vec![format!("{p}.php")]
                };
                return first_hit(&cands, ctx);
            }
            // Namespace path; a PSR-4 prefix may not exist on disk, so drop
            // leading segments until the suffix matches exactly one file.
            let segs: Vec<&str> = raw.split(['\\', '/']).filter(|s| !s.is_empty()).collect();
            if segs.is_empty() || segs.len() > import_limits::MAX_SEGS {
                return None;
            }
            let last = segs[segs.len() - 1];
            let base = if last.ends_with(".php") {
                last.to_string()
            } else {
                format!("{last}.php")
            };
            for start in 0..segs.len() {
                let joined = segs[start..].join("/");
                let suffix = if last.ends_with(".php") {
                    joined
                } else {
                    format!("{joined}.php")
                };
                match suffix_match(&[(base.clone(), suffix)], ctx) {
                    ModRes::Hit(i) => return Some(i),
                    ModRes::Ambiguous => return None,
                    ModRes::Miss => {}
                }
            }
            None
        }
        Lang::Swift => {
            // Module import — usually external; only a unique same-named file
            // in the repo resolves.
            let cands = [(format!("{raw}.swift"), format!("{raw}.swift"))];
            match suffix_match(&cands, ctx) {
                ModRes::Hit(i) => Some(i),
                ModRes::Ambiguous | ModRes::Miss => None,
            }
        }
        Lang::Dart => {
            if let Some(rest) = raw.strip_prefix("package:") {
                // `package:pkg/path.dart` — drop the package name, match the
                // remaining path (conventionally under `lib/`).
                let rest = rest.split_once('/').map_or("", |(_, r)| r);
                if rest.is_empty() {
                    return None;
                }
                let base = rest.rsplit('/').next().unwrap_or(rest).to_string();
                let cands = [
                    (base.clone(), rest.to_string()),
                    (base, format!("lib/{rest}")),
                ];
                match suffix_match(&cands, ctx) {
                    ModRes::Hit(i) => Some(i),
                    ModRes::Ambiguous | ModRes::Miss => None,
                }
            } else if raw.contains(':') {
                None // `dart:core` and friends — external
            } else {
                let p = normalize_join(dir, raw)?;
                let cands = if p.ends_with(".dart") {
                    vec![p]
                } else {
                    vec![format!("{p}.dart")]
                };
                first_hit(&cands, ctx)
            }
        }
    }
}

/// Resolve every file's captured imports into deduped `(importer, imported)`
/// edges. Self-edges are dropped; the total is bounded by
/// [`import_limits::MAX_EDGES`]. Fail-open: anything unresolvable simply
/// yields no edge.
fn resolve_edges(
    files: &[FileSymbols],
    langs: &[Lang],
    imports: &[Vec<String>],
) -> Vec<(usize, usize)> {
    if files.is_empty() || files.len() != langs.len() || files.len() != imports.len() {
        return Vec::new();
    }
    let ctx = ResolveCtx::new(files, langs);
    let mut edges: Vec<(usize, usize)> = Vec::new();
    'outer: for (i, specs) in imports.iter().enumerate() {
        for spec in specs {
            if edges.len() >= import_limits::MAX_EDGES {
                break 'outer;
            }
            if let Some(j) = resolve_import(spec, langs[i], &files[i].rel_path, &ctx) {
                if j != i {
                    edges.push((i, j));
                }
            }
        }
    }
    edges.sort_unstable();
    edges.dedup();
    edges
}

// ---------------------------------------------------------------------------
// Building the index (with mtime cache).
// ---------------------------------------------------------------------------

/// Build the structured [`SymbolIndex`] for `root` — scan every code file,
/// extract + rank symbols. Uses the on-disk mtime cache when valid (see
/// the internal cache loader). Fail-open: an empty/unreadable repo yields an
/// empty index.
#[must_use]
pub fn symbol_index(root: &Path) -> SymbolIndex {
    load_or_scan(root)
}

/// Scan from scratch (no cache) — exposed for tests and for the cache miss path.
#[cfg(test)]
fn scan(root: &Path) -> SymbolIndex {
    let Ok(rooted) = RootedDir::open_no_follow(root) else {
        return SymbolIndex::default();
    };
    let Some(files) = code_files(&rooted, limits::MAX_FILES, limits::MAX_NODES) else {
        return SymbolIndex::default();
    };
    scan_files(&rooted, &files)
}

fn scan_files(root: &RootedDir, files: &[RepoFile]) -> SymbolIndex {
    let mut file_syms: Vec<FileSymbols> = Vec::new();
    let mut file_langs: Vec<Lang> = Vec::new();
    let mut file_imports: Vec<Vec<String>> = Vec::new();
    let mut total = 0usize;
    for file in files {
        if total >= limits::MAX_TOTAL_SYMBOLS {
            break;
        }
        let Some(ext) = file.relative.extension().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(lang) = Lang::from_ext(&ext.to_ascii_lowercase()) else {
            continue;
        };
        let text = match read_bounded(root, &file.relative, limits::MAX_FILE_BYTES) {
            Some(t) => t,
            None => continue,
        };
        let symbols = extract_symbols(&text, lang);
        if symbols.is_empty() {
            continue;
        }
        total += symbols.len();
        file_imports.push(extract_imports(&text, lang));
        file_langs.push(lang);
        file_syms.push(FileSymbols {
            rel_path: rel_display(&file.relative),
            symbols,
            score: 0.0,
        });
    }
    let edges = resolve_edges(&file_syms, &file_langs, &file_imports);
    let mut index = SymbolIndex {
        files: file_syms,
        edges,
    };
    rank(&mut index);
    index
}

fn read_bounded(root: &RootedDir, relative: &Path, max: usize) -> Option<String> {
    let bytes = root
        .read_prefix(relative, u64::try_from(max).unwrap_or(u64::MAX))
        .ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

// ---------------------------------------------------------------------------
// Ranking — lightweight importance heuristic (PageRank-style by reference).
// ---------------------------------------------------------------------------

/// Rank files (and reorder them by score, descending). The score blends:
/// - **entry-point bonus** — files named `main`/`index`/`lib`/… are the repo's
///   public face;
/// - **export ratio** — a file exporting many symbols is a module others use;
/// - **inbound imports (fan-in)** — how many other files actually import this
///   file (resolved [`SymbolIndex::edges`]); a file that participates in no
///   edge falls back to the historical same-name proxy (how many *other*
///   files declare a symbol of the same name), so an edge-less repo ranks
///   EXACTLY as before (fail-open);
/// - a mild **shallowness** bonus (top-level files tend to be more central).
///
/// This is deliberately O(files × symbols) with a single reference pass — no
/// iterative eigenvector solve. It's a *heuristic* to order an outline, not a
/// precise call graph. The sort reorders `files`, so `edges` indices are
/// remapped through the same permutation to stay consistent.
fn rank(index: &mut SymbolIndex) {
    if index.files.is_empty() {
        return;
    }

    // 1) Count, per exported symbol name, how many files declare it. Keys are
    //    OWNED `String`s so this map outlives the immutable borrow of
    //    `index.files` and doesn't conflict with the mutable scoring pass below.
    //    We only count *exported* names (private helpers pollute the count and
    //    aren't the symbols other modules depend on).
    let mut def_count: HashMap<String, usize> = HashMap::new();
    for f in &index.files {
        for s in &f.symbols {
            if s.exported && s.name.len() >= 3 {
                *def_count.entry(s.name.clone()).or_insert(0) += 1;
            }
        }
    }

    // 2) Inbound-reference proxy. We don't keep full file text, so a precise
    //    call graph is out of scope (and would need a heavy parser). Instead we
    //    use a cheap structural proxy: a symbol's importance ≈ the number of
    //    OTHER files that *also* declare a symbol of the same name — shared
    //    model names, interfaces implemented in many files, route handlers, etc.
    //    Deterministic and stable; a good-enough degree-centrality stand-in.
    let ref_count: HashMap<String, usize> = def_count
        .into_iter()
        .map(|(name, n)| (name, n.saturating_sub(1)))
        .collect();

    // 2b) Real fan-in from resolved import edges: inbound import count per
    //     file. A file that touches no edge keeps the same-name proxy below,
    //     so an edge-less scan scores identically to the pre-edge behaviour.
    let n = index.files.len();
    let mut fan_in = vec![0usize; n];
    let mut has_edge = vec![false; n];
    for &(from, to) in &index.edges {
        if from < n && to < n {
            fan_in[to] += 1;
            has_edge[from] = true;
            has_edge[to] = true;
        }
    }

    // 3) Score each file.
    for (i, f) in index.files.iter_mut().enumerate() {
        let stem = file_stem_lc(&f.rel_path);
        let entry_bonus = if ENTRY_STEMS.contains(&stem.as_str()) {
            6.0
        } else {
            0.0
        };
        let depth = f.rel_path.matches('/').count() as f64;
        let shallow_bonus = (4.0 - depth).max(0.0); // shallower → more central

        let exported = f.symbols.iter().filter(|s| s.exported).count() as f64;
        let export_score = exported.min(20.0) * 0.5;

        let inbound_score = if has_edge[i] {
            // Real fan-in: files other files actually import rank higher.
            (fan_in[i] as f64).min(20.0) * 1.5
        } else {
            // No resolved edges for this file → same-name proxy (fail-open).
            let inbound: usize = f
                .symbols
                .iter()
                .filter_map(|s| ref_count.get(s.name.as_str()))
                .sum();
            (inbound as f64).min(20.0) * 1.5
        };

        // A file with very many symbols gets a small bonus (it's a hub) but
        // capped so a generated file doesn't dominate.
        let size_score = (f.symbols.len() as f64).min(30.0) * 0.1;

        f.score = entry_bonus + shallow_bonus + export_score + inbound_score + size_score;

        // Sort symbols within the file: exported first, then by line order.
        f.symbols
            .sort_by(|a, b| b.exported.cmp(&a.exported).then(a.line.cmp(&b.line)));
    }

    // 4) Order files by descending score, then by path for stability — and
    //    remap the edge indices through the same permutation so they keep
    //    pointing at the right files.
    let mut tagged: Vec<(usize, FileSymbols)> = std::mem::take(&mut index.files)
        .into_iter()
        .enumerate()
        .collect();
    tagged.sort_by(|a, b| {
        b.1.score
            .partial_cmp(&a.1.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.rel_path.cmp(&b.1.rel_path))
    });
    let mut new_pos = vec![0usize; tagged.len()];
    for (new_i, (old_i, _)) in tagged.iter().enumerate() {
        new_pos[*old_i] = new_i;
    }
    index.files = tagged.into_iter().map(|(_, f)| f).collect();
    for e in &mut index.edges {
        if e.0 < new_pos.len() && e.1 < new_pos.len() {
            *e = (new_pos[e.0], new_pos[e.1]);
        }
    }
    index.edges.sort_unstable();
    index.edges.dedup();
}

/// Lower-cased file stem (`src/Main.rs` → `main`) for the entry-point check.
fn file_stem_lc(rel_path: &str) -> String {
    rel_path
        .rsplit('/')
        .next()
        .unwrap_or(rel_path)
        .rsplit_once('.')
        .map_or(rel_path, |(stem, _)| stem)
        .to_ascii_lowercase()
}

// ---------------------------------------------------------------------------
// Rendering — token-budgeted, scope-personalised signature outline.
// ---------------------------------------------------------------------------

/// Render the repo map: a `path:line`-keyed signature outline of `root`,
/// **personalised** by `scope` (path hints — files the current task touches
/// rank first) and capped at `budget_chars`.
///
/// `scope` entries are matched as **substrings** of each file's relative path
/// (so `"checkout"` matches `src/pages/checkout/Cart.tsx`); when any scope hint
/// is supplied, files matching it are emitted first (and never truncated away
/// before unrelated files). With an empty `scope`, the global importance order
/// from the internal global ranker is used.
///
/// Fail-open: an empty repo / unreadable root / `budget_chars == 0` returns an
/// empty `String`.
#[must_use]
pub fn repo_map(root: &Path, scope: &[String], budget_chars: usize) -> String {
    if budget_chars == 0 {
        return String::new();
    }
    let index = symbol_index(root);
    render_map(&index, scope, budget_chars)
}

/// Render an already-built [`SymbolIndex`] (split out for testing without I/O).
fn render_map(index: &SymbolIndex, scope: &[String], budget_chars: usize) -> String {
    if index.is_empty() || budget_chars == 0 {
        return String::new();
    }

    // Personalisation: scope-matching files come first (and are never
    // truncated away before unrelated files).
    let scope_lc: Vec<String> = scope
        .iter()
        .map(|s| s.to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    let matches_scope = |rel: &str| -> bool {
        if scope_lc.is_empty() {
            return false;
        }
        let rel_lc = rel.to_lowercase();
        scope_lc.iter().any(|hint| rel_lc.contains(hint.as_str()))
    };

    let n = index.files.len();
    let scope_hit: Vec<bool> = index
        .files
        .iter()
        .map(|f| matches_scope(&f.rel_path))
        .collect();

    // Random-walk-with-restart over the import graph, seeded by the
    // scope-matched files: structurally related files (imports of / importers
    // of the seeds, transitively) order ahead of unrelated ones. With no
    // seeds or no edges every score stays 0.0 and the stable sort below
    // reduces EXACTLY to the historical partition order (fail-open).
    let seeds: Vec<usize> = scope_hit
        .iter()
        .enumerate()
        .filter_map(|(i, &hit)| hit.then_some(i))
        .collect();
    let rwr = if seeds.is_empty() || index.edges.is_empty() {
        vec![0.0; n]
    } else {
        rwr_scores(n, &index.edges, &seeds)
    };

    // Budget hygiene: tests / fixtures / i18n / generated files don't eat the
    // firmware budget ahead of product code — unless the scope names them.
    let low_value: Vec<bool> = index
        .files
        .iter()
        .enumerate()
        .map(|(i, f)| !scope_hit[i] && is_low_value_path(&f.rel_path))
        .collect();

    let mut ordered: Vec<usize> = (0..n).collect();
    ordered.sort_by(|&a, &b| {
        scope_hit[b]
            .cmp(&scope_hit[a]) // scope matches first
            .then(low_value[a].cmp(&low_value[b])) // low-value files last
            .then(
                rwr[b]
                    .partial_cmp(&rwr[a])
                    .unwrap_or(std::cmp::Ordering::Equal),
            ) // structural relatedness
            .then(a.cmp(&b)) // stable: rank order breaks ties
    });

    let mut out = String::new();
    let header = "# Repo map (signature outline — path:line)\n";
    if header.len() < budget_chars {
        out.push_str(header);
    }

    // Whole-file commit discipline: a file's block is either emitted complete
    // or not at all — trimming to budget drops whole files from the tail
    // instead of splitting a block mid-way.
    let max_block = budget_chars.saturating_sub(header.len());
    for &fi in &ordered {
        let Some(block) = render_file_block(&index.files[fi], max_block) else {
            break;
        };
        if out.len() + block.len() > budget_chars {
            break;
        }
        out.push_str(&block);
    }

    out
}

/// Render one file's complete block (blank line + path + every symbol line),
/// capped at `max_len`. The cap only bites when the file alone could never
/// fit the whole budget: then trailing symbols are dropped at a whole-line
/// boundary with a `… +N more` marker (the block is still committed
/// atomically by the caller). `None` when not even the path line fits.
fn render_file_block(f: &FileSymbols, max_len: usize) -> Option<String> {
    /// Room reserved for the truncation marker line (`  … +N more\n` ≤ 32 B),
    /// so appending the marker can never itself overflow `max_len`.
    const MARKER_RESERVE: usize = 32;
    let mut block = format!("\n{}\n", f.rel_path);
    if block.len() > max_len {
        return None;
    }
    let total = f.symbols.len();
    for (k, s) in f.symbols.iter().enumerate() {
        let line = format!(
            "  {} {}  ·{}\n",
            s.kind.label(),
            truncate_sig(&s.signature, 160),
            s.line
        );
        // A non-final line must leave room for the marker it would force.
        let reserve = if k + 1 == total { 0 } else { MARKER_RESERVE };
        if block.len() + line.len() + reserve > max_len {
            if k == 0 {
                return None; // not even one symbol fits — skip the file
            }
            let more = total - k;
            let _ = std::fmt::Write::write_fmt(&mut block, format_args!("  … +{more} more\n"));
            return Some(block);
        }
        block.push_str(&line);
    }
    Some(block)
}

/// Path heuristics for files that should not eat the token budget ahead of
/// product code: tests, fixtures, i18n catalogs, generated output. They are
/// *demoted*, never dropped — and a scope hint naming them overrides the
/// demotion entirely (see [`render_map`]).
fn is_low_value_path(rel: &str) -> bool {
    let lc = rel.to_ascii_lowercase();
    let dir_seg = |seg: &str| -> bool {
        lc.starts_with(&format!("{seg}/")) || lc.contains(&format!("/{seg}/"))
    };
    dir_seg("tests")
        || dir_seg("test")
        || dir_seg("__tests__")
        || dir_seg("testdata")
        || dir_seg("spec")
        || dir_seg("fixtures")
        || dir_seg("fixture")
        || dir_seg("__fixtures__")
        || dir_seg("__mocks__")
        || dir_seg("__snapshots__")
        || dir_seg("i18n")
        || dir_seg("locales")
        || dir_seg("locale")
        || dir_seg("generated")
        || dir_seg("__generated__")
        || lc.contains("_test.")
        || lc.contains(".test.")
        || lc.contains(".spec.")
        || lc.contains("_spec.")
        || lc.contains(".generated.")
        || lc.contains(".g.dart")
        || lc.contains("_pb2.py")
        || lc.contains(".pb.go")
        || lc.contains(".min.js")
        || lc.ends_with(".lock")
        || lc.ends_with("conftest.py")
}

/// Personalised random walk with restart over the UNDIRECTED import graph:
/// `p ← (1-α)·W·p + α·r`, where `r` is uniform over the seed files and `W`
/// spreads each node's mass equally over its neighbours. Restart α = 0.25,
/// 25 iterations, computed only on the subgraph reachable from the seeds —
/// every unreachable file scores exactly `0.0` (so ties fall back to the
/// caller's rank order). Fail-open: no seeds / no edges → all zeros.
fn rwr_scores(n: usize, edges: &[(usize, usize)], seeds: &[usize]) -> Vec<f64> {
    const ALPHA: f64 = 0.25;
    const ITERS: usize = 25;
    let mut scores = vec![0.0f64; n];
    if n == 0 || edges.is_empty() {
        return scores;
    }
    let seeds: Vec<usize> = seeds.iter().copied().filter(|&s| s < n).collect();
    if seeds.is_empty() {
        return scores;
    }

    // Undirected adjacency (imports relate both ways for "what's relevant").
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    for &(a, b) in edges {
        if a < n && b < n && a != b {
            adj[a].push(b);
            adj[b].push(a);
        }
    }

    // Bound the walk to the component(s) reachable from the seeds.
    let mut in_sub = vec![false; n];
    let mut queue: std::collections::VecDeque<usize> = seeds.iter().copied().collect();
    for &s in &seeds {
        in_sub[s] = true;
    }
    while let Some(u) = queue.pop_front() {
        for &v in &adj[u] {
            if !in_sub[v] {
                in_sub[v] = true;
                queue.push_back(v);
            }
        }
    }

    #[allow(clippy::cast_precision_loss)]
    let restart = 1.0 / seeds.len() as f64;
    let mut p = vec![0.0f64; n];
    for &s in &seeds {
        p[s] += restart;
    }
    for _ in 0..ITERS {
        let mut next = vec![0.0f64; n];
        for u in 0..n {
            if !in_sub[u] || p[u] <= 0.0 || adj[u].is_empty() {
                continue;
            }
            #[allow(clippy::cast_precision_loss)]
            let share = p[u] * (1.0 - ALPHA) / adj[u].len() as f64;
            for &v in &adj[u] {
                next[v] += share;
            }
        }
        for &s in &seeds {
            next[s] += ALPHA * restart;
        }
        p = next;
    }
    for (i, v) in p.into_iter().enumerate() {
        if in_sub[i] {
            scores[i] = v;
        }
    }
    scores
}

/// Truncate a signature line to `max` chars (char-boundary safe), adding an
/// ellipsis marker. Keeps the outline scannable and within budget.
fn truncate_sig(sig: &str, max: usize) -> String {
    if sig.chars().count() <= max {
        return sig.to_string();
    }
    let mut s: String = sig.chars().take(max.saturating_sub(1)).collect();
    s.push('…');
    s
}

// ---------------------------------------------------------------------------
// mtime cache — incremental: skip the full re-scan when nothing changed.
// ---------------------------------------------------------------------------

/// Cache directory for the repo map, relative to the project root.
pub const REPOMAP_CACHE_DIR: &str = ".umadev/repomap-cache";

/// A valid cache can be sizeable in a very large repository (40k bounded
/// symbols plus import edges), but it must never be an unbounded read from a
/// workspace-controlled path.
const MAX_REPOMAP_CACHE_BYTES: u64 = 96 * 1024 * 1024;
const MAX_REPOMAP_SIGNATURE_BYTES: u64 = 32 * 1024 * 1024;

/// Schema version of the repo-map cache. The signature keys on each file's
/// mtime+size, which captures CONTENT edits but NOT a change to the symbol-scan
/// regexes / `SymbolKind` set / cache encoding: after such an upgrade the cached
/// `symbols.json` is silently stale until a file happens to change. Bumping this
/// (folded into [`mtime_signature`]) invalidates every older-schema cache. Bump
/// it whenever the language patterns, ranking, or cache wire form change.
///
/// v2: import edges ([`SymbolIndex::edges`], `E`-lines in the wire form) +
/// fan-in ranking — v1 caches carry no edges and must rebuild.
///
/// v3: the index body carries the digest of its signature. This makes the two
/// atomic cache files one logical generation: racing writers can cause a
/// harmless miss, but can never pair one writer's signature with another
/// writer's symbol body and return a false cache hit.
const REPOMAP_SCHEMA_VERSION: u32 = 3;

fn read_cache_file(root: &RootedDir, name: &str, max_bytes: u64) -> Option<Vec<u8>> {
    root.read_bounded(&Path::new(REPOMAP_CACHE_DIR).join(name), max_bytes)
        .ok()
}

fn ensure_repomap_cache_dir(root: &RootedDir) -> bool {
    root.ensure_dir(Path::new(".umadev"), false).is_ok()
        && root.ensure_dir(Path::new(REPOMAP_CACHE_DIR), true).is_ok()
}

/// One in-process memo entry: the resolved index plus the cheap freshness keys.
struct MemoEntry {
    /// The resolved [`SymbolIndex`] for a root (scope-independent — scope is
    /// applied later in [`render_map`], so the index is the same across scopes).
    index: SymbolIndex,
    /// When this entry was last validated against a full scan. The memo is
    /// trusted only for [`MEMO_TTL`] after this instant — a fixed (not sliding)
    /// window, so rapid repeat calls can never extend trust past the TTL and
    /// mask a change indefinitely.
    validated_at: std::time::Instant,
    /// The root directory's OWN mtime at validation — a single cheap `stat`,
    /// NOT a recursive walk. A bump (a direct child added / removed / renamed)
    /// invalidates the memo immediately even inside the TTL. Captured AFTER the
    /// scan so the cache write under `.umadev/` is already accounted for.
    /// `None` when the root is unreadable (then the TTL alone governs).
    root_mtime: Option<std::time::SystemTime>,
    /// The exact root generation this index came from.
    root: RootedDir,
}

/// How long an in-process memo entry is trusted before the next call re-verifies
/// with a full scan. Short by design: it only elides the walk on rapid
/// successive opens (e.g. several firmware composes within a turn), never masks a
/// real change for long. Kept below the repo-map cache test's ~1.1s edit gap so a
/// genuine edit always re-scans.
const MEMO_TTL: std::time::Duration = std::time::Duration::from_millis(800);

/// The process-wide repo-map memo, keyed by canonical root path. Fail-open: a
/// poisoned lock simply takes the full scan path.
fn memo_table() -> &'static std::sync::Mutex<HashMap<PathBuf, MemoEntry>> {
    static TABLE: OnceLock<std::sync::Mutex<HashMap<PathBuf, MemoEntry>>> = OnceLock::new();
    TABLE.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// One cheap `stat` of the root directory's own mtime (NOT a recursive walk).
/// Fail-open to `None` on any error.
fn dir_mtime(root: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(root).ok()?.modified().ok()
}

/// Resolve the index for `root`, with a cheap in-process fast-path on top of the
/// authoritative on-disk-cached scan ([`load_or_scan_full`]).
///
/// A warm (re-)open in the SAME process within [`MEMO_TTL`] AND with the root
/// dir's own mtime unchanged returns the memoized index WITHOUT the full
/// recursive directory walk + per-file `metadata()` stat that `code_files` +
/// `mtime_signature` would cost on every call. Bounded staleness: once the TTL
/// elapses (or the root dir bumps) the next call falls through to the full scan,
/// so a real file change still refreshes. Fail-open throughout: a poisoned lock
/// or a missing entry just takes the full scan.
fn load_or_scan(root: &Path) -> SymbolIndex {
    let Ok(rooted) = RootedDir::open_no_follow(root) else {
        return SymbolIndex::default();
    };
    let memo_key = root.to_path_buf();
    let now = std::time::Instant::now();

    // Fast-path: a still-fresh memo for this root short-circuits the walk+stat.
    // Valid iff inside the TTL AND the root dir's own mtime is unchanged.
    if let Ok(table) = memo_table().lock() {
        if let Some(entry) = table.get(&memo_key) {
            if entry.root.matches_path(root).unwrap_or(false)
                && entry.root_mtime == dir_mtime(root)
                && now.duration_since(entry.validated_at) < MEMO_TTL
            {
                return entry.index.clone();
            }
        }
    }

    // Slow path: the authoritative on-disk-cached full scan. A real change (the
    // TTL elapsed or the dir bumped) always lands here, so correctness holds.
    let index = load_or_scan_full(&rooted);

    // Refresh the memo for the next rapid re-open. Capture the root mtime AFTER
    // the scan: writing the cache under `.umadev/` may itself bump it, so a
    // follow-up call's cheap re-stat must compare against the post-write value.
    // Fail-open: a poisoned lock just skips the memo (next call simply re-walks).
    if let Ok(mut table) = memo_table().lock() {
        if let Ok(root_capability) = rooted.try_clone() {
            table.insert(
                memo_key,
                MemoEntry {
                    index: index.clone(),
                    validated_at: now,
                    root_mtime: dir_mtime(root),
                    root: root_capability,
                },
            );
        }
    }
    index
}

/// Load the cached index if its mtime signature still matches the repo, else
/// scan fresh and refresh the cache. All cache I/O is fail-open: any error
/// (no dir, corrupt file, write failure) falls through to a live scan and never
/// surfaces an error.
fn load_or_scan_full(root: &RootedDir) -> SymbolIndex {
    let Some(files) = code_files(root, limits::MAX_FILES, limits::MAX_NODES) else {
        return SymbolIndex::default();
    };
    if files.is_empty() {
        return SymbolIndex::default();
    }
    let signature = mtime_signature(&files);
    // Cache hit: signature matches AND the data deserialises.
    let stored_sig = read_cache_file(root, "signature.txt", MAX_REPOMAP_SIGNATURE_BYTES)
        .and_then(|bytes| String::from_utf8(bytes).ok());
    if stored_sig.as_deref() == Some(signature.as_str()) {
        if let Some(index) = read_cache_file(root, "symbols.json", MAX_REPOMAP_CACHE_BYTES)
            .and_then(|bytes| decode_index(&bytes, &signature))
        {
            return index;
        }
    }

    // Miss → scan and best-effort write the cache.
    let index = scan_files(root, &files);
    if let Some(bytes) = ensure_repomap_cache_dir(root)
        .then(|| encode_index(&index, &signature))
        .flatten()
    {
        // Body first, signature last: the signature is the generation commit
        // record. The digest inside the body rejects a cross-writer pairing.
        if root
            .atomic_write(
                &Path::new(REPOMAP_CACHE_DIR).join("symbols.json"),
                &bytes,
                true,
            )
            .is_ok()
        {
            let _ = root.atomic_write(
                &Path::new(REPOMAP_CACHE_DIR).join("signature.txt"),
                signature.as_bytes(),
                true,
            );
        }
    }
    index
}

/// Force the next [`symbol_index`] / [`repo_map`] call to re-scan by deleting
/// the cache signature. Fail-open (a missing cache is a no-op).
pub fn invalidate_cache(root: &Path) {
    if let Ok(rooted) = RootedDir::open_no_follow(root) {
        let _ = rooted.remove_regular_file(&Path::new(REPOMAP_CACHE_DIR).join("signature.txt"));
    }
    // Also drop the in-process fast-path memo, else the next call would return
    // the still-fresh memoized index instead of truly re-scanning. Fail-open: a
    // poisoned lock leaves the memo (the TTL still expires it shortly).
    if let Ok(mut table) = memo_table().lock() {
        table.remove(root);
    }
}

/// A deterministic mtime+size signature of the code files: one line per file,
/// `<rel_path>\t<mtime_nanos>\t<size>`, sorted, prefixed with the schema
/// version. Unlike the knowledge corpus signature (content hash), the repo map
/// is *local* working state, so cheap mtime+size is the right tradeoff (no need
/// to read every file to hash it). A read failure for a file omits it from the
/// signature (fail-open).
///
/// mtime is captured at NANOSECOND resolution, not seconds: a 1-second
/// granularity missed a same-second, same-length edit (rewrite a line to one of
/// equal byte length within the same second → identical signature → stale
/// `symbols.json`). Nanosecond mtime (which every modern filesystem tracks)
/// closes that window at no extra cost.
fn mtime_signature(files: &[RepoFile]) -> String {
    let mut entries: Vec<String> = files
        .iter()
        .map(|file| {
            let mtime = file
                .modified
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0u128, |d| d.as_nanos());
            let rel = rel_display(&file.relative);
            format!("{rel}\t{mtime}\t{}", file.len)
        })
        .collect();
    entries.sort();
    // Fold in the schema version so a scan-logic/cache-format upgrade
    // invalidates every older cache even when no file changed.
    let body = entries.join("\n");
    format!("schema=v{REPOMAP_SCHEMA_VERSION}\n{body}")
}

// --- Tiny self-contained (de)serialisation for the cache -------------------
// We avoid a serde derive on the public types to keep them dependency-free in
// shape; the cache format is a private, line-oriented encoding. A decode
// failure → cache miss (fail-open). This is NOT a stable public format.

/// Encode an index to the private cache wire form. Returns `None` on any
/// internal write error (never happens for `String`, but keeps the call site
/// uniformly fail-open).
fn signature_digest(signature: &str) -> String {
    format!("{:x}", Sha256::digest(signature.as_bytes()))
}

fn encode_index(index: &SymbolIndex, signature: &str) -> Option<Vec<u8>> {
    use std::fmt::Write as _;
    let mut s = String::new();
    writeln!(s, "H\t{}", signature_digest(signature)).ok()?;
    for f in &index.files {
        // F-line: file path + score.
        writeln!(s, "F\t{}\t{}", escape(&f.rel_path), f.score).ok()?;
        for sym in &f.symbols {
            // S-line: kind, exported, line, name, signature.
            writeln!(
                s,
                "S\t{}\t{}\t{}\t{}\t{}",
                kind_code(sym.kind),
                u8::from(sym.exported),
                sym.line,
                escape(&sym.name),
                escape(&sym.signature),
            )
            .ok()?;
        }
    }
    for &(from, to) in &index.edges {
        // E-line: import edge (importer file index, imported file index).
        writeln!(s, "E\t{from}\t{to}").ok()?;
    }
    Some(s.into_bytes())
}

/// Decode the private cache wire form. Any malformed line → `None` (cache miss).
fn decode_index(bytes: &[u8], expected_signature: &str) -> Option<SymbolIndex> {
    if u64::try_from(bytes.len()).ok()? > MAX_REPOMAP_CACHE_BYTES {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    let mut files: Vec<FileSymbols> = Vec::new();
    let mut edges: Vec<(usize, usize)> = Vec::new();
    let mut symbols = 0usize;
    let max_edges = limits::MAX_FILES.saturating_mul(import_limits::MAX_IMPORTS_PER_FILE);
    let max_lines = 1usize
        .saturating_add(limits::MAX_FILES)
        .saturating_add(limits::MAX_TOTAL_SYMBOLS)
        .saturating_add(max_edges);
    for (line_index, line) in text.lines().enumerate() {
        if line_index >= max_lines {
            return None;
        }
        let mut parts = line.split('\t');
        match parts.next()? {
            "H" if line_index == 0 => {
                if parts.next()? != signature_digest(expected_signature) || parts.next().is_some() {
                    return None;
                }
            }
            "F" => {
                if line_index == 0 || files.len() >= limits::MAX_FILES {
                    return None;
                }
                let encoded_path = parts.next()?;
                if encoded_path.len() > limits::MAX_LINE_BYTES.saturating_mul(4) {
                    return None;
                }
                let rel_path = unescape(encoded_path);
                let score = parts.next()?.parse::<f64>().ok()?;
                if !score.is_finite() || parts.next().is_some() {
                    return None;
                }
                files.push(FileSymbols {
                    rel_path,
                    symbols: Vec::new(),
                    score,
                });
            }
            "S" => {
                if line_index == 0 || symbols >= limits::MAX_TOTAL_SYMBOLS {
                    return None;
                }
                let f = files.last_mut()?;
                if f.symbols.len() >= limits::MAX_SYMBOLS_PER_FILE {
                    return None;
                }
                let kind = kind_from_code(parts.next()?)?;
                let exported = match parts.next()? {
                    "0" => false,
                    "1" => true,
                    _ => return None,
                };
                let lineno = parts.next()?.parse::<usize>().ok()?;
                let encoded_name = parts.next()?;
                let encoded_signature = parts.next()?;
                if encoded_name.len() > limits::MAX_LINE_BYTES.saturating_mul(2)
                    || encoded_signature.len() > limits::MAX_LINE_BYTES.saturating_mul(2)
                    || parts.next().is_some()
                {
                    return None;
                }
                let name = unescape(encoded_name);
                let signature = unescape(encoded_signature);
                f.symbols.push(Symbol {
                    name,
                    kind,
                    signature,
                    line: lineno,
                    exported,
                });
                symbols = symbols.saturating_add(1);
            }
            "E" => {
                if line_index == 0 || edges.len() >= max_edges {
                    return None;
                }
                let from = parts.next()?.parse::<usize>().ok()?;
                let to = parts.next()?.parse::<usize>().ok()?;
                if parts.next().is_some() {
                    return None;
                }
                edges.push((from, to));
            }
            _ => return None,
        }
    }
    if !text.starts_with("H\t") {
        return None;
    }
    // An edge pointing outside the file set is corrupt → cache miss.
    if edges
        .iter()
        .any(|&(from, to)| from >= files.len() || to >= files.len())
    {
        return None;
    }
    Some(SymbolIndex { files, edges })
}

/// Escape tab / newline / backslash so a field round-trips through the
/// tab-separated cache format.
fn escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
}

/// Inverse of [`escape`].
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('t') => out.push('\t'),
                Some('n') => out.push('\n'),
                Some('\\') => out.push('\\'),
                Some(other) => out.push(other),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Single-char code for a [`SymbolKind`] in the cache format.
fn kind_code(k: SymbolKind) -> char {
    match k {
        SymbolKind::Function => 'f',
        SymbolKind::Class => 'c',
        SymbolKind::Enum => 'e',
        SymbolKind::Interface => 'i',
        SymbolKind::TypeAlias => 't',
        SymbolKind::Const => 'k',
        SymbolKind::Other => 'o',
    }
}

/// Inverse of [`kind_code`].
fn kind_from_code(s: &str) -> Option<SymbolKind> {
    Some(match s {
        "f" => SymbolKind::Function,
        "c" => SymbolKind::Class,
        "e" => SymbolKind::Enum,
        "i" => SymbolKind::Interface,
        "t" => SymbolKind::TypeAlias,
        "k" => SymbolKind::Const,
        "o" => SymbolKind::Other,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    /// Helper: write a file under `root`, creating parent dirs.
    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, body).unwrap();
    }

    /// Collect all symbol names across the index (for assertions).
    fn names(index: &SymbolIndex) -> Vec<String> {
        index
            .files
            .iter()
            .flat_map(|f| f.symbols.iter().map(|s| s.name.clone()))
            .collect()
    }

    // --- (1) multi-language symbol extraction --------------------------------

    #[test]
    fn extract_typescript() {
        let syms = extract_symbols(
            "export function hello(a: string): number { return 1 }\n\
             export class Widget {}\n\
             interface Shape { area(): number }\n\
             export const enum Color { Red }\n\
             export type Id = string\n\
             export const make = (x: number) => x * 2\n\
             const helper = async () => 1\n",
            Lang::JsTs,
        );
        let got: Vec<(&str, SymbolKind)> = syms.iter().map(|s| (s.name.as_str(), s.kind)).collect();
        assert!(got.contains(&("hello", SymbolKind::Function)));
        assert!(got.contains(&("Widget", SymbolKind::Class)));
        assert!(got.contains(&("Shape", SymbolKind::Interface)));
        assert!(got.contains(&("Color", SymbolKind::Enum)));
        assert!(got.contains(&("Id", SymbolKind::TypeAlias)));
        assert!(got.contains(&("make", SymbolKind::Function)));
        assert!(got.contains(&("helper", SymbolKind::Function)));
        // export-ness is detected for the leading `export` lines.
        let hello = syms.iter().find(|s| s.name == "hello").unwrap();
        assert!(hello.exported);
        let helper = syms.iter().find(|s| s.name == "helper").unwrap();
        assert!(!helper.exported);
    }

    #[test]
    fn extract_python() {
        let syms = extract_symbols(
            "def public_fn(x):\n    pass\n\
             async def fetch(self):\n    pass\n\
             class Service:\n    pass\n\
             def _private():\n    pass\n",
            Lang::Python,
        );
        let n: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(n.contains(&"public_fn"));
        assert!(n.contains(&"fetch"));
        assert!(n.contains(&"Service"));
        assert!(n.contains(&"_private"));
        // Underscore prefix → not exported.
        assert!(!syms.iter().find(|s| s.name == "_private").unwrap().exported);
        assert!(
            syms.iter()
                .find(|s| s.name == "public_fn")
                .unwrap()
                .exported
        );
    }

    #[test]
    fn extract_rust() {
        let syms = extract_symbols(
            "pub fn run(arg: u32) -> Result<()> { Ok(()) }\n\
             pub(crate) async fn fetch() {}\n\
             pub struct Engine { x: u8 }\n\
             enum Mode { A, B }\n\
             pub trait Driver {}\n\
             pub type Alias = u32;\n\
             pub const MAX: usize = 9;\n\
             fn private_helper() {}\n",
            Lang::Rust,
        );
        let got: Vec<(&str, SymbolKind, bool)> = syms
            .iter()
            .map(|s| (s.name.as_str(), s.kind, s.exported))
            .collect();
        assert!(got.contains(&("run", SymbolKind::Function, true)));
        assert!(got.contains(&("fetch", SymbolKind::Function, true)));
        assert!(got.contains(&("Engine", SymbolKind::Class, true)));
        assert!(got.contains(&("Mode", SymbolKind::Enum, false)));
        assert!(got.contains(&("Driver", SymbolKind::Interface, true)));
        assert!(got.contains(&("Alias", SymbolKind::TypeAlias, true)));
        assert!(got.contains(&("MAX", SymbolKind::Const, true)));
        assert!(got.contains(&("private_helper", SymbolKind::Function, false)));
    }

    #[test]
    fn extract_go() {
        let syms = extract_symbols(
            "func PublicFn() {}\n\
             func (s *Server) Handle() {}\n\
             type User struct {}\n\
             type Repo interface {}\n\
             func privateFn() {}\n",
            Lang::Go,
        );
        let got: Vec<(&str, SymbolKind, bool)> = syms
            .iter()
            .map(|s| (s.name.as_str(), s.kind, s.exported))
            .collect();
        assert!(got.contains(&("PublicFn", SymbolKind::Function, true)));
        assert!(got.contains(&("Handle", SymbolKind::Function, true)));
        assert!(got.contains(&("User", SymbolKind::Class, true)));
        assert!(got.contains(&("Repo", SymbolKind::Interface, true)));
        // lowercase → unexported in Go.
        assert!(got.contains(&("privateFn", SymbolKind::Function, false)));
    }

    #[test]
    fn extract_java_kotlin_ruby_php_csharp_swift_dart() {
        // Java
        let j = extract_symbols(
            "public class Account {}\npublic interface Repo {}\npublic enum State {}\n",
            Lang::Java,
        );
        assert!(j
            .iter()
            .any(|s| s.name == "Account" && s.kind == SymbolKind::Class));
        assert!(j
            .iter()
            .any(|s| s.name == "Repo" && s.kind == SymbolKind::Interface));
        assert!(j
            .iter()
            .any(|s| s.name == "State" && s.kind == SymbolKind::Enum));
        // Kotlin fun shares the Java pattern set.
        let k = extract_symbols("suspend fun load() {}\n", Lang::Java);
        assert!(k
            .iter()
            .any(|s| s.name == "load" && s.kind == SymbolKind::Function));
        // Ruby
        let rb = extract_symbols(
            "def greet\nend\nclass Foo\nend\nmodule Bar\nend\n",
            Lang::Ruby,
        );
        assert!(rb
            .iter()
            .any(|s| s.name == "greet" && s.kind == SymbolKind::Function));
        assert!(rb
            .iter()
            .any(|s| s.name == "Foo" && s.kind == SymbolKind::Class));
        assert!(rb.iter().any(|s| s.name == "Bar"));
        // PHP
        let php = extract_symbols(
            "public function handle() {}\nclass Controller {}\ninterface Plug {}\n",
            Lang::Php,
        );
        assert!(php
            .iter()
            .any(|s| s.name == "handle" && s.kind == SymbolKind::Function));
        assert!(php
            .iter()
            .any(|s| s.name == "Controller" && s.kind == SymbolKind::Class));
        assert!(php
            .iter()
            .any(|s| s.name == "Plug" && s.kind == SymbolKind::Interface));
        // C#
        let cs = extract_symbols(
            "public class Svc {}\npublic interface IRepo {}\n",
            Lang::CSharp,
        );
        assert!(cs
            .iter()
            .any(|s| s.name == "Svc" && s.kind == SymbolKind::Class));
        assert!(cs
            .iter()
            .any(|s| s.name == "IRepo" && s.kind == SymbolKind::Interface));
        // Swift
        let sw = extract_symbols(
            "public func run() {}\nstruct Model {}\nprotocol P {}\n",
            Lang::Swift,
        );
        assert!(sw
            .iter()
            .any(|s| s.name == "run" && s.kind == SymbolKind::Function));
        assert!(sw
            .iter()
            .any(|s| s.name == "Model" && s.kind == SymbolKind::Class));
        assert!(sw
            .iter()
            .any(|s| s.name == "P" && s.kind == SymbolKind::Interface));
        // Dart
        let dt = extract_symbols(
            "class Page {}\nenum Status { ok }\nmixin Logger {}\n",
            Lang::Dart,
        );
        assert!(dt
            .iter()
            .any(|s| s.name == "Page" && s.kind == SymbolKind::Class));
        assert!(dt
            .iter()
            .any(|s| s.name == "Status" && s.kind == SymbolKind::Enum));
        assert!(dt
            .iter()
            .any(|s| s.name == "Logger" && s.kind == SymbolKind::Interface));
    }

    #[test]
    fn does_not_match_calls_or_comments() {
        // Function *calls* and comments must not be captured as declarations.
        let syms = extract_symbols(
            "// function notReal()\n\
             hello();\n\
             const x = foo();\n\
             /* class AlsoNotReal */\n",
            Lang::JsTs,
        );
        let n = names(&SymbolIndex {
            files: vec![FileSymbols {
                rel_path: "x.ts".into(),
                symbols: syms,
                score: 0.0,
            }],
            edges: Vec::new(),
        });
        assert!(!n.iter().any(|s| s == "notReal"));
        assert!(!n.iter().any(|s| s == "AlsoNotReal"));
        assert!(!n.iter().any(|s| s == "hello"));
    }

    #[test]
    fn multiline_block_comment_symbols_are_not_captured() {
        // LOW #6: declaration-looking lines INSIDE a multi-line `/* ... */` block
        // comment must NOT be captured as phantom symbols; the real symbol after
        // the comment must still be found.
        let syms = extract_symbols(
            "/*\nexport class CommentedOut {}\nfunction alsoCommented() {}\n*/\nexport class Real {}\n",
            Lang::JsTs,
        );
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"Real"),
            "the real symbol after the comment is captured"
        );
        assert!(
            !names.contains(&"CommentedOut"),
            "a class inside the block comment must not be captured: {names:?}"
        );
        assert!(
            !names.contains(&"alsoCommented"),
            "a fn inside the block comment must not be captured: {names:?}"
        );
    }

    #[test]
    fn python_docstring_symbols_are_not_captured() {
        // LOW #6: a `def`/`class` inside a triple-quoted docstring is documentation
        // text, not a symbol.
        let syms = extract_symbols(
            "def real_fn():\n    \"\"\"\n    def fake_in_doc():\n    class FakeInDoc:\n    \"\"\"\n    pass\nclass RealClass:\n    pass\n",
            Lang::Python,
        );
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"real_fn"),
            "the real def is captured: {names:?}"
        );
        assert!(
            names.contains(&"RealClass"),
            "the real class is captured: {names:?}"
        );
        assert!(
            !names.contains(&"fake_in_doc"),
            "a def inside the docstring must not be captured: {names:?}"
        );
        assert!(
            !names.contains(&"FakeInDoc"),
            "a class inside the docstring must not be captured: {names:?}"
        );
    }

    // --- (2) ranking ---------------------------------------------------------

    #[test]
    fn ranking_prefers_entry_points_and_shared_symbols() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        // Entry-point file at the root.
        write(
            root,
            "index.ts",
            "export function bootstrap() {}\nexport class App {}\n",
        );
        // A deeply-nested helper with a private fn — should rank below.
        write(
            root,
            "src/internal/deep/util.ts",
            "function localThing() {}\n",
        );
        // A shared model name declared in two files (boosts inbound proxy).
        write(root, "src/models/user.ts", "export class UserModel {}\n");
        write(
            root,
            "src/api/user.ts",
            "export class UserModel {}\nexport function getUser() {}\n",
        );

        let index = symbol_index(root);
        assert!(!index.is_empty());
        // The entry-point file should be ranked first (highest score).
        let top = &index.files[0];
        assert_eq!(top.rel_path, "index.ts", "entry point should rank first");
        // The deeply-nested private-only helper should rank last.
        let last = index.files.last().unwrap();
        assert_eq!(last.rel_path, "src/internal/deep/util.ts");
    }

    #[test]
    fn exported_symbols_sort_before_private_within_file() {
        let mut index = SymbolIndex {
            files: vec![FileSymbols {
                rel_path: "a.rs".into(),
                symbols: vec![
                    Symbol {
                        name: "priv_a".into(),
                        kind: SymbolKind::Function,
                        signature: "fn priv_a()".into(),
                        line: 1,
                        exported: false,
                    },
                    Symbol {
                        name: "pub_b".into(),
                        kind: SymbolKind::Function,
                        signature: "pub fn pub_b()".into(),
                        line: 2,
                        exported: true,
                    },
                ],
                score: 0.0,
            }],
            edges: Vec::new(),
        };
        rank(&mut index);
        assert_eq!(index.files[0].symbols[0].name, "pub_b");
        assert_eq!(index.files[0].symbols[1].name, "priv_a");
    }

    // --- (3) scope personalisation ------------------------------------------

    #[test]
    fn scope_hint_promotes_matching_files_first() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        // An entry point that would normally rank first.
        write(root, "index.ts", "export function main() {}\n");
        // The task-relevant file, deeper and not an entry point.
        write(
            root,
            "src/pages/checkout/Cart.tsx",
            "export function Cart() {}\nexport class CartItem {}\n",
        );

        // No scope → global order: entry point first.
        let global = repo_map(root, &[], 4000);
        let idx_pos_global = global.find("index.ts").unwrap();
        let cart_pos_global = global.find("Cart.tsx").unwrap();
        assert!(
            idx_pos_global < cart_pos_global,
            "without scope, entry point ranks first"
        );

        // Scope hint "checkout" → Cart.tsx must come first.
        let scoped = repo_map(root, &["checkout".to_string()], 4000);
        let cart_pos = scoped.find("Cart.tsx").unwrap();
        let idx_pos = scoped.find("index.ts").unwrap();
        assert!(
            cart_pos < idx_pos,
            "scope hint should promote the matching file"
        );
    }

    // --- (4) token-budget truncation ----------------------------------------

    #[test]
    fn budget_truncates_and_never_exceeds() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        // Many symbols across many files.
        for i in 0..40 {
            write(
                root,
                &format!("src/mod{i}.ts"),
                "export function alpha() {}\nexport function beta() {}\nexport class Gamma {}\n",
            );
        }
        let budget = 300;
        let map = repo_map(root, &[], budget);
        assert!(!map.is_empty());
        assert!(
            map.len() <= budget,
            "rendered map {} must fit budget {}",
            map.len(),
            budget
        );
        // Zero budget → empty.
        assert!(repo_map(root, &[], 0).is_empty());
    }

    #[test]
    fn scope_files_survive_truncation_over_unrelated() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "src/auth/login.ts", "export function login() {}\n");
        for i in 0..30 {
            write(
                root,
                &format!("src/other/f{i}.ts"),
                "export function noise() {}\n",
            );
        }
        // Small budget: the scope-matching file should still appear because it
        // is rendered first.
        let map = repo_map(root, &["login".to_string()], 220);
        assert!(
            map.contains("login.ts"),
            "scope file must survive truncation: {map:?}"
        );
    }

    // --- (5) mtime incremental cache ----------------------------------------

    #[test]
    fn cache_hit_when_unchanged_and_refreshes_on_change() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "a.rs", "pub fn one() {}\n");

        // First call populates the cache.
        let first = symbol_index(root);
        assert_eq!(first.symbol_count(), 1);
        let sig_path = root.join(REPOMAP_CACHE_DIR).join("signature.txt");
        assert!(sig_path.exists(), "cache signature should be written");
        let sig1 = fs::read_to_string(&sig_path).unwrap();

        // Second call: cache hit → identical signature on disk (not rewritten
        // with a different value), identical result.
        let second = symbol_index(root);
        assert_eq!(second.symbol_count(), 1);
        let sig2 = fs::read_to_string(&sig_path).unwrap();
        assert_eq!(sig1, sig2);
        assert_eq!(first, second);

        // Change a file (and bump its mtime explicitly to be robust on fast FS).
        std::thread::sleep(std::time::Duration::from_millis(1100));
        write(root, "a.rs", "pub fn one() {}\npub fn two() {}\n");
        let third = symbol_index(root);
        assert_eq!(third.symbol_count(), 2, "changed file should be re-scanned");
        let sig3 = fs::read_to_string(&sig_path).unwrap();
        assert_ne!(sig1, sig3, "signature should change after edit");
    }

    // --- (5b) in-process fast-path memo (Fix A2) ----------------------------

    #[test]
    fn fast_path_memo_short_circuits_full_scan_within_ttl() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "a.rs", "pub fn one() {}\n");

        // First call: a full scan that populates BOTH the on-disk cache and the
        // in-process memo.
        let _ = symbol_index(root);
        let sig_path = root.join(REPOMAP_CACHE_DIR).join("signature.txt");
        assert!(sig_path.exists(), "first scan writes the on-disk signature");

        // Delete the on-disk signature DIRECTLY (not via `invalidate_cache`,
        // which also clears the in-process memo). Removing a file deep under
        // `.umadev/` does not bump the root dir's own mtime, so the memo stays
        // fresh.
        fs::remove_file(&sig_path).unwrap();

        // Immediate re-call (within MEMO_TTL): the in-process fast-path returns
        // the memoized index WITHOUT the full walk — so it never reaches the
        // on-disk scan and the signature is NOT rewritten. (A full re-walk would
        // miss the on-disk sig and rewrite it.) This observes "no re-walk".
        let _ = symbol_index(root);
        assert!(
            !sig_path.exists(),
            "fast-path memo must short-circuit the full scan within the TTL"
        );
    }

    #[test]
    fn fast_path_memo_rescans_after_ttl_on_change() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "a.rs", "pub fn one() {}\n");
        assert_eq!(symbol_index(root).symbol_count(), 1);

        // Past the TTL the memo is stale, so the next call re-verifies with a
        // full scan and picks up the edit — correctness still holds.
        std::thread::sleep(MEMO_TTL + std::time::Duration::from_millis(300));
        write(root, "a.rs", "pub fn one() {}\npub fn two() {}\n");
        assert_eq!(
            symbol_index(root).symbol_count(),
            2,
            "a real change after the TTL is re-scanned"
        );
    }

    #[test]
    fn mtime_signature_carries_schema_version() {
        // A scan-logic/cache-format upgrade (bump REPOMAP_SCHEMA_VERSION) must
        // invalidate older caches even when no file changed: the version is
        // folded into the signature, so an old `.sig` can no longer match.
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "a.rs", "pub fn one() {}\n");
        let rooted = RootedDir::open_no_follow(root).unwrap();
        let files = code_files(&rooted, limits::MAX_FILES, limits::MAX_NODES).unwrap();
        let sig = mtime_signature(&files);
        assert!(
            sig.starts_with(&format!("schema=v{REPOMAP_SCHEMA_VERSION}")),
            "signature must be prefixed with the schema version: {sig}"
        );
    }

    #[test]
    fn bounded_code_walk_selects_a_stable_lexicographic_subset() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        // Deliberately create in the opposite order. The file cap must be
        // applied after stable ordering, not after filesystem enumeration.
        write(root, "z.rs", "fn z() {}\n");
        write(root, "b.rs", "fn b() {}\n");
        write(root, "a.rs", "fn a() {}\n");
        let rooted = RootedDir::open_no_follow(root).unwrap();
        let files = code_files(&rooted, 2, limits::MAX_NODES).unwrap();
        let names = files
            .iter()
            .filter_map(|file| file.relative.file_name().and_then(|name| name.to_str()))
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["a.rs", "b.rs"]);
    }

    #[test]
    fn directory_entry_cap_is_enforced_before_collection_grows() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "a.rs", "fn a() {}\n");
        write(root, "b.rs", "fn b() {}\n");
        write(root, "c.rs", "fn c() {}\n");
        let rooted = RootedDir::open_no_follow(root).unwrap();
        assert!(
            code_files(&rooted, limits::MAX_FILES, 2).is_none(),
            "a directory larger than the node budget must fail closed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn source_links_and_fifos_are_never_followed_or_opened() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let root = dir.path();
        write(root, "safe.rs", "pub fn safe() {}\n");
        fs::write(
            outside.path().join("outside.rs"),
            "pub fn outside_secret() {}\n",
        )
        .unwrap();
        symlink(outside.path().join("outside.rs"), root.join("linked.rs")).unwrap();
        symlink(outside.path(), root.join("linked-dir")).unwrap();
        assert!(std::process::Command::new("mkfifo")
            .arg(root.join("blocked.rs"))
            .status()
            .unwrap()
            .success());

        let started = std::time::Instant::now();
        let index = scan(root);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        let symbols = names(&index);
        assert!(symbols.contains(&"safe".to_string()));
        assert!(!symbols.contains(&"outside_secret".to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn linked_root_is_rejected_and_open_root_generation_stays_pinned() {
        use std::os::unix::fs::symlink;

        let parent = tempdir().unwrap();
        let outside = tempdir().unwrap();
        write(outside.path(), "outside.rs", "pub fn outside_secret() {}\n");
        let linked_root = parent.path().join("linked-root");
        symlink(outside.path(), &linked_root).unwrap();
        assert!(
            scan(&linked_root).is_empty(),
            "a linked root must fail closed"
        );

        let root = parent.path().join("project");
        fs::create_dir(&root).unwrap();
        write(&root, "source.rs", "pub fn original() {}\n");
        let rooted = RootedDir::open_no_follow(&root).unwrap();
        let files = code_files(&rooted, limits::MAX_FILES, limits::MAX_NODES).unwrap();
        let moved = parent.path().join("project-moved");
        fs::rename(&root, &moved).unwrap();
        fs::create_dir(&root).unwrap();
        write(&root, "source.rs", "pub fn replacement() {}\n");

        let index = scan_files(&rooted, &files);
        let symbols = names(&index);
        assert!(symbols.contains(&"original".to_string()));
        assert!(!symbols.contains(&"replacement".to_string()));
    }

    #[test]
    fn invalidate_cache_forces_rescan() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "a.rs", "pub fn one() {}\n");
        let _ = symbol_index(root);
        let sig_path = root.join(REPOMAP_CACHE_DIR).join("signature.txt");
        assert!(sig_path.exists());
        invalidate_cache(root);
        assert!(!sig_path.exists(), "invalidate removes the signature");
        // Re-scan repopulates it.
        let _ = symbol_index(root);
        assert!(sig_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn cache_read_write_and_invalidation_never_follow_a_managed_symlink() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let root = dir.path();
        write(root, "a.rs", "pub fn one() {}\n");
        fs::create_dir(root.join(".umadev")).unwrap();
        fs::write(outside.path().join("signature.txt"), "outside-signature").unwrap();
        fs::write(outside.path().join("symbols.json"), "outside-symbols").unwrap();
        symlink(outside.path(), root.join(".umadev").join("repomap-cache")).unwrap();

        let rooted = RootedDir::open_no_follow(root).unwrap();
        let scanned = load_or_scan_full(&rooted);
        assert_eq!(
            scanned.symbol_count(),
            1,
            "unsafe cache falls back to a live scan"
        );
        invalidate_cache(root);

        assert_eq!(
            fs::read_to_string(outside.path().join("signature.txt")).unwrap(),
            "outside-signature"
        );
        assert_eq!(
            fs::read_to_string(outside.path().join("symbols.json")).unwrap(),
            "outside-symbols"
        );
    }

    #[test]
    fn cache_roundtrip_preserves_index() {
        let index = SymbolIndex {
            files: vec![FileSymbols {
                rel_path: "src/x.ts".into(),
                symbols: vec![Symbol {
                    name: "Foo".into(),
                    kind: SymbolKind::Class,
                    signature: "export class Foo extends Bar<\tTab>".into(),
                    line: 12,
                    exported: true,
                }],
                score: 3.5,
            }],
            edges: Vec::new(),
        };
        let bytes = encode_index(&index, "sig-a").unwrap();
        let back = decode_index(&bytes, "sig-a").unwrap();
        assert_eq!(index, back);
    }

    #[test]
    fn decode_rejects_garbage_returns_none() {
        assert!(decode_index(b"not a valid cache line\n", "sig-a").is_none());
        assert!(decode_index(b"\xff\xfe\x00", "sig-a").is_none()); // invalid UTF-8
    }

    #[test]
    fn decode_rejects_a_body_from_another_signature_generation() {
        let bytes = encode_index(&SymbolIndex::default(), "sig-a").unwrap();
        assert!(decode_index(&bytes, "sig-b").is_none());
    }

    // --- (6) fail-open: empty / unreadable / pathological --------------------

    #[test]
    fn empty_repo_yields_empty() {
        let dir = tempdir().unwrap();
        assert!(symbol_index(dir.path()).is_empty());
        assert_eq!(repo_map(dir.path(), &[], 1000), "");
    }

    #[test]
    fn missing_root_does_not_panic() {
        let missing = Path::new("/nonexistent/umadev/repomap/path/xyz");
        assert!(symbol_index(missing).is_empty());
        assert_eq!(repo_map(missing, &["x".into()], 1000), "");
    }

    #[test]
    fn skips_vendor_and_build_dirs() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "src/real.rs", "pub fn real() {}\n");
        write(
            root,
            "node_modules/dep/index.js",
            "export function dep() {}\n",
        );
        write(root, "target/gen.rs", "pub fn gen() {}\n");
        write(root, ".git/hook.py", "def hidden(): pass\n");
        let index = symbol_index(root);
        let n = names(&index);
        assert!(n.contains(&"real".to_string()));
        assert!(!n.contains(&"dep".to_string()), "node_modules skipped");
        assert!(!n.contains(&"gen".to_string()), "target skipped");
        assert!(!n.contains(&"hidden".to_string()), "dotdir skipped");
    }

    #[test]
    fn huge_minified_line_does_not_panic_or_hang() {
        // A single 5 MB line full of `function` keywords — must be skipped, and
        // certainly must not hang or panic.
        let mut giant = String::from("export function realOne() {}\n");
        giant.push_str(&"function x(){};".repeat(400_000)); // ~6 MB single line
        let syms = extract_symbols(&giant, Lang::JsTs);
        // Only the short first line should yield a symbol; the giant line is
        // over MAX_LINE_BYTES and skipped.
        assert_eq!(syms.iter().filter(|s| s.name == "realOne").count(), 1);
    }

    #[test]
    fn huge_file_byte_cap_is_respected() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        // Build a file that exceeds MAX_FILE_BYTES with many small functions.
        let mut body = String::new();
        // Each line ~24 bytes; need > 512 KiB → ~25k lines.
        for i in 0..30_000 {
            body.push_str(&format!("export function f{i}() {{}}\n"));
        }
        assert!(body.len() > limits::MAX_FILE_BYTES);
        write(root, "big.ts", &body);
        let index = symbol_index(root);
        // It scans only up to the byte cap; we just assert it didn't panic and
        // captured *some* but not all symbols.
        let count = index.symbol_count();
        assert!(count > 0);
        assert!(count < 30_000, "byte cap should truncate the read");
    }

    #[test]
    fn malformed_unicode_content_is_handled() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        // Write raw invalid-UTF-8 bytes mixed with a valid declaration.
        let mut bytes = b"pub fn valid() {}\n".to_vec();
        bytes.extend_from_slice(&[0xff, 0xfe, 0x00, 0x80]);
        bytes.extend_from_slice(b"\npub fn after() {}\n");
        fs::write(root.join("weird.rs"), &bytes).unwrap();
        // lossy decode → still extracts the valid lines, never panics.
        let index = symbol_index(root);
        let n = names(&index);
        assert!(n.contains(&"valid".to_string()));
        assert!(n.contains(&"after".to_string()));
    }

    // --- (7) cross-platform path rendering ----------------------------------

    #[test]
    fn rel_display_normalises_separators() {
        // Simulate a Windows-style absolute path under a root.
        let file = Path::new("C:\\proj\\src\\app.ts");
        let rel = rel_display(file);
        // Whatever the platform, the output must never contain a backslash.
        assert!(
            !rel.contains('\\'),
            "rendered path must be /-separated: {rel}"
        );
    }

    #[test]
    fn rendered_map_uses_forward_slashes() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "src/deep/nested/file.ts", "export function go() {}\n");
        let map = repo_map(root, &[], 4000);
        assert!(map.contains("src/deep/nested/file.ts"));
        assert!(!map.contains('\\'));
    }

    #[test]
    fn map_contains_path_line_and_signature() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "lib.rs", "pub fn alpha(x: u8) -> u8 { x }\n");
        let map = repo_map(root, &[], 4000);
        assert!(map.contains("lib.rs"));
        assert!(map.contains("fn pub fn alpha(x: u8) -> u8 { x }") || map.contains("alpha"));
        // line marker present.
        assert!(map.contains("·1"));
    }

    // --- (8) import extraction per language ----------------------------------

    #[test]
    fn extract_imports_js_ts_forms() {
        let specs = extract_imports(
            "import React from 'react';\n\
             import { a, b } from './mod';\n\
             import './side-effect';\n\
             export { x } from '../lib/x';\n\
             const y = require('./y');\n\
             const z = await import('./z');\n\
             } from './multiline';\n\
             // import notme from './no'\n\
             /* import alsonot from './no2' */\n",
            Lang::JsTs,
        );
        for want in [
            "react",
            "./mod",
            "./side-effect",
            "../lib/x",
            "./y",
            "./z",
            "./multiline",
        ] {
            assert!(specs.iter().any(|s| s == want), "missing {want}: {specs:?}");
        }
        assert!(
            !specs.iter().any(|s| s.contains("./no")),
            "comments skipped: {specs:?}"
        );
    }

    #[test]
    fn extract_imports_python_forms() {
        let specs = extract_imports(
            "import os\n\
             import a.b.c\n\
             from x.y import z\n\
             from . import sibling\n\
             from ..pkg import thing\n\
             import m1, m2\n\
             \"\"\"\n\
             import fake_in_docstring\n\
             \"\"\"\n",
            Lang::Python,
        );
        for want in ["os", "a.b.c", "x.y", ".", "..pkg", "m1", "m2"] {
            assert!(specs.iter().any(|s| s == want), "missing {want}: {specs:?}");
        }
        assert!(
            !specs.iter().any(|s| s.contains("fake_in_docstring")),
            "docstring imports skipped: {specs:?}"
        );
    }

    #[test]
    fn extract_imports_rust_forms() {
        let specs = extract_imports(
            "use std::collections::HashMap;\n\
             pub use crate::foo::bar;\n\
             use super::baz::{A, B};\n\
             mod util;\n\
             pub(crate) mod inner;\n\
             // use commented::out;\n",
            Lang::Rust,
        );
        assert!(
            specs.iter().any(|s| s == "std::collections::HashMap"),
            "{specs:?}"
        );
        assert!(specs.iter().any(|s| s == "crate::foo::bar"), "{specs:?}");
        assert!(
            specs.iter().any(|s| s.starts_with("super::baz")),
            "{specs:?}"
        );
        assert!(specs.iter().any(|s| s == "mod:util"), "{specs:?}");
        assert!(specs.iter().any(|s| s == "mod:inner"), "{specs:?}");
        assert!(!specs.iter().any(|s| s.contains("commented")), "{specs:?}");
    }

    #[test]
    fn extract_imports_go_forms() {
        let specs = extract_imports(
            "package main\n\
             import \"single/pkg\"\n\
             import (\n\
             \t\"fmt\"\n\
             \talias \"example.com/app/tools\"\n\
             )\n\
             func main() {}\n",
            Lang::Go,
        );
        assert_eq!(
            specs,
            vec!["single/pkg", "fmt", "example.com/app/tools"],
            "single + block imports captured in order"
        );
    }

    #[test]
    fn extract_imports_java_ruby_php_dart_swift_csharp() {
        let j = extract_imports(
            "import com.ex.Foo;\nimport static com.ex.Bar.baz;\n",
            Lang::Java,
        );
        assert!(j.contains(&"com.ex.Foo".to_string()), "{j:?}");
        assert!(j.contains(&"com.ex.Bar.baz".to_string()), "{j:?}");

        let rb = extract_imports(
            "require 'json'\nrequire_relative '../lib/util'\n",
            Lang::Ruby,
        );
        assert!(rb.contains(&"json".to_string()), "{rb:?}");
        assert!(rb.contains(&"rel:../lib/util".to_string()), "{rb:?}");

        let php = extract_imports(
            "use App\\Http\\Controller;\nrequire_once('lib/db.php');\n",
            Lang::Php,
        );
        assert!(
            php.contains(&"App\\Http\\Controller".to_string()),
            "{php:?}"
        );
        assert!(php.contains(&"lib/db.php".to_string()), "{php:?}");

        let dt = extract_imports(
            "import 'package:app/src/widget.dart';\nimport 'util.dart';\npart 'gen.g.dart';\n",
            Lang::Dart,
        );
        assert!(
            dt.contains(&"package:app/src/widget.dart".to_string()),
            "{dt:?}"
        );
        assert!(dt.contains(&"util.dart".to_string()), "{dt:?}");
        assert!(dt.contains(&"gen.g.dart".to_string()), "{dt:?}");

        let sw = extract_imports("import UIKit\n@testable import MyApp\n", Lang::Swift);
        assert!(sw.contains(&"UIKit".to_string()), "{sw:?}");
        assert!(sw.contains(&"MyApp".to_string()), "{sw:?}");

        let cs = extract_imports("using System.Text;\nusing var f = Open(x);\n", Lang::CSharp);
        assert!(cs.contains(&"System.Text".to_string()), "{cs:?}");
        assert_eq!(cs.len(), 1, "`using var` is not an import: {cs:?}");
    }

    // --- (9) edge resolution --------------------------------------------------

    /// Helper: index of a file in the rank-ordered index by its rel path.
    fn idx_of(index: &SymbolIndex, rel: &str) -> usize {
        index
            .files
            .iter()
            .position(|f| f.rel_path == rel)
            .unwrap_or_else(|| panic!("{rel} not in index"))
    }

    #[test]
    fn relative_import_edges_resolved_js() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "src/app.ts",
            "import { util } from './util';\nimport Helper from '../lib/helper';\nexport function app() {}\n",
        );
        write(root, "src/util.ts", "export function util() {}\n");
        write(root, "lib/helper.ts", "export default class Helper {}\n");
        let index = scan(root);
        let app = idx_of(&index, "src/app.ts");
        let util = idx_of(&index, "src/util.ts");
        let helper = idx_of(&index, "lib/helper.ts");
        assert_eq!(
            index.edges.len(),
            2,
            "two relative imports resolve: {:?}",
            index.edges
        );
        assert!(index.edges.contains(&(app, util)));
        assert!(index.edges.contains(&(app, helper)));
    }

    #[test]
    fn rust_mod_and_use_edges_resolved_and_deduped() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "src/main.rs",
            "mod util;\nuse crate::util::helper;\nfn main() {}\n",
        );
        write(root, "src/util.rs", "pub fn helper() {}\n");
        let index = scan(root);
        let main = idx_of(&index, "src/main.rs");
        let util = idx_of(&index, "src/util.rs");
        // `mod util;` and `use crate::util::helper` hit the same file → deduped.
        assert_eq!(index.edges, vec![(main, util)]);
    }

    #[test]
    fn go_import_block_edge_resolved_via_package_dir() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "main.go",
            "package main\nimport (\n\t\"fmt\"\n\t\"example.com/app/tools\"\n)\nfunc main() {}\n",
        );
        write(root, "tools/tool.go", "package tools\nfunc Tool() {}\n");
        let index = scan(root);
        let main = idx_of(&index, "main.go");
        let tool = idx_of(&index, "tools/tool.go");
        assert_eq!(
            index.edges,
            vec![(main, tool)],
            "module-path suffix resolves the package dir"
        );
    }

    #[test]
    fn java_fqn_import_edge_resolved() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "src/main/java/com/ex/App.java",
            "import com.ex.svc.Service;\npublic class App {}\n",
        );
        write(
            root,
            "src/main/java/com/ex/svc/Service.java",
            "public class Service {}\n",
        );
        let index = scan(root);
        let app = idx_of(&index, "src/main/java/com/ex/App.java");
        let svc = idx_of(&index, "src/main/java/com/ex/svc/Service.java");
        assert_eq!(index.edges, vec![(app, svc)]);
    }

    #[test]
    fn ambiguous_module_import_declined() {
        // Two files answer `import util` → the resolution is DECLINED, not
        // guessed (a wrong edge is worse than a missing one).
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "main.py", "import util\ndef run():\n    pass\n");
        write(root, "a/util.py", "def a_fn():\n    pass\n");
        write(root, "b/util.py", "def b_fn():\n    pass\n");
        let index = scan(root);
        assert!(
            index.edges.is_empty(),
            "ambiguous import must resolve to no edge: {:?}",
            index.edges
        );

        // Control: with exactly one candidate the same import resolves.
        let dir2 = tempdir().unwrap();
        let root2 = dir2.path();
        write(root2, "main.py", "import util\ndef run():\n    pass\n");
        write(root2, "a/util.py", "def a_fn():\n    pass\n");
        let index2 = scan(root2);
        let main = idx_of(&index2, "main.py");
        let util = idx_of(&index2, "a/util.py");
        assert_eq!(index2.edges, vec![(main, util)]);
    }

    #[test]
    fn import_escaping_repo_root_is_declined() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "app.ts",
            "import { x } from '../../outside';\nexport function app() {}\n",
        );
        write(root, "outside.ts", "export function x() {}\n");
        let index = scan(root);
        assert!(
            index.edges.is_empty(),
            "an import escaping the root never resolves: {:?}",
            index.edges
        );
    }

    // --- (10) fan-in ranking ---------------------------------------------------

    #[test]
    fn fan_in_ranking_beats_same_name_proxy() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        // core.ts is imported by three files but shares no symbol name — the
        // old same-name proxy would give it ZERO inbound signal.
        write(root, "pkg/core.ts", "export function coreThing() {}\n");
        for i in 1..=3 {
            write(
                root,
                &format!("consumer{i}.ts"),
                &format!(
                    "import {{ coreThing }} from './pkg/core';\nexport function c{i}() {{}}\n"
                ),
            );
        }
        // Two files share a symbol name (the proxy's favourite) but nothing
        // imports them.
        write(root, "pkg/dup_a.ts", "export class SharedThing {}\n");
        write(root, "pkg/dup_b.ts", "export class SharedThing {}\n");
        let index = scan(root);
        assert_eq!(
            index.edges.len(),
            3,
            "three inbound imports: {:?}",
            index.edges
        );
        let core = idx_of(&index, "pkg/core.ts");
        let dup = idx_of(&index, "pkg/dup_a.ts");
        assert!(
            core < dup,
            "real fan-in must outrank the same-name proxy (core at {core}, dup at {dup}): {:?}",
            index
                .files
                .iter()
                .map(|f| f.rel_path.as_str())
                .collect::<Vec<_>>()
        );
    }

    // --- (11) RWR scope personalisation ----------------------------------------

    /// One-symbol file for hand-built render_map indexes.
    fn one_sym_file(rel: &str) -> FileSymbols {
        FileSymbols {
            rel_path: rel.into(),
            symbols: vec![Symbol {
                name: "sym".into(),
                kind: SymbolKind::Function,
                signature: "fn sym()".into(),
                line: 1,
                exported: true,
            }],
            score: 0.0,
        }
    }

    /// Extract the emitted file order from a rendered map.
    fn rendered_order(map: &str) -> Vec<String> {
        map.lines()
            .filter(|l| !l.is_empty() && !l.starts_with(' ') && !l.starts_with('#'))
            .map(ToString::to_string)
            .collect()
    }

    #[test]
    fn rwr_zero_edges_falls_back_to_exact_partition_order() {
        // With NO edges, scope ordering must be EXACTLY the historical stable
        // partition: scope matches first, everything else in rank order.
        let index = SymbolIndex {
            files: vec![
                one_sym_file("src/other.ts"),
                one_sym_file("src/seed/entry.ts"),
                one_sym_file("src/linked.ts"),
            ],
            edges: Vec::new(),
        };
        let map = render_map(&index, &["seed".to_string()], 4000);
        assert_eq!(
            rendered_order(&map),
            vec!["src/seed/entry.ts", "src/other.ts", "src/linked.ts"],
            "zero edges → exactly the stable-partition order"
        );
    }

    #[test]
    fn rwr_orders_import_neighbours_before_unrelated() {
        // linked.ts imports the scoped seed → the walk pulls it ahead of the
        // (rank-higher) unrelated file.
        let index = SymbolIndex {
            files: vec![
                one_sym_file("src/other.ts"),
                one_sym_file("src/seed/entry.ts"),
                one_sym_file("src/linked.ts"),
            ],
            edges: vec![(2, 1)],
        };
        let map = render_map(&index, &["seed".to_string()], 4000);
        assert_eq!(
            rendered_order(&map),
            vec!["src/seed/entry.ts", "src/linked.ts", "src/other.ts"],
            "structurally related files order ahead of unrelated ones"
        );
    }

    #[test]
    fn rwr_scores_decay_with_distance_and_unreachable_zero() {
        // 0-1-2 chain seeded at 0; 3 is disconnected. (No seed-vs-neighbour
        // claim: on a path the degree-2 neighbour legitimately accumulates
        // comparable mass — what matters for ordering is distance decay and
        // that unreachable files stay at exactly zero.)
        let edges = vec![(0, 1), (1, 2)];
        let scores = rwr_scores(4, &edges, &[0]);
        assert!(scores[0] > scores[2], "seed outranks the two-hop node");
        assert!(scores[1] > scores[2], "one hop outranks two hops");
        assert!(scores[2] > 0.0, "reachable node gets mass");
        assert!(
            (scores[3] - 0.0).abs() < f64::EPSILON,
            "unreachable node scores exactly zero"
        );
    }

    // --- (12) budget hygiene ----------------------------------------------------

    #[test]
    fn budget_never_splits_a_file_block() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        for i in 0..10 {
            write(
                root,
                &format!("src/mod{i}.ts"),
                "export function alpha() {}\nexport function beta() {}\nexport class Gamma {}\n",
            );
        }
        let budget = 350;
        let map = repo_map(root, &[], budget);
        assert!(map.len() <= budget, "map fits budget");
        let headers = map.lines().filter(|l| l.ends_with(".ts")).count();
        assert!(headers >= 1, "at least one whole file fits: {map:?}");
        assert!(headers < 10, "budget forces dropping tail files");
        // Every emitted file block is COMPLETE: all three symbol lines present.
        for marker in ["\u{b7}1", "\u{b7}2", "\u{b7}3"] {
            assert_eq!(
                map.matches(marker).count(),
                headers,
                "no block is split mid-way ({marker}): {map:?}"
            );
        }
    }

    #[test]
    fn low_value_files_deprioritized_unless_scope_names_them() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "src/core.ts", "export function coreMain() {}\n");
        write(root, "src/core.test.ts", "export function coreTest() {}\n");
        write(root, "tests/helper.ts", "export function helper() {}\n");

        // No scope: product code before test/fixture files.
        let map = repo_map(root, &[], 4000);
        let core = map.find("src/core.ts\n").expect("core present");
        let test = map
            .find("src/core.test.ts")
            .expect("test present (demoted, not dropped)");
        let tests_dir = map.find("tests/helper.ts").expect("tests dir present");
        assert!(core < test, "product code before .test. file: {map}");
        assert!(core < tests_dir, "product code before tests/ dir: {map}");

        // Scope naming the test file overrides the demotion.
        let scoped = repo_map(root, &["core.test".to_string()], 4000);
        let test_pos = scoped.find("src/core.test.ts").unwrap();
        let core_pos = scoped.find("src/core.ts\n").unwrap();
        assert!(
            test_pos < core_pos,
            "scope-named test file comes first: {scoped}"
        );
    }

    #[test]
    fn giant_first_file_truncates_at_symbol_boundary_with_marker() {
        // A single file whose block alone exceeds the whole budget still
        // renders whole symbol lines plus a `… +N more` marker — never a
        // half-line.
        let mut body = String::new();
        for i in 0..50 {
            body.push_str(&format!("export function verylongname{i}() {{}}\n"));
        }
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, "src/huge.ts", &body);
        let budget = 400;
        let map = repo_map(root, &[], budget);
        assert!(map.len() <= budget);
        assert!(
            map.contains("src/huge.ts"),
            "the file still appears: {map:?}"
        );
        assert!(map.contains("more\n"), "truncation marker present: {map:?}");
        // Whole lines only: every symbol line ends with its `·N` marker.
        for line in map
            .lines()
            .filter(|l| l.starts_with("  ") && !l.contains("more"))
        {
            assert!(
                line.contains('\u{b7}'),
                "no half-emitted symbol line: {line:?}"
            );
        }
    }

    // --- (13) cache with edges ---------------------------------------------------

    #[test]
    fn cache_roundtrip_preserves_edges() {
        let index = SymbolIndex {
            files: vec![one_sym_file("a.ts"), one_sym_file("b.ts")],
            edges: vec![(0, 1)],
        };
        let bytes = encode_index(&index, "sig-a").unwrap();
        let back = decode_index(&bytes, "sig-a").unwrap();
        assert_eq!(index, back);
    }

    #[test]
    fn decode_rejects_out_of_range_edge() {
        let index = SymbolIndex {
            files: vec![one_sym_file("a.ts")],
            edges: Vec::new(),
        };
        let mut bytes = encode_index(&index, "sig-a").unwrap();
        bytes.extend_from_slice(b"E\t0\t5\n");
        assert!(
            decode_index(&bytes, "sig-a").is_none(),
            "edge past the file set is corrupt"
        );
    }

    #[test]
    fn edges_survive_disk_cache_reload() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "src/app.ts",
            "import { util } from './util';\nexport function app() {}\n",
        );
        write(root, "src/util.ts", "export function util() {}\n");
        // First call scans + writes the cache; second hits the on-disk cache.
        let rooted = RootedDir::open_no_follow(root).unwrap();
        let first = load_or_scan_full(&rooted);
        assert_eq!(first.edges.len(), 1);
        let second = load_or_scan_full(&rooted);
        assert_eq!(first, second, "edges round-trip through the on-disk cache");
    }
}
