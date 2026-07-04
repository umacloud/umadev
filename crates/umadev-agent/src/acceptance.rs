//! Task-level acceptance — does the delivered code actually implement the plan?
//!
//! UmaDev decomposes the requirement into an architecture API table (+ an
//! execution plan). A one-shot delegator forgets that plan the moment it hands
//! off; a *director* checks the team's work against it. After the dev phases
//! this re-reads the breakdown and the real workspace and reports the **gaps**
//! — planned endpoints with no implementation found — so the director can
//! re-delegate the unfinished work instead of shipping a half-built product.
//!
//! This reuses `umadev-contract`'s architecture parser (the same typed
//! `ApiSpec` UD-CODE-003 uses for frontend↔backend alignment). It is purely
//! deterministic Rust — no LLM needed for the *check* — and fail-open: any
//! missing artifact / unparseable doc yields no gaps rather than a false alarm.

use std::path::{Path, PathBuf};

use crate::fswalk::{classify_no_follow, EntryKind};

/// Source + style extensions. The code files form the "implementation surface"
/// for the endpoint-coverage check; the style files (css/…) matter for the
/// post-phase governance scan, where hardcoded colors live.
const SRC_EXT: &[&str] = &[
    "tsx", "jsx", "ts", "js", "mjs", "cjs", "vue", "svelte", "astro", "py", "rs", "go", "java",
    "rb", "php", "cs", "kt", "ex", "exs", "dart", "swift", "scala", "c", "cc", "cpp", "h", "hpp",
    "css", "scss", "sass", "less", "html",
];

/// Directories never worth scanning (build output / vendored deps / VCS /
/// UmaDev's OWN artifact dirs).
///
/// `output` holds UmaDev's own deliverable DOCS (`output/<slug>-prd.md` etc.) —
/// and crucially a base may drop an `output/preview.html` or `output/*.css`
/// there. Those must NEVER count as "real delivered source": the source-present
/// honesty hard-gate ("did the base actually build the product, or hallucinate
/// 'done'?") would be fooled by a stray doc-dir HTML into passing a zero-code
/// build. So `output` is skipped alongside the build-output / vendor dirs.
/// (`.umadev` is already skipped via the leading-dot rule.)
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

/// Maximum directory depth for the source walk. Enterprise Vue/Java admin
/// projects commonly nest real code below `src/views/.../components/...`, so an
/// 8-level cap misses legitimate source and feeds empty/partial evidence to QA.
/// File count + skip dirs remain the real monorepo guard.
const MAX_SOURCE_DEPTH: usize = 16;

/// Maximum number of source files scanned (guards a pathological monorepo).
const MAX_SOURCE_FILES: usize = 600;

/// Recursively collect source files (bounded by depth + file count).
fn collect(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > MAX_SOURCE_DEPTH || out.len() > MAX_SOURCE_FILES {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        // No-follow: `symlink_metadata` classifies a symlink AS a symlink
        // (`Skip`), so a link inside the tree can never make the walk descend
        // into — or collect a file from — OUTSIDE the workspace, and a symlink
        // cycle is unreachable.
        match classify_no_follow(&p) {
            EntryKind::Dir => {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name.starts_with('.') || SKIP_DIRS.contains(&name) {
                    continue;
                }
                collect(&p, out, depth + 1);
            }
            EntryKind::File => {
                if let Some(ext) = p.extension().and_then(|s| s.to_str()) {
                    if SRC_EXT.contains(&ext) && is_nontrivial_source(&p, ext) {
                        out.push(p);
                    }
                }
            }
            EntryKind::Skip => {}
        }
    }
}

