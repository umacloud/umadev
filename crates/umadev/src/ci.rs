//! `umadev ci` — run governance on every source file in the workspace.
//!
//! This is the CI/CD entry point: scan all source files under the project root,
//! run the full governance rule set on each, and exit non-zero if any file
//! violates a rule. Designed to run in a GitHub Action / pre-commit hook so
//! governance violations are caught BEFORE code is pushed.
//!
//! ## Usage
//! ```bash
//! umadev ci                      # scan + fail on any violation
//! umadev ci --report-only        # scan but always exit 0 (for reporting)
//! umadev ci --changed-only       # scan only git-changed files
//! ```
//!
//! ## Output
//! One line per violation: `BLOCK  <clause>  <file>:<line>  <reason>`.
//! Summary at the end: `UmaDev: 3 files blocked, 5 violations (exit 1)`.

use std::path::{Path, PathBuf};
use umadev_governance::{scan_content_with_policy, Policy};

/// File extensions the CI scan considers "source" (governance-eligible).
const SCAN_EXTENSIONS: &[&str] = &[
    "js", "jsx", "ts", "tsx", "py", "rb", "go", "rs", "java", "kt", "swift", "php", "vue",
    "svelte", "astro",
];

/// Directories to skip during the scan (deps, build output, VCS).
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    "target",
    "dist",
    "build",
    ".next",
    ".nuxt",
    ".output",
    ".svelte-kit",
    "vendor",
    ".cache",
    "__pycache__",
    ".venv",
    "venv",
    "coverage",
    ".turbo",
];

/// CI scan options.
#[derive(Debug, Clone)]
pub struct CiOptions {
    /// Only report violations without failing (exit 0).
    pub report_only: bool,
    /// Only scan git-tracked changed files (vs all files).
    pub changed_only: bool,
    /// Project root to scan.
    pub project_root: PathBuf,
}

/// Result of a CI scan.
#[derive(Debug, Default)]
pub struct CiResult {
    /// Total source files scanned.
    pub files_scanned: usize,
    /// Number of files with at least one violation.
    pub files_blocked: usize,
    /// Total violations found.
    pub violations: usize,
    /// Whether the scan should fail CI (files_blocked > 0 && !report_only).
    pub failed: bool,
}

/// Run the CI governance scan. Prints violations to stdout, returns the
/// summary. Exit code is 1 when `failed` is true (the caller maps this).
///
/// # Errors
/// Returns an error only on a filesystem traversal failure.
pub fn run(opts: &CiOptions) -> std::io::Result<CiResult> {
    let policy = Policy::load(&opts.project_root);
    let files = collect_source_files(&opts.project_root, opts.changed_only)?;
    let mut result = CiResult {
        files_scanned: files.len(),
        ..Default::default()
    };

    for file in &files {
        let rel = file
            .strip_prefix(&opts.project_root)
            .unwrap_or(file)
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        // Read the content to scan. In `--changed-only` mode (the pre-commit
        // hook) we read the STAGED blob (`git show :<file>`), NOT the on-disk
        // file: the commit captures the index, so judging the dirty working copy
        // would block a commit on an unstaged hunk and pass a clean staged
        // version by its dirty working state. Otherwise read on disk. Best-effort:
        // skip an unreadable file (a binary blob, a path removed from the index).
        let content = if opts.changed_only {
            let Some(staged) = read_staged_blob(&opts.project_root, &rel) else {
                continue;
            };
            staged
        } else {
            let Ok(disk) = std::fs::read_to_string(file) else {
                continue;
            };
            disk
        };
        let decision = scan_content_with_policy(&rel, &content, &policy);
        if decision.block {
            result.files_blocked += 1;
            result.violations += 1;
            println!(
                "BLOCK  {}  {}  {}",
                decision.clause,
                rel,
                decision.reason.split('.').next().unwrap_or("violation"),
            );
        }
    }

    // UD-SEC-016: run `npm audit` if a package-lock.json is present, to catch
    // known-vulnerable dependencies (OWASP A06). Best-effort: if npm isn't
    // installed or the audit fails, skip silently (the file scan still ran).
    if opts.project_root.join("package-lock.json").exists() {
        if let Ok(audit_result) = npm_audit(&opts.project_root) {
            if audit_result.critical + audit_result.high > 0 {
                result.violations += audit_result.critical + audit_result.high;
                result.files_blocked += 1;
                println!(
                    "BLOCK  UD-SEC-016  package.json  {} critical, {} high vulnerabilities in dependencies",
                    audit_result.critical, audit_result.high,
                );
            } else if audit_result.total() > 0 {
                println!(
                    "WARN   UD-SEC-016  {} lower-severity vulnerabilities (moderate/low) in dependencies",
                    audit_result.moderate + audit_result.low,
                );
            }
        }
    }

    // Summary is printed AFTER the npm-audit block so its counts reflect any
    // UD-SEC-016 CVE hits — otherwise a JS project with a critical CVE printed
    // "0 blocked, 0 violations" and then a "BLOCK UD-SEC-016" line, exiting 1
    // while the summary claimed a clean scan.
    println!("{}", scan_summary(&result));

    result.failed = result.files_blocked > 0 && !opts.report_only;
    Ok(result)
}

