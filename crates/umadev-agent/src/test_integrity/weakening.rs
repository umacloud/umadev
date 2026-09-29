//! Whether an edit to a test-runner config or to `package.json`'s test script
//! WEAKENS the suite — drops existing tests from the run, excuses failures, or
//! stops running tests at all — as opposed to setting the runner up (a test
//! environment, setup files, fixtures, mocks) or replacing `npm init`'s
//! placeholder with a real runner. Only a weakening is a test-integrity finding.
//!
//! Static and comparative: the before and after contents of one file, plus the
//! project's test files. Where a changed test-selection setting cannot be
//! evaluated (a regular expression, a variable), the change counts as a
//! weakening, since it may drop tests; a plain setup edit never reaches that
//! point.

use std::collections::BTreeMap;

use super::FileMetrics;

/// Directives that skip, deselect or excuse tests, or let a run pass with
/// nothing run. Adding one to a runner config is a weakening.
const MASKING_DIRECTIVES: &[&str] = &[
    "passwithnotests",
    "testnamepattern",
    "onlychanged",
    "it.skip",
    "test.skip",
    "describe.skip",
    ".only(",
    "xit(",
    "xdescribe(",
    "xtest(",
    "pytest.skip(",
    "pytest.mark.skip",
    "xfail",
    "add_marker",
    "deselect",
    "collect_ignore",
    "pytest_ignore_collect",
    "--ignore",
    "|| true",
    "|| exit 0",
];

/// Settings that name WHICH tests a runner collects.
const INCLUDE_KEYS: &[&str] = &[
    "testmatch",
    "testregex",
    "roots",
    "include",
    "spec",
    "testdir",
    "specpattern",
    "testpaths",
    "python_files",
    "python_classes",
    "python_functions",
];

/// Settings that name tests a runner must NOT collect.
const EXCLUDE_KEYS: &[&str] = &[
    "testpathignorepatterns",
    "modulepathignorepatterns",
    "exclude",
    "ignore",
    "testignore",
    "excludespecpattern",
    "norecursedirs",
];

/// Why an edit to the harness file `path` weakens the suite, or `None` when it
/// only sets the runner up. `tests` are the project's test files after the edit.
pub(super) fn harness_weakening(
    path: &str,
    before: &str,
    after: &str,
    tests: &BTreeMap<String, FileMetrics>,
) -> Option<String> {
    let before = before.to_ascii_lowercase();
    let after = after.to_ascii_lowercase();
    for directive in MASKING_DIRECTIVES {
        if after.matches(directive).count() > before.matches(directive).count() {
            return Some(format!("adds `{directive}`, which skips or excuses tests"));
        }
    }
    if addopts_filters(&after) > addopts_filters(&before) {
        return Some("adds a `-k` / `-m` test filter to addopts".to_string());
    }
    selection_weakening(path, &before, &after, tests)
}

/// `-k` / `-m` selection flags in pytest `addopts` lines.
fn addopts_filters(content: &str) -> usize {
    content
        .lines()
        .filter(|line| line.contains("addopts"))
        .map(|line| {
            line.split(|c: char| c.is_whitespace() || matches!(c, '=' | '"' | '\'' | ','))
                .filter(|token| *token == "-k" || *token == "-m")
                .count()
        })
        .sum()
}

/// A changed test-selection setting that stops collecting an existing test.
fn selection_weakening(
    path: &str,
    before: &str,
    after: &str,
    tests: &BTreeMap<String, FileMetrics>,
) -> Option<String> {
    let before_lines = selection_lines(before);
    let after_lines = selection_lines(after);
    if before_lines == after_lines {
        return None;
    }
    let relevant = relevant_tests(path, tests);
    for key in INCLUDE_KEYS.iter().chain(EXCLUDE_KEYS) {
        let old: Vec<&str> = lines_for(&before_lines, key);
        let new: Vec<&str> = lines_for(&after_lines, key);
        if old == new {
            continue;
        }
        let exclude = EXCLUDE_KEYS.contains(key);
        let unevaluable = || Some(format!("changes `{key}`, which decides which tests run"));
        for test in &relevant {
            let Some(was_collected) = collected(&old, exclude, test) else {
                return unevaluable();
            };
            let Some(is_collected) = collected(&new, exclude, test) else {
                return unevaluable();
            };
            if was_collected && !is_collected {
                return Some(format!("its `{key}` change stops collecting {test}"));
            }
        }
    }
    None
}

