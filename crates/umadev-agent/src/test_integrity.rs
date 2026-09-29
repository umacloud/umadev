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
//! the director loop's internal build-step driver) and drives a bounded rework round
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

use crate::acceptance::{is_python_venv, MAX_SOURCE_DEPTH, SKIP_DIRS};
use crate::fswalk::{classify_no_follow, EntryKind};

mod weakening;

/// Code extensions a test file can carry. Used to decide which files even get
/// classified as a possible test (harness configs are matched separately by
/// exact name, since they carry non-code extensions like `.ini` / `.xml`).
const CODE_EXT: &[&str] = &[
    "ts", "tsx", "js", "jsx", "mjs", "cjs", "vue", "svelte", "py", "rs", "go", "java", "rb", "php",
    "cs", "kt", "kts", "ex", "exs", "dart", "swift", "scala", "groovy",
];

/// Exact filenames (case-insensitive) of dedicated test-runner / harness config.
/// A DELETE of one of these during a build step, or an EDIT that drops existing
/// tests from the run or skips / excuses them, is a gaming signal (the runner is
/// being weakened to pass). An edit that only sets the runner up — a test
/// environment, setup files, fixtures, mocks — and a fresh ADD are legitimate
/// test setup and are NOT flagged. Deliberately narrow — multi-purpose files
/// (`pyproject.toml`, `Cargo.toml`, `vite.config.*`) are excluded so an
/// unrelated edit never trips the guard.
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

const MAX_TEST_FILE_BYTES: usize = 1024 * 1024;
const MAX_TEST_SCAN_BYTES: usize = 64 * 1024 * 1024;
const MAX_TEST_SURFACE_FILES: usize = 800;
const MAX_TEST_SCAN_ENTRIES: usize = 20_000;
const MAX_IMPL_SURFACE_BYTES: usize = 1_500_000;
const MAX_PACKAGE_JSON_BYTES: usize = 1024 * 1024;
const MAX_ASSERTION_LINES: usize = 1024;

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
    /// Hashes of the file's assertion lines (at most [`MAX_ASSERTION_LINES`]) —
    /// whether a pre-existing assertion was REWRITTEN, the only way an impl's
    /// output can be baked into a test that already checked something else.
    assertion_lines: BTreeSet<u64>,
    /// `assertion_lines` hit its cap, so it cannot answer "was one rewritten?".
    assertion_lines_capped: bool,
    /// A Playwright / Cypress end-to-end spec rather than a unit test — which
    /// runner's config decides whether it is collected.
    end_to_end: bool,
}