/// Whether a candidate source file carries REAL, substantive content — not an empty
/// file nor a pure-comment / whitespace stub. A base that hallucinates "done" can
/// leave a 0-byte or `// TODO` `index.js`; counting that as delivered source would let
/// the source-present honesty hard-gate (`director::verify_source_present` +
/// `continuous`'s HARD STOP + the `SourcePresent` acceptance) PASS on a build that
/// produced NOTHING. So a file is counted only when it has at least one
/// non-whitespace, non-comment token — mirroring `truncated_missing_artifacts`'
/// `doc_present` (a non-trivial-size floor) but precise enough not to false-reject a
/// genuinely tiny real file (`fn main(){}`). Fail-open toward the gate's intent: an
/// unreadable / non-UTF8 file is treated as NOT real source (we cannot confirm its
/// content), the safe answer for an honesty gate.
fn is_nontrivial_source(p: &Path, ext: &str) -> bool {
    match std::fs::read_to_string(p) {
        Ok(content) => {
            // `#` is a line comment in these languages; in CSS/SCSS a leading `#` is an
            // id selector and in Rust `#[…]` an attribute, so `#` must NOT be treated as
            // a comment there (else a real `#header{…}` stylesheet would mis-read as a
            // stub).
            let hash_comments = matches!(ext, "py" | "rb" | "ex" | "exs");
            has_code_content(&content, hash_comments)
        }
        Err(_) => false,
    }
}

/// True iff `src` has at least one substantive (non-whitespace, non-comment) line — a
/// heuristic that distinguishes real source from a whitespace/comment-only stub. It
/// strips `/* … */` and `<!-- … -->` block comments, then skips blank lines and `//`
/// (and, when `hash_comments`, `#`)-prefixed line comments; any line that survives is
/// substantive. Deliberately CONSERVATIVE: it must never mis-classify REAL code as a
/// stub (that would fail a genuine build's source-present gate — a worse bug than the
/// one this guards), so it treats only UNAMBIGUOUS comment forms as comments.
/// Deterministic + dependency-free (no language parser).
fn has_code_content(src: &str, hash_comments: bool) -> bool {
    let stripped = strip_block_comments(src);
    stripped.lines().any(|raw| {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("//") {
            return false;
        }
        if hash_comments && line.starts_with('#') {
            return false;
        }
        true
    })
}

/// Remove `/* … */` and `<!-- … -->` block comments from `src` (crude, non-nested; an
/// unterminated block drops to end-of-input). Every needle is ASCII so the byte
/// offsets land on char boundaries (UTF-8 safe). Used only by [`has_code_content`].
fn strip_block_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut rest = src;
    loop {
        let slash = rest.find("/*");
        let html = rest.find("<!--");
        let (start, open_len, close, close_len) = match (slash, html) {
            (Some(a), Some(b)) if a <= b => (a, 2, "*/", 2),
            (Some(_), Some(b)) => (b, 4, "-->", 3),
            (Some(a), None) => (a, 2, "*/", 2),
            (None, Some(b)) => (b, 4, "-->", 3),
            (None, None) => {
                out.push_str(rest);
                break;
            }
        };
        out.push_str(&rest[..start]);
        let after = &rest[start + open_len..];
        match after.find(close) {
            Some(e) => rest = &after[e + close_len..],
            None => break, // unterminated block comment → drop the remainder
        }
    }
    out
}

/// Collect the project's source files (bounded: depth 8, 600 files; skips
/// build/vendor/VCS dirs). Shared by the acceptance check and the post-phase
/// governance catch-up scan (real-file governance for brains without a
/// real-time pre-write hook).
#[must_use]
pub fn source_files(project_root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect(project_root, &mut files, 0);
    files
}

