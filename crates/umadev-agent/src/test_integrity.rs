//! Test-integrity guard — a deterministic, fail-open anti-reward-hacking floor
//! over the **test files** the team edits during a build step (UD-QA-001).
//!
//! A borrowed brain can make a failing suite "pass" without delivering working
//! code by **gaming the tests** instead of fixing the implementation: deleting a
//! test file, removing a test function, stripping assertions out of a kept test,
//! marking a test `skip` / `xfail` / `.only` / `#[ignore]`, commenting the
//! checks out, hard-coding the implementation's exact output as the expected
//! value, or weakening the test runner / test command itself. Every one of those
//! moves makes `npm test` / `pytest` / `cargo test` report green — so a check
//! that only reads "did the suite pass?" is fooled.
//!
//! UmaDev owns a **deterministic floor the borrowed brain cannot edit**. This
//! module makes that floor *enforce test integrity*: it snapshots the project's
//! test files BEFORE a build step's doer turn and compares them to the AFTER
//! state, flagging the gaming signals above. A violation means the step's passing
//! test signal is **not trusted** — the finding is folded into the step's
//! deterministic acceptance as a blocking signal (see
//! [`crate::director_loop::drive_build_step`]) and drives a bounded rework round
//! with a typed, evidence-bearing directive that names the gamed file. A
//! genuinely-passing, un-gamed suite produces no findings and is unaffected.
//!
//! **Fail-open by contract.** If integrity cannot be determined — no baseline
//! snapshot, an unreadable tree, an unparseable file — the guard returns *no
//! findings* (it never fabricates a block). Adding new tests is never a
//! violation; only the destruction / weakening of pre-existing test signal is.
//! Every heuristic is comparative (before vs after) and the rework it triggers is
//! bounded by the caller's existing fix-round / stall counters, so a heuristic
//! false-positive can never become an infinite rework loop.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::fswalk::{classify_no_follow, EntryKind};

/// Directories never worth scanning — build output / vendored deps / VCS /
/// UmaDev's own artifact dirs. Mirrors the acceptance/checkpoint skip sets so the
/// guard never walks `node_modules` or a base's `output/` doc blackboard.
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

/// Code extensions a test file can carry. Used to decide which files even get
/// classified as a possible test (harness configs are matched separately by
/// exact name, since they carry non-code extensions like `.ini` / `.xml`).
const CODE_EXT: &[&str] = &[
    "ts", "tsx", "js", "jsx", "mjs", "cjs", "vue", "svelte", "py", "rs", "go", "java", "rb", "php",
    "cs", "kt", "kts", "ex", "exs", "dart", "swift", "scala", "groovy",
];

/// Exact filenames (case-insensitive) of dedicated test-runner / harness config.
/// An EDIT or DELETE of one of these during a build step is a gaming signal (the
/// runner is being weakened to pass); a fresh ADD is legitimate test setup and is
/// NOT flagged. Deliberately narrow — multi-purpose files (`pyproject.toml`,
/// `Cargo.toml`, `vite.config.*`) are excluded so an unrelated edit never trips
/// the guard.
const HARNESS_FILES: &[&str] = &[
    "jest.config.js",
    "jest.config.ts",
    "jest.config.mjs",
    "jest.config.cjs",
    "jest.config.json",
    "jest.setup.js",
    "jest.setup.ts",
    "vitest.config.js",
    "vitest.config.ts",
    "vitest.config.mjs",
    "vitest.setup.js",
    "vitest.setup.ts",
    ".mocharc.json",
    ".mocharc.js",
    ".mocharc.cjs",
    ".mocharc.yml",
    ".mocharc.yaml",
    "pytest.ini",
    "tox.ini",
    "conftest.py",
    "phpunit.xml",
    "phpunit.xml.dist",
    "karma.conf.js",
    "playwright.config.js",
    "playwright.config.ts",
    "cypress.config.js",
    "cypress.config.ts",
    "jasmine.json",
    ".nycrc",
    ".nycrc.json",
];

/// Per-file test metrics captured in a [`TestSnapshot`] — the comparable surface
/// the before/after diff reasons over.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct FileMetrics {
    /// Number of assertion calls (`assert` / `expect` / `.should` …).
    assertions: usize,
    /// Number of TRIVIALLY-TRUE assertions whose subject is a constant truth
    /// (`expect(true)` / `assert(true)` / `assertTrue(true)` / `XCTAssertTrue(true)`
    /// / Python `assert True` …). H2: assertion COUNTS alone can't catch a body
    /// rewritten in place (`expect(add(1,2)).toEqual(3)` → `expect(true).toBe(true)`)
    /// — the count is unchanged — so a RISE in this signal flags the neutering.
    trivial_asserts: usize,
    /// Number of test declarations (`it(` / `test(` / `def test_` / `#[test]` …).
    test_fns: usize,
    /// Number of skip / xfail / focus markers (`skip` / `xfail` / `.only` /
    /// `#[ignore]` …) — a test that is present but not actually run.
    skips: usize,
    /// Number of commented-out test / assertion lines.
    commented: usize,
    /// Distinctive quoted literals on assertion lines (≥ 12 chars), capped — the
    /// best-effort "hard-coded the impl's output into the test" needle.
    literals: BTreeSet<String>,
}