/// A point-in-time snapshot of the project's TEST surface — every test file's
/// the private per-file metrics, every harness-config file's content, and the
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
    /// Workspace-relative path → content, for each harness-config file.
    harness: BTreeMap<String, String>,
    /// The `scripts.test` value from `package.json`, if present.
    test_command: Option<String>,
    /// False when any directory/file or the aggregate budget prevented a full
    /// snapshot. Comparing an incomplete snapshot would turn unavailable input
    /// into fabricated "deleted test" findings.
    complete: bool,
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
/// fail-open: skips heavy/vendor dirs, caps the files it reads, and marks the
/// snapshot incomplete on any unavailable input. [`check`] never compares an
/// incomplete snapshot. Call this immediately BEFORE a build step's doer turn;
/// pass the result to [`check`] after the turn.
#[must_use]
pub fn snapshot(project_root: &Path) -> TestSnapshot {
    let mut snap = TestSnapshot {
        complete: true,
        ..TestSnapshot::default()
    };
    let root_is_real_directory = std::fs::canonicalize(project_root)
        .ok()
        .and_then(|path| std::fs::symlink_metadata(path).ok())
        .is_some_and(|metadata| umadev_state::fs::metadata_is_real_dir(&metadata));
    if !root_is_real_directory {
        snap.complete = false;
        return snap;
    }
    let mut budget = crate::bounded_fs::Utf8ReadBudget::new(
        MAX_TEST_SCAN_BYTES,
        MAX_TEST_FILE_BYTES.max(MAX_PACKAGE_JSON_BYTES),
    );
    let mut entries_seen = 0usize;
    walk(
        project_root,
        project_root,
        &mut snap,
        &mut budget,
        &mut entries_seen,
        0,
    );
    if snap.complete {
        match read_test_command(project_root, &mut budget) {
            Ok(command) => snap.test_command = command,
            Err(_) => snap.complete = false,
        }
    }
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
    if !before.complete || before.is_empty() {
        // Nothing existed to protect before this step; only additions are
        // possible, and additions are never a violation. Fail-open.
        return Vec::new();
    }
    let after = snapshot(project_root);
    if !after.complete {
        return Vec::new();
    }
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

    // --- Harness / runner config deleted, or edited so it runs fewer tests or
    //     excuses failures. Setting the runner up (an environment, setup files,
    //     fixtures, mocks) is test setup, not a weakening. ---
    for (path, before_body) in &before.harness {
        let Some(after_body) = after.harness.get(path) else {
            out.push(format!(
                "test-integrity: test harness/runner config deleted during this build step — \
                 {path} (do not remove the test runner config to pass; restore it)"
            ));
            continue;
        };
        if after_body == before_body {
            continue;
        }
        if let Some(why) = weakening::harness_weakening(path, before_body, after_body, &after.tests)
        {
            out.push(format!(
                "test-integrity: test harness/runner config modified during this build step — \
                 {path}: {why} (do not weaken the test runner to pass; revert that change and fix \
                 the code instead)"
            ));
        }
    }

    // --- The test command itself (package.json scripts.test) stopped running the
    //     suite or started excusing failures. Replacing `npm init`'s placeholder
    //     with a real runner, or switching runners, is not a weakening. ---
    if let Some(before_cmd) = &before.test_command {
        match &after.test_command {
            None if weakening::test_script_ran_tests(before_cmd) => out.push(
                "test-integrity: the project's test command (package.json scripts.test) was \
                 removed during this build step — restore it; the suite cannot be trusted if the \
                 command that runs it was deleted"
                    .to_string(),
            ),
            Some(after_cmd) => {
                if let Some(why) = weakening::test_script_weakening(before_cmd, after_cmd) {
                    out.push(format!(
                        "test-integrity: the test command was changed during this build step \
                         (scripts.test: {before_cmd:?} -> {after_cmd:?}) — {why}; do not weaken \
                         the test command to force a green, revert it and fix the code"
                    ));
                }
            }
            None => {}
        }
    }

    out
}