/// Locate the designer's **design-tokens** deliverable on the blackboard — the
/// `design-tokens.json` (the token source of truth: a type scale, color palette,
/// spacing, the component list) and/or `design-tokens.css` (the same tokens as CSS
/// custom properties the frontend imports). This is the deterministic backing for
/// the `DesignTokensPresent` acceptance: the designer seat is only "done" when its
/// design system is a REAL file, not a narrated claim (anti-theatre).
///
/// Bounded recursive scan (depth 6, first 8 hits) for a file named
/// `design-tokens.json` / `design-tokens.css` (case-insensitive). Unlike
/// [`source_files`] it deliberately DESCENDS into `output/` — a tokens file
/// legitimately lives on the doc blackboard there — while still skipping
/// build/vendor/VCS dirs. Pure + fail-open: an unreadable tree yields an empty list
/// (treated by the caller as "absent"), never an error.
#[must_use]
pub fn design_tokens_files(project_root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    find_design_tokens(project_root, &mut out, 0);
    out
}

/// Recursive worker for [`design_tokens_files`] (bounded: depth 6, 8 files).
fn find_design_tokens(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > 6 || out.len() >= 8 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        // No-follow (see `collect`): never descend a symlinked directory nor
        // pick up a symlinked file from outside the workspace.
        match classify_no_follow(&p) {
            EntryKind::Dir => {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                // Skip dot-dirs + build/vendor/VCS dirs — but DO descend into
                // `output` (the tokens file is a legitimate blackboard artifact
                // there), so the generic `output` skip does not apply to this scan.
                if name.starts_with('.') || (SKIP_DIRS.contains(&name) && name != "output") {
                    continue;
                }
                find_design_tokens(&p, out, depth + 1);
            }
            EntryKind::File => {
                if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                    let lower = name.to_ascii_lowercase();
                    if lower == "design-tokens.json" || lower == "design-tokens.css" {
                        out.push(p);
                    }
                }
            }
            EntryKind::Skip => {}
        }
    }
}

/// The concatenated source of the project (bounded to ~2 MB) — the surface we
/// search for endpoint implementations.
fn implementation_surface(root: &Path) -> String {
    let files = source_files(root);
    let mut buf = String::new();
    for f in &files {
        if let Ok(s) = std::fs::read_to_string(f) {
            buf.push_str(&s);
            buf.push('\n');
        }
        if buf.len() > 2_000_000 {
            break;
        }
    }
    buf
}

/// A bounded digest of the delivered code — file path headers + contents up to
/// `max_bytes` — to feed an LLM acceptance judge (which reasons about whether
/// the code meets the PRD criteria, beyond what a grep can check).
#[must_use]
pub fn code_digest(project_root: &Path, max_bytes: usize) -> String {
    let mut buf = String::new();
    for f in source_files(project_root) {
        if buf.len() >= max_bytes {
            break;
        }
        if let Ok(content) = std::fs::read_to_string(&f) {
            let rel = f.strip_prefix(project_root).unwrap_or(&f);
            buf.push_str(&format!(
                "\n// ===== {} =====\n",
                rel.to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "/")
            ));
            // Truncate by BYTES on a char boundary — `max_bytes` is a byte
            // budget; taking N *chars* would overshoot ~3× on CJK source.
            for ch in content.chars() {
                if buf.len() + ch.len_utf8() > max_bytes {
                    break;
                }
                buf.push(ch);
            }
        }
    }
    buf
}

/// Static prefix of a path template, up to the first param segment
/// (`:id` / `{id}` / `*`). `/api/users/:id` → `/api/users/`. This is the
/// coverage needle: if a planned endpoint's static prefix appears nowhere in
/// the source, nothing was built for it.
fn static_prefix(path: &str) -> &str {
    let cut = path.find([':', '{', '*']).unwrap_or(path.len());
    &path[..cut]
}