/// Whether `test` is collected under the setting `lines` (every line of one key).
/// No line means the runner's default: everything, for an include setting;
/// nothing excluded, for an exclude setting. `None` when a pattern cannot be
/// evaluated.
fn collected(lines: &[&str], exclude: bool, test: &str) -> Option<bool> {
    if lines.is_empty() {
        return Some(true);
    }
    let covered = patterns_cover(lines, test)?;
    Some(covered != exclude)
}

fn lines_for<'a>(lines: &'a [(&'static str, String)], key: &str) -> Vec<&'a str> {
    lines
        .iter()
        .filter(|(k, _)| *k == key)
        .map(|(_, line)| line.as_str())
        .collect()
}

/// The test files a harness config governs: Python tests for pytest files, PHP
/// tests for PHPUnit, end-to-end specs for Playwright / Cypress, and the other
/// JavaScript / TypeScript tests for unit runners.
fn relevant_tests<'a>(path: &str, tests: &'a BTreeMap<String, FileMetrics>) -> Vec<&'a str> {
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    let python = std::path::Path::new(&name)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("py") || ext.eq_ignore_ascii_case("ini"));
    let php = name.starts_with("phpunit");
    let e2e_runner = name.starts_with("playwright") || name.starts_with("cypress");
    tests
        .iter()
        .filter(|(test, metrics)| {
            let ext = test.rsplit_once('.').map_or("", |(_, ext)| ext);
            if python {
                ext == "py"
            } else if php {
                ext == "php"
            } else {
                matches!(
                    ext,
                    "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" | "vue" | "svelte"
                ) && metrics.end_to_end == e2e_runner
            }
        })
        .map(|(test, _)| test.as_str())
        .collect()
}

/// The `(key, line)` pairs of lowercased `content` that set a test-selection
/// key. Lines inside a `coverage` block are left out: a coverage `include` /
/// `exclude` decides what is measured, not which tests run.
fn selection_lines(content: &str) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut coverage_depth: Option<usize> = None;
    for raw in content.lines() {
        let line = raw.trim();
        let in_coverage = coverage_depth.is_some() || line.contains("coverage");
        if !in_coverage {
            if let Some(key) = selection_key(line) {
                out.push((key, line.to_string()));
            }
        }
        let opens = line.matches(['{', '[', '(']).count();
        let closes = line.matches(['}', ']', ')']).count();
        if coverage_depth.is_none() && line.contains("coverage") && opens > closes {
            coverage_depth = Some(depth);
        }
        depth = (depth + opens).saturating_sub(closes);
        if coverage_depth.is_some_and(|start| depth <= start) {
            coverage_depth = None;
        }
    }
    out
}

/// The test-selection key `line` sets (`key:` / `"key":` / `key =`), if any.
fn selection_key(line: &str) -> Option<&'static str> {
    INCLUDE_KEYS
        .iter()
        .chain(EXCLUDE_KEYS)
        .copied()
        .find(|key| {
            line.match_indices(key).any(|(at, _)| {
                let starts_word = line[..at]
                    .chars()
                    .next_back()
                    .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'));
                let rest = line[at + key.len()..]
                    .trim_start_matches(['"', '\''])
                    .trim_start();
                starts_word && (rest.starts_with(':') || rest.starts_with('='))
            })
        })
}

/// Whether any pattern on `lines` covers `test`; `None` when a pattern cannot be
/// evaluated (a regular expression, a brace list) or a line holds no literal
/// pattern at all (`include: configDefaults.include`).
fn patterns_cover(lines: &[&str], test: &str) -> Option<bool> {
    let mut unknown = false;
    for line in lines {
        let patterns = pattern_literals(line);
        if patterns.is_empty() {
            unknown = true;
        }
        for pattern in patterns {
            match pattern_covers(&pattern, test) {
                Some(true) => return Some(true),
                Some(false) => {}
                None => unknown = true,
            }
        }
    }
    if unknown {
        None
    } else {
        Some(false)
    }
}