/// Render the one-line scan summary from the FINAL [`CiResult`] — after the
/// UD-SEC-016 npm-audit block has folded any CVE hits into `files_blocked` /
/// `violations`. Keeping this pure (a function of the final counts) is what
/// stops the printed summary from contradicting a subsequent `BLOCK` line or
/// the process exit code.
fn scan_summary(result: &CiResult) -> String {
    format!(
        "\nUmaDev: {} file(s) scanned, {} blocked, {} violation(s).",
        result.files_scanned, result.files_blocked, result.violations,
    )
}

/// Result of an `npm audit --json` scan.
#[derive(Debug, Default)]
pub struct NpmAuditResult {
    pub critical: usize,
    pub high: usize,
    pub moderate: usize,
    pub low: usize,
}

impl NpmAuditResult {
    fn total(&self) -> usize {
        self.critical + self.high + self.moderate + self.low
    }
}

/// Run `npm audit --json` and count vulnerabilities by severity (UD-SEC-016).
/// Returns an error only if npm isn't available or the command fails; a
/// successful run with zero vulns returns an all-zero result.
fn npm_audit(project_root: &Path) -> std::io::Result<NpmAuditResult> {
    let output = umadev_host::std_command("npm")
        .args(["audit", "--json"])
        .current_dir(project_root)
        .output()?;
    // npm audit exits non-zero when vulns are found, but stdout still has JSON.
    let text = String::from_utf8_lossy(&output.stdout);
    parse_npm_audit(&text).map_or(Ok(NpmAuditResult::default()), Ok)
}

/// Parse `npm audit --json` output into a severity-count summary.
/// Handles both npm 7+ format (top-level `vulnerabilities` map) and the
/// legacy `metadata.vulnerabilities` format.
fn parse_npm_audit(text: &str) -> Option<NpmAuditResult> {
    let val: serde_json::Value = serde_json::from_str(text).ok()?;
    let mut result = NpmAuditResult::default();
    // npm 7+: top-level "vulnerabilities" object with per-advisory "severity".
    if let Some(vulns) = val.get("vulnerabilities").and_then(|v| v.as_object()) {
        for (_, info) in vulns {
            let severity = info.get("severity").and_then(|s| s.as_str()).unwrap_or("");
            match severity {
                "critical" => result.critical += 1,
                "high" => result.high += 1,
                "moderate" => result.moderate += 1,
                "low" => result.low += 1,
                _ => {}
            }
        }
        return Some(result);
    }
    // Legacy: "metadata.vulnerabilities" with counts.
    if let Some(meta) = val.get("metadata").and_then(|m| m.get("vulnerabilities")) {
        let get = |k: &str| meta.get(k).and_then(serde_json::Value::as_u64).unwrap_or(0);
        result.critical = usize::try_from(get("critical")).unwrap_or(0);
        result.high = usize::try_from(get("high")).unwrap_or(0);
        result.moderate = usize::try_from(get("moderate")).unwrap_or(0);
        result.low = usize::try_from(get("low")).unwrap_or(0);
        return Some(result);
    }
    None
}