/// A point-in-time snapshot of the project's TEST surface — every test file's
/// [`FileMetrics`], every harness-config file's content hash, and the
/// `package.json` test command. Captured once before a build step's doer turn and
/// compared to the after-state by [`check`].
///
/// Self-contained and fail-open: an unreadable tree yields an empty snapshot
/// (against which nothing can be flagged as deleted/weakened — only additions,
/// which are never flagged).
#[derive(Debug, Clone, Default)]
pub struct TestSnapshot {
    /// Workspace-relative path → metrics, for each identified test file.
    tests: BTreeMap<String, FileMetrics>,
    /// Workspace-relative path → content hash, for each harness-config file.
    harness: BTreeMap<String, u64>,
    /// The `scripts.test` value from `package.json`, if present.
    test_command: Option<String>,
}

impl TestSnapshot {
    /// `true` when the snapshot observed no tests, no harness, and no test
    /// command — i.e. there was no test surface to protect yet.
    #[must_use]
    fn is_empty(&self) -> bool {
        self.tests.is_empty() && self.harness.is_empty() && self.test_command.is_none()
    }
}

/// Capture the project's test surface into a [`TestSnapshot`]. Bounded and
/// fail-open: skips heavy/vendor dirs, caps the files it reads, and treats any IO
/// error as "absent" rather than erroring. Call this immediately BEFORE a build
/// step's doer turn; pass the result to [`check`] after the turn.
#[must_use]
pub fn snapshot(project_root: &Path) -> TestSnapshot {
    let mut snap = TestSnapshot::default();
    walk(project_root, project_root, &mut snap, 0);
    snap.test_command = read_test_command(project_root);
    snap
}

/// Compare the project's CURRENT test surface against a `before` snapshot and
/// return blocking findings for any test-gaming signal detected across the step's
/// doer turn. Empty result = clean OR nothing could be determined (fail-open).
///
/// `before == None` (no baseline was captured) is the explicit fail-open path: it
/// returns no findings, never a spurious block. Each finding is a typed,
/// evidence-bearing line that NAMES the gamed file, suitable for folding into a
/// bounded rework directive.
#[must_use]
pub fn check(project_root: &Path, before: Option<&TestSnapshot>) -> Vec<String> {
    let Some(before) = before else {
        return Vec::new(); // no baseline → cannot determine integrity → fail-open
    };
    if before.is_empty() {
        // Nothing existed to protect before this step; only additions are
        // possible, and additions are never a violation. Fail-open.
        return Vec::new();
    }
    let after = snapshot(project_root);
    let mut out = Vec::new();

    // --- Test files: deletions, removed test functions, weakened assertions ---
    for (path, before_m) in &before.tests {
        match after.tests.get(path) {
            None => out.push(format!(
                "test-integrity: test file deleted during this build step — {path} (a passing \
                 suite must keep its tests; restore the file or remove it from scope honestly, \
                 don't delete tests to make the build pass)"
            )),
            Some(after_m) => {
                if after_m.test_fns < before_m.test_fns {
                    out.push(format!(
                        "test-integrity: {path} lost {n} test function(s) this step ({b}->{a}) — \
                         restore the removed test(s) or justify the removal; a build step must not \
                         delete test cases to pass",
                        n = before_m.test_fns - after_m.test_fns,
                        b = before_m.test_fns,
                        a = after_m.test_fns,
                    ));
                }
                // L5: an INDEPENDENT check (was an `else if` after the test-fn drop) — a
                // step that BOTH removes a test function AND strips assertions out of a
                // KEPT test would otherwise report only the function loss, hiding the
                // stripping. Both findings fold into one rework directive; over-reporting
                // a genuine reduction of test signal is safe (it only fires when the
                // before>after counts prove signal was lost).
                if after_m.assertions < before_m.assertions {
                    out.push(format!(
                        "test-integrity: {path} lost {n} assertion(s) this step ({b}->{a}) — \
                         assertions were weakened/stripped; restore the checks (or, if a test was \
                         legitimately removed, the removal must be justified) instead of deleting them",
                        n = before_m.assertions - after_m.assertions,
                        b = before_m.assertions,
                        a = after_m.assertions,
                    ));
                }
                // H2: assertion COUNTS can stay identical while bodies are rewritten in
                // place to trivially-true (`expect(add(1,2)).toEqual(3)` →
                // `expect(true).toBe(true)`). A RISE in the trivially-true signal is the
                // tell the count check misses — the exact `assert(true)` gaming this
                // module exists to stop.
                if after_m.trivial_asserts > before_m.trivial_asserts {
                    out.push(format!(
                        "test-integrity: {path} added {n} trivially-true assertion(s) this step \
                         ({b}->{a}) — expect(true)/assert(true)/assertTrue(true) always pass; assert \
                         the real behavior/contract instead of neutering the checks to force a green",
                        n = after_m.trivial_asserts - before_m.trivial_asserts,
                        b = before_m.trivial_asserts,
                        a = after_m.trivial_asserts,
                    ));
                }
                if after_m.skips > before_m.skips {
                    out.push(format!(
                        "test-integrity: {path} added a skip/xfail/ignore/only marker this step \
                         ({b}->{a}) — un-skip the test and make it pass for real instead of \
                         disabling it",
                        b = before_m.skips,
                        a = after_m.skips,
                    ));
                }
                if after_m.commented > before_m.commented {
                    out.push(format!(
                        "test-integrity: {path} commented out test/assertion code this step \
                         ({b}->{a}) — uncomment the checks and make them pass for real",
                        b = before_m.commented,
                        a = after_m.commented,
                    ));
                }
            }
        }
    }

    // --- New skip markers / commented tests in a test file that already existed
    //     but had none before (covered above only when it stayed present; this
    //     also catches a file that gained skips while losing nothing else). The
    //     loop above handles existing-path cases; nothing extra needed here. ---

    // --- Hard-coded literal matching the implementation output (best-effort) ---
    out.extend(hardcoded_literal_findings(project_root, before, &after));

    // --- Harness / runner config edited or deleted during a build step ---
    for (path, before_hash) in &before.harness {
        match after.harness.get(path) {
            None => out.push(format!(
                "test-integrity: test harness/runner config deleted during this build step — \
                 {path} (do not remove the test runner config to pass; restore it)"
            )),
            Some(after_hash) if after_hash != before_hash => out.push(format!(
                "test-integrity: test harness/runner config modified during this build step — \
                 {path} (do not weaken the test runner to pass; revert the harness change and fix \
                 the code instead)"
            )),
            Some(_) => {}
        }
    }

    // --- The test command itself (package.json scripts.test) was changed ---
    if let Some(before_cmd) = &before.test_command {
        match &after.test_command {
            None => out.push(
                "test-integrity: the project's test command (package.json scripts.test) was \
                 removed during this build step — restore it; the suite cannot be trusted if the \
                 command that runs it was deleted"
                    .to_string(),
            ),
            Some(after_cmd) if after_cmd != before_cmd => out.push(format!(
                "test-integrity: the test command was changed during this build step \
                 (scripts.test: {before_cmd:?} -> {after_cmd:?}) — do not weaken the test command \
                 to force a green; revert it and fix the code"
            )),
            Some(_) => {}
        }
    }

    out
}

