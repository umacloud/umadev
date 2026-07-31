//! `adopt` — brownfield onboarding for an existing repository.
//!
//! The rest of the pipeline is **greenfield**: `research → three docs →
//! scaffold a brand-new app`. That leaves the highest-value case unserved —
//! a customer who already has a real codebase and wants UmaDev to drive
//! *incremental* work on it, not regenerate it from scratch.
//!
//! `run_adopt` does exactly that. It works on an **already-populated**
//! workspace and produces the minimal artifacts the engine needs to operate
//! on existing code safely. Every step **reuses** a capability that already
//! exists elsewhere in the workspace rather than reinventing it:
//!
//! 1. **Stack detection** — reuses [`crate::verify`] (`detect_project` +
//!    `verify_steps` + `detect_dev_server`) to recover the language, package
//!    manager, and the real test / build / lint commands.
//! 2. **Source-tree index** — reuses the knowledge crate's chunker + BM25
//!    index, but points it at the *user's* source directory and writes a
//!    **separate** index under `.umadev/project-source-index/` so later base
//!    calls can retrieve real code (not just the curated standards corpus).
//! 3. **Reverse API contract** — reuses the contract crate's frontend-call
//!    extractor to recover `(method, path)` pairs from real source, folds
//!    them into an [`ApiSpec`], and writes it to `.umadev/contracts/` as the
//!    **adopted baseline** so FE↔BE alignment checks work on existing code.
//! 4. **Boundary doc** — writes a *lean* `UMADEV.md` (detected commands +
//!    boundaries + one or two non-obvious decisions). Deliberately compact:
//!    research shows that dumping a whole directory tree into auto-context
//!    measurably *lowers* success and *raises* cost, so this is a hand-sized
//!    brief, not a file census.
//! 5. **Baseline marker** — writes `.umadev/adopt.json` recording "this is an
//!    existing project + here's its baseline", so the planner / runner can
//!    bias toward **incremental change rather than rewrite**.
//!
//! ## Safety contract
//! Fail-open, like the rest of the governance layer. Every step is wrapped so
//! a failure (unreadable file, permission error, malformed source) **skips
//! that step and records a note** — `run_adopt` never returns an `Err` and
//! never panics. A workspace where everything fails still yields a valid (if
//! sparse) report.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::verify::{detect_dev_server, detect_project, verify_steps, ProjectKind};
use umadev_contract::{render_json, write_contract, ApiSpec, Endpoint, HttpVerb, SecurityKind};
use umadev_knowledge::{chunk_text, Bm25Index};

/// Directory (relative to the project root) holding the adopted project's own
/// source-code BM25 index. Kept separate from `.umadev/kb-index/` (the curated
/// standards corpus) so the two never collide and either can be rebuilt
/// independently.
pub const PROJECT_SOURCE_INDEX_DIR: &str = ".umadev/project-source-index";

/// File name of the brownfield baseline marker, under `.umadev/`.
pub const ADOPT_MARKER_FILE: &str = "adopt.json";

/// Durable adopt artifacts are workspace-controlled inputs on later runs.
/// Bound them before allocation and reject link/special-file indirection.
const MAX_ADOPT_MARKER_BYTES: usize = 2 * 1024 * 1024;
const MAX_PROJECT_SOURCE_INDEX_BYTES: usize = 256 * 1024 * 1024;

/// Max directory depth the source walker descends. Mirrors the depth guards
/// used elsewhere (contract extractor: 8, knowledge walker: 6); 8 is generous
/// enough for `src/a/b/c/d/...` layouts without risking a pathological walk.
const MAX_WALK_DEPTH: usize = 8;

/// Cap on the number of source files indexed, so an enormous monorepo can't
/// make adoption hang. Overridable via `UMADEV_ADOPT_MAX_FILES` (`0` =
/// unlimited). Matches the spirit of the knowledge crate's file cap.
const DEFAULT_MAX_FILES: usize = 4000;

/// One detected verify/build command, flattened for the report + boundary doc.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectedCommand {
    /// Step label (`install` / `lint` / `test` / `build` / `typecheck` / …).
    pub name: String,
    /// The full command line, e.g. `cargo test --quiet`.
    pub command: String,
}