/// Walk the project root and collect all source files (by extension), skipping
/// deps/build/VCS directories. When `changed_only` is set, restricts to
/// `git diff` tracked files.
fn collect_source_files(root: &Path, changed_only: bool) -> std::io::Result<Vec<PathBuf>> {
    if changed_only {
        return git_changed_files(root);
    }
    let mut files = Vec::new();
    walk_dir(root, &mut files);
    Ok(files)
}

/// Recursive directory walk collecting source files.
fn walk_dir(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            // Skip deps/build/VCS directories.
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            if SKIP_DIRS.contains(&name) || name.starts_with('.') {
                continue;
            }
            walk_dir(&path, files);
        } else if ft.is_file() {
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or_default();
            if SCAN_EXTENSIONS.contains(&ext) {
                files.push(path);
            }
        }
    }
}

/// Get the files in the STAGED index that differ from `HEAD` — the exact set a
/// commit would capture. This powers the pre-commit hook, so it must be the
/// staged scope (`--cached`), NOT the working tree: a `git diff HEAD` would also
/// include unstaged edits, blocking a commit on a violation that isn't part of
/// it. With no commits yet, `--cached` compares against the empty tree (all
/// staged files appear as new), so it still works without the ls-files fallback;
/// that fallback covers only the "not a git repo" case.
fn git_changed_files(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    // `-c core.quotePath=false` + `-z`: emit NUL-separated, UNQUOTED paths so a
    // staged file with a non-ASCII (`café.tsx`) or spaced name is scanned rather
    // than dropped. At git's default (`core.quotePath=true`) such a path is
    // octal-escaped + double-quoted (`"caf\303\251.tsx"`), so `extension()`
    // yields `tsx"` and it silently falls out of SCAN_EXTENSIONS — a real
    // violation would never be scanned. `-z` also removes the quoting entirely,
    // so the raw path round-trips to `git show :<rel>` in `read_staged_blob`.
    let output = std::process::Command::new("git")
        .args([
            "-c",
            "core.quotePath=false",
            "diff",
            "--name-only",
            "-z",
            "--cached",
        ])
        .current_dir(root)
        .output();
    let out = match output {
        Ok(o) if o.status.success() => o.stdout,
        _ => {
            // Not a git repo — fall back to ls-files (tracked, index == HEAD).
            let ls = std::process::Command::new("git")
                .args(["-c", "core.quotePath=false", "ls-files", "-z"])
                .current_dir(root)
                .output();
            match ls {
                Ok(o) if o.status.success() => o.stdout,
                _ => return Ok(Vec::new()),
            }
        }
    };
    let text = String::from_utf8_lossy(&out);
    let files: Vec<PathBuf> = text
        .split('\0')
        .filter(|l| !l.is_empty())
        .filter(|l| {
            let ext = Path::new(l)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or_default();
            SCAN_EXTENSIONS.contains(&ext)
        })
        .map(|l| root.join(l))
        .collect();
    Ok(files)
}