/// Planned endpoints (from the architecture API table) with no implementation
/// evidence in the workspace. Empty when there's no architecture doc / no
/// endpoints (fail-open — never a false alarm).
///
/// ## What counts as "implemented"
/// The check matches each planned endpoint against the project's real **backend
/// route REGISTRATIONS** (`umadev_contract::extract_backend_routes`) — an
/// `app.get(...)` / `@app.route(...)` / `@GetMapping(...)` / `.route("/x", get(...))`
/// site, comment-stripped — NOT a raw substring over all source. This closes
/// the biggest false-pass in the deterministic floor: previously a planned
/// endpoint counted as done if its static path prefix appeared **anywhere** in
/// the concatenated source — including a frontend `fetch('/api/login')` call or
/// a `// TODO app.post('/api/login')` comment — so a backend that lived only as
/// frontend call-sites falsely PASSED.
///
/// ## Fail-open (no false failures)
/// When the extractor finds **no** backend registration anywhere — a genuine
/// pure-frontend project (the endpoint legitimately isn't ours to serve), or a
/// backend in a framework we can't parse — the check falls back to the legacy
/// surface behavior rather than falsely flagging every endpoint. Only a project
/// that HAS a backend we can read is held to the "real registration exists"
/// standard. Path parameters are normalised (`:id` ≡ `{id}` ≡ `<id>`) and a
/// mount/global/controller prefix the regex can't reconstruct is tolerated by a
/// right-aligned tail match (see `umadev_contract::route_registered`).
#[must_use]
pub fn task_acceptance_gaps(project_root: &Path, slug: &str) -> Vec<String> {
    let arch = std::fs::read_to_string(project_root.join(format!("output/{slug}-architecture.md")))
        .unwrap_or_default();
    if arch.trim().is_empty() {
        return Vec::new();
    }
    let spec = umadev_contract::parse_architecture(&arch, slug);
    if spec.is_empty() {
        return Vec::new();
    }

    let backend_routes = umadev_contract::extract_backend_routes(project_root);
    if backend_routes.is_empty() {
        // Fail-open: no recognised backend registration exists anywhere. Preserve
        // the legacy surface behavior so a pure-frontend project — or a backend
        // in a framework we cannot parse — is NOT falsely failed.
        return legacy_surface_gaps(project_root, &spec);
    }

    // The project HAS a backend we can read: every planned endpoint must have a
    // real route registration. A frontend `fetch`/`axios` call or a comment is
    // not a registration, so it can no longer satisfy the endpoint.
    let mut gaps = Vec::new();
    for e in &spec.endpoints {
        // Skip endpoints too generic to verify (`/`, `/api`, a purely-param
        // path) — mirrors the legacy `needle.len() < 4` skip.
        if !umadev_contract::path_has_checkable_segment(&e.path) {
            continue;
        }
        if !umadev_contract::route_registered(&backend_routes, e.method, &e.path) {
            gaps.push(format!(
                "{} {} — {}",
                e.method.as_str(),
                e.path,
                e.description
            ));
        }
    }
    gaps
}

