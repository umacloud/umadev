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

/// Source + style extensions. The code files form the "implementation surface"
/// for the endpoint-coverage check; the style files (css/…) matter for the
/// post-phase governance scan, where hardcoded colors live.
const SRC_EXT: &[&str] = &[
    "tsx", "jsx", "ts", "js", "vue", "svelte", "astro", "py", "rs", "go", "java", "rb", "php",
    "cs", "kt", "ex", "exs", "dart", "swift", "css", "scss", "sass", "less", "html",
];

/// Directories never worth scanning (build output / vendored deps / VCS).
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "dist",
    "build",
    ".git",
    "vendor",
    "__pycache__",
    ".next",
    "out",
    "coverage",
];

/// Recursively collect source files (bounded: depth 8, 600 files).
fn collect(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > 8 || out.len() > 600 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with('.') || SKIP_DIRS.contains(&name) {
                continue;
            }
            collect(&p, out, depth + 1);
        } else if let Some(ext) = p.extension().and_then(|s| s.to_str()) {
            if SRC_EXT.contains(&ext) {
                out.push(p);
            }
        }
    }
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
            buf.push_str(&format!("\n// ===== {} =====\n", rel.display()));
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
}
