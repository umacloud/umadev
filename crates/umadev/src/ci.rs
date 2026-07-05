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
use umadev_governance::{
    check_sensitive_path, pre_write_floor_decision, scan_content_with_policy, Policy,
};

/// File extensions the CI scan considers "source" (governance-eligible).
const SCAN_EXTENSIONS: &[&str] = &[
    "js", "jsx", "ts", "tsx", "py", "rb", "go", "rs", "java", "kt", "swift", "php", "vue",
    "svelte", "astro",
];

/// Is `rel` a security-sensitive path that MUST be scanned by the bypass-immune
/// floor REGARDLESS of its extension?
///
/// The `SCAN_EXTENSIONS` allow-list silently drops exactly the files most likely
/// to leak a live secret — `.env` (no extension), `id_rsa`, `*.pem`, `credentials`,
/// anything under `.ssh/` — so a staged `.env` with a `sk_live_…` key used to scan
/// as "0 files, 0 blocked, exit 0". This predicate pulls those paths back into
/// scope so [`pre_write_floor_decision`] can block them. It is a SUPERSET of the
/// floor's own path guard ([`check_sensitive_path`]) plus the two dotenv/cert
/// forms the guard's fixed suffix list omits (`.env.<anything>`, `*.pem`), so a
/// secret in any of them reaches the content floor. Segment-aware via the reused
/// guard, so `messages.ts` never matches.
fn is_sensitive_scan_path(rel: &str) -> bool {
    let lower = rel.replace('\\', "/").to_ascii_lowercase();
    // *.pem (private keys / certs) anywhere in the tree.
    if std::path::Path::new(&lower)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("pem"))
    {
        return true;
    }
    // Any dotenv variant: `.env`, `.env.local`, `.env.staging`, `.env.<x>`.
    let last = lower.rsplit('/').next().unwrap_or("");
    if last == ".env" || last.starts_with(".env.") {
        return true;
    }
    // Reuse the floor's EXACT path guard for the rest (`.ssh/` `.aws/` `.git/`
    // segments, `id_rsa` / `credentials` / `.npmrc` / … suffixes) so CI stays in
    // lockstep with the floor rather than drifting from a hand-copied list.
    check_sensitive_path(rel, "").block
}

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
        // Bypass-immune irreversible floor FIRST (path-type + content
        // secret/password): a `.umadev/rules.toml` that disabled UD-SEC-001/003/018/026
        // must NOT let a leaked secret or a sensitive-path write (e.g. a staged
        // `.env`) pass CI. Only when the floor is clean do we fall through to the
        // policy-aware content scan (which honours disabled clauses for everything
        // else).
        let floor = pre_write_floor_decision(&rel, &content);
        let decision = if floor.block {
            floor
        } else {
            scan_content_with_policy(&rel, &content, &policy)
        };
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

/// How long to wait for `npm audit` before giving up. `npm audit` reaches out
/// to the registry and can stall indefinitely (a hung registry, a proxy, a
/// broken lockfile). 60s is generous for a real audit yet bounds a stuck one.
const NPM_AUDIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Run `cmd` to completion, capping the wait at `timeout`. Returns
/// `Ok(Some(stdout))` when the child exits within budget, `Ok(None)` when it
/// overruns (the child is killed + reaped — fail-open), or `Err(..)` only when
/// the child can't be spawned or polled.
///
/// stdout is drained on a worker thread so a large report can't fill the OS
/// pipe buffer and deadlock the poll loop; a killed child closes the pipe,
/// which ends the read, so the thread always terminates.
fn run_capturing_with_timeout(
    mut cmd: std::process::Command,
    timeout: std::time::Duration,
) -> std::io::Result<Option<String>> {
    use std::io::Read;
    use std::process::Stdio;
    use std::time::Instant;

    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let stdout = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(mut out) = stdout {
            let _ = out.read_to_string(&mut buf);
        }
        buf
    });

    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            break; // child exited within budget
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            // Do NOT join the reader here. A shell child can fork a grandchild
            // (e.g. `sh -c "sleep 10"` -> `sleep`) that inherits the stdout pipe and
            // holds it open until IT exits, so `read_to_string` would block far past
            // our budget. On timeout we skip the audit and discard its output, so we
            // detach the reader and return promptly (it ends when the pipe closes).
            return Ok(None); // timed out — fail-open (skip), promptly
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    Ok(Some(reader.join().unwrap_or_default()))
}