/// Best-effort: a test file that, this step, started asserting a distinctive
/// literal which appears VERBATIM in the (non-test) implementation source — the
/// classic "bake the impl's exact output into the expected value so the test
/// trivially passes" move. Conservative on purpose: only NEW literals (absent
/// from the before snapshot), only ≥ 12 chars, only when found in impl source,
/// and at most one report per file — so a legitimately-shared constant rarely
/// trips it, and the bound caps any residual noise.
fn hardcoded_literal_findings(
    project_root: &Path,
    before: &TestSnapshot,
    after: &TestSnapshot,
) -> Vec<String> {
    // Collect the NEW literals across all test files first; only read the impl
    // surface if there is at least one candidate (keeps the common path cheap).
    let empty_lits: BTreeSet<String> = BTreeSet::new();
    let mut candidates: Vec<(&String, &String)> = Vec::new();
    for (path, after_m) in &after.tests {
        let before_lits = before.tests.get(path).map_or(&empty_lits, |m| &m.literals);
        for lit in &after_m.literals {
            if !before_lits.contains(lit) {
                candidates.push((path, lit));
            }
        }
    }
    if candidates.is_empty() {
        return Vec::new();
    }
    let impl_src = impl_surface(project_root);
    if impl_src.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut reported: BTreeSet<&String> = BTreeSet::new();
    for (path, lit) in candidates {
        if reported.contains(path) {
            continue; // at most one report per file (conservative)
        }
        if impl_src.contains(lit.as_str()) {
            reported.insert(path);
            let shown = truncate_literal(lit);
            out.push(format!(
                "test-integrity: {path} now asserts a hard-coded literal that matches the \
                 implementation's own output ({shown:?}) — assert the behavior/contract, not the \
                 impl's exact output baked in to force a green"
            ));
        }
    }
    out
}

/// Truncate a literal for display in a finding (keep it short; literals can be
/// long). Operates on chars so it never splits a multibyte boundary.
fn truncate_literal(lit: &str) -> String {
    const MAX: usize = 48;
    if lit.chars().count() <= MAX {
        return lit.to_string();
    }
    let head: String = lit.chars().take(MAX).collect();
    format!("{head}…")
}

/// Bounded recursive walk: classify each file as a harness config or a test file
/// and record it into `snap`. Mirrors the depth/skip bounds of the acceptance
/// scan. Fail-open: an unreadable dir is skipped silently.
fn walk(root: &Path, dir: &Path, snap: &mut TestSnapshot, depth: usize) {
    if depth > 8 || snap.tests.len() + snap.harness.len() > 800 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        // No-follow: a symlinked dir/file is skipped so the test snapshot never
        // walks OUT of the workspace or loops through a symlink cycle. A real
        // file falls through to the classification below.
        match classify_no_follow(&p) {
            EntryKind::Dir => {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name.starts_with('.') || SKIP_DIRS.contains(&name) {
                    continue;
                }
                walk(root, &p, snap, depth + 1);
                continue;
            }
            EntryKind::Skip => continue,
            EntryKind::File => {}
        }
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let name_lower = name.to_ascii_lowercase();
        let rel = p
            .strip_prefix(root)
            .unwrap_or(&p)
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        let rel_lower = rel.to_ascii_lowercase();

        // Harness config (matched by exact name) — record a content hash.
        if HARNESS_FILES.contains(&name_lower.as_str()) {
            if let Ok(content) = read_capped(&p) {
                snap.harness.insert(rel, hash_str(&content));
            }
            continue;
        }
        // Test file? Code ext + path/name heuristic, or a Rust file with inline
        // `#[test]`. Read content once and reuse for the inline-test check + the
        // metrics, so we never read a file twice.
        let ext = p
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !CODE_EXT.contains(&ext.as_str()) {
            continue;
        }
        let Ok(content) = read_capped(&p) else {
            continue;
        };
        if is_test_file(&rel_lower, &name_lower, &ext, &content) {
            snap.tests.insert(rel, file_metrics(&content));
        }
    }
}