/// Read the STAGED content of `rel` (a workspace-relative, forward-slash path)
/// from the git index via `git show :<rel>`. Returns `None` when the path isn't
/// staged, the blob is binary/unreadable, or git is unavailable (fail-open: the
/// caller skips the file).
fn read_staged_blob(root: &Path, rel: &str) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["show", &format!(":{rel}")])
        .current_dir(root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    // A staged binary blob isn't valid UTF-8 — treat as unreadable (skip), the
    // same as the on-disk `read_to_string` path does.
    String::from_utf8(output.stdout).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ci_scans_clean_project() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("clean.ts"), "export const x: number = 1;").unwrap();
        let result = run(&CiOptions {
            report_only: false,
            changed_only: false,
            project_root: tmp.path().to_path_buf(),
        })
        .unwrap();
        assert_eq!(result.files_scanned, 1);
        assert_eq!(result.files_blocked, 0);
        assert!(!result.failed);
    }

    #[test]
    fn ci_flags_violation() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("bad.tsx"), "<b>🔍</b>").unwrap();
        let result = run(&CiOptions {
            report_only: false,
            changed_only: false,
            project_root: tmp.path().to_path_buf(),
        })
        .unwrap();
        assert_eq!(result.files_blocked, 1);
        assert!(result.failed);
    }

    #[test]
    fn ci_report_only_does_not_fail() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("bad.tsx"), "<b>🔍</b>").unwrap();
        let result = run(&CiOptions {
            report_only: true,
            changed_only: false,
            project_root: tmp.path().to_path_buf(),
        })
        .unwrap();
        assert_eq!(result.files_blocked, 1);
        assert!(!result.failed); // report-only → exit 0
    }

    #[test]
    fn ci_skips_node_modules() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("node_modules")).unwrap();
        // A violation inside node_modules must NOT be scanned.
        std::fs::write(tmp.path().join("node_modules/x.tsx"), "<b>🔍</b>").unwrap();
        std::fs::write(tmp.path().join("clean.ts"), "export const x = 1;").unwrap();
        let result = run(&CiOptions {
            report_only: false,
            changed_only: false,
            project_root: tmp.path().to_path_buf(),
        })
        .unwrap();
        assert_eq!(result.files_blocked, 0);
        assert!(!result.failed);
    }

    #[test]
    fn ci_respects_disabled_policy() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sd_dir = tmp.path().join(".umadev");
        std::fs::create_dir_all(&sd_dir).unwrap();
        std::fs::write(
            sd_dir.join("rules.toml"),
            "[disabled]\nclauses = [\"UD-CODE-001\"]\n",
        )
        .unwrap();
        // Emoji is UD-CODE-001 — disabled → should pass.
        std::fs::write(tmp.path().join("bad.tsx"), "<b>🔍</b>").unwrap();
        let result = run(&CiOptions {
            report_only: false,
            changed_only: false,
            project_root: tmp.path().to_path_buf(),
        })
        .unwrap();
        assert_eq!(result.files_blocked, 0);
    }

    #[test]
    fn walk_collects_only_source_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("app.ts"), "x").unwrap();
        std::fs::write(tmp.path().join("readme.md"), "x").unwrap();
        std::fs::write(tmp.path().join("data.json"), "x").unwrap();
        let mut files = Vec::new();
        walk_dir(tmp.path(), &mut files);
        let names: Vec<String> = files
            .iter()
            .map(|f| f.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"app.ts".to_string()));
        assert!(!names.contains(&"readme.md".to_string()));
        assert!(!names.contains(&"data.json".to_string()));
    }

    // --- M2: changed-only uses the STAGED index, not the working tree -------

    /// Run a git command in `dir`; returns false if git is missing/fails.
    fn git(dir: &Path, args: &[&str]) -> bool {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .is_ok_and(|o| o.status.success())
    }

    /// Init a throwaway repo with a committed identity, or `false` if git is
    /// unavailable (the caller then skips — no hard git dependency in tests).
    fn init_repo(dir: &Path) -> bool {
        git(dir, &["init", "-q"])
            && git(dir, &["config", "user.email", "t@t.test"])
            && git(dir, &["config", "user.name", "test"])
    }

    #[test]
    fn changed_only_scans_staged_blob_not_dirty_working_tree() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        if !init_repo(root) {
            return; // git not available — skip
        }
        let file = root.join("app.tsx");
        // Commit a clean baseline.
        std::fs::write(&file, "export const x = 1;\n").unwrap();
        assert!(git(root, &["add", "app.tsx"]));
        assert!(git(root, &["commit", "-q", "-m", "base"]));
        // STAGE a different but still CLEAN version (so it appears in --cached).
        std::fs::write(&file, "export const y = 2;\n").unwrap();
        assert!(git(root, &["add", "app.tsx"]));
        // Dirty the WORKING TREE with an emoji violation — but do NOT stage it.
        std::fs::write(&file, "<b>\u{1f50d}</b>\n").unwrap();

        let result = run(&CiOptions {
            report_only: false,
            changed_only: true,
            project_root: root.to_path_buf(),
        })
        .unwrap();
        // The staged version is clean → no block, even though the working copy
        // (which the OLD `git diff HEAD` + on-disk read judged) is dirty.
        assert_eq!(
            result.files_blocked, 0,
            "must judge the STAGED blob, not the dirty working copy"
        );
        assert!(!result.failed);
    }

    #[test]
    fn changed_only_flags_a_violation_in_the_staged_version() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        if !init_repo(root) {
            return; // git not available — skip
        }
        let file = root.join("app.tsx");
        std::fs::write(&file, "export const x = 1;\n").unwrap();
        assert!(git(root, &["add", "app.tsx"]));
        assert!(git(root, &["commit", "-q", "-m", "base"]));
        // STAGE a version WITH a violation; clean it up in the working tree.
        std::fs::write(&file, "<b>\u{1f50d}</b>\n").unwrap();
        assert!(git(root, &["add", "app.tsx"]));
        std::fs::write(&file, "export const ok = 3;\n").unwrap(); // clean working copy

        let result = run(&CiOptions {
            report_only: false,
            changed_only: true,
            project_root: root.to_path_buf(),
        })
        .unwrap();
        // The STAGED blob carries the violation → blocked, regardless of the
        // now-clean working copy.
        assert_eq!(result.files_blocked, 1, "staged violation must be flagged");
        assert!(result.failed);
    }

    #[test]
    fn changed_only_scans_non_ascii_staged_filename() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        if !init_repo(root) {
            return; // git not available — skip
        }
        // A non-ASCII filename: at git's default core.quotePath=true, `git diff
        // --name-only --cached` would emit `"caf\303\251.tsx"` (octal-escaped +
        // quoted), so `extension()` sees `tsx"` and the file drops out of the
        // scan. The -c core.quotePath=false + -z fix must scan it.
        let file = root.join("café.tsx");
        std::fs::write(&file, "<b>\u{1f50d}</b>\n").unwrap(); // emoji violation
        assert!(git(root, &["add", "café.tsx"]));

        // Sanity: git_changed_files must surface the non-ASCII path (unquoted).
        let listed = git_changed_files(root).unwrap();
        assert!(
            listed.iter().any(|p| p.ends_with("café.tsx")),
            "the non-ASCII staged path must be listed unquoted, got {listed:?}"
        );

        let result = run(&CiOptions {
            report_only: false,
            changed_only: true,
            project_root: root.to_path_buf(),
        })
        .unwrap();
        assert_eq!(
            result.files_blocked, 1,
            "a violation in a non-ASCII staged filename must be scanned + blocked"
        );
        assert!(result.failed);
    }

    #[test]
    fn scan_summary_reflects_post_audit_counts() {
        // Emulate the result AFTER the npm-audit block folded a critical CVE in:
        // the summary must report the inflated counts, not the pre-audit "0
        // blocked, 0 violations" that used to precede a `BLOCK UD-SEC-016` line.
        let result = CiResult {
            files_scanned: 3,
            files_blocked: 1,
            violations: 2,
            failed: true,
        };
        let line = scan_summary(&result);
        assert!(line.contains("3 file(s) scanned"), "{line}");
        assert!(line.contains("1 blocked"), "{line}");
        assert!(line.contains("2 violation(s)"), "{line}");
    }

    // --- UD-SEC-016: npm audit parsing ----------------------------------

    #[test]
    fn npm_audit_parses_npm7_format() {
        let json = r#"{"vulnerabilities":{"lodash":{"severity":"high"},"react":{"severity":"critical"},"left-pad":{"severity":"low"}}}"#;
        let result = parse_npm_audit(json).unwrap();
        assert_eq!(result.critical, 1);
        assert_eq!(result.high, 1);
        assert_eq!(result.low, 1);
    }

    #[test]
    fn npm_audit_parses_legacy_format() {
        let json =
            r#"{"metadata":{"vulnerabilities":{"critical":2,"high":3,"moderate":1,"low":0}}}"#;
        let result = parse_npm_audit(json).unwrap();
        assert_eq!(result.critical, 2);
        assert_eq!(result.high, 3);
        assert_eq!(result.moderate, 1);
    }

    #[test]
    fn npm_audit_parses_clean() {
        let json = r#"{"vulnerabilities":{}}"#;
        let result = parse_npm_audit(json).unwrap();
        assert_eq!(result.total(), 0);
    }

    #[test]
    fn npm_audit_returns_none_on_garbage() {
        assert!(parse_npm_audit("not json").is_none());
    }
}