/// The path patterns written on one setting line: its quoted strings, or for an
/// INI line (`testpaths = tests unit`) the words after the `=`.
fn pattern_literals(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    for quote in ['"', '\'', '`'] {
        let mut rest = line;
        while let Some(start) = rest.find(quote) {
            let after = &rest[start + 1..];
            let Some(end) = after.find(quote) else {
                break;
            };
            let literal = &after[..end];
            let is_key = INCLUDE_KEYS.contains(&literal) || EXCLUDE_KEYS.contains(&literal);
            if !literal.trim().is_empty() && !is_key {
                out.push(literal.to_string());
            }
            rest = &after[end + 1..];
        }
    }
    if out.is_empty() {
        if let Some((_, value)) = line.split_once('=') {
            out.extend(
                value
                    .split(|c: char| c.is_whitespace() || c == ',')
                    .filter(|word| !word.is_empty())
                    .map(str::to_string),
            );
        }
    }
    out
}

/// Whether the lowercased path pattern covers the test file `test`; `None` when
/// it is a regular expression rather than a glob or a plain path.
fn pattern_covers(pattern: &str, test: &str) -> Option<bool> {
    let pattern = pattern
        .trim()
        .trim_start_matches("<rootdir>")
        .trim_start_matches("./")
        .trim_start_matches('/');
    if pattern.len() > 256 || pattern.contains(['\\', '(', ')', '|', '^', '$', '+', '[', ']']) {
        return None;
    }
    let test = test.to_ascii_lowercase();
    if pattern.contains(['*', '?', '{']) {
        let test = test.as_bytes();
        return Some(expand_braces(pattern)?.iter().any(|glob| {
            let glob = glob.as_bytes();
            glob_match(glob, test)
                || test
                    .iter()
                    .enumerate()
                    .any(|(i, b)| *b == b'/' && glob_match(glob, &test[i + 1..]))
        }));
    }
    // A plain path or directory name (jest's ignore patterns match substrings).
    let plain = pattern.trim_end_matches('/');
    if plain.is_empty() {
        return Some(false);
    }
    Some(
        test == plain
            || test.starts_with(&format!("{plain}/"))
            || test.ends_with(&format!("/{plain}"))
            || test.contains(&format!("/{plain}/")),
    )
}

/// The globs a `{a,b}` brace list stands for (`*.{test,spec}.ts` →
/// `*.test.ts`, `*.spec.ts`); `None` for unbalanced or runaway braces.
fn expand_braces(pattern: &str) -> Option<Vec<String>> {
    let Some(open) = pattern.find('{') else {
        return Some(vec![pattern.to_string()]);
    };
    let close = open + pattern[open..].find('}')?;
    let (head, tail) = (&pattern[..open], &pattern[close + 1..]);
    let mut out = Vec::new();
    for option in pattern[open + 1..close].split(',') {
        if option.contains('{') {
            return None;
        }
        out.extend(expand_braces(&format!("{head}{option}{tail}"))?);
        if out.len() > 64 {
            return None;
        }
    }
    Some(out)
}

/// Glob match with `**` (any path, including none), `*` (within one segment) and
/// `?` (one character within a segment).
fn glob_match(pattern: &[u8], text: &[u8]) -> bool {
    match pattern.split_first() {
        None => text.is_empty(),
        Some((b'*', rest)) if rest.first() == Some(&b'*') => {
            let rest = &rest[1..];
            let rest = rest.strip_prefix(b"/").unwrap_or(rest);
            (0..=text.len()).any(|i| glob_match(rest, &text[i..]))
        }
        Some((b'*', rest)) => {
            let segment = text.iter().position(|b| *b == b'/').unwrap_or(text.len());
            (0..=segment).any(|i| glob_match(rest, &text[i..]))
        }
        Some((b'?', rest)) => {
            text.first().is_some_and(|b| *b != b'/') && glob_match(rest, &text[1..])
        }
        Some((c, rest)) => text.first() == Some(c) && glob_match(rest, &text[1..]),
    }
}

/// What a `package.json` test script does when it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TestScript {
    /// It runs something that tests the project.
    Runs,
    /// It runs nothing and succeeds (`echo ok`, `exit 0`, `true`).
    NoOpPass,
    /// It runs nothing and fails — `npm init`'s
    /// `echo "Error: no test specified" && exit 1` placeholder.
    AlwaysFails,
}