/// Read a file, capped at 1 MiB so a pathological file can't blow the budget.
fn read_capped(path: &Path) -> std::io::Result<String> {
    let bytes = std::fs::read(path)?;
    let capped = if bytes.len() > 1_048_576 {
        &bytes[..1_048_576]
    } else {
        &bytes[..]
    };
    Ok(String::from_utf8_lossy(capped).into_owned())
}

/// Concatenate the project's NON-test implementation source (bounded ~1.5 MB) —
/// the surface the hard-coded-literal heuristic searches for the impl's own
/// output. Reuses the shared bounded [`crate::acceptance::source_files`] collector
/// and filters out the test files.
fn impl_surface(project_root: &Path) -> String {
    let mut buf = String::new();
    for f in crate::acceptance::source_files(project_root) {
        let name_lower = f
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        // M4: classify on the path RELATIVE to the project root — the same as `walk()`
        // does. Using the ABSOLUTE path made `is_test_file`'s `by_dir` heuristic match a
        // `/test/`, `/tests/`, or `/spec/` segment in the project ROOT itself (e.g.
        // `/builds/test/app/...`), so EVERY file was misread as a test file → the impl
        // surface came back empty → the hard-coded-literal anti-gaming check no-opped
        // repo-wide. Strip the root first; fall back to the full path if not a prefix.
        let rel_lower = f
            .strip_prefix(project_root)
            .unwrap_or(f.as_path())
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/")
            .to_ascii_lowercase();
        let ext = f
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let Ok(content) = read_capped(&f) else {
            continue;
        };
        if is_test_file(&rel_lower, &name_lower, &ext, &content) {
            continue; // skip test files — we want the IMPLEMENTATION surface
        }
        buf.push_str(&content);
        buf.push('\n');
        if buf.len() > 1_500_000 {
            break;
        }
    }
    buf
}

/// Identify a test file by the universal conventions: name markers
/// (`*.test.*` / `*.spec.*` / `test_*` / `*_test.*` / `*_spec.*` / `*Test.java`),
/// a test directory (`/tests/` / `/test/` / `/__tests__/` / `/spec/`), or — for
/// Rust — an inline `#[test]` / `#[tokio::test]`. `rel_lower` / `name_lower` /
/// `ext` are pre-lowercased; `content` is the file body (for the Rust inline
/// case).
fn is_test_file(rel_lower: &str, name_lower: &str, ext: &str, content: &str) -> bool {
    let by_name = name_lower.contains(".test.")
        || name_lower.contains(".spec.")
        || name_lower.starts_with("test_")
        || name_lower.ends_with("_test.py")
        || name_lower.ends_with("_test.go")
        || name_lower.ends_with("_test.rs")
        || name_lower.ends_with("_test.ts")
        || name_lower.ends_with("_test.js")
        || name_lower.ends_with("_test.rb")
        || name_lower.ends_with("_spec.rb")
        || name_lower.ends_with("_test.dart")
        || name_lower.ends_with("test.java")
        || name_lower.ends_with("tests.java")
        || name_lower.ends_with("test.kt")
        || name_lower.ends_with("tests.kt")
        || name_lower.ends_with("spec.groovy")
        || name_lower.ends_with("test.scala");
    let by_dir = rel_lower.contains("/tests/")
        || rel_lower.contains("/test/")
        || rel_lower.contains("/__tests__/")
        || rel_lower.starts_with("tests/")
        || rel_lower.starts_with("test/")
        || rel_lower.contains("/spec/")
        || rel_lower.starts_with("spec/");
    if by_name || by_dir {
        return true;
    }
    // Rust: a file carrying inline `#[test]` / `#[tokio::test]` is a real test
    // file even when its path/name follows no convention.
    if ext == "rs" && (content.contains("#[test]") || content.contains("#[tokio::test]")) {
        return true;
    }
    false
}