/// The structured outcome of an `adopt` run. Serialised into
/// `.umadev/adopt.json` (the baseline marker) and rendered to the user.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdoptReport {
    /// Marker for downstream consumers: this is a brownfield baseline.
    pub mode: String,
    /// ISO-8601 UTC timestamp of the adoption.
    pub adopted_at: String,
    /// Detected project kind label (`rust` / `node` / `python` / `go` /
    /// `deno` / `none`).
    pub stack: String,
    /// Detected dev-server label (e.g. `Vite dev server`), when one applies.
    #[serde(default)]
    pub dev_server: String,
    /// The reusable verify/build/test/lint commands recovered from the stack.
    pub commands: Vec<DetectedCommand>,
    /// Number of `(method, path)` API endpoints reverse-derived from source
    /// and written to the adopted contract baseline.
    pub api_endpoints: u32,
    /// Number of source files indexed into the project-source BM25 index.
    pub indexed_files: u32,
    /// Number of retrievable chunks the index holds (one file → N chunks).
    pub indexed_chunks: u32,
    /// Workspace-relative paths of artifacts written (contract, index,
    /// boundary doc, marker). For the friendly summary.
    pub artifacts: Vec<String>,
    /// Non-fatal notes — each step that was skipped (and why) lands here, so
    /// the fail-open behaviour is visible rather than silent.
    pub notes: Vec<String>,
}

impl AdoptReport {
    /// Whether the workspace actually looked like a real project (a known
    /// stack OR some indexed source). A truly empty directory still produces
    /// a (sparse) report, but this lets the caller phrase the summary honestly.
    #[must_use]
    pub fn looks_adopted(&self) -> bool {
        self.stack != ProjectKind::None.as_str() || self.indexed_files > 0
    }
}

/// Adopt an existing repository at `project_root`.
///
/// Runs all five steps, each fail-open. Returns a populated [`AdoptReport`]
/// and, as a side effect, writes the project-source index, the adopted API
/// contract, the lean boundary doc, and the `.umadev/adopt.json` baseline
/// marker. Never errors — a step that can't run is recorded in `notes` and
/// skipped.
#[must_use]
pub fn run_adopt(project_root: &Path) -> AdoptReport {
    let mut notes: Vec<String> = Vec::new();
    let mut artifacts: Vec<String> = Vec::new();

    // --- Step 1: detect the stack + recover real commands ---------------
    let kind = detect_project(project_root);
    let commands = detect_commands(kind, project_root);
    let dev_server = detect_dev_server(project_root)
        .map(|d| d.label.to_string())
        .unwrap_or_default();
    if kind == ProjectKind::None {
        notes.push(
            "no recognised manifest (Cargo.toml / package.json / pyproject.toml / go.mod / \
             deno.json) — stack detection skipped; commands will be empty"
                .to_string(),
        );
    }

    // --- Step 2: index the user's own source tree ----------------------
    let (indexed_files, indexed_chunks) = match index_project_source(project_root) {
        Ok((files, chunks, rel)) => {
            if files > 0 {
                artifacts.push(rel);
            } else {
                notes.push("no indexable source files found — project-source index skipped".into());
            }
            (files, chunks)
        }
        Err(e) => {
            notes.push(format!("source indexing skipped: {e}"));
            (0, 0)
        }
    };

    // --- Step 3: reverse-derive the API contract baseline --------------
    let contract = adopt_api_contract(project_root);
    artifacts.extend(contract.written_paths);
    if contract.discovered_calls == 0 {
        notes.push(
            "no frontend API calls found in source — adopted contract baseline is empty".into(),
        );
    } else if contract.unresolved_methods > 0 {
        notes.push(format!(
            "{} frontend API call(s) had no mechanically known HTTP method and were not \
             fabricated into the typed baseline; replace ambiguous wrappers with typed calls \
             or document their methods before relying on the contract",
            contract.unresolved_methods
        ));
    }
    let api_endpoints = contract.endpoint_count;

    // --- Step 4: write the lean boundary doc ---------------------------
    let mut report = AdoptReport {
        mode: "brownfield".to_string(),
        adopted_at: now_iso8601(),
        stack: kind.as_str().to_string(),
        dev_server,
        commands,
        api_endpoints,
        indexed_files,
        indexed_chunks,
        artifacts,
        notes,
    };

    match write_boundary_doc(project_root, &report) {
        Ok(rel) => report.artifacts.push(rel),
        Err(e) => report.notes.push(format!("boundary doc skipped: {e}")),
    }

    // --- Step 5: drop the brownfield baseline marker -------------------
    match write_adopt_marker(project_root, &report) {
        Ok(rel) => report.artifacts.push(rel),
        Err(e) => report.notes.push(format!("baseline marker skipped: {e}")),
    }

    report
}

