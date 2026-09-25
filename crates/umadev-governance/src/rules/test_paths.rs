//! Which paths hold tests, fixtures, examples and generated files — where the
//! noisiest rules stand down.

use super::extension_of;

/// Directory names that make every file below them a test / fixture / example.
/// Matched as whole path segments, so `contest_detail/` or `latest/` is not one.
const TEST_DIRS: &[&str] = &[
    "tests",
    "test",
    "__tests__",
    "testdata",
    "fixtures",
    "fixture",
    "mocks",
    "examples",
    "example",
];

/// `true` for a path where the NOISIEST secret detectors (the entropy + JWT
/// fallback) must be suppressed to avoid flooding: a test / fixture / example /
/// sample / template path (realistic-but-fake secrets), a generated LOCKFILE
/// (full of SRI integrity hashes), or a minified bundle (one giant high-entropy
/// line). The high-signal detectors (PEM, named keys, provider shapes) still fire
/// on these, so a REAL key here is not a free pass.
///
/// Segment-aware and separator-agnostic: a Windows path (`C:\proj\tests\…`) is
/// judged like a POSIX one; directory markers match whole segments; `test_` only
/// starts a file name (`test_auth.py`, never `latest_users.py`); `.test.` /
/// `.spec.` / `_test.` are file-name parts; `.dist` / `.template` only end a file
/// name (`phpunit.xml.dist`, never `geo.distance.ts` or `email.template.ts`).
pub(super) fn looks_like_secret_test_path(file_path: &str) -> bool {
    let normalized = file_path.replace('\\', "/").to_ascii_lowercase();
    let mut segments = normalized.split('/').filter(|segment| !segment.is_empty());
    let Some(name) = segments.next_back() else {
        return false;
    };
    segments.any(|dir| TEST_DIRS.contains(&dir))
        || is_test_file_name(name)
        || is_generated_file_name(name)
}

fn is_test_file_name(name: &str) -> bool {
    name.starts_with("test_")
        || matches!(name, "test.rs" | "tests.rs")
        || [
            ".test.",
            ".spec.",
            "_test.",
            "_tests.",
            ".mock.",
            ".min.",
            ".example.",
            ".sample.",
        ]
        .iter()
        .any(|part| name.contains(part))
        || [".example", ".sample", ".template", ".dist", ".mock"]
            .iter()
            .any(|suffix| name.ends_with(suffix))
}

/// Generated lockfiles: high-entropy integrity hashes everywhere, no secrets.
/// (`*.lock` covers Cargo.lock / yarn.lock / poetry.lock / composer.lock / …)
fn is_generated_file_name(name: &str) -> bool {
    extension_of(name) == "lock"
        || matches!(
            name,
            "package-lock.json" | "npm-shrinkwrap.json" | "pnpm-lock.yaml" | "go.sum"
        )
        || name.ends_with("-lock.json")
        || name.ends_with("-lock.yaml")
}