/// Run `npm audit --json` and count vulnerabilities by severity (UD-SEC-016).
/// Returns an error only if npm isn't available or the command can't be
/// spawned; a successful run with zero vulns returns an all-zero result.
///
/// **Bounded + fail-open.** The subprocess is capped at [`NPM_AUDIT_TIMEOUT`];
/// if it overruns, the child is killed and an all-zero result is returned (the
/// audit is SKIPPED, never hangs CI).
fn npm_audit(project_root: &Path) -> std::io::Result<NpmAuditResult> {
    let mut cmd = umadev_host::std_command("npm");
    cmd.args(["audit", "--json"]).current_dir(project_root);
    // npm audit exits non-zero when vulns are found, but stdout still has JSON.
    match run_capturing_with_timeout(cmd, NPM_AUDIT_TIMEOUT)? {
        Some(text) => Ok(parse_npm_audit(&text).unwrap_or_default()),
        None => Ok(NpmAuditResult::default()), // timed out → skip (fail-open)
    }
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
            // Source by extension, OR a sensitive path regardless of extension, so
            // a committed `.env` / `*.pem` / `credentials` reaches the floor in a
            // full scan too (the same set the changed-only path pulls in).
            if SCAN_EXTENSIONS.contains(&ext) || is_sensitive_scan_path(&path.to_string_lossy()) {
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
            // Source by extension, OR a sensitive path (`.env` / `id_rsa` / `*.pem`
            // / `.ssh/*` / `credentials`) regardless of extension — the floor must
            // see a staged secret file even though it carries no source extension.
            SCAN_EXTENSIONS.contains(&ext) || is_sensitive_scan_path(l)
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
    fn changed_only_blocks_a_staged_dotenv_secret() {
        // REPRODUCTION: a staged `.env` carrying a live Stripe key. `.env` has no
        // source extension, so the OLD collect-by-extension scan never saw it —
        // "0 file(s) scanned, 0 blocked, exit 0". The floor must now pull the
        // sensitive path into scope and BLOCK it (UD-SEC-001 path guard), failing
        // CI with a non-zero exit.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        if !init_repo(root) {
            return; // git not available — skip
        }
        std::fs::write(
            root.join(".env"),
            "STRIPE_SECRET_KEY=aB3xK9pQ7mNr2WvT5sZ8dF1gH4jL6cE0\n",
        )
        .unwrap();
        assert!(git(root, &["add", ".env"]));

        // Sanity: the sensitive path is now in the changed-file scan set despite
        // having no source extension.
        let listed = git_changed_files(root).unwrap();
        assert!(
            listed.iter().any(|p| p.ends_with(".env")),
            "a staged .env must be listed for scanning, got {listed:?}"
        );

        let result = run(&CiOptions {
            report_only: false,
            changed_only: true,
            project_root: root.to_path_buf(),
        })
        .unwrap();
        assert!(
            result.files_blocked >= 1,
            "a staged .env secret must be blocked, not silently skipped"
        );
        assert!(result.failed, "and it must fail CI (non-zero exit)");
    }

    #[test]
    fn changed_only_blocks_a_secret_in_a_no_extension_file() {
        // A staged `credentials` file (no extension) with a live secret must be
        // scanned + blocked too — the sensitive-path scope is not limited to
        // dotenv files.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        if !init_repo(root) {
            return; // git not available — skip
        }
        std::fs::write(
            root.join("credentials"),
            "aws_secret_access_key = aB3xK9pQ7mNr2WvT5sZ8dF1gH4jL6cE0\n",
        )
        .unwrap();
        assert!(git(root, &["add", "credentials"]));
        let result = run(&CiOptions {
            report_only: false,
            changed_only: true,
            project_root: root.to_path_buf(),
        })
        .unwrap();
        assert!(
            result.files_blocked >= 1,
            "a staged credentials secret must block"
        );
        assert!(result.failed);
    }

    #[test]
    fn is_sensitive_scan_path_matches_the_floor_set() {
        // The extension-agnostic scan predicate covers the dotenv/cert/key set …
        assert!(is_sensitive_scan_path(".env"));
        assert!(is_sensitive_scan_path("apps/api/.env.production"));
        assert!(is_sensitive_scan_path(".env.staging")); // arbitrary dotenv variant
        assert!(is_sensitive_scan_path("certs/server.pem"));
        assert!(is_sensitive_scan_path("deploy/id_rsa"));
        assert!(is_sensitive_scan_path("secrets/credentials"));
        assert!(is_sensitive_scan_path(".ssh/known_hosts"));
        // … and does NOT sweep in ordinary source (that rides SCAN_EXTENSIONS).
        assert!(!is_sensitive_scan_path("src/messages.ts"));
        assert!(!is_sensitive_scan_path("README.md"));
        // A dotenv TEMPLATE is in scan SCOPE (any `.env.*`), but the floor's path
        // guard never auto-blocks it — only its content is judged, so a
        // placeholder file passes while a real `.env` is blocked on the path.
        assert!(is_sensitive_scan_path(".env.example"));
        assert!(!check_sensitive_path(".env.example", "").block);
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

    // --- bounded npm-audit wait (never hangs CI) ------------------------

    #[cfg(unix)]
    #[test]
    fn capturing_timeout_kills_a_stuck_child_fast() {
        use std::time::{Duration, Instant};
        // A child that would run for 10s, capped at 200ms → returns None (skip)
        // WELL before the child's own runtime, and quickly.
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg("sleep 10");
        let started = Instant::now();
        let out = run_capturing_with_timeout(cmd, Duration::from_millis(200)).unwrap();
        let elapsed = started.elapsed();
        assert!(
            out.is_none(),
            "an overrunning audit must be skipped, not returned"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "the wait must be bounded, took {elapsed:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn capturing_returns_stdout_when_child_exits_in_time() {
        use std::time::Duration;
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg("printf 'hello-audit'");
        let out = run_capturing_with_timeout(cmd, Duration::from_secs(10)).unwrap();
        assert_eq!(out.as_deref(), Some("hello-audit"));
    }
}