/// Compute [`FileMetrics`] for one test file's content. Deterministic + language
/// agnostic: counts assertion calls, test declarations, skip/focus markers, and
/// commented-out test lines, and collects distinctive assertion literals.
fn file_metrics(content: &str) -> FileMetrics {
    let lower = content.to_ascii_lowercase();
    let assertions = count_token(&lower, "assert")
        + count_token(&lower, "expect")
        + count_token(&lower, ".should")
        + count_token(&lower, "verify(");
    let trivial_asserts = count_trivial_true_asserts(&lower);

    // Test declarations — word-boundary for ambiguous short tokens (`it(`,
    // `test(`), plain substring for the punctuation-anchored ones.
    let test_fns = count_token(&lower, "def test_")
        + count_token(&lower, "func test")
        + count_token(&lower, "#[test]")
        + count_token(&lower, "#[tokio::test]")
        + count_token(&lower, "@test")
        + count_token(&lower, "it(")
        + count_token(&lower, "test(")
        + count_token(&lower, "describe(")
        + count_token(&lower, "context(")
        + count_token(&lower, "specify(")
        + count_token(&lower, "scenario(")
        + count_token(&lower, "fit(")
        + count_token(&lower, "xit(")
        + count_token(&lower, "fdescribe(")
        + count_token(&lower, "xdescribe(")
        + count_token(&lower, "xtest(");

    let skips = count_token(&lower, ".only(")
        + count_token(&lower, ".skip(")
        + count_token(&lower, ".todo(")
        + count_token(&lower, "xit(")
        + count_token(&lower, "xdescribe(")
        + count_token(&lower, "xtest(")
        + count_token(&lower, "fit(")
        + count_token(&lower, "fdescribe(")
        + count_token(&lower, "@pytest.mark.skip")
        + count_token(&lower, "@pytest.mark.xfail")
        + count_token(&lower, "@unittest.skip")
        + count_token(&lower, "pytest.skip(")
        + count_token(&lower, "#[ignore]")
        + count_token(&lower, "t.skip(")
        + count_token(&lower, "t.skipnow(")
        + count_token(&lower, "@disabled")
        + count_token(&lower, "@ignore")
        + count_token(&lower, "xfail")
        + count_token(&lower, "skip_if")
        + count_token(&lower, "test.skip")
        + count_token(&lower, "it.skip")
        + count_token(&lower, "describe.skip");

    let mut commented = 0usize;
    let mut literals = BTreeSet::new();
    for line in content.lines() {
        let trimmed = line.trim_start();
        // A FULL-LINE comment (`//` / `#` / leading-`*` jsdoc / `/*` / `--`).
        // `#` also opens Rust attributes (`#[test]`), but those carry no
        // assert/expect/it/test token so they never count as a commented test.
        let full_line_comment = trimmed.starts_with("//")
            || trimmed.starts_with('#')
            || trimmed.starts_with("* ")
            || trimmed.starts_with("/*")
            || trimmed.starts_with("--");
        // An INLINE block comment `/* … */` wrapping a test/assertion token —
        // the common "comment the check out in place" gaming form. Only the text
        // BETWEEN the delimiters is inspected, so a live line with an unrelated
        // `/* note */` is not mistaken for a commented-out test.
        let inline_commented_test = match (line.find("/*"), line.rfind("*/")) {
            (Some(s), Some(e)) if s + 2 <= e => contains_test_token(&line[s + 2..e]),
            _ => false,
        };
        if (full_line_comment && contains_test_token(line)) || inline_commented_test {
            commented += 1;
        }
        if !full_line_comment {
            collect_assertion_literals(line, &mut literals);
        }
    }

    FileMetrics {
        assertions,
        trivial_asserts,
        test_fns,
        skips,
        commented,
        literals,
    }
}

/// Count TRIVIALLY-TRUE assertions — ones whose asserted SUBJECT is the literal
/// `true` (or an `assert True`), so they pass no matter what the implementation does.
/// These are the classic "neuter the test in place" form: rewrite
/// `expect(add(1,2)).toEqual(3)` → `expect(true).toBe(true)` to force a green while
/// keeping the assertion COUNT identical. Conservative on purpose: only the
/// constant-subject forms are counted, so a legitimate `expect(isValid).toBe(true)`
/// (subject is a VARIABLE) is NOT flagged. `lower` is the lowercased file body.
fn count_trivial_true_asserts(lower: &str) -> usize {
    // Each needle's asserted subject is a constant truth — `expect(true)` covers
    // `expect(true).toBe(true)` / `.toEqual(true)` / `.toBeTruthy()`; `assert(true)`
    // covers JS `assert(true)` and Swift/JUnit `XCTAssert(true)`; `asserttrue(true)`
    // covers `assertTrue(true)` / `XCTAssertTrue(true)`; `assert!(true)` is Rust;
    // `assert true` is Python `assert True`.
    const TRIVIAL: &[&str] = &[
        "expect(true)",
        "assert(true)",
        "assert!(true)",
        "asserttrue(true)",
        "assert true",
    ];
    TRIVIAL.iter().map(|n| lower.matches(n).count()).sum()
}

/// `true` when `s` (any case) mentions an assertion / test-declaration token —
/// the needle for "is there test code here?" used by the commented-out-test
/// detection.
fn contains_test_token(s: &str) -> bool {
    let lc = s.to_ascii_lowercase();
    lc.contains("assert")
        || lc.contains("expect(")
        || lc.contains(".should")
        || lc.contains("it(")
        || lc.contains("test(")
        || lc.contains("describe(")
}

/// From an assertion line, collect distinctive quoted literals (≥ 12 chars) — the
/// hard-coded-output needle. Only lines that look like an assertion contribute, so
/// import paths / test descriptions are ignored. Capped at 20 literals per file
/// (enforced by the `BTreeSet` callers via [`file_metrics`]'s overall bound below).
fn collect_assertion_literals(line: &str, out: &mut BTreeSet<String>) {
    if out.len() >= 20 {
        return;
    }
    let lc = line.to_ascii_lowercase();
    let assertish = lc.contains("assert")
        || lc.contains("expect")
        || lc.contains(".should")
        || lc.contains("tobe")
        || lc.contains("toequal")
        || lc.contains("to_eq")
        || lc.contains("equal(")
        || lc.contains("==");
    if !assertish {
        return;
    }
    for quote in ['"', '\'', '`'] {
        let mut rest = line;
        while let Some(start) = rest.find(quote) {
            let after = &rest[start + 1..];
            if let Some(end) = after.find(quote) {
                let lit = &after[..end];
                if lit.chars().count() >= 12 && !lit.trim().is_empty() {
                    out.insert(lit.to_string());
                    if out.len() >= 20 {
                        return;
                    }
                }
                rest = &after[end + 1..];
            } else {
                break;
            }
        }
    }
}

