//! PR-ready review report — turn the evidence UmaDev already computed into the
//! single artifact a reviewer reads first.
//!
//! Most generated PRs hand a reviewer raw code and a one-line title. This module
//! flips that: it asserts, with a citation to a concrete file/number for each
//! claim, that the change is safe to merge — **CI was not weakened** (no test
//! files deleted, no `it.skip` / `#[ignore]` introduced in the diff), the
//! **API contract** holds (frontend↔backend alignment), **acceptance gaps** are
//! enumerated (planned endpoints with no implementation), the **governance +
//! security scans** verdicts, the **runtime evidence** (the app actually
//! booted + answered), and a **rollback** hint. The reviewer sees exactly what
//! was checked and what to look at by hand.
//!
//! Everything is **fail-open + deterministic + reuse**: no new model endpoint,
//! no new heavy deps. Each section reads an artifact UmaDev already produced
//! (`umadev-contract`, `acceptance`, `coverage`, the quality gate JSON, the
//! `security` scan, the `runtime_proof` JSON) and degrades to an honest "not
//! available" line rather than fabricating a pass. The git-diff CI check is the
//! one live probe; a missing/!git repo simply downgrades to "could not diff".

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::phases::QualityReport;
use crate::security::SecurityScan;

/// Review artifacts are project-controlled and consulted automatically. Two
/// MiB is generous for the structured reports while keeping every read hard
/// bounded before parsing.
const MAX_REVIEW_ARTIFACT_BYTES: usize = 2 * 1024 * 1024;
const REVIEW_GIT_TIMEOUT: Duration = Duration::from_secs(10);
const REVIEW_DIFF_BYTES: usize = 16 * 1024 * 1024;

/// One assertion in the review report: a claim, the verdict, and the concrete
/// evidence backing it. Kept as data (not just rendered text) so the renderer is
/// a pure function over it and the unit tests can assert on the structure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewClaim {
    /// Short title (e.g. "CI integrity").
    pub title: String,
    /// `pass` | `warn` | `fail` | `info` — drives the checkbox glyph.
    pub verdict: Verdict,
    /// The human-readable assertion, already carrying its evidence citation.
    pub detail: String,
}

/// Verdict class for a review claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Asserted and verified.
    Pass,
    /// Asserted with a caveat the reviewer should glance at.
    Warn,
    /// A real problem the reviewer must resolve before merge.
    Fail,
    /// Context, no judgement (e.g. rollback instructions).
    Info,
}

impl Verdict {
    /// Markdown checkbox / glyph for the claim line.
    fn glyph(self) -> &'static str {
        match self {
            Verdict::Pass => "[x]",
            Verdict::Warn => "[!]",
            Verdict::Fail => "[ ]",
            Verdict::Info => "[i]",
        }
    }
}

/// The assembled review report: an ordered list of claims plus the slug.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewReport {
    /// Project slug (filename stem).
    pub slug: String,
    /// Ordered claims, top to bottom.
    pub claims: Vec<ReviewClaim>,
}

impl ReviewReport {
    /// `true` iff no claim is a hard `Fail` — i.e. nothing blocks merge from
    /// UmaDev's deterministic checks (a human reviewer still has the final say).
    #[must_use]
    pub fn mergeable(&self) -> bool {
        !self.claims.iter().any(|c| c.verdict == Verdict::Fail)
    }
}

/// Workspace-relative path of the review report. The slug is sanitized
/// (using the runner's internal slug sanitizer) so a hostile/accidental slug
/// (`../x`, `/tmp/x`) can't move the report outside `output/`.
#[must_use]
pub fn review_report_rel_path(slug: &str) -> String {
    let slug = crate::runner::sanitize_slug(slug);
    format!("output/{slug}-review-report.md")
}

/// Build the review report by reading every artifact UmaDev already produced.
/// Pure assembly + a single git-diff probe — fail-open throughout: a missing
/// artifact yields an honest "not available" claim, never a panic.
#[must_use]
pub fn build_review_report(project_root: &Path, slug: &str) -> ReviewReport {
    // Sanitize once at the boundary so every artifact path derived below
    // (and the slug echoed into the report) stays inside `output/`.
    let slug = &crate::runner::sanitize_slug(slug);
    let claims = vec![
        ci_integrity_claim(project_root),
        contract_claim(project_root, slug),
        acceptance_claim(project_root, slug),
        coverage_claim(project_root, slug),
        quality_claim(project_root, slug),
        security_claim(project_root),
        runtime_claim(project_root),
        rollback_claim(project_root, slug),
    ];

    ReviewReport {
        slug: slug.clone(),
        claims,
    }
}