fn classify_test_script(script: &str) -> TestScript {
    let lower = script.to_ascii_lowercase();
    let mut runs = false;
    let mut fails = false;
    for part in lower
        .split([';', '&', '|'])
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        match part.split_whitespace().next().unwrap_or("") {
            "echo" | "printf" | "true" | ":" => {}
            "exit" => fails |= part != "exit" && part != "exit 0",
            "false" => fails = true,
            _ => runs = true,
        }
    }
    if runs {
        TestScript::Runs
    } else if fails {
        TestScript::AlwaysFails
    } else {
        TestScript::NoOpPass
    }
}

/// Suffixes and flags that make a test script pass when its tests fail or when
/// none run.
fn failure_masks(script: &str) -> usize {
    let compact = script
        .to_ascii_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    [
        "|| true",
        "|| exit 0",
        "|| echo",
        "|| :",
        "; exit 0",
        "; true",
        "--passwithnotests",
    ]
    .iter()
    .map(|mask| compact.matches(mask).count())
    .sum()
}

/// Whether the test script `before` actually ran tests (so removing it loses
/// test signal). `npm init`'s placeholder and a no-op never did.
pub(super) fn test_script_ran_tests(before: &str) -> bool {
    classify_test_script(before) == TestScript::Runs
}

/// Why changing the test script from `before` to `after` weakens the suite, or
/// `None` when it does not — replacing the placeholder with a runner, switching
/// runners, or adding options is ordinary test setup.
pub(super) fn test_script_weakening(before: &str, after: &str) -> Option<&'static str> {
    let old = classify_test_script(before);
    let new = classify_test_script(after);
    if new == TestScript::NoOpPass && old != TestScript::NoOpPass {
        return Some("it no longer runs any test");
    }
    if new == TestScript::Runs && failure_masks(after) > failure_masks(before) {
        return Some("it now passes when tests fail or none run");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scripts_are_classified_by_what_they_run() {
        use TestScript::{AlwaysFails, NoOpPass, Runs};
        for (script, kind) in [
            ("echo \"Error: no test specified\" && exit 1", AlwaysFails),
            ("exit 1", AlwaysFails),
            ("echo ok", NoOpPass),
            ("exit 0", NoOpPass),
            ("true", NoOpPass),
            ("", NoOpPass),
            ("jest", Runs),
            ("vitest run --coverage", Runs),
            ("echo testing && jest", Runs),
            ("jest || true", Runs),
        ] {
            assert_eq!(classify_test_script(script), kind, "{script:?}");
        }
    }

    #[test]
    fn globs_follow_path_segments() {
        assert!(glob_match(b"**/*.test.js", b"src/app.test.js"));
        assert!(glob_match(b"**/*.test.js", b"app.test.js"));
        assert!(!glob_match(b"*.test.js", b"src/app.test.js"));
        assert!(glob_match(b"src/**", b"src/a/b.test.ts"));
        assert!(!glob_match(b"**/__none__/*.js", b"src/app.test.js"));
        assert_eq!(pattern_covers("e2e/**", "e2e/login.spec.ts"), Some(true));
        assert_eq!(pattern_covers("e2e/**", "src/app.test.ts"), Some(false));
        assert_eq!(pattern_covers("/src/", "src/app.test.js"), Some(true));
        assert_eq!(
            pattern_covers("<rootdir>/e2e/", "src/app.test.js"),
            Some(false)
        );
        assert_eq!(pattern_covers("\\.e2e\\.ts$", "src/app.test.js"), None);
        assert_eq!(
            pattern_covers("src/**/*.{test,spec}.ts", "src/a/b.spec.ts"),
            Some(true)
        );
        assert_eq!(
            pattern_covers("src/**/*.{test,spec}.ts", "lib/b.spec.ts"),
            Some(false)
        );
    }

    #[test]
    fn coverage_settings_are_not_test_selection() {
        let config = "export default defineConfig({\n  test: {\n    coverage: {\n      \
                      exclude: ['**/*.test.ts'],\n    },\n    exclude: ['e2e/**'],\n  },\n});\n";
        let lines = selection_lines(&config.to_ascii_lowercase());
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].1.contains("e2e"), "{lines:?}");
    }
}