/// Count occurrences of `token` in `haystack`. For a token that begins with an
/// alphanumeric char the count is WORD-BOUNDARY aware (the char preceding a match
/// must not be `[A-Za-z0-9_]`), so `it(` is not counted inside `edit(` and
/// `test(` is not counted inside `latest(`. A token that begins with punctuation
/// (`.skip(`, `#[ignore]`, `@disabled`) is matched as a plain substring — the
/// leading punctuation already anchors it. `haystack`/`token` are expected
/// lowercased by the caller.
fn count_token(haystack: &str, token: &str) -> usize {
    if token.is_empty() {
        return 0;
    }
    let boundary = token
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric());
    if !boundary {
        return haystack.matches(token).count();
    }
    let bytes = haystack.as_bytes();
    let mut count = 0;
    let mut from = 0;
    while let Some(idx) = haystack[from..].find(token) {
        let abs = from + idx;
        let prev_ok = abs == 0 || {
            let pb = bytes[abs - 1];
            !(pb.is_ascii_alphanumeric() || pb == b'_')
        };
        if prev_ok {
            count += 1;
        }
        from = abs + token.len();
        if from >= haystack.len() {
            break;
        }
    }
    count
}

/// Read `package.json`'s `scripts.test` value, if present. Fail-open: a missing /
/// unparseable file, or no `test` script, yields `None`.
fn read_test_command(project_root: &Path) -> Option<String> {
    let body = std::fs::read_to_string(project_root.join("package.json")).ok()?;
    let json: serde_json::Value = serde_json::from_str(&body).ok()?;
    json.get("scripts")?
        .get("test")?
        .as_str()
        .map(str::to_string)
}