/// Best-effort: a PRE-EXISTING test whose assertion was rewritten this step to
/// assert a distinctive literal that appears VERBATIM in the (non-test)
/// implementation source — the classic "bake the impl's exact output into the
/// expected value so the test trivially passes" move. Conservative on purpose:
/// only NEW literals (absent from the before snapshot), only ≥ 12 chars, only when
/// found in impl source, and at most one report per file.
///
/// Asserting the contract's own text — the error message an API returns, a
/// button's label — is how that text is tested, so a brand-new test file, or new
/// assertions added beside every old one, are never flagged: only a file that
/// lost (rewrote) one of its earlier assertion lines is a candidate.
fn hardcoded_literal_findings(
    project_root: &Path,
    before: &TestSnapshot,
    after: &TestSnapshot,
) -> Vec<String> {
    // Collect the NEW literals across all test files first; only read the impl
    // surface if there is at least one candidate (keeps the common path cheap).
    let mut candidates: Vec<(&String, &String)> = Vec::new();
    for (path, after_m) in &after.tests {
        let Some(before_m) = before.tests.get(path) else {
            continue; // a new test file adds coverage; it has nothing to weaken
        };
        if after_m.assertion_lines_capped
            || before_m.assertion_lines.is_subset(&after_m.assertion_lines)
        {
            continue; // every earlier assertion survives: the literal is new coverage
        }
        for lit in &after_m.literals {
            if !before_m.literals.contains(lit) {
                candidates.push((path, lit));
            }
        }
    }
    if candidates.is_empty() {
        return Vec::new();
    }
    let Some(impl_src) = impl_surface(project_root) else {
        return Vec::new();
    };
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
/// and record it into `snap`. Uses the depth/skip bounds of the acceptance scan:
/// the same skipped build/vendor dirs, Python virtualenvs whatever their name, and
/// the same depth. Past that depth the walk stops descending — the same cut on
/// both sides of a comparison, so it cannot fabricate a deleted test — instead of
/// discarding the whole snapshot, which silenced the guard for every deep Maven
/// or Java package tree. An unreadable directory, or a tree over the file/entry
/// caps, still marks the whole snapshot incomplete so a missing entry cannot be
/// misreported as a deleted test.
fn walk(
    root: &Path,
    dir: &Path,
    snap: &mut TestSnapshot,
    budget: &mut crate::bounded_fs::Utf8ReadBudget,
    entries_seen: &mut usize,
    depth: usize,
) {
    if depth > MAX_SOURCE_DEPTH {
        return;
    }
    if snap.tests.len() + snap.harness.len() >= MAX_TEST_SURFACE_FILES {
        snap.complete = false;
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        snap.complete = false;
        return;
    };
    for entry in rd {
        *entries_seen = entries_seen.saturating_add(1);
        if *entries_seen > MAX_TEST_SCAN_ENTRIES
            || snap.tests.len() + snap.harness.len() >= MAX_TEST_SURFACE_FILES
        {
            snap.complete = false;
            return;
        }
        let Ok(e) = entry else {
            snap.complete = false;
            return;
        };
        let p = e.path();
        // No-follow: a symlinked dir/file is skipped so the test snapshot never
        // walks OUT of the workspace or loops through a symlink cycle. A real
        // file falls through to the classification below.
        match classify_no_follow(&p) {
            EntryKind::Dir => {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name.starts_with('.') || SKIP_DIRS.contains(&name) || is_python_venv(&p) {
                    continue;
                }
                walk(root, &p, snap, budget, entries_seen, depth + 1);
                if !snap.complete {
                    return;
                }
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

        // Harness config (matched by exact name) — record its content.
        if HARNESS_FILES.contains(&name_lower.as_str()) {
            let Ok(content) = budget.read_utf8_beneath(root, &p) else {
                snap.complete = false;
                return;
            };
            snap.harness.insert(rel, content);
            continue;
        }
        // Test file? Code ext + path/name heuristic, or a Rust file with inline
        // `#[test]`. Only those candidates are read at all: an ordinary source
        // file (e.g. a vendored `three.min.js` over the per-file cap) is never a
        // test, so it must neither be read nor able to mark the snapshot
        // incomplete — that would silence the guard for the whole project.
        let ext = p
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !CODE_EXT.contains(&ext.as_str()) {
            continue;
        }
        let by_path = is_test_path(&rel_lower, &name_lower);
        if !by_path && ext != "rs" {
            continue;
        }
        // A per-file failure is only knowable as such while a full per-file
        // allowance remains; below that, a failure may be aggregate exhaustion.
        let full_allowance = budget.remaining_bytes() >= MAX_TEST_FILE_BYTES;
        let content = match budget.read_utf8_beneath(root, &p) {
            Ok(content) => content,
            // A Rust file that is not a test by path/name is only read to look
            // for inline `#[test]`s; if that file itself is unreadable (too big,
            // not UTF-8) it is treated as non-test, consistently before and after.
            Err(_) if !by_path && full_allowance => continue,
            Err(_) => {
                snap.complete = false;
                return;
            }
        };
        if by_path || is_test_file(&rel_lower, &name_lower, &ext, &content) {
            snap.tests.insert(rel, file_metrics(&content));
        }
    }
}

/// Concatenate the project's NON-test implementation source (bounded ~1.5 MB) —
/// the surface the hard-coded-literal heuristic searches for the impl's own
/// output. Reuses the shared bounded [`crate::acceptance::source_files`] collector
/// and filters out the test files.
fn impl_surface(project_root: &Path) -> Option<String> {
    let mut buf = String::new();
    let mut budget =
        crate::bounded_fs::Utf8ReadBudget::new(MAX_IMPL_SURFACE_BYTES, MAX_TEST_FILE_BYTES);
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
        let Ok(content) = budget.read_utf8_beneath(project_root, &f) else {
            return None;
        };
        if is_test_file(&rel_lower, &name_lower, &ext, &content) {
            continue; // skip test files — we want the IMPLEMENTATION surface
        }
        buf.push_str(&content);
        buf.push('\n');
    }
    Some(buf)
}

/// Identify a test file by the universal conventions: name markers
/// (`*.test.*` / `*.spec.*` / `test_*` / `*_test.*` / `*_spec.*` / `*Test.java`),
/// a test directory (`/tests/` / `/test/` / `/__tests__/` / `/spec/`), or — for
/// Rust — an inline `#[test]` / `#[tokio::test]`. `rel_lower` / `name_lower` /
/// `ext` are pre-lowercased; `content` is the file body (for the Rust inline
/// case).
fn is_test_file(rel_lower: &str, name_lower: &str, ext: &str, content: &str) -> bool {
    if is_test_path(rel_lower, name_lower) {
        return true;
    }
    // Rust: a file carrying inline `#[test]` / `#[tokio::test]` is a real test
    // file even when its path/name follows no convention.
    ext == "rs" && (content.contains("#[test]") || content.contains("#[tokio::test]"))
}

/// The path/name half of [`is_test_file`]: `true` when the workspace-relative
/// path (`rel_lower`, `/`-separated) or the file name (`name_lower`), both
/// pre-lowercased, follow a universal test-file convention. Needs no content.
fn is_test_path(rel_lower: &str, name_lower: &str) -> bool {
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
        || rel_lower.starts_with("__tests__/")
        || rel_lower.starts_with("tests/")
        || rel_lower.starts_with("test/")
        || rel_lower.contains("/spec/")
        || rel_lower.starts_with("spec/");
    by_name || by_dir
}

/// Content-free test-file classification for a repo-relative, `/`-separated
/// path (e.g. from a diff header): a code file whose path/name follows a test
/// convention. The same heuristic the snapshot uses, minus Rust inline tests.
pub(crate) fn is_test_source_path(rel: &str) -> bool {
    let rel_lower = rel.to_ascii_lowercase();
    let name_lower = rel_lower.rsplit('/').next().unwrap_or("");
    let ext = name_lower.rsplit_once('.').map_or("", |(_, ext)| ext);
    CODE_EXT.contains(&ext) && is_test_path(&rel_lower, name_lower)
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
        + JASMINE_DECLARATIONS
            .iter()
            .map(|token| count_statement_call(&lower, token))
            .sum::<usize>();

    // Skip / focus markers in TEST-CALL position only: `it.skip(` / `describe.only(`
    // / `test.todo(` and jasmine's `xit(` / `fit(` starting a statement — never a
    // query builder's `.skip(10)` or an estimator's `model.fit(X, y)`.
    let skips = TEST_APIS
        .iter()
        .flat_map(|api| TEST_MODIFIERS.iter().map(move |m| format!("{api}.{m}")))
        .map(|marker| count_token(&lower, &marker))
        .sum::<usize>()
        + JASMINE_DECLARATIONS
            .iter()
            .map(|token| count_statement_call(&lower, token))
            .sum::<usize>()
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
        + count_token(&lower, "skip_if");

    let mut commented = 0usize;
    let mut literals = BTreeSet::new();
    let mut assertion_lines = BTreeSet::new();
    let mut assertion_lines_capped = false;
    for line in content.lines() {
        let trimmed = line.trim_start();
        // Rust doc comments (`///`, `//!` — their examples run as doc-tests) and
        // attributes (`#[tokio::test(flavor = …)]`) open like comments but are code.
        let doc_or_attribute = trimmed.starts_with("///")
            || trimmed.starts_with("//!")
            || trimmed.starts_with("#[")
            || trimmed.starts_with("#!");
        // A FULL-LINE comment (`//` / `#` / leading-`*` jsdoc / `/*` / `--`).
        let full_line_comment = !doc_or_attribute
            && (trimmed.starts_with("//")
                || trimmed.starts_with('#')
                || trimmed.starts_with("* ")
                || trimmed.starts_with("/*")
                || trimmed.starts_with("--"));
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
        if !full_line_comment && is_assertion_line(line) {
            collect_assertion_literals(line, &mut literals);
            if assertion_lines.len() < MAX_ASSERTION_LINES {
                assertion_lines.insert(hash_str(line.trim()));
            } else {
                assertion_lines_capped = true;
            }
        }
    }

    FileMetrics {
        assertions,
        trivial_asserts,
        test_fns,
        skips,
        commented,
        literals,
        assertion_lines,
        assertion_lines_capped,
        end_to_end: lower.contains("@playwright/test")
            || lower.contains("cypress")
            || lower.contains("cy.visit("),
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

/// `true` when `s` (any case) holds assertion / test-declaration CODE — the
/// needle for "is there test code here?" used by the commented-out-test
/// detection. Whole tokens only, so `limit(10)` is not `it(` and the word
/// "Assert" labelling an Arrange / Act / Assert step is not an assertion.
fn contains_test_token(s: &str) -> bool {
    let lc = s.to_ascii_lowercase();
    lc.contains(".should")
        || ["expect(", "it(", "test(", "describe("]
            .iter()
            .any(|token| count_token(&lc, token) > 0)
        || asserts_as_code(&lc)
}

/// Words that follow "assert" in prose ("Assert that …", "assert the result …")
/// rather than in Python's `assert <expression>` statement.
const ASSERT_PROSE: &[&str] = &[
    "that", "the", "it", "we", "this", "there", "all", "no", "nothing", "on", "and",
];

/// Whether lowercased `lc` uses `assert` as code: `assert(…)`, `assert!(…)` /
/// `assert_eq!`, `assertEquals(…)`, `assert.equal(…)`, or Python's
/// `assert total == 3` — not the word "Assert" in a comment.
fn asserts_as_code(lc: &str) -> bool {
    let bytes = lc.as_bytes();
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut from = 0;
    while let Some(idx) = lc[from..].find("assert") {
        let at = from + idx;
        from = at + "assert".len();
        if at > 0 && is_word(bytes[at - 1]) {
            continue;
        }
        let mut end = from;
        while end < bytes.len() && is_word(bytes[end]) {
            end += 1;
        }
        match bytes.get(end) {
            Some(b'(' | b'!') => return true,
            // `assert.equal(…)`: a member CALL, not a sentence's full stop.
            Some(b'.') => {
                let mut member = end + 1;
                while member < bytes.len() && is_word(bytes[member]) {
                    member += 1;
                }
                if member > end + 1 && bytes.get(member) == Some(&b'(') {
                    return true;
                }
            }
            // Python's statement form, `assert <expression>`.
            Some(b' ') if end == from => {
                let rest = lc[end..].trim_start();
                let first_word = rest
                    .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                    .next()
                    .unwrap_or("");
                if !ASSERT_PROSE.contains(&first_word)
                    && ["==", "!=", "(", "<", ">", " is ", " in "]
                        .iter()
                        .any(|op| rest.contains(op))
                {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// Whether a line looks like an assertion (`assert…` / `expect…` / `.should` /
/// `toBe` / `toEqual` / `to_eq` / `equal(` / `==`).
fn is_assertion_line(line: &str) -> bool {
    let lc = line.to_ascii_lowercase();
    lc.contains("assert")
        || lc.contains("expect")
        || lc.contains(".should")
        || lc.contains("tobe")
        || lc.contains("toequal")
        || lc.contains("to_eq")
        || lc.contains("equal(")
        || lc.contains("==")
}

/// From an assertion line ([`is_assertion_line`]), collect distinctive quoted
/// literals (≥ 12 chars) — the hard-coded-output needle. Only assertion lines
/// contribute, so import paths / test descriptions are ignored. Capped at 20
/// literals per file.
fn collect_assertion_literals(line: &str, out: &mut BTreeSet<String>) {
    if out.len() >= 20 {
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

/// Test APIs whose `.skip` / `.only` / `.todo` modifier disables or focuses tests.
const TEST_APIS: &[&str] = &["it", "test", "describe", "context", "suite", "specify"];
const TEST_MODIFIERS: &[&str] = &["skip", "only", "todo"];

/// Jasmine-style disabled (`x…`) and focused (`f…`) test declarations.
const JASMINE_DECLARATIONS: &[&str] = &["xit(", "xdescribe(", "xtest(", "fit(", "fdescribe("];

/// Count `token` (a call such as `fit(`) where it STARTS A STATEMENT: preceded on
/// its line by nothing but whitespace or a `{`, `(`, `;`, `,`, `}` or an arrow's
/// `>`. So jasmine's `fit('focuses', …)` counts, while `model.fit(X, y)`,
/// `def fit(self, X)` and `refit(` do not. `haystack` / `token` are lowercased.
pub(crate) fn count_statement_call(haystack: &str, token: &str) -> usize {
    let mut count = 0;
    let mut from = 0;
    while let Some(idx) = haystack[from..].find(token) {
        let at = from + idx;
        let line_start = haystack[..at].rfind('\n').map_or(0, |i| i + 1);
        let before = haystack[line_start..at].trim_end();
        if matches!(
            before.chars().next_back(),
            None | Some('{' | '(' | ';' | ',' | '}' | '>')
        ) {
            count += 1;
        }
        from = at + token.len();
    }
    count
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

/// Read `package.json`'s `scripts.test` value, if present. Missing or
/// unparseable JSON yields `Ok(None)`; an unavailable/oversized file is an
/// incomplete snapshot and therefore an error to the caller.
fn read_test_command(
    project_root: &Path,
    budget: &mut crate::bounded_fs::Utf8ReadBudget,
) -> std::io::Result<Option<String>> {
    let path = project_root.join("package.json");
    let body = match budget.read_utf8_beneath(project_root, &path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) else {
        return Ok(None);
    };
    Ok(json
        .get("scripts")
        .and_then(|scripts| scripts.get("test"))
        .and_then(|test| test.as_str())
        .map(str::to_string))
}

/// A stable, dependency-free 64-bit content hash (FNV-1a) — enough to tell
/// whether an assertion line survived between two snapshots. Not cryptographic;
/// only ever compared for equality.
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

    #[test]
    fn an_oversized_after_snapshot_cannot_fabricate_a_deleted_test() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "tests/app.test.js", GOOD_TEST);
        let before = snapshot(tmp.path());
        assert!(before.complete);

        let file = fs::File::create(tmp.path().join("tests/app.test.js")).unwrap();
        file.set_len((MAX_TEST_FILE_BYTES + 1) as u64).unwrap();
        drop(file);
        let after = snapshot(tmp.path());
        assert!(!after.complete);
        assert!(
            check(tmp.path(), Some(&before)).is_empty(),
            "unavailable after-state is not evidence that the test was deleted"
        );
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

    // ── legitimate test setup is not gaming (S04-4) ──────────────────────────

    #[test]
    fn replacing_npm_placeholder_test_script_is_not_gaming() {
        // `npm init -y` writes a test script that always fails; replacing it with a
        // real runner is the first thing a QA step does, not a weakened test command.
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "package.json",
            r#"{ "scripts": { "test": "echo \"Error: no test specified\" && exit 1" } }"#,
        );
        write(tmp.path(), "src/app.test.js", GOOD_TEST);
        let before = snapshot(tmp.path());
        write(
            tmp.path(),
            "package.json",
            r#"{ "scripts": { "test": "vitest run" } }"#,
        );
        let findings = check(tmp.path(), Some(&before));
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn switching_or_extending_a_real_test_runner_is_not_gaming() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "package.json",
            r#"{ "scripts": { "test": "jest" } }"#,
        );
        write(tmp.path(), "src/app.test.js", GOOD_TEST);
        let before = snapshot(tmp.path());
        write(
            tmp.path(),
            "package.json",
            r#"{ "scripts": { "test": "jest --coverage" } }"#,
        );
        assert!(check(tmp.path(), Some(&before)).is_empty());
        write(
            tmp.path(),
            "package.json",
            r#"{ "scripts": { "test": "vitest run" } }"#,
        );
        assert!(check(tmp.path(), Some(&before)).is_empty());
    }

    #[test]
    fn a_test_command_that_stops_running_or_masks_failures_is_flagged() {
        for weakened in [
            "echo ok",
            "exit 0",
            "true",
            "jest --passWithNoTests",
            "jest || true",
            "jest; exit 0",
        ] {
            let tmp = TempDir::new().unwrap();
            write(
                tmp.path(),
                "package.json",
                r#"{ "scripts": { "test": "jest" } }"#,
            );
            write(tmp.path(), "src/app.test.js", GOOD_TEST);
            let before = snapshot(tmp.path());
            write(
                tmp.path(),
                "package.json",
                &format!(r#"{{ "scripts": {{ "test": "{weakened}" }} }}"#),
            );
            let findings = check(tmp.path(), Some(&before));
            assert!(
                findings.iter().any(|f| f.contains("test command")),
                "`{weakened}` weakens the test command: {findings:?}"
            );
        }
        // Removing a command that ran the suite is still a weakening.
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "package.json",
            r#"{ "scripts": { "test": "jest" } }"#,
        );
        write(tmp.path(), "src/app.test.js", GOOD_TEST);
        let before = snapshot(tmp.path());
        write(tmp.path(), "package.json", r#"{ "scripts": {} }"#);
        assert!(check(tmp.path(), Some(&before))
            .iter()
            .any(|f| f.contains("test command")));
    }

    #[test]
    fn adding_a_fixture_to_conftest_is_not_gaming() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "tests/conftest.py",
            "import pytest\n\n@pytest.fixture\ndef app():\n    return create_app()\n",
        );
        write(
            tmp.path(),
            "tests/test_app.py",
            "def test_home(app):\n    assert app.get('/').status_code == 200\n",
        );
        let before = snapshot(tmp.path());
        write(
            tmp.path(),
            "tests/conftest.py",
            "import pytest\n\n@pytest.fixture\ndef app():\n    return create_app()\n\n\
             @pytest.fixture\ndef client(app):\n    return app.test_client()\n",
        );
        let findings = check(tmp.path(), Some(&before));
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn runner_setup_edits_are_not_gaming() {
        // A first component test needs `environment: 'jsdom'`; a global mock lives in
        // the jest setup file. Neither changes which tests run or how they pass.
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "vitest.config.ts",
            "export default defineConfig({\n  test: {\n    globals: true,\n  },\n});\n",
        );
        write(
            tmp.path(),
            "jest.setup.js",
            "import '@testing-library/jest-dom';\n",
        );
        write(tmp.path(), "src/app.test.js", GOOD_TEST);
        let before = snapshot(tmp.path());
        write(
            tmp.path(),
            "vitest.config.ts",
            "export default defineConfig({\n  test: {\n    globals: true,\n    \
             environment: 'jsdom',\n    setupFiles: ['./src/setupTests.ts'],\n  },\n});\n",
        );
        write(
            tmp.path(),
            "jest.setup.js",
            "import '@testing-library/jest-dom';\njest.mock('./src/api');\n",
        );
        let findings = check(tmp.path(), Some(&before));
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn a_harness_edit_that_drops_or_skips_tests_is_flagged() {
        for (before_cfg, after_cfg) in [
            (
                "module.exports = { testMatch: ['**/*.test.js'] };\n",
                "module.exports = { testMatch: ['**/*.test.js'], passWithNoTests: true };\n",
            ),
            (
                "module.exports = {};\n",
                "module.exports = { testPathIgnorePatterns: ['/src/'] };\n",
            ),
        ] {
            let tmp = TempDir::new().unwrap();
            write(tmp.path(), "jest.config.js", before_cfg);
            write(tmp.path(), "src/app.test.js", GOOD_TEST);
            let before = snapshot(tmp.path());
            write(tmp.path(), "jest.config.js", after_cfg);
            let findings = check(tmp.path(), Some(&before));
            assert!(
                findings.iter().any(|f| f.contains("harness")),
                "`{after_cfg}`: {findings:?}"
            );
        }
        // A conftest that starts deselecting collected tests.
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "tests/conftest.py", "import pytest\n");
        write(
            tmp.path(),
            "tests/test_app.py",
            "def test_home():\n    assert home() == 200\n",
        );
        let before = snapshot(tmp.path());
        write(
            tmp.path(),
            "tests/conftest.py",
            "import pytest\n\ncollect_ignore = ['test_app.py']\n",
        );
        let findings = check(tmp.path(), Some(&before));
        assert!(
            findings.iter().any(|f| f.contains("harness")),
            "{findings:?}"
        );
    }

    // ── asserting the contract's own text is not gaming (S04-5) ──────────────

    #[test]
    fn asserting_an_error_message_the_api_returns_is_not_gaming() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "src/a.test.js", GOOD_TEST);
        write(
            tmp.path(),
            "src/auth.js",
            "export function login() { return { error: 'Invalid email or password' }; }\n",
        );
        let before = snapshot(tmp.path());
        // A brand-new test file asserting the error text the PRD specifies.
        write(
            tmp.path(),
            "src/auth.test.js",
            "it('rejects a bad password', async () => {\n  \
             const res = await login('a@b.c', 'nope');\n  \
             expect(res.error).toBe('Invalid email or password');\n});\n",
        );
        let findings = check(tmp.path(), Some(&before));
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn a_new_test_in_an_existing_file_may_assert_ui_text() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "src/SignUp.jsx",
            "export const SignUp = () => <button>Create account</button>;\n",
        );
        write(tmp.path(), "src/SignUp.test.jsx", GOOD_TEST);
        let before = snapshot(tmp.path());
        write(
            tmp.path(),
            "src/SignUp.test.jsx",
            &format!(
                "{GOOD_TEST}it('shows the call to action', () => {{\n  render(<SignUp />);\n  \
                 expect(screen.getByRole('button', {{ name: 'Create account' }})).toBeInTheDocument();\n}});\n"
            ),
        );
        let findings = check(tmp.path(), Some(&before));
        assert!(findings.is_empty(), "{findings:?}");
    }

    // ── counters see code, not substrings (S04-7) ────────────────────────────

    #[test]
    fn ordinary_calls_and_aaa_comments_are_not_gaming_markers() {
        let mongoose = "it('paginates', async () => {\n  // Arrange\n  await seed(30);\n  \
                        // Act: fetch page two with limit(10)\n  \
                        const users = await User.find().skip(10).limit(10);\n  \
                        // Assert\n  expect(users).toHaveLength(10);\n});\n";
        let sklearn = "def test_model_fits():\n    model = LinearRegression()\n    \
                       model.fit(X, y)\n    assert model.score(X, y) > 0.9\n";
        let rust = "/// Adds two numbers.\n///\n/// ```\n/// assert_eq!(add(1, 2), 3);\n/// ```\n\
                    pub fn add(a: i32, b: i32) -> i32 { a + b }\n\n#[cfg(test)]\nmod tests {\n    \
                    use super::*;\n\n    #[tokio::test(flavor = \"multi_thread\")]\n    \
                    async fn adds() { assert_eq!(add(1, 2), 3); }\n}\n";
        for (name, body) in [("mongoose", mongoose), ("sklearn", sklearn), ("rust", rust)] {
            let m = file_metrics(body);
            assert_eq!(m.skips, 0, "{name}: {m:?}");
            assert_eq!(m.commented, 0, "{name}: {m:?}");
        }
    }

    // ── deep and virtualenv trees keep the guard armed (S04-8) ───────────────

    #[test]
    fn deep_package_tree_keeps_the_guard_armed() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "tests/test_a.py",
            "def test_a():\n    assert a() == 1\n",
        );
        // A Maven / enterprise Java tree nests source ten levels down.
        write(
            tmp.path(),
            "jeecg-boot/jeecg-module-system/jeecg-system-biz/src/main/java/org/jeecg/modules/system/Service.java",
            "public class Service {}\n",
        );
        // A `python -m venv venv` tree is not the project's code.
        write(tmp.path(), "venv/pyvenv.cfg", "home = /usr/bin\n");
        for i in 0..40 {
            write(
                tmp.path(),
                &format!("venv/lib/python3.12/site-packages/pkg{i}/tests/test_x.py"),
                "def test_x():\n    assert True\n",
            );
        }
        let before = snapshot(tmp.path());
        fs::remove_file(tmp.path().join("tests/test_a.py")).unwrap();
        let findings = check(tmp.path(), Some(&before));
        assert!(
            findings
                .iter()
                .any(|f| f.contains("test file deleted") && f.contains("tests/test_a.py")),
            "{findings:?}"
        );
    }

    #[test]
    fn oversized_non_test_source_does_not_disable_the_guard() {
        // A vendored minified bundle (> the per-file read cap) is not a test file;
        // it must not make the snapshot incomplete and silence every finding.
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "tests/a.test.js", GOOD_TEST);
        write(
            tmp.path(),
            "public/vendor.js",
            &"x".repeat(MAX_TEST_FILE_BYTES + 100 * 1024),
        );
        let before = snapshot(tmp.path());
        assert!(
            before.complete,
            "a big non-test file must not mark the snapshot incomplete"
        );
        fs::remove_file(tmp.path().join("tests/a.test.js")).unwrap();
        let findings = check(tmp.path(), Some(&before));
        assert!(
            findings
                .iter()
                .any(|f| f.contains("test file deleted") && f.contains("a.test.js")),
            "{findings:?}"
        );
    }
}