/// Legacy substring-over-source coverage check, retained as the fail-open
/// fallback for projects with no backend registration we can read (pure
/// frontend, or an unparseable framework). This is exactly the pre-hardening
/// behavior — deliberately lenient (a path appearing anywhere counts) — so we
/// never turn a project we can't reason about into a false failure.
fn legacy_surface_gaps(project_root: &Path, spec: &umadev_contract::ApiSpec) -> Vec<String> {
    let surface = implementation_surface(project_root);
    if surface.trim().is_empty() {
        // No source at all → the base wrote nothing; every endpoint is a gap.
        return spec
            .endpoints
            .iter()
            .map(|e| format!("{} {} — {}", e.method.as_str(), e.path, e.description))
            .collect();
    }
    let mut gaps = Vec::new();
    for e in &spec.endpoints {
        let needle = static_prefix(&e.path);
        // Skip needles too generic to be a meaningful signal (e.g. "/", "/api").
        if needle.len() < 4 {
            continue;
        }
        if !surface.contains(needle) {
            gaps.push(format!(
                "{} {} — {}",
                e.method.as_str(),
                e.path,
                e.description
            ));
        }
    }
    gaps
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn arch_doc() -> &'static str {
        "# API\n\n\
         | Method | Path | Description | Auth |\n\
         |---|---|---|---|\n\
         | GET | /api/todos | list todos | none |\n\
         | POST | /api/todos | create todo | required |\n\
         | DELETE | /api/todos/:id | delete todo | required |\n"
    }

    #[test]
    fn no_arch_doc_means_no_gaps() {
        let tmp = TempDir::new().unwrap();
        assert!(task_acceptance_gaps(tmp.path(), "demo").is_empty());
    }

    #[test]
    fn flags_unimplemented_endpoints_and_clears_when_built() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("output")).unwrap();
        fs::write(tmp.path().join("output/demo-architecture.md"), arch_doc()).unwrap();
        // No source yet → all 3 endpoints are gaps.
        let gaps = task_acceptance_gaps(tmp.path(), "demo");
        assert_eq!(gaps.len(), 3, "{gaps:?}");

        // Now "implement" /api/todos (covers GET + POST, and DELETE via prefix).
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(
            tmp.path().join("src/api.ts"),
            "app.get('/api/todos', list); app.post('/api/todos', create); app.delete('/api/todos/:id', del);",
        )
        .unwrap();
        let gaps2 = task_acceptance_gaps(tmp.path(), "demo");
        assert!(gaps2.is_empty(), "all endpoints implemented: {gaps2:?}");
    }

    #[test]
    fn frontend_fetch_or_comment_no_longer_satisfies_backend_endpoint() {
        // THE BUG BEING FIXED: a planned endpoint that exists ONLY as a frontend
        // fetch() call and a comment must NOT count as implemented, once the
        // project is shown to have a real backend. Previously the substring check
        // over concatenated source passed it (a backend that lived only as
        // frontend call-sites falsely PASSED).
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("output")).unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(
            tmp.path().join("output/demo-architecture.md"),
            "# API\n\n\
             | Method | Path | Description | Auth |\n\
             |---|---|---|---|\n\
             | GET | /api/todos | list todos | none |\n\
             | POST | /api/login | login | none |\n",
        )
        .unwrap();
        // A REAL backend registers /api/todos (so the project is in strict mode)…
        fs::write(
            tmp.path().join("src/server.js"),
            "app.get('/api/todos', listTodos);",
        )
        .unwrap();
        // …but /api/login exists ONLY as a frontend fetch and a comment.
        fs::write(
            tmp.path().join("src/web.tsx"),
            "// app.post('/api/login', doLogin)\nfunction f(){ return fetch('/api/login'); }",
        )
        .unwrap();

        let gaps = task_acceptance_gaps(tmp.path(), "demo");
        assert_eq!(gaps.len(), 1, "only /api/login must be a gap: {gaps:?}");
        assert!(gaps[0].contains("/api/login"), "{gaps:?}");
        assert!(
            !gaps.iter().any(|g| g.contains("/api/todos")),
            "the real backend route must pass: {gaps:?}"
        );
    }

    #[test]
    fn real_backend_registration_passes() {
        // A backend with a real `app.post('/api/login')` registration passes.
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("output")).unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(
            tmp.path().join("output/demo-architecture.md"),
            "# API\n\n\
             | Method | Path | Description | Auth |\n\
             |---|---|---|---|\n\
             | POST | /api/login | login | none |\n",
        )
        .unwrap();
        fs::write(
            tmp.path().join("src/server.js"),
            "app.post('/api/login', (req,res) => res.json({ok:true}));",
        )
        .unwrap();
        assert!(
            task_acceptance_gaps(tmp.path(), "demo").is_empty(),
            "a real registration must clear the endpoint"
        );
    }

    #[test]
    fn param_path_normalized_across_frameworks() {
        // Planned /api/users/:id is served by a FastAPI `{id}` registration.
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("output")).unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(
            tmp.path().join("output/demo-architecture.md"),
            "# API\n\n\
             | Method | Path | Description | Auth |\n\
             |---|---|---|---|\n\
             | GET | /api/users/:id | get user | none |\n",
        )
        .unwrap();
        fs::write(
            tmp.path().join("src/main.py"),
            "@app.get('/api/users/{id}')\ndef get_user(id): ...",
        )
        .unwrap();
        assert!(
            task_acceptance_gaps(tmp.path(), "demo").is_empty(),
            ":id must match a {{id}} registration"
        );
    }

    #[test]
    fn pure_frontend_project_not_falsely_failed() {
        // No backend registration anywhere (only a frontend fetch). The endpoint
        // legitimately isn't ours to serve → fail-open to legacy behavior, not a
        // false failure.
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("output")).unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(
            tmp.path().join("output/demo-architecture.md"),
            "# API\n\n\
             | Method | Path | Description | Auth |\n\
             |---|---|---|---|\n\
             | GET | /api/todos | list | none |\n",
        )
        .unwrap();
        fs::write(
            tmp.path().join("src/web.tsx"),
            "export const load = () => fetch('/api/todos');",
        )
        .unwrap();
        assert!(
            task_acceptance_gaps(tmp.path(), "demo").is_empty(),
            "a pure-frontend project must not be failed for a backend it never owned"
        );
    }

    #[test]
    fn design_tokens_files_found_in_src_and_output_and_empty_when_absent() {
        let tmp = TempDir::new().unwrap();
        // Absent → empty (the DesignTokensPresent acceptance then rejects).
        assert!(
            design_tokens_files(tmp.path()).is_empty(),
            "no tokens file → empty (absent)"
        );
        // A CSS token file in a conventional style dir is found.
        fs::create_dir_all(tmp.path().join("src/styles")).unwrap();
        fs::write(
            tmp.path().join("src/styles/design-tokens.css"),
            ":root{--color-bg:#fff}",
        )
        .unwrap();
        let found = design_tokens_files(tmp.path());
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].ends_with("design-tokens.css"));
        // A JSON token file on the doc blackboard (output/, normally skipped for
        // source) is ALSO found — the scan descends into output for this artifact.
        fs::create_dir_all(tmp.path().join("output")).unwrap();
        fs::write(
            tmp.path().join("output/design-tokens.json"),
            r##"{"color":{"bg":"#fff"}}"##,
        )
        .unwrap();
        let found = design_tokens_files(tmp.path());
        assert_eq!(
            found.len(),
            2,
            "both the css and the output json: {found:?}"
        );
        assert!(found.iter().any(|p| p.ends_with("design-tokens.json")));
    }

    #[test]
    fn partial_implementation_reports_only_the_gap() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("output")).unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(
            tmp.path().join("output/demo-architecture.md"),
            "# API\n\n\
             | Method | Path | Description | Auth |\n\
             |---|---|---|---|\n\
             | GET | /api/todos | list | none |\n\
             | GET | /api/users | users | none |\n",
        )
        .unwrap();
        // Only /api/todos is implemented.
        fs::write(tmp.path().join("src/api.ts"), "fetch('/api/todos')").unwrap();
        let gaps = task_acceptance_gaps(tmp.path(), "demo");
        assert_eq!(gaps.len(), 1);
        assert!(gaps[0].contains("/api/users"));
    }

    #[test]
    fn source_files_include_deep_enterprise_view_paths() {
        // Repro shape from enterprise Vue/admin projects:
        // src/views/biz/app/page/component/widgets/customer/detail/...
        // This used to exceed the depth-8 walk cap, making real delivered code
        // invisible to the source-present gate and QA/code-review digest.
        let tmp = TempDir::new().unwrap();
        let deep = tmp
            .path()
            .join("src/views/biz/app/page/component/widgets/customer/detail/panels");
        fs::create_dir_all(&deep).unwrap();
        fs::write(
            deep.join("ProfilePanel.vue"),
            "<script setup>const loaded = true</script>\n<template><main /></template>",
        )
        .unwrap();

        let found = source_files(tmp.path());
        assert!(
            found.iter().any(|p| p.ends_with("ProfilePanel.vue")),
            "deep real source must be visible to QA/source-present scans: {found:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn source_files_no_follow_symlinks_out_and_cycle_terminates() {
        use std::os::unix::fs::symlink;
        // OUTSIDE the workspace: a real source file the walk must NEVER reach.
        // If a symlink were followed this would be fed to the LLM judge / SAST.
        let outside = TempDir::new().unwrap();
        fs::create_dir_all(outside.path().join("secret")).unwrap();
        fs::write(
            outside.path().join("secret/leaked.rs"),
            "fn leaked() { let x = 1; x + 1; }\n",
        )
        .unwrap();

        // The workspace: one real in-tree file, a dir symlink escaping OUTSIDE,
        // and a self-referential symlink cycle (src/loop -> src).
        let ws = TempDir::new().unwrap();
        fs::create_dir_all(ws.path().join("src")).unwrap();
        fs::write(
            ws.path().join("src/main.rs"),
            "fn main() { let y = 2; println!(\"{y}\"); }\n",
        )
        .unwrap();
        symlink(outside.path(), ws.path().join("src/escape")).unwrap();
        symlink(ws.path().join("src"), ws.path().join("src/loop")).unwrap();

        // Terminates (no stack overflow / hang) because a dir symlink is never
        // descended, so the cycle is unreachable.
        let found = source_files(ws.path());

        // No regression: the real in-tree file is still collected.
        assert!(
            found.iter().any(|p| p.ends_with("main.rs")),
            "in-tree source must still be walked: {found:?}"
        );
        // A symlink must not pull a file from OUTSIDE the workspace into scope.
        assert!(
            !found.iter().any(|p| p.ends_with("leaked.rs")),
            "a symlink must not pull files from outside the workspace: {found:?}"
        );
        assert!(
            !found.iter().any(|p| p.to_string_lossy().contains("escape")),
            "the walk must not traverse an escaping symlink at all: {found:?}"
        );
    }

    #[test]
    fn empty_and_comment_only_stubs_do_not_count_as_source() {
        // MEDIUM M1: a base that hallucinates "done" can leave a 0-byte or one-comment
        // stub. Those must NOT count as delivered source (else the source-present
        // honesty hard-gate passes on a build that produced nothing). A real tiny file
        // DOES count.
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        // A 0-byte file, a whitespace-only file, and comment-only stubs across styles.
        fs::write(tmp.path().join("src/empty.js"), "").unwrap();
        fs::write(tmp.path().join("src/blank.ts"), "  \n\t\n").unwrap();
        fs::write(tmp.path().join("src/todo.jsx"), "// TODO build this\n").unwrap();
        fs::write(tmp.path().join("src/stub.css"), "/* placeholder */\n").unwrap();
        fs::write(tmp.path().join("src/note.html"), "<!-- nothing yet -->\n").unwrap();
        fs::write(tmp.path().join("src/hash.py"), "# placeholder\n").unwrap();
        assert!(
            source_files(tmp.path()).is_empty(),
            "stubs must not be counted as real source: {:?}",
            source_files(tmp.path())
        );

        // A genuinely tiny but REAL file is counted (no false-reject of small code) …
        fs::write(tmp.path().join("src/main.rs"), "fn main(){}").unwrap();
        // … as is a CSS file whose only line is an id selector (`#` is NOT a comment
        // in CSS — it must not be mis-read as a stub) …
        fs::write(tmp.path().join("src/app.css"), "#header{color:red}\n").unwrap();
        // … and a Python file with a real statement after a comment line.
        fs::write(tmp.path().join("src/run.py"), "# header\nprint(1)\n").unwrap();
        let found = source_files(tmp.path());
        assert_eq!(
            found.len(),
            3,
            "the three real files count, the six stubs don't: {found:?}"
        );
    }
}