/// A stable, dependency-free 64-bit content hash (FNV-1a) — enough to detect that
/// a harness-config file changed between two snapshots. Not cryptographic; only
/// ever compared for equality.
fn hash_str(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
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

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, body).unwrap();
    }

    const GOOD_TEST: &str = "describe('todo', () => {\n\
         it('adds', () => { expect(add(1,2)).toEqual(3); });\n\
         it('subs', () => { expect(sub(5,2)).toEqual(3); });\n\
         });\n";

    #[test]
    fn no_baseline_is_fail_open() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "src/app.test.js", GOOD_TEST);
        // No baseline snapshot → cannot determine integrity → no findings.
        assert!(check(tmp.path(), None).is_empty());
    }

    #[test]
    fn empty_baseline_never_flags_additions() {
        let tmp = TempDir::new().unwrap();
        let before = snapshot(tmp.path()); // empty: no tests yet
                                           // The step ADDS a test file — legitimate, never a violation.
        write(tmp.path(), "src/app.test.js", GOOD_TEST);
        assert!(check(tmp.path(), Some(&before)).is_empty());
    }

    #[test]
    fn unchanged_suite_is_clean() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "src/app.test.js", GOOD_TEST);
        let before = snapshot(tmp.path());
        // Step touches nothing in the tests.
        let findings = check(tmp.path(), Some(&before));
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn adding_more_tests_is_not_flagged() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "src/app.test.js", GOOD_TEST);
        let before = snapshot(tmp.path());
        // The step ADDS a third test + assertions — strictly more coverage.
        write(
            tmp.path(),
            "src/app.test.js",
            &format!("{GOOD_TEST}it('muls', () => {{ expect(mul(2,3)).toEqual(6); }});\n"),
        );
        let findings = check(tmp.path(), Some(&before));
        assert!(
            findings.is_empty(),
            "adding tests must be clean: {findings:?}"
        );
    }

    #[test]
    fn deleting_a_test_file_is_flagged() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "src/app.test.js", GOOD_TEST);
        let before = snapshot(tmp.path());
        fs::remove_file(tmp.path().join("src/app.test.js")).unwrap();
        let findings = check(tmp.path(), Some(&before));
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(findings[0].contains("test file deleted"));
        assert!(findings[0].contains("app.test.js"), "names the file");
    }

    #[test]
    fn removing_a_test_function_is_flagged() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "src/app.test.js", GOOD_TEST);
        let before = snapshot(tmp.path());
        // Drop one of the two `it(...)` test cases.
        write(
            tmp.path(),
            "src/app.test.js",
            "describe('todo', () => {\n\
             it('adds', () => { expect(add(1,2)).toEqual(3); });\n\
             });\n",
        );
        let findings = check(tmp.path(), Some(&before));
        assert!(
            findings.iter().any(|f| f.contains("lost 1 test function")),
            "{findings:?}"
        );
    }

    #[test]
    fn weakening_assertions_without_removing_a_test_is_flagged() {
        let tmp = TempDir::new().unwrap();
        // One test with two assertions.
        write(
            tmp.path(),
            "src/app.test.js",
            "it('works', () => { expect(a).toEqual(1); expect(b).toEqual(2); });\n",
        );
        let before = snapshot(tmp.path());
        // Same single test, but one assertion stripped out (gaming).
        write(
            tmp.path(),
            "src/app.test.js",
            "it('works', () => { expect(a).toEqual(1); });\n",
        );
        let findings = check(tmp.path(), Some(&before));
        assert!(
            findings.iter().any(|f| f.contains("lost 1 assertion")),
            "{findings:?}"
        );
    }

    #[test]
    fn rewriting_assertions_to_trivially_true_in_place_is_flagged() {
        // H2 regression: a body rewritten in place to a trivially-true assertion
        // (`expect(add(1,2)).toEqual(3)` → `expect(true).toBe(true)`) keeps the test-fn
        // AND assertion COUNTS identical, so the count-based checks see no drop. The
        // trivially-true signal must catch the neutering.
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "src/app.test.js",
            "it('adds', () => { expect(add(1,2)).toEqual(3); });\n",
        );
        let before = snapshot(tmp.path());
        // Same fn, same assertion COUNT — only the body is neutered to always-true.
        write(
            tmp.path(),
            "src/app.test.js",
            "it('adds', () => { expect(true).toBe(true); });\n",
        );
        let findings = check(tmp.path(), Some(&before));
        // The count-based checks see no drop …
        assert!(
            !findings
                .iter()
                .any(|f| f.contains("lost") && f.contains("assertion")),
            "the assertion COUNT is unchanged, so the drop check must not fire: {findings:?}"
        );
        // … but the trivially-true signal catches the in-place rewrite.
        assert!(
            findings.iter().any(|f| f.contains("trivially-true")),
            "an in-place rewrite to expect(true) must be flagged: {findings:?}"
        );
    }

    #[test]
    fn removing_a_test_and_stripping_a_kept_test_are_both_reported() {
        // L5 regression: with the old `else if`, a step that BOTH removed a test fn AND
        // stripped assertions from a KEPT test reported only the fn loss, hiding the
        // stripping. The two checks are now INDEPENDENT.
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "src/app.test.js",
            "it('one', () => { expect(a).toEqual(1); });\n\
             it('two', () => { expect(b).toEqual(2); expect(c).toEqual(3); });\n",
        );
        let before = snapshot(tmp.path());
        // Remove test #1 entirely AND strip one assertion out of kept test #2.
        write(
            tmp.path(),
            "src/app.test.js",
            "it('two', () => { expect(b).toEqual(2); });\n",
        );
        let findings = check(tmp.path(), Some(&before));
        assert!(
            findings.iter().any(|f| f.contains("lost 1 test function")),
            "the removed test must be reported: {findings:?}"
        );
        assert!(
            findings.iter().any(|f| f.contains("assertion")),
            "L5: the assertion-strip in the kept test must ALSO be reported: {findings:?}"
        );
    }

    #[test]
    fn hardcoded_literal_check_runs_even_when_root_path_has_a_test_segment() {
        // M4 regression: a project ROOT path containing a `/test/` segment must NOT make
        // every file look like a test (which emptied the impl surface and no-opped the
        // hard-coded-literal anti-gaming check repo-wide). Classification is done on the
        // path RELATIVE to the root, so the impl file is still scanned.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("builds/test/app"); // root PATH carries a "/test/" segment
        fs::create_dir_all(&root).unwrap();
        // Impl source carrying a distinctive literal.
        write(
            &root,
            "src/api.js",
            "export function token() { return \"sk-live-abcdef123456\"; }\n",
        );
        // A test file that does NOT yet assert that literal.
        write(
            &root,
            "src/api.test.js",
            "it('x', () => { expect(ok()).toEqual(true); });\n",
        );
        let before = snapshot(&root);
        // The step bakes the impl's EXACT literal into the test as the expected value.
        write(
            &root,
            "src/api.test.js",
            "it('x', () => { expect(token()).toEqual(\"sk-live-abcdef123456\"); });\n",
        );
        let findings = check(&root, Some(&before));
        assert!(
            findings.iter().any(|f| f.contains("hard-coded literal")),
            "the hard-coded-literal check must still run when the ROOT path has a test segment: {findings:?}"
        );
    }

    #[test]
    fn adding_a_skip_marker_is_flagged() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "src/app.test.js",
            "it('works', () => { expect(a).toEqual(1); });\n",
        );
        let before = snapshot(tmp.path());
        // Convert `it(` to `it.skip(` — the test stays but never runs.
        write(
            tmp.path(),
            "src/app.test.js",
            "it.skip('works', () => { expect(a).toEqual(1); });\n",
        );
        let findings = check(tmp.path(), Some(&before));
        assert!(
            findings
                .iter()
                .any(|f| f.contains("skip/xfail/ignore/only")),
            "{findings:?}"
        );
    }

    #[test]
    fn adding_pytest_xfail_is_flagged() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "tests/test_app.py",
            "def test_add():\n    assert add(1, 2) == 3\n",
        );
        let before = snapshot(tmp.path());
        write(
            tmp.path(),
            "tests/test_app.py",
            "import pytest\n@pytest.mark.xfail\ndef test_add():\n    assert add(1, 2) == 3\n",
        );
        let findings = check(tmp.path(), Some(&before));
        assert!(
            findings.iter().any(|f| f.contains("skip/xfail")),
            "{findings:?}"
        );
    }

    #[test]
    fn rust_inline_ignore_is_flagged() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "src/lib.rs",
            "#[test]\nfn it_adds() { assert_eq!(add(1,2), 3); }\n",
        );
        let before = snapshot(tmp.path());
        // Mark the inline Rust test #[ignore].
        write(
            tmp.path(),
            "src/lib.rs",
            "#[test]\n#[ignore]\nfn it_adds() { assert_eq!(add(1,2), 3); }\n",
        );
        let findings = check(tmp.path(), Some(&before));
        assert!(
            findings.iter().any(|f| f.contains("skip/xfail/ignore")),
            "{findings:?}"
        );
    }

    #[test]
    fn commenting_out_assertions_is_flagged() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "src/app.test.js",
            "it('works', () => { expect(a).toEqual(1); });\n",
        );
        let before = snapshot(tmp.path());
        write(
            tmp.path(),
            "src/app.test.js",
            "it('works', () => { /* expect(a).toEqual(1); */ });\n",
        );
        let findings = check(tmp.path(), Some(&before));
        assert!(
            findings.iter().any(|f| f.contains("commented out")),
            "{findings:?}"
        );
    }

    #[test]
    fn editing_harness_config_is_flagged_but_adding_one_is_not() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "jest.config.js",
            "module.exports = { testMatch: ['**/*.test.js'] };\n",
        );
        write(tmp.path(), "src/app.test.js", GOOD_TEST);
        let before = snapshot(tmp.path());
        // Weaken the runner so it matches nothing.
        write(
            tmp.path(),
            "jest.config.js",
            "module.exports = { testMatch: ['**/__none__/*.js'] };\n",
        );
        let findings = check(tmp.path(), Some(&before));
        assert!(
            findings
                .iter()
                .any(|f| f.contains("harness/runner config modified")),
            "{findings:?}"
        );

        // A FRESH harness add (absent before) is legit setup → not flagged.
        let tmp2 = TempDir::new().unwrap();
        write(tmp2.path(), "src/app.test.js", GOOD_TEST);
        let before2 = snapshot(tmp2.path());
        write(tmp2.path(), "jest.config.js", "module.exports = {};\n");
        let findings2 = check(tmp2.path(), Some(&before2));
        assert!(
            !findings2.iter().any(|f| f.contains("harness")),
            "adding a harness config is legitimate setup: {findings2:?}"
        );
    }

    #[test]
    fn changing_test_command_is_flagged() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "package.json",
            r#"{ "scripts": { "test": "jest" } }"#,
        );
        write(tmp.path(), "src/app.test.js", GOOD_TEST);
        let before = snapshot(tmp.path());
        // Replace the real runner with a no-op that always "passes".
        write(
            tmp.path(),
            "package.json",
            r#"{ "scripts": { "test": "echo ok" } }"#,
        );
        let findings = check(tmp.path(), Some(&before));
        assert!(
            findings
                .iter()
                .any(|f| f.contains("test command was changed")),
            "{findings:?}"
        );
    }

    #[test]
    fn hardcoded_literal_matching_impl_output_is_flagged() {
        let tmp = TempDir::new().unwrap();
        // Implementation emits a distinctive token.
        write(
            tmp.path(),
            "src/app.js",
            "export function banner() { return 'SUPER-SECRET-BANNER-9000'; }\n",
        );
        write(
            tmp.path(),
            "src/app.test.js",
            "it('returns a banner', () => { expect(typeof banner()).toBe('string'); });\n",
        );
        let before = snapshot(tmp.path());
        // The test is rewritten to assert the impl's EXACT output verbatim.
        write(
            tmp.path(),
            "src/app.test.js",
            "it('returns a banner', () => { expect(banner()).toBe('SUPER-SECRET-BANNER-9000'); });\n",
        );
        let findings = check(tmp.path(), Some(&before));
        assert!(
            findings.iter().any(|f| f.contains("hard-coded literal")),
            "{findings:?}"
        );
    }

    #[test]
    fn count_token_respects_word_boundaries() {
        // `it(` is a test decl, but not inside `edit(` / `audit(`.
        assert_eq!(count_token("it( x ); edit( y )", "it("), 1);
        // `test(` not inside `latest(`.
        assert_eq!(count_token("latest( ) ; test( )", "test("), 1);
        // Punctuation-anchored token matched as plain substring.
        assert_eq!(count_token("a.skip( ) b.skip( )", ".skip("), 2);
    }

    #[test]
    fn pure_refactor_keeping_all_signal_is_clean() {
        // A legit edit that reorders + renames variables but keeps every test +
        // assertion must NOT be flagged.
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "tests/test_app.py",
            "def test_add():\n    assert add(1, 2) == 3\n\n\
             def test_sub():\n    assert sub(5, 2) == 3\n",
        );
        let before = snapshot(tmp.path());
        write(
            tmp.path(),
            "tests/test_app.py",
            "def test_sub():\n    assert sub(5, 2) == 3\n\n\
             def test_add():\n    assert add(1, 2) == 3\n",
        );
        let findings = check(tmp.path(), Some(&before));
        assert!(findings.is_empty(), "reorder is clean: {findings:?}");
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_no_follow_symlinks_out_and_cycle_terminates() {
        use std::os::unix::fs::symlink;
        // OUTSIDE the workspace: a "test" file that must never enter the snapshot.
        let outside = TempDir::new().unwrap();
        std::fs::create_dir_all(outside.path().join("tests")).unwrap();
        write(
            outside.path(),
            "tests/evil.test.js",
            "test('leak', () => { expect(1).toBe(1); });\n",
        );

        // The workspace: a real in-tree test file, a dir symlink escaping
        // OUTSIDE, and a self-cycle symlink.
        let ws = TempDir::new().unwrap();
        write(
            ws.path(),
            "tests/app.test.js",
            "test('add', () => { expect(1 + 2).toBe(3); });\n",
        );
        symlink(outside.path(), ws.path().join("escape")).unwrap();
        symlink(ws.path(), ws.path().join("loop")).unwrap();

        // Terminates: an escaping / cyclic dir symlink is never descended.
        let snap = snapshot(ws.path());

        assert!(
            snap.tests.keys().any(|k| k.ends_with("app.test.js")),
            "in-tree test must still be snapshotted: {:?}",
            snap.tests.keys().collect::<Vec<_>>()
        );
        assert!(
            !snap.tests.keys().any(|k| k.contains("evil")),
            "a symlink must not pull a test file from outside the workspace: {:?}",
            snap.tests.keys().collect::<Vec<_>>()
        );
        assert!(
            !snap.tests.keys().any(|k| k.contains("escape")),
            "walk must not traverse an escaping symlink: {:?}",
            snap.tests.keys().collect::<Vec<_>>()
        );
    }
}