/// Read the brownfield baseline marker if the workspace was adopted. Lets the
/// planner / runner discover that they should bias toward incremental change.
/// Fail-open: a missing or malformed marker returns `None`.
#[must_use]
pub fn read_adopt_marker(project_root: &Path) -> Option<AdoptReport> {
    let path = project_root.join(".umadev").join(ADOPT_MARKER_FILE);
    let body =
        crate::bounded_fs::read_utf8_beneath(project_root, &path, MAX_ADOPT_MARKER_BYTES).ok()?;
    serde_json::from_str(&body).ok()
}

/// Whether this workspace has been adopted (brownfield baseline present).
/// A cheap predicate the runner can call to choose incremental-vs-rewrite.
#[must_use]
pub fn is_adopted(project_root: &Path) -> bool {
    read_adopt_marker(project_root).is_some()
}

// ---------------------------------------------------------------------------
// Step 1 — stack commands
// ---------------------------------------------------------------------------

/// Flatten the [`crate::verify`] step sequence into display commands. Reuses
/// the exact install/lint/test/build commands the verify loop would run, so
/// the boundary doc never drifts from what the engine actually executes.
fn detect_commands(kind: ProjectKind, project_root: &Path) -> Vec<DetectedCommand> {
    verify_steps(kind, project_root)
        .unwrap_or_default()
        .into_iter()
        .map(|s| {
            let command = if s.args.is_empty() {
                s.program.clone()
            } else {
                format!("{} {}", s.program, s.args.join(" "))
            };
            DetectedCommand {
                name: s.name.to_string(),
                command,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Step 2 — project-source index
// ---------------------------------------------------------------------------

/// Build a BM25 index over the user's real source tree and persist it to
/// `.umadev/project-source-index/source.bin`.
///
/// Returns `(file_count, chunk_count, relative_index_path)`. The chunker is
/// markdown-aware but degrades gracefully on non-markdown source (a file with
/// no `## H2` becomes a single chunk), which is exactly what we want for code.
///
/// Fail-open at the boundary: a serialisation/write error is returned as an
/// `Err(String)` for the caller to record as a note, never a panic.
fn index_project_source(project_root: &Path) -> Result<(u32, u32, String), String> {
    let mut files: Vec<PathBuf> = Vec::new();
    let max_files = env_max_files();
    collect_source_files(project_root, &mut files, 0, max_files);

    if files.is_empty() {
        return Ok((0, 0, String::new()));
    }

    // A source file over this byte cap is skipped BEFORE it is read whole into memory: a
    // committed lockfile (`package-lock.json` / `pnpm-lock.yaml` - both at root with indexable
    // extensions), a big `.sql` dump, or a minified bundle would otherwise blow memory and
    // flood the source index with noise.
    const MAX_SOURCE_BYTES: usize = 512 * 1024;
    let mut chunks = Vec::new();
    let mut file_count: u32 = 0;
    for abs in &files {
        let Ok(body) = crate::bounded_fs::read_utf8_beneath(project_root, abs, MAX_SOURCE_BYTES)
        else {
            continue; // unreadable / binary → skip (fail-open)
        };
        if body.trim().is_empty() {
            continue;
        }
        let rel = abs
            .strip_prefix(project_root)
            .map(|p| p.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/"))
            .unwrap_or_else(|_| {
                abs.to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "/")
            });
        let file_chunks = chunk_text(&rel, &body);
        if !file_chunks.is_empty() {
            file_count += 1;
            chunks.extend(file_chunks);
        }
    }

    if chunks.is_empty() {
        return Ok((0, 0, String::new()));
    }

    let chunk_count = u32::try_from(chunks.len()).unwrap_or(u32::MAX);
    let index = Bm25Index::from_chunks(chunks);

    let root = std::fs::canonicalize(project_root).map_err(|e| format!("resolve project: {e}"))?;
    let state_dir = umadev_state::fs::ensure_real_child_dir(&root, ".umadev")
        .map_err(|e| format!("create state dir: {e}"))?;
    let dir = umadev_state::fs::ensure_real_child_dir(&state_dir, "project-source-index")
        .map_err(|e| format!("create index dir: {e}"))?;
    let index_path = dir.join("source.bin");
    let bytes = serde_json::to_vec(&index).map_err(|e| format!("serialise index: {e}"))?;
    if bytes.len() > MAX_PROJECT_SOURCE_INDEX_BYTES {
        return Err(format!(
            "serialised source index exceeds {MAX_PROJECT_SOURCE_INDEX_BYTES} bytes"
        ));
    }
    umadev_state::fs::atomic_write(&index_path, &bytes).map_err(|e| format!("write index: {e}"))?;

    let rel = format!("{PROJECT_SOURCE_INDEX_DIR}/source.bin");
    Ok((file_count, chunk_count, rel))
}

/// Load the project-source BM25 index written by the internal source indexer.
/// Fail-open: a missing / corrupt index returns `None`, so a caller wiring
/// real-code retrieval into later phases degrades to "no extra context".
#[must_use]
pub fn load_project_source_index(project_root: &Path) -> Option<Bm25Index> {
    let path = project_root
        .join(PROJECT_SOURCE_INDEX_DIR)
        .join("source.bin");
    let bytes =
        crate::bounded_fs::read_bytes_beneath(project_root, &path, MAX_PROJECT_SOURCE_INDEX_BYTES)
            .ok()?;
    let index: Bm25Index = serde_json::from_slice(&bytes).ok()?;
    index.is_consistent().then_some(index)
}

/// Source-file extensions worth indexing. Broad enough to cover the common
/// commercial stacks (web / backend / mobile / systems) without pulling in
/// binaries, lockfiles, or vendored assets.
const SOURCE_EXTS: &[&str] = &[
    // web / TS-JS
    "ts", "tsx", "js", "jsx", "mjs", "cjs", "vue", "svelte", "astro", "css", "scss",
    // backend / general
    "rs", "go", "py", "rb", "java", "kt", "kts", "php", "cs", "scala", "swift", "c", "h", "cc",
    "cpp", "hpp", "m", "mm", "ex", "exs", "dart", "sql",
    // config / docs that carry real signal
    "md", "toml", "yaml", "yml", "json", "proto", "graphql", "gql",
];

/// Directory names never worth indexing — vendored deps, build output,
/// VCS internals, and UmaDev's own runtime state.
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    ".svn",
    ".hg",
    "target",
    "dist",
    "build",
    "out",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".turbo",
    ".cache",
    "coverage",
    "vendor",
    "__pycache__",
    ".venv",
    "venv",
    ".mypy_cache",
    ".pytest_cache",
    ".gradle",
    ".idea",
    ".vscode",
    // UmaDev's own per-project state + generated outputs.
    ".umadev",
    "output",
    "release",
];

/// Resolve the per-run file cap from `UMADEV_ADOPT_MAX_FILES` (`0` = unlimited),
/// else [`DEFAULT_MAX_FILES`].
fn env_max_files() -> usize {
    match std::env::var("UMADEV_ADOPT_MAX_FILES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
    {
        Some(0) => usize::MAX,
        Some(n) => n,
        None => DEFAULT_MAX_FILES,
    }
}

/// Recursively collect indexable source files under `dir`, skipping vendored /
/// generated / hidden-state directories, bounded by depth and a file cap.
fn collect_source_files(dir: &Path, out: &mut Vec<PathBuf>, depth: usize, max_files: usize) {
    if depth > MAX_WALK_DEPTH || out.len() >= max_files {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return; // unreadable dir → skip (fail-open)
    };
    let mut entries = entries.flatten().collect::<Vec<_>>();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        if out.len() >= max_files {
            return;
        }
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            // Skip the never-index list and any dot-directory (hidden state).
            if SKIP_DIRS.contains(&name.as_ref()) || name.starts_with('.') {
                continue;
            }
            collect_source_files(&path, out, depth + 1, max_files);
        } else if ft.is_file() && has_source_ext(&path) {
            out.push(path);
        }
    }
}

/// Whether a file's extension is in [`SOURCE_EXTS`] (case-insensitive).
fn has_source_ext(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .is_some_and(|e| SOURCE_EXTS.contains(&e.as_str()))
}

// ---------------------------------------------------------------------------
// Step 3 — reverse API contract
// ---------------------------------------------------------------------------

/// Reverse-derive an [`ApiSpec`] from the real frontend source and write it to
/// `.umadev/contracts/` as the adopted baseline.
///
/// Reuses `umadev_contract::extract_frontend_calls` (which already walks the
/// tree fail-open and normalises template paths to `:param`). Each unique
/// `(method, path)` becomes an [`Endpoint`] so the existing FE↔BE alignment
/// validators operate on the existing codebase, not just freshly-scaffolded
/// code.
///
struct AdoptContractResult {
    endpoint_count: u32,
    written_paths: Vec<String>,
    discovered_calls: usize,
    unresolved_methods: usize,
}

/// Unknown wrapper methods are reported but never invented as GET in the typed
/// baseline: an adopted contract becomes future enforcement, so a guess here
/// would turn into a durable false requirement.
fn adopt_api_contract(project_root: &Path) -> AdoptContractResult {
    let calls = umadev_contract::extract_frontend_calls(project_root);
    let discovered_calls = calls.len();
    let unresolved_methods = calls.iter().filter(|call| !call.method_known).count();

    let mut endpoints: Vec<Endpoint> = Vec::new();
    for call in calls.iter().filter(|call| call.method_known) {
        let op = operation_id_for(call.method, &call.path);
        endpoints.push(Endpoint {
            method: call.method,
            path: call.path.clone(),
            operation_id: op,
            description: format!("Adopted from frontend call in {}", call.file),
            request_shape: String::new(),
            response_shape: String::new(),
            // We cannot infer auth from a bare call site; leave it open and
            // let a later contract review tighten it (never claim Bearer we
            // didn't see).
            security: SecurityKind::None,
        });
    }

    let spec = ApiSpec {
        endpoints,
        title: "Adopted API (reverse-derived from existing source)".to_string(),
    };

    let count = u32::try_from(spec.len()).unwrap_or(u32::MAX);
    // Do not create an empty contract that could be mistaken for proof that the
    // project has no API. Unknown-only input remains an explicit audit note.
    let written = if spec.is_empty() {
        Vec::new()
    } else {
        write_contract(project_root, &spec)
    };
    // Touch render_json so a future caller can diff without re-walking (and so
    // the dependency is exercised in one place). Best-effort, ignored on error.
    let _ = render_json(&spec);

    let written_paths: Vec<String> = written
        .iter()
        .map(|p| {
            p.strip_prefix(project_root)
                .unwrap_or(p)
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/")
        })
        .collect();
    AdoptContractResult {
        endpoint_count: count,
        written_paths,
        discovered_calls,
        unresolved_methods,
    }
}

/// Build a stable, readable operationId from a method + path, e.g.
/// `GET /api/users/:id` → `getApiUsersId`. Deterministic so re-adopting the
/// same source yields the same ids.
fn operation_id_for(method: HttpVerb, path: &str) -> String {
    let mut id = method.as_str().to_ascii_lowercase();
    let mut capitalise_next = true;
    for seg in path.split('/') {
        let seg = seg.trim_start_matches(':');
        for ch in seg.chars() {
            if ch.is_ascii_alphanumeric() {
                if capitalise_next {
                    id.extend(ch.to_uppercase());
                    capitalise_next = false;
                } else {
                    id.push(ch);
                }
            } else {
                capitalise_next = true;
            }
        }
        capitalise_next = true;
    }
    id
}

// ---------------------------------------------------------------------------
// Step 4 — lean boundary doc
// ---------------------------------------------------------------------------

/// Write the lean `UMADEV.md` boundary brief. Compact by design — detected
/// commands, hard boundaries, and a couple of non-obvious decisions. No
/// directory-tree dump (auto-context bloat measurably lowers task success and
/// raises cost, so we keep the brief hand-sized).
///
/// Returns the workspace-relative path written. Idempotent in spirit: it
/// overwrites the UmaDev-managed brief each adopt (the file is generated, not
/// hand-authored), but never touches a pre-existing `AGENTS.md`.
fn write_boundary_doc(project_root: &Path, report: &AdoptReport) -> Result<String, String> {
    let mut md = String::new();
    md.push_str("# UMADEV.md — brownfield boundary brief\n\n");
    md.push_str(
        "This is an **existing** project adopted by UmaDev. Work **incrementally**: \
         change the smallest surface that satisfies the requirement, match the \
         conventions already in this codebase, and never regenerate or rewrite \
         files wholesale.\n\n",
    );

    md.push_str("## Detected stack\n\n");
    md.push_str(&format!("- Stack: `{}`\n", report.stack));
    if !report.dev_server.is_empty() {
        md.push_str(&format!("- Dev server: {}\n", report.dev_server));
    }
    md.push('\n');

    md.push_str("## Commands (run these, do not invent new ones)\n\n");
    if report.commands.is_empty() {
        md.push_str("_No build/test commands detected — confirm with the maintainer._\n");
    } else {
        for c in &report.commands {
            md.push_str(&format!("- **{}**: `{}`\n", c.name, c.command));
        }
    }
    md.push('\n');

    md.push_str("## Boundaries (hard rules)\n\n");
    md.push_str("- Do not reformat or rewrite files unrelated to the task.\n");
    md.push_str("- Reuse existing modules, helpers, and patterns before adding new ones.\n");
    md.push_str(
        "- Frontend API calls must match the adopted contract in \
         `.umadev/contracts/openapi.yaml` (FE↔BE alignment).\n",
    );
    md.push_str("- Keep public APIs and on-disk formats backward-compatible unless asked.\n\n");

    md.push_str("## Key decisions (non-obvious context)\n\n");
    // One or two genuinely useful, non-obvious facts — not a census.
    if report.api_endpoints > 0 {
        md.push_str(&format!(
            "- A baseline API contract of {} endpoint(s) was reverse-derived from existing \
             frontend calls; treat it as the current truth, not a wishlist.\n",
            report.api_endpoints
        ));
    }
    if report.indexed_files > 0 {
        md.push_str(&format!(
            "- {} source file(s) are indexed for retrieval under \
             `.umadev/project-source-index/`; prefer searching existing code over guessing.\n",
            report.indexed_files
        ));
    }
    md.push_str(
        "- This brief is generated by `umadev adopt`; the authoritative project README and \
         in-repo docs still apply.\n",
    );

    // Never clobber a user's hand-authored AGENTS.md; write our own filename.
    // The project may itself be opened through a link, so resolve that trusted
    // root first, then refuse a linked/special UMADEV.md leaf.
    let root = std::fs::canonicalize(project_root).map_err(|e| format!("resolve project: {e}"))?;
    let path = root.join("UMADEV.md");
    umadev_state::fs::atomic_write(&path, md.as_bytes())
        .map_err(|e| format!("write UMADEV.md: {e}"))?;
    Ok("UMADEV.md".to_string())
}

// ---------------------------------------------------------------------------
// Step 5 — baseline marker
// ---------------------------------------------------------------------------

/// Write the brownfield baseline marker to `.umadev/adopt.json`. This is the
/// signal the planner / runner read (via [`read_adopt_marker`] / [`is_adopted`])
/// to choose incremental change over a rewrite.
fn write_adopt_marker(project_root: &Path, report: &AdoptReport) -> Result<String, String> {
    let root = std::fs::canonicalize(project_root).map_err(|e| format!("resolve project: {e}"))?;
    let dir = umadev_state::fs::ensure_real_child_dir(&root, ".umadev")
        .map_err(|e| format!("create .umadev: {e}"))?;
    let path = dir.join(ADOPT_MARKER_FILE);
    let body =
        serde_json::to_string_pretty(report).map_err(|e| format!("serialise marker: {e}"))?;
    if body.len() > MAX_ADOPT_MARKER_BYTES {
        return Err(format!(
            "serialised adopt marker exceeds {MAX_ADOPT_MARKER_BYTES} bytes"
        ));
    }
    umadev_state::fs::atomic_write(&path, body.as_bytes())
        .map_err(|e| format!("write marker: {e}"))?;
    Ok(format!(".umadev/{ADOPT_MARKER_FILE}"))
}

// ---------------------------------------------------------------------------
// small helpers
// ---------------------------------------------------------------------------

/// Current UTC time as a compact ISO-8601 string (mirrors `state.rs`).
fn now_iso8601() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    struct EnvRestore {
        key: &'static str,
        prior: Option<std::ffi::OsString>,
    }

    impl EnvRestore {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let prior = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, prior }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            match self.prior.take() {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, body).unwrap();
    }

    #[test]
    fn adopt_rust_project_detects_stack_and_commands() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "Cargo.toml",
            "[package]\nname = \"x\"\nversion = \"0.1.0\"",
        );
        write(tmp.path(), "src/main.rs", "fn main() { println!(\"hi\"); }");

        let report = run_adopt(tmp.path());
        assert_eq!(report.stack, "rust");
        assert_eq!(report.mode, "brownfield");
        // Rust verify sequence is fmt → clippy → test → build.
        let names: Vec<&str> = report.commands.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"test"), "commands: {names:?}");
        assert!(names.contains(&"build"), "commands: {names:?}");
        assert!(report.looks_adopted());
    }

    #[test]
    fn adopt_writes_marker_and_is_readable() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "Cargo.toml", "[package]\nname=\"x\"");
        write(
            tmp.path(),
            "src/lib.rs",
            "pub fn add(a:i32,b:i32)->i32{a+b}",
        );

        let report = run_adopt(tmp.path());
        assert!(is_adopted(tmp.path()), "marker file must exist");
        let round = read_adopt_marker(tmp.path()).expect("marker readable");
        assert_eq!(round.mode, "brownfield");
        assert_eq!(round.stack, report.stack);
        // The marker path is recorded as an artifact.
        assert!(report.artifacts.iter().any(|a| a.ends_with("adopt.json")));
    }

    #[test]
    fn adopt_writes_lean_boundary_doc() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "package.json",
            r#"{"name":"x","scripts":{"build":"vite build"}}"#,
        );
        write(tmp.path(), "src/app.ts", "export const x = 1;");

        let report = run_adopt(tmp.path());
        let doc = tmp.path().join("UMADEV.md");
        assert!(doc.is_file(), "UMADEV.md must be written");
        let body = fs::read_to_string(&doc).unwrap();
        assert!(body.contains("brownfield"));
        assert!(body.contains("Boundaries"));
        assert!(body.contains("incrementally") || body.contains("Incremental"));
        // Lean: no directory-tree dump. A boundary brief should be well under
        // ~120 lines even for a real repo.
        assert!(body.lines().count() < 120, "boundary doc should stay lean");
        assert!(report.artifacts.iter().any(|a| a == "UMADEV.md"));
    }

    #[test]
    fn adopt_indexes_source_into_separate_dir() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "Cargo.toml", "[package]\nname=\"x\"");
        write(
            tmp.path(),
            "src/main.rs",
            "## overview\nfn main() {}\n\n## details\nfn helper() {}",
        );
        write(tmp.path(), "README.md", "# Project\n\n## Setup\nrun it");

        let report = run_adopt(tmp.path());
        assert!(report.indexed_files >= 1, "should index source files");
        assert!(report.indexed_chunks >= 1);
        // The index lives in its OWN dir, separate from the curated kb-index.
        let idx = tmp.path().join(PROJECT_SOURCE_INDEX_DIR).join("source.bin");
        assert!(idx.is_file(), "project-source index must exist separately");
        assert!(
            !tmp.path().join(".umadev/kb-index").exists(),
            "must NOT touch kb-index"
        );
        // And it loads back as a usable BM25 index.
        let loaded = load_project_source_index(tmp.path()).expect("index loads");
        assert!(loaded.doc_count >= 1);
    }

    #[test]
    fn project_source_loader_rejects_shape_valid_inconsistent_index() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(PROJECT_SOURCE_INDEX_DIR);
        fs::create_dir_all(&dir).unwrap();
        let mut index = Bm25Index::from_chunks(chunk_text("src/app.rs", "alpha beta"));
        assert!(index.is_consistent());
        index.doc_count = index.doc_count.saturating_add(1);
        fs::write(dir.join("source.bin"), serde_json::to_vec(&index).unwrap()).unwrap();

        assert!(
            load_project_source_index(tmp.path()).is_none(),
            "redundant index fields must be validated before query code sees them"
        );
    }

    #[cfg(unix)]
    #[test]
    fn adopt_state_does_not_follow_linked_marker_or_index_directory() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "src/app.rs", "pub fn app() -> bool { true }");
        fs::create_dir(tmp.path().join(".umadev")).unwrap();

        let outside = TempDir::new().unwrap();
        let outside_index = outside.path().join("index");
        fs::create_dir(&outside_index).unwrap();
        symlink(
            &outside_index,
            tmp.path().join(".umadev/project-source-index"),
        )
        .unwrap();
        assert!(index_project_source(tmp.path()).is_err());
        assert!(
            fs::read_dir(&outside_index).unwrap().next().is_none(),
            "the index writer must not escape through a linked managed directory"
        );

        let report = AdoptReport {
            mode: "brownfield".into(),
            adopted_at: "2026-01-01T00:00:00Z".into(),
            stack: "rust".into(),
            dev_server: String::new(),
            commands: Vec::new(),
            api_endpoints: 0,
            indexed_files: 1,
            indexed_chunks: 1,
            artifacts: Vec::new(),
            notes: Vec::new(),
        };
        let boundary_target = outside.path().join("outside-UMADEV.md");
        fs::write(&boundary_target, "outside boundary").unwrap();
        symlink(&boundary_target, tmp.path().join("UMADEV.md")).unwrap();
        assert!(write_boundary_doc(tmp.path(), &report).is_err());
        assert_eq!(
            fs::read_to_string(&boundary_target).unwrap(),
            "outside boundary"
        );

        let marker_target = outside.path().join("outside-adopt.json");
        let original = serde_json::to_vec(&report).unwrap();
        fs::write(&marker_target, &original).unwrap();
        symlink(
            &marker_target,
            tmp.path().join(".umadev").join(ADOPT_MARKER_FILE),
        )
        .unwrap();

        assert!(read_adopt_marker(tmp.path()).is_none());
        assert!(!is_adopted(tmp.path()));
        assert!(write_adopt_marker(tmp.path(), &report).is_err());
        assert_eq!(fs::read(marker_target).unwrap(), original);
    }

    #[test]
    fn adopt_reverse_derives_api_contract() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "package.json", r#"{"name":"x"}"#);
        write(
            tmp.path(),
            "src/api.ts",
            "fetch('/api/users');\naxios.post('/api/orders', body);",
        );

        let report = run_adopt(tmp.path());
        assert!(report.api_endpoints >= 2, "should derive ≥2 endpoints");
        let contract = tmp.path().join(".umadev/contracts/openapi.yaml");
        assert!(contract.is_file(), "adopted contract baseline must exist");
        assert!(report.artifacts.iter().any(|a| a.contains("openapi.")));
    }

    #[test]
    fn adopt_does_not_fabricate_an_unknown_wrapper_method() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "package.json", r#"{"name":"x"}"#);
        write(tmp.path(), "src/api.ts", "request('/api/users');\n");

        let report = run_adopt(tmp.path());
        assert_eq!(report.api_endpoints, 0);
        assert!(
            report.notes.iter().any(|note| note.contains("HTTP method")),
            "the unresolved method must be visible: {:?}",
            report.notes
        );
        assert!(
            !tmp.path().join(".umadev/contracts/openapi.yaml").exists(),
            "unknown-only calls must not produce a false GET contract"
        );
    }

    #[test]
    fn adopt_skips_vendored_and_state_dirs() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "package.json", r#"{"name":"x"}"#);
        write(tmp.path(), "src/real.ts", "export const a = 1;");
        // These must be ignored by the source walker.
        write(
            tmp.path(),
            "node_modules/dep/index.js",
            "module.exports = {};",
        );
        write(tmp.path(), ".umadev/audit/x.jsonl", "{}");
        write(tmp.path(), "dist/bundle.js", "console.log(1)");

        let mut files = Vec::new();
        collect_source_files(tmp.path(), &mut files, 0, usize::MAX);
        let rels: Vec<String> = files
            .iter()
            .map(|p| {
                p.strip_prefix(tmp.path())
                    .unwrap()
                    .to_string_lossy()
                    .to_string()
            })
            .collect();
        assert!(rels.iter().any(|r| r.contains("real.ts")));
        assert!(!rels.iter().any(|r| r.contains("node_modules")), "{rels:?}");
        assert!(!rels.iter().any(|r| r.contains(".umadev")), "{rels:?}");
        assert!(!rels.iter().any(|r| r.contains("dist")), "{rels:?}");
    }

    #[test]
    fn adopt_empty_dir_is_fail_open() {
        // A genuinely empty directory must not panic and must not claim to be
        // a real project, but still produces a valid (sparse) report + marker.
        let tmp = TempDir::new().unwrap();
        let report = run_adopt(tmp.path());
        assert_eq!(report.stack, "none");
        assert_eq!(report.api_endpoints, 0);
        assert_eq!(report.indexed_files, 0);
        assert!(!report.looks_adopted());
        assert!(!report.notes.is_empty(), "should record skip notes");
        // The marker is still written (so re-adopt / status can read it).
        assert!(is_adopted(tmp.path()));
    }

    #[test]
    fn operation_id_is_stable_and_readable() {
        assert_eq!(
            operation_id_for(HttpVerb::Get, "/api/users/:id"),
            "getApiUsersId"
        );
        assert_eq!(
            operation_id_for(HttpVerb::Post, "/api/orders"),
            "postApiOrders"
        );
        // Deterministic — same input, same id.
        assert_eq!(
            operation_id_for(HttpVerb::Delete, "/api/x/:id"),
            operation_id_for(HttpVerb::Delete, "/api/x/:id")
        );
    }

    #[test]
    fn bounded_adopt_walk_selects_stable_paths() {
        let tmp = TempDir::new().unwrap();
        for name in ["z.rs", "b.rs", "a.rs"] {
            std::fs::write(tmp.path().join(name), "fn main() {}\n").unwrap();
        }
        let mut files = Vec::new();
        collect_source_files(tmp.path(), &mut files, 0, 2);
        let names = files
            .iter()
            .filter_map(|path| path.file_name().and_then(|name| name.to_str()))
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["a.rs", "b.rs"]);
    }

    #[test]
    fn env_max_files_zero_is_unlimited() {
        // Guard the override semantics while restoring the process-global env
        // even if an assertion below fails.
        let _env = EnvRestore::set("UMADEV_ADOPT_MAX_FILES", "0");
        assert_eq!(env_max_files(), usize::MAX);
        std::env::set_var("UMADEV_ADOPT_MAX_FILES", "7");
        assert_eq!(env_max_files(), 7);
        std::env::remove_var("UMADEV_ADOPT_MAX_FILES");
        assert_eq!(env_max_files(), DEFAULT_MAX_FILES);
    }
}