/// Build, render, and write the review report to `output/<slug>-review-report.md`.
/// Returns the written path.
pub fn write_review_report(project_root: &Path, slug: &str) -> std::io::Result<PathBuf> {
    let report = build_review_report(project_root, slug);
    let md = render_review_md(&report);
    let path = project_root.join(review_report_rel_path(slug));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, md)?;
    Ok(path)
}

// =====================================================================
// per-claim builders (each reuses an already-computed data source)
// =====================================================================

/// CI integrity — the headline reviewer fear: "did this change quietly weaken
/// the test suite?". We diff against the repo HEAD and assert no test file was
/// deleted and no test was newly skipped/ignored in the added lines. Fail-open:
/// not a git repo / no HEAD → `Info` ("could not diff"), never a false `Fail`.
fn ci_integrity_claim(project_root: &Path) -> ReviewClaim {
    let Some(diff) = git_diff(project_root) else {
        return ReviewClaim {
            title: "CI integrity".to_string(),
            verdict: Verdict::Info,
            detail: "Could not produce a git diff (not a repo, or no prior commit) — \
                     CI-weakening could not be checked automatically; review test changes by hand."
                .to_string(),
        };
    };
    let signals = scan_ci_weakening(&diff);
    if signals.is_empty() {
        ReviewClaim {
            title: "CI integrity".to_string(),
            verdict: Verdict::Pass,
            detail: "No test files deleted and no tests newly skipped/ignored in the diff \
                     (scanned the diff since the default-branch merge-base for removed test files + added skip markers)."
                .to_string(),
        }
    } else {
        ReviewClaim {
            title: "CI integrity".to_string(),
            verdict: Verdict::Fail,
            detail: format!(
                "CI may have been weakened ({} signal(s)): {}. Confirm these are intentional \
                 before merge.",
                signals.len(),
                signals
                    .iter()
                    .take(5)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        }
    }
}

/// Frontend↔backend API contract — reuse `umadev-contract` exactly as the
/// quality gate does: parse the architecture API table, extract the real
/// frontend calls, cross-validate.
fn contract_claim(project_root: &Path, slug: &str) -> ReviewClaim {
    let arch_path = project_root.join(format!("output/{slug}-architecture.md"));
    let arch = read(project_root, &arch_path);
    if arch.trim().is_empty() {
        return ReviewClaim {
            title: "API contract".to_string(),
            verdict: Verdict::Info,
            detail: "No architecture doc to derive a contract from (docs-only stage).".to_string(),
        };
    }
    let arch_spec = umadev_contract::parse_architecture(&arch, slug);
    let calls = umadev_contract::extract_frontend_calls(project_root);
    let violations = umadev_contract::validate_frontend_vs_contract(&calls, &arch_spec);
    if violations.is_empty() {
        ReviewClaim {
            title: "API contract".to_string(),
            verdict: Verdict::Pass,
            detail: format!(
                "All {} extracted frontend call(s) align with the {} endpoint(s) in \
                 `output/{slug}-architecture.md` (UD-CODE-003).",
                calls.len(),
                arch_spec.len()
            ),
        }
    } else {
        ReviewClaim {
            title: "API contract".to_string(),
            verdict: Verdict::Warn,
            detail: format!(
                "{} frontend↔contract mismatch(es): {}.",
                violations.len(),
                violations
                    .iter()
                    .take(4)
                    .map(|v| v.detail.clone())
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        }
    }
}

/// Acceptance gaps — reuse `acceptance::task_acceptance_gaps`: planned endpoints
/// with no implementation evidence in the workspace.
fn acceptance_claim(project_root: &Path, slug: &str) -> ReviewClaim {
    let gaps = crate::acceptance::task_acceptance_gaps(project_root, slug);
    if gaps.is_empty() {
        ReviewClaim {
            title: "Acceptance".to_string(),
            verdict: Verdict::Pass,
            detail: "Every endpoint planned in the architecture API table has implementation \
                     evidence in the source tree (no acceptance gaps)."
                .to_string(),
        }
    } else {
        ReviewClaim {
            title: "Acceptance".to_string(),
            verdict: Verdict::Warn,
            detail: format!(
                "{} planned endpoint(s) have NO implementation found: {}.",
                gaps.len(),
                gaps.iter().take(4).cloned().collect::<Vec<_>>().join("; ")
            ),
        }
    }
}

/// Requirement coverage — reuse `coverage::uncovered_requirements`: PRD `FR-NNN`
/// ids cited by no task/plan.
fn coverage_claim(project_root: &Path, slug: &str) -> ReviewClaim {
    let uncovered = crate::coverage::uncovered_requirements(project_root, slug);
    if uncovered.is_empty() {
        ReviewClaim {
            title: "Requirement coverage".to_string(),
            verdict: Verdict::Pass,
            detail: "Every PRD functional requirement (FR-NNN) is cited by the execution plan \
                     or a task (no orphaned requirements)."
                .to_string(),
        }
    } else {
        ReviewClaim {
            title: "Requirement coverage".to_string(),
            verdict: Verdict::Warn,
            detail: format!(
                "{} PRD requirement(s) cited by no task: {}.",
                uncovered.len(),
                uncovered.join(", ")
            ),
        }
    }
}

/// Quality gate + governance — reuse the persisted quality-gate JSON (which
/// already folds in the governance block-event counts and the design scans).
fn quality_claim(project_root: &Path, slug: &str) -> ReviewClaim {
    let Some(report) = read_quality(project_root, slug) else {
        return ReviewClaim {
            title: "Quality gate".to_string(),
            verdict: Verdict::Info,
            detail: "No quality gate report yet (runs at the `quality` phase).".to_string(),
        };
    };
    let failed: Vec<&str> = report
        .checks
        .iter()
        .filter(|c| c.status == "failed")
        .map(|c| c.name.as_str())
        .collect();
    if report.passed && failed.is_empty() {
        ReviewClaim {
            title: "Quality gate".to_string(),
            verdict: Verdict::Pass,
            detail: format!(
                "Quality gate PASSED at {}/100 across {} checks \
                 (see `output/{slug}-quality-gate.json`).",
                report.total_score,
                report.checks.len()
            ),
        }
    } else {
        ReviewClaim {
            title: "Quality gate".to_string(),
            verdict: if report.passed {
                Verdict::Warn
            } else {
                Verdict::Fail
            },
            detail: format!(
                "Quality gate at {}/100 ({}); failing check(s): {}.",
                report.total_score,
                if report.passed {
                    "passed with warnings"
                } else {
                    "BELOW threshold"
                },
                if failed.is_empty() {
                    "none (sub-threshold weighted score)".to_string()
                } else {
                    failed.join(", ")
                }
            ),
        }
    }
}

/// Security scan — reuse the persisted `.umadev/audit/security-scan.json`.
fn security_claim(project_root: &Path) -> ReviewClaim {
    let path = project_root.join(crate::security::security_scan_rel_path());
    let raw = read(project_root, &path);
    if raw.trim().is_empty() {
        // No file → the scan simply hasn't run yet.
        return ReviewClaim {
            title: "Security scan".to_string(),
            verdict: Verdict::Info,
            detail: "No security scan recorded yet (runs at the `delivery` phase).".to_string(),
        };
    }
    let Ok(scan) = serde_json::from_str::<SecurityScan>(&raw) else {
        // #14 — the file EXISTS but is corrupt / truncated. It used to collapse to the same
        // "no scan recorded yet" as an absent file, which HIDES a scan that may have found a
        // leaked secret. Surface it as a Warn so a human re-runs the scan before merge rather
        // than reading "nothing scanned".
        return ReviewClaim {
            title: "Security scan".to_string(),
            verdict: Verdict::Warn,
            detail: "A security-scan file exists but could not be parsed (corrupt or \
                     truncated) — re-run the security scan before merge so a real finding \
                     is not silently hidden."
                .to_string(),
        };
    };
    if !scan.any_ran() {
        return ReviewClaim {
            title: "Security scan".to_string(),
            verdict: Verdict::Info,
            detail: format!(
                "No security scanners available on this machine — {} \
                 (install gitleaks / npm-audit / cargo-audit / pip-audit to enable).",
                scan.summary_line()
            ),
        };
    }
    if scan.has_findings() {
        // A leaked SECRET is a hard block (Fail -> not mergeable); an advisory dependency
        // CVE is a Warn (a human reviewer decides). Without the Fail path a PR carrying a
        // LIVE leaked key from the owned SAST / gitleaks scan could still be reported
        // "ready to merge".
        let verdict = if scan.has_secret_findings() {
            Verdict::Fail
        } else {
            Verdict::Warn
        };
        ReviewClaim {
            title: "Security scan".to_string(),
            verdict,
            detail: format!(
                "{} (see `.umadev/audit/security-scan.json`).",
                scan.summary_line()
            ),
        }
    } else {
        ReviewClaim {
            title: "Security scan".to_string(),
            verdict: Verdict::Pass,
            detail: format!(
                "{} (see `.umadev/audit/security-scan.json`).",
                scan.summary_line()
            ),
        }
    }
}

/// Runtime evidence — reuse the persisted `.umadev/audit/runtime-proof.json`.
fn runtime_claim(project_root: &Path) -> ReviewClaim {
    let path = project_root.join(crate::runtime_proof::runtime_proof_rel_path());
    let Some(proof) = read(project_root, &path)
        .pipe_opt(|s| serde_json::from_str::<crate::runtime_proof::RuntimeProof>(s).ok())
    else {
        return ReviewClaim {
            title: "Runtime evidence".to_string(),
            verdict: Verdict::Info,
            detail: "No runtime proof recorded (`umadev verify --runtime` not run).".to_string(),
        };
    };
    if proof.status.is_verified() {
        let ok = proof.routes.iter().filter(|r| r.ok).count();
        ReviewClaim {
            title: "Runtime evidence".to_string(),
            verdict: Verdict::Pass,
            detail: format!(
                "App booted and answered: {} (route checks {}/{} OK; see \
                 `.umadev/audit/runtime-proof.json`).",
                proof.summary_line(),
                ok,
                proof.routes.len()
            ),
        }
    } else {
        ReviewClaim {
            title: "Runtime evidence".to_string(),
            verdict: Verdict::Info,
            detail: format!("Runtime not exercised: {}.", proof.summary_line()),
        }
    }
}

/// Rollback hint — always present (`Info`). Names the proof-pack + checkpoint
/// surfaces a reviewer would use to revert. We point at the shadow-checkpoint
/// repo when it exists, and always at plain `git revert`.
fn rollback_claim(project_root: &Path, slug: &str) -> ReviewClaim {
    let has_checkpoints = project_root.join(".umadev/checkpoints.git/HEAD").exists();
    let checkpoint_line = if has_checkpoints {
        " UmaDev checkpoints exist — `umadev rollback` rewinds files to a pre-phase snapshot."
    } else {
        ""
    };
    ReviewClaim {
        title: "Rollback".to_string(),
        verdict: Verdict::Info,
        detail: format!(
            "To revert: `git revert <merge-commit>` (or reset the feature branch).{checkpoint_line} \
             The full evidence bundle is in `release/proof-pack-{slug}-*.zip` for post-merge audit."
        ),
    }
}

// =====================================================================
// rendering
// =====================================================================

/// Render the report to PR-ready markdown. Pure function over [`ReviewReport`].
#[must_use]
pub fn render_review_md(report: &ReviewReport) -> String {
    let mut out = String::new();
    out.push_str(&format!("# Review report — {}\n\n", report.slug));
    let verdict = if report.mergeable() {
        "No blocking issues from automated checks — ready for human review."
    } else {
        "Blocking issue(s) detected — resolve the failing claim(s) before merge."
    };
    out.push_str(&format!("> {verdict}\n\n"));
    out.push_str(
        "Generated by UmaDev from the run's own evidence. Each claim cites the file or number \
         it is derived from; nothing here is asserted without a source.\n\n",
    );
    out.push_str("## Reviewer checklist\n\n");
    for c in &report.claims {
        out.push_str(&format!(
            "- {} **{}** — {}\n",
            c.verdict.glyph(),
            c.title,
            c.detail
        ));
    }
    out.push_str(
        "\n## Legend\n\n\
         - `[x]` verified · `[!]` verified with a caveat · `[ ]` blocking, must resolve · \
         `[i]` context / not applicable\n",
    );
    out
}

// =====================================================================
// helpers
// =====================================================================

fn read(project_root: &Path, path: &Path) -> String {
    crate::bounded_fs::read_utf8_beneath(project_root, path, MAX_REVIEW_ARTIFACT_BYTES)
        .unwrap_or_default()
}

/// Tiny `Option`-combinator so the claim builders read top-down. Returns `None`
/// when the source string is empty (no artifact) OR the mapper returns `None`.
trait PipeOpt {
    fn pipe_opt<T>(self, f: impl FnOnce(&str) -> Option<T>) -> Option<T>;
}
impl PipeOpt for String {
    fn pipe_opt<T>(self, f: impl FnOnce(&str) -> Option<T>) -> Option<T> {
        if self.trim().is_empty() {
            None
        } else {
            f(&self)
        }
    }
}

fn read_quality(project_root: &Path, slug: &str) -> Option<QualityReport> {
    let path = project_root.join(format!("output/{slug}-quality-gate.json"));
    let body = read(project_root, &path);
    if body.trim().is_empty() {
        return None;
    }
    serde_json::from_str(&body).ok()
}

/// `git diff HEAD` (with rename detection) over the workspace, or `None` when
/// it's not a usable git repo. Fail-open: any spawn error / non-zero exit → None.
fn git_diff(project_root: &Path) -> Option<String> {
    // Prefer diffing against the MERGE-BASE with the default branch: a PR whose changes are
    // already COMMITTED to the feature branch produces an EMPTY `git diff HEAD`, so the
    // CI-weakening scan would PASS having inspected NOTHING (R-H1). `git diff <merge-base>`
    // covers everything since the branch diverged - committed AND uncommitted. Fall back to
    // `git diff HEAD` (uncommitted only) when no default branch / merge-base is resolvable.
    if let Some(base) = merge_base_with_default(project_root) {
        if let Some(d) = run_git_diff(project_root, &base) {
            return Some(d);
        }
    }
    run_git_diff(project_root, "HEAD")
}

/// `git diff --find-renames <against>` in `project_root`; `None` on spawn failure or a
/// non-zero git exit.
fn run_git_diff(project_root: &Path, against: &str) -> Option<String> {
    let out = crate::external_command::bounded_git_output(
        project_root,
        &["diff", "--no-ext-diff", "--find-renames", against],
        REVIEW_GIT_TIMEOUT,
        REVIEW_DIFF_BYTES,
    )?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

/// Resolve the merge-base of HEAD with the repo default branch (tries the common
/// remotes/locals in order). `None` when git is absent or none exist (a brand-new repo with
/// no default branch), so `git_diff` falls back to the working-tree diff.
fn merge_base_with_default(project_root: &Path) -> Option<String> {
    for base in ["origin/main", "origin/master", "main", "master"] {
        let out = crate::external_command::bounded_git_output(
            project_root,
            &["merge-base", "HEAD", base],
            REVIEW_GIT_TIMEOUT,
            8 * 1024,
        )?;
        if out.status.success() {
            let sha = String::from_utf8(out.stdout).ok()?.trim().to_string();
            if matches!(sha.len(), 40 | 64) && sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Some(sha);
            }
        }
    }
    None
}

/// Whether hay contains needle with a WORD BOUNDARY before it: the char immediately
/// preceding the match must not be a word char (letter/digit/underscore). Stops
/// process.exit( / std::process::exit( from matching the xit( skip marker, and
/// edit.skip / latest.skip from matching it.skip - a false "CI weakened" that blocked
/// legitimate PRs.
fn contains_at_word_boundary(hay: &str, needle: &str) -> bool {
    let mut from = 0;
    while let Some(rel) = hay[from..].find(needle) {
        let at = from + rel;
        let boundary = match hay[..at].chars().next_back() {
            Some(ch) => !ch.is_alphanumeric() && ch != '_',
            None => true,
        };
        if boundary {
            return true;
        }
        from = at + needle.len();
    }
    false
}

/// Whether a repo-relative path is a CI / workflow / shell / build file - where an
/// ambiguous disable form like `|| true` or `if: false` genuinely weakens the build, as
/// opposed to being ordinary source (`const x = a || true`).
#[allow(clippy::case_sensitive_file_extension_comparisons)] // `path` is lower-cased first
fn is_ci_or_build_file(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    p.contains(".github/workflows/")
        || p.contains(".gitlab-ci")
        || p.contains(".circleci")
        || p.contains("azure-pipelines")
        || p.contains("jenkinsfile")
        || p.ends_with(".yml")
        || p.ends_with(".yaml")
        || p.ends_with(".sh")
        || p.ends_with(".bash")
        || p.ends_with("makefile")
        || p.contains("/makefile")
}

/// Scan a unified diff for signals that CI/test coverage was weakened:
/// 1. a test FILE was deleted (`deleted file ... <test-path>`), or
/// 2. a test was newly skipped/ignored in the ADDED lines.
///
/// Returns a list of human descriptions (empty == clean). Pure + testable.
#[must_use]
pub fn scan_ci_weakening(diff: &str) -> Vec<String> {
    let mut signals = Vec::new();
    let mut cur_file = String::new();
    let mut pending_delete = false;

    // Markers that, when ADDED (a `+` line), disable a test.
    const SKIP_MARKERS: &[&str] = &[
        "it.skip",
        "describe.skip",
        "test.skip",
        "xit(",
        "xdescribe(",
        "#[ignore]",
        "#[ignore ",
        "@pytest.mark.skip",
        "@unittest.skip",
        "@Disabled",
        "@Ignore",
        "t.Skip(",
    ];
    // CI-DISABLING directives (distinct from per-test skips): an ADDED line that makes a
    // failing step stop failing the build. UNAMBIGUOUS phrases - never appear in ordinary
    // source - matched anywhere:
    const CI_DISABLE_MARKERS_ALWAYS: &[&str] = &[
        "continue-on-error: true",
        "--passwithnotests",
        "--no-verify",
        "fail_ci_if_error: false",
    ];
    // AMBIGUOUS forms that ALSO occur in normal code (const x = a || true; a schema field
    // visible_if: false) - only flag these in a CI / workflow / shell / build file, else the
    // CI-integrity claim false-Fails a change that never touched CI (M2 regression).
    const CI_DISABLE_MARKERS_FILE_SCOPED: &[&str] = &["|| true", "|| exit 0", "if: false"];

    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            // `a/path b/path` — take the b-side path.
            cur_file = rest.split(" b/").nth(1).unwrap_or("").trim().to_string();
            pending_delete = false;
            continue;
        }
        if line.starts_with("deleted file mode") {
            pending_delete = true;
            continue;
        }
        if pending_delete && is_test_path(&cur_file) {
            signals.push(format!("deleted test file `{cur_file}`"));
            pending_delete = false;
            continue;
        }
        // Added line introducing a skip marker (exclude the `+++` header).
        if line.starts_with('+') && !line.starts_with("+++") {
            let added = &line[1..];
            for m in SKIP_MARKERS {
                if contains_at_word_boundary(added, m) {
                    signals.push(format!(
                        "added skip/ignore (`{}`) in `{}`",
                        m.trim_end_matches(['(', ' ']),
                        if cur_file.is_empty() {
                            "<file>"
                        } else {
                            &cur_file
                        }
                    ));
                    break;
                }
            }
            let added_lc = added.to_ascii_lowercase();
            let scoped: &[&str] = if is_ci_or_build_file(&cur_file) {
                CI_DISABLE_MARKERS_FILE_SCOPED
            } else {
                &[]
            };
            for m in CI_DISABLE_MARKERS_ALWAYS.iter().chain(scoped.iter()) {
                if added_lc.contains(m) {
                    signals.push(format!(
                        "added CI-weakening directive (`{}`) in `{}`",
                        m.trim(),
                        if cur_file.is_empty() {
                            "<file>"
                        } else {
                            &cur_file
                        }
                    ));
                    break;
                }
            }
        }
    }
    signals
}

/// Heuristic: does this path look like a test file? (Matches the common
/// conventions across the stacks UmaDev targets.)
fn is_test_path(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    p.contains("/tests/")
        || p.contains("/test/")
        || p.contains("__tests__")
        || p.ends_with("_test.go")
        || p.ends_with("_test.py")
        || p.ends_with("test.ts")
        || p.ends_with("test.tsx")
        || p.ends_with("test.js")
        || p.ends_with("test.jsx")
        || p.ends_with(".test.ts")
        || p.ends_with(".test.tsx")
        || p.ends_with(".test.js")
        || p.ends_with(".spec.ts")
        || p.ends_with(".spec.tsx")
        || p.ends_with(".spec.js")
        || p.ends_with("_spec.rb")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn review_report_path_is_sanitized_against_traversal() {
        // Normal slug lands where it always did.
        assert_eq!(
            review_report_rel_path("my-app"),
            "output/my-app-review-report.md"
        );
        // Hostile slugs can't escape output/ or become absolute.
        for hostile in ["../etc/passwd", "/tmp/x", "..\\..\\x"] {
            let rel = review_report_rel_path(hostile);
            assert!(rel.starts_with("output/"), "{hostile:?} -> {rel:?}");
            assert!(!rel.contains(".."), "{hostile:?} -> {rel:?}");
            assert!(
                !std::path::Path::new(&rel).is_absolute(),
                "{hostile:?} -> absolute {rel:?}"
            );
        }
    }

    #[test]
    fn review_artifact_read_is_hard_bounded() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("artifact.json");
        fs::write(&path, vec![b'x'; MAX_REVIEW_ARTIFACT_BYTES + 1]).unwrap();
        assert!(read(tmp.path(), &path).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn review_artifact_read_does_not_follow_symlinks() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        fs::write(outside.path(), "not review evidence").unwrap();
        let link = tmp.path().join("artifact.json");
        symlink(outside.path(), &link).unwrap();
        assert!(read(tmp.path(), &link).is_empty());
    }

    #[test]
    fn clean_diff_has_no_signals() {
        let diff = "diff --git a/src/lib.rs b/src/lib.rs\n\
                    @@ -1 +1,2 @@\n\
                    +pub fn add(a: i32, b: i32) -> i32 { a + b }\n";
        assert!(scan_ci_weakening(diff).is_empty());
    }

    #[test]
    fn detects_deleted_test_file() {
        let diff = "diff --git a/src/api.test.ts b/src/api.test.ts\n\
                    deleted file mode 100644\n\
                    index abc..000\n\
                    --- a/src/api.test.ts\n\
                    +++ /dev/null\n";
        let s = scan_ci_weakening(diff);
        assert_eq!(s.len(), 1);
        assert!(s[0].contains("deleted test file"));
        assert!(s[0].contains("api.test.ts"));
    }

    #[test]
    fn detects_added_skip_markers() {
        let diff = "diff --git a/tests/login_test.py b/tests/login_test.py\n\
                    @@ -1,3 +1,4 @@\n\
                    +@pytest.mark.skip\n\
                    +def test_login(): ...\n";
        let s = scan_ci_weakening(diff);
        assert!(s.iter().any(|x| x.contains("skip/ignore")));
    }

    #[test]
    fn exit_call_is_not_a_false_ci_weakening_but_real_skip_markers_are() {
        // R-H2: process.exit( / std::process::exit( must NOT match the xit( skip marker
        // (word-boundary guard), else a legit PR was blocked as CI weakened.
        let diff = "diff --git a/src/x.rs b/src/x.rs\n+    std::process::exit(1);\n";
        assert!(
            scan_ci_weakening(diff).is_empty(),
            "process::exit must not be flagged as a skip marker"
        );
        let real = "diff --git a/a.test.js b/a.test.js\n+  xit('skips this', () => {});\n";
        assert!(!scan_ci_weakening(real).is_empty(), "real xit( is a skip");
        let edit = "diff --git a/a.js b/a.js\n+  const s = latest.skip;\n";
        assert!(
            scan_ci_weakening(edit).is_empty(),
            "latest.skip is not it.skip"
        );
    }

    #[test]
    fn ci_disabling_directives_are_flagged() {
        // DB-M1: forms that make a failing step stop failing the build must be flagged.
        let gha = "diff --git a/ci.yml b/ci.yml\n+    continue-on-error: true\n";
        assert!(
            !scan_ci_weakening(gha).is_empty(),
            "continue-on-error weakens CI"
        );
        let shell = "diff --git a/Makefile b/Makefile\n+\tnpm test || true\n";
        assert!(
            !scan_ci_weakening(shell).is_empty(),
            "|| true swallows the exit"
        );
        let jest = "diff --git a/pkg.json b/pkg.json\n+  jest --passWithNoTests\n";
        assert!(
            !scan_ci_weakening(jest).is_empty(),
            "--passWithNoTests weakens CI"
        );
        let ok = "diff --git a/x.rs b/x.rs\n+    let ok = true;\n";
        assert!(
            scan_ci_weakening(ok).is_empty(),
            "a normal line is not weakening"
        );
        // M2 regression: || true / if: false in ORDINARY source (not a CI/build file) must
        // NOT be flagged - they occur in real code.
        let src = "diff --git a/app.ts b/app.ts\n+  const enabled = isDev || true;\n";
        assert!(
            scan_ci_weakening(src).is_empty(),
            "|| true in .ts source is not weakening"
        );
        let schema = "diff --git a/schema.json b/schema.json\n+  \"visible_if\": false\n";
        assert!(
            scan_ci_weakening(schema).is_empty(),
            "visible_if: false in json is fine"
        );
        let wf = "diff --git a/.github/workflows/ci.yml b/.github/workflows/ci.yml\n+    run: npm test || true\n";
        assert!(
            !scan_ci_weakening(wf).is_empty(),
            "|| true in a workflow IS weakening"
        );
    }

    #[test]
    fn detects_rust_ignore_in_added_lines_only() {
        // An `#[ignore]` that already existed (context line, no `+`) is fine;
        // only a freshly ADDED one counts.
        let added = "diff --git a/src/lib.rs b/src/lib.rs\n+#[ignore]\n+fn t() {}\n";
        assert_eq!(scan_ci_weakening(added).len(), 1);
        let context = "diff --git a/src/lib.rs b/src/lib.rs\n #[ignore]\n fn t() {}\n";
        assert!(scan_ci_weakening(context).is_empty());
    }

    #[test]
    fn deleting_a_non_test_file_is_not_a_signal() {
        let diff = "diff --git a/README.md b/README.md\n\
                    deleted file mode 100644\n";
        assert!(scan_ci_weakening(diff).is_empty());
    }

    #[test]
    fn is_test_path_matches_conventions() {
        assert!(is_test_path("src/foo.test.ts"));
        assert!(is_test_path("spec/models/user_spec.rb"));
        assert!(is_test_path("pkg/handler_test.go"));
        assert!(is_test_path("app/__tests__/Button.jsx"));
        assert!(!is_test_path("src/main.rs"));
        assert!(!is_test_path("docs/readme.md"));
    }

    #[test]
    fn render_is_pure_and_lists_every_claim() {
        let report = ReviewReport {
            slug: "demo".to_string(),
            claims: vec![
                ReviewClaim {
                    title: "CI integrity".to_string(),
                    verdict: Verdict::Pass,
                    detail: "no weakening".to_string(),
                },
                ReviewClaim {
                    title: "Acceptance".to_string(),
                    verdict: Verdict::Fail,
                    detail: "1 gap".to_string(),
                },
            ],
        };
        assert!(!report.mergeable());
        let md = render_review_md(&report);
        assert!(md.contains("# Review report — demo"));
        assert!(md.contains("CI integrity"));
        assert!(md.contains("Acceptance"));
        assert!(md.contains("[x]"));
        assert!(md.contains("[ ]")); // the failing claim
        assert!(md.contains("Blocking issue"));
    }

    #[test]
    fn build_on_bare_workspace_is_fail_open() {
        // No artifacts at all → every data-driven claim degrades to Info/Pass,
        // nothing panics, and the report is renderable + writable.
        let tmp = TempDir::new().unwrap();
        let report = build_review_report(tmp.path(), "demo");
        assert_eq!(report.claims.len(), 8);
        // Rollback claim is always present.
        assert!(report.claims.iter().any(|c| c.title == "Rollback"));
        let path = write_review_report(tmp.path(), "demo").unwrap();
        assert!(path.exists());
        assert!(fs::read_to_string(&path).unwrap().contains("Review report"));
    }

    #[test]
    fn acceptance_gap_surfaces_as_warn_claim() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("output")).unwrap();
        fs::write(
            tmp.path().join("output/demo-architecture.md"),
            "# API\n\n\
             | Method | Path | Description | Auth |\n\
             |---|---|---|---|\n\
             | GET | /api/widgets | list widgets | none |\n",
        )
        .unwrap();
        let report = build_review_report(tmp.path(), "demo");
        let accept = report
            .claims
            .iter()
            .find(|c| c.title == "Acceptance")
            .unwrap();
        // No source implements /api/widgets → a gap → Warn.
        assert_eq!(accept.verdict, Verdict::Warn);
        assert!(accept.detail.contains("/api/widgets"));
    }

    #[test]
    fn corrupt_security_scan_warns_instead_of_reading_as_no_scan() {
        // #14 — a corrupt / truncated security-scan.json must NOT collapse to the same
        // "no scan recorded yet" as an absent file (that hides a scan that may have found a
        // leaked secret); it Warns so a human re-runs it. An ABSENT file still reads Info.
        let tmp = TempDir::new().unwrap();
        let scan_path = tmp.path().join(crate::security::security_scan_rel_path());
        fs::create_dir_all(scan_path.parent().unwrap()).unwrap();
        fs::write(&scan_path, "{ not valid json ]").unwrap();
        let corrupt = security_claim(tmp.path());
        assert_eq!(corrupt.verdict, Verdict::Warn, "corrupt scan must warn");
        assert!(corrupt.detail.contains("could not be parsed"));

        let absent = security_claim(TempDir::new().unwrap().path());
        assert_eq!(
            absent.verdict,
            Verdict::Info,
            "absent scan stays neutral Info"
        );
    }
}
