//! Per-project governance policy — `.umadev/rules.toml`.
//!
//! Lets a project tune which governance rules are enforced and add path exclusions,
//! without forking the rule engine. The hook and the `scan_content` entry
//! point consult this policy before applying each rule.
//!
//! ## File format
//! ```toml
//! # .umadev/rules.toml — project governance overrides
//! #
//! # Every rule defaults to enabled. List the ones you want OFF here:
//! [disabled]
//! clauses = ["UG-LINT-001"]   # e.g. allow inline styles in this project
//!
//! # Paths the host may write even though a rule would otherwise block them.
//! # Anchored, segment-aware globs: `**` = any path segments, `*` = any chars
//! # within one segment, and a bare suffix like `.test.ts` matches by ends_with.
//! [exclusions]
//! paths = ["src/legacy/**", "**/*.test.ts"]
//!
//! # Custom extra domain blocklist entries (merged with the built-ins).
//! [extra]
//! blocked_domains = ["internal-bad-proxy.corp"]
//! ```
//!
//! ## Resolution
//! - Missing file → all defaults (everything on, no exclusions); silent.
//! - Unparseable file → falls back to defaults (all rules ON) **and emits a
//!   loud stderr diagnostic**. This is safe-by-default, but note it is STRICTER
//!   than a user who had disabled clauses / excluded paths intended — so we
//!   never do it silently. A bare fall-back would silently re-enable rules the
//!   user turned off (fail-closed vs their on-disk intent) with no signal; the
//!   warning tells the user their overrides were ignored until they fix the
//!   TOML. Governance itself stays fail-open — `load` never returns an error
//!   that could block the host, and the honest, visible default is to enforce.
//! - The policy is loaded once per hook invocation (it's a short-lived
//!   process); the agent runner caches it for the pipeline lifetime.

use std::collections::HashSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Why a policy load did not yield a parsed user policy. Lets the loader tell a
/// missing file (silent default) apart from a present-but-broken file (default
/// + loud diagnostic, so the user's ignored overrides are never invisible).
#[derive(Debug)]
enum PolicyLoadError {
    /// The file could not be read (absent or unreadable) — a silent default is
    /// the correct, expected behavior.
    Missing,
    /// The file exists but cannot be consumed within the safe input contract.
    Read(String),
    /// The file was read but is not valid TOML; the string is the parser error.
    Parse(String),
}

/// Per-project governance policy loaded from `.umadev/rules.toml`.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Policy {
    /// Rules the project has chosen to disable.
    #[serde(default)]
    pub disabled: DisabledSection,
    /// Path patterns exempt from governance (rules skip these files).
    #[serde(default)]
    pub exclusions: ExclusionsSection,
    /// Extra project-specific blocklist entries.
    #[serde(default)]
    pub extra: ExtraSection,
}

/// `[disabled]` section.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct DisabledSection {
    /// Rule ids to turn off. The key remains `clauses` for file compatibility.
    #[serde(default)]
    pub clauses: Vec<String>,
}

/// `[exclusions]` section.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ExclusionsSection {
    /// Path globs/patterns to skip (suffix or `**` wildcard match).
    #[serde(default)]
    pub paths: Vec<String>,
}

/// `[extra]` section — project-specific additions.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ExtraSection {
    /// Extra malicious domains to block (merged with built-ins).
    #[serde(default)]
    pub blocked_domains: Vec<String>,
}

fn canonical_rule_id(rule_id: &str) -> String {
    crate::rules::LEGACY_LINT_ID_ALIASES
        .iter()
        .find_map(|(legacy, current)| {
            legacy
                .eq_ignore_ascii_case(rule_id)
                .then(|| current.to_ascii_lowercase())
        })
        .unwrap_or_else(|| rule_id.to_ascii_lowercase())
}

impl Policy {
    /// A governance policy is configuration, not an unbounded data store.
    const MAX_POLICY_BYTES: u64 = 1024 * 1024;

    /// Load the policy from `<project_root>/.umadev/rules.toml`.
    /// Fail-open: a missing or unparseable file returns the default policy
    /// (everything enabled) so the host is never blocked by a config error. A
    /// present-but-unparseable file additionally emits a loud stderr diagnostic
    /// (see [`Policy::load_from`]) so the user's overrides are never silently
    /// dropped.
    #[must_use]
    pub fn load(project_root: &Path) -> Self {
        let path = project_root.join(".umadev").join("rules.toml");
        Self::finish_load(
            &path,
            Self::try_load_beneath(project_root, Path::new(".umadev/rules.toml")),
        )
    }

    /// Load from an explicit path. Fail-open on any error, but **honest** about
    /// a broken file: a missing file returns the default policy silently, while
    /// a file that exists but does not parse falls back to the default policy
    /// (all rules ON) AND writes a warning to stderr naming the file and the
    /// parse error. That matters because the default is STRICTER than a user who
    /// disabled clauses / excluded paths — silently swapping their relaxed
    /// policy for the strict default on a TOML typo would be fail-closed against
    /// their intent, with no way to notice. The warning makes the ignore
    /// visible so they can fix it; governance never blocks the host on this.
    #[must_use]
    pub fn load_from(path: &Path) -> Self {
        Self::finish_load(path, Self::try_load_from(path))
    }

    fn finish_load(path: &Path, result: Result<Self, PolicyLoadError>) -> Self {
        match result {
            Ok(policy) => policy,
            Err(PolicyLoadError::Missing) => Self::default(),
            Err(PolicyLoadError::Read(msg)) => {
                eprintln!(
                    "UmaDev governance: could not safely read {} ({msg}); IGNORING your \
                     rules.toml overrides and enforcing ALL default rules.",
                    path.display()
                );
                Self::default()
            }
            Err(PolicyLoadError::Parse(msg)) => {
                eprintln!(
                    "UmaDev governance: could not parse {} ({msg}); IGNORING your \
                     rules.toml overrides and enforcing ALL default rules. Any \
                     disabled clauses / path exclusions are OFF until you fix the \
                     TOML.",
                    path.display()
                );
                Self::default()
            }
        }
    }

    /// Read + parse the policy file, distinguishing "no file" (a legitimate,
    /// silent default) from "file present but unparseable" (the honest signal
    /// that the user's on-disk intent is being ignored, so the caller can warn).
    /// The public [`Policy::load_from`] wraps this; it is split out so the
    /// distinction is unit-testable without capturing stderr.
    ///
    /// # Errors
    /// [`PolicyLoadError::Missing`] when the file can't be read (absent /
    /// unreadable); [`PolicyLoadError::Parse`] when it reads but is not valid
    /// TOML (message carries the parser error).
    fn try_load_from(path: &Path) -> Result<Self, PolicyLoadError> {
        let Some(parent) = path.parent() else {
            return Err(PolicyLoadError::Read(
                "policy path has no parent directory".to_owned(),
            ));
        };
        let Some(name) = path.file_name() else {
            return Err(PolicyLoadError::Read(
                "policy path has no filename".to_owned(),
            ));
        };
        Self::try_load_beneath(parent, Path::new(name))
    }

    fn try_load_beneath(root: &Path, relative: &Path) -> Result<Self, PolicyLoadError> {
        let bytes =
            match umadev_state::fs::read_bounded_beneath(root, relative, Self::MAX_POLICY_BYTES) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Err(PolicyLoadError::Missing)
                }
                Err(error) => return Err(PolicyLoadError::Read(error.to_string())),
            };
        let text = String::from_utf8(bytes)
            .map_err(|_| PolicyLoadError::Read("policy is not valid UTF-8".to_owned()))?;
        toml::from_str(&text).map_err(|e| PolicyLoadError::Parse(e.to_string()))
    }

    /// Write the default policy template to `<project_root>/.umadev/rules.toml`
    /// so users have a starting point. Idempotent — won't overwrite an existing
    /// file. Used by `umadev init`.
    ///
    /// # Errors
    /// Returns an error only on filesystem failure (not on the file existing).
    pub fn write_default_template(project_root: &Path) -> std::io::Result<PathBuf> {
        let dir = project_root.join(".umadev");
        let path = dir.join("rules.toml");
        std::fs::create_dir_all(&dir)?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(mut file) => {
                if let Err(error) = file
                    .write_all(DEFAULT_TEMPLATE.as_bytes())
                    .and_then(|()| file.sync_all())
                {
                    drop(file);
                    let _ = std::fs::remove_file(&path);
                    return Err(error);
                }
                Ok(path)
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(path),
            Err(error) => Err(error),
        }
    }

    /// `true` when a rule is disabled by this policy. IDs are matched
    /// case-insensitively. Legacy craft IDs such as `UD-CODE-003` are aliases
    /// for their independent `UG-LINT-*` replacements, so existing policy files
    /// keep working without making new findings look like specification clauses.
    #[must_use]
    pub fn is_disabled(&self, rule_id: &str) -> bool {
        let canonical = canonical_rule_id(rule_id);
        self.disabled
            .clauses
            .iter()
            .any(|configured| canonical_rule_id(configured) == canonical)
    }

    /// `true` when `file_path` matches an exclusion pattern (governance skips it).
    /// Supports simple globs: `**` matches any path segments, `*` matches one
    /// segment, and a bare suffix like `.test.ts` matches by `ends_with`.
    #[must_use]
    pub fn is_excluded(&self, file_path: &str) -> bool {
        let normalized = file_path.replace('\\', "/");
        for pat in &self.exclusions.paths {
            if glob_match(pat, &normalized) {
                return true;
            }
        }
        false
    }

    /// Merge the extra blocked domains into a single lowercase set (with the
    /// caller-provided built-ins).
    #[must_use]
    pub fn all_blocked_domains(&self, builtins: &[&str]) -> HashSet<String> {
        let mut set: HashSet<String> = builtins.iter().map(|s| s.to_ascii_lowercase()).collect();
        for d in &self.extra.blocked_domains {
            set.insert(d.to_ascii_lowercase());
        }
        set
    }
}

/// Anchored, segment-aware glob matcher. Matching happens over `/`-split path
/// segments so a pattern only excludes what it structurally targets — NOT any
/// path that merely CONTAINS the fragment:
/// - `**` matches zero or more whole path segments.
/// - `*` matches any run of characters within a SINGLE segment (never `/`).
/// - all other characters match literally, and the pattern is anchored to the
///   FULL path (so `src/legacy/**` matches `src/legacy/x` but NOT
///   `other/src/legacy/x`, `x/src/legacy`, or `src/legacy-ish/x`).
///
/// A bare pattern with no `/` and no `*` (a plain suffix like `.test.ts`) keeps
/// the historical `ends_with` semantics, so `.test.ts` still excludes
/// `src/foo.test.ts`. Everything else goes through the anchored segment match.
fn glob_match(pattern: &str, path: &str) -> bool {
    let pat = pattern.replace('\\', "/");
    let path = path.replace('\\', "/");
    // Bare suffix pattern (no slash, no wildcard): historical ends_with match.
    if !pat.contains('/') && !pat.contains('*') {
        return path == pat || path.ends_with(&pat);
    }
    let pattern_segments: Vec<&str> = pat.split('/').collect();
    let target_segments: Vec<&str> = path.split('/').collect();
    glob_match_segments(&pattern_segments, &target_segments)
}

/// Match `/`-split pattern segments against `/`-split path segments, honoring
/// `**` (zero or more whole segments) anchored to the full path. Recursion is
/// bounded by the segment counts (short, fixed exclusion lists), so this is
/// cheap and terminates.
fn glob_match_segments(pat: &[&str], path: &[&str]) -> bool {
    let Some((&seg, rest)) = pat.split_first() else {
        // Pattern exhausted → match iff the path is also exhausted (anchored).
        return path.is_empty();
    };
    if seg == "**" {
        // `**` matches zero or more whole segments: trailing `**` matches
        // anything remaining; otherwise try consuming 0..=path.len() segments.
        if rest.is_empty() {
            return true;
        }
        return (0..=path.len()).any(|skip| glob_match_segments(rest, &path[skip..]));
    }
    match path.split_first() {
        Some((&first, path_rest)) if segment_match(seg, first) => {
            glob_match_segments(rest, path_rest)
        }
        _ => false,
    }
}

/// Match a single path segment against a single pattern segment where `*` is a
/// wildcard for any run of characters WITHIN the segment (it never crosses a
/// `/`, since segments are already split). Anchored: the whole segment must be
/// consumed. Standard iterative wildcard match with backtracking.
fn segment_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0_usize, 0_usize);
    let (mut star, mut mark) = (None, 0_usize);
    while ti < t.len() {
        if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// The default `.umadev/rules.toml` template written by `init`.
pub const DEFAULT_TEMPLATE: &str = "\
# UmaDev governance policy — .umadev/rules.toml
#
# Tune which rules are enforced in THIS project. Everything defaults to ON.
# Normative checks use UD-* specification IDs; independent lints use UG-LINT-*.

# Rules to turn OFF (case-insensitive; key retained for compatibility). Example:
#   [disabled]
#   clauses = [\"UG-LINT-001\"]   # allow inline styles in this project
[disabled]
clauses = []

# Paths the host may write even though a rule would block them (globs ok):
#   [exclusions]
#   paths = [\"src/legacy/**\", \"**/*.test.ts\", \"scripts/debug.ts\"]
[exclusions]
paths = []

# Project-specific extra blocklist entries:
#   [extra]
#   blocked_domains = [\"internal-bad-proxy.corp\"]
[extra]
blocked_domains = []
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_returns_default() {
        let p = Policy::load_from(Path::new("/nonexistent/rules.toml"));
        assert!(!p.is_disabled("UD-ARCH-002"));
        assert!(!p.is_excluded("src/app.ts"));
    }

    #[test]
    fn garbage_file_returns_default() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("rules.toml");
        std::fs::write(&path, "this is not {{{ valid toml").unwrap();
        let p = Policy::load_from(&path);
        assert!(!p.is_disabled("UD-ARCH-002"));
    }

    #[test]
    fn oversized_policy_is_ignored_in_the_strict_direction() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join(".umadev");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(
            dir.join("rules.toml"),
            vec![b'x'; usize::try_from(Policy::MAX_POLICY_BYTES).unwrap() + 1],
        )
        .unwrap();
        let policy = Policy::load(tmp.path());
        assert!(!policy.is_disabled("UD-CODE-001"));
        assert!(!policy.is_excluded("src/app.ts"));
    }

    #[cfg(unix)]
    #[test]
    fn linked_or_fifo_policy_never_relaxes_rules_or_blocks() {
        use std::os::unix::fs::symlink;
        use std::time::{Duration, Instant};

        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join(".umadev");
        std::fs::create_dir(&dir).unwrap();
        let outside = tmp.path().join("outside.toml");
        std::fs::write(&outside, "[disabled]\nclauses=['UD-CODE-001']\n").unwrap();
        symlink(&outside, dir.join("rules.toml")).unwrap();
        assert!(!Policy::load(tmp.path()).is_disabled("UD-CODE-001"));
        std::fs::remove_file(dir.join("rules.toml")).unwrap();

        assert!(std::process::Command::new("mkfifo")
            .arg(dir.join("rules.toml"))
            .status()
            .unwrap()
            .success());
        let started = Instant::now();
        assert!(!Policy::load(tmp.path()).is_disabled("UD-CODE-001"));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn try_load_distinguishes_missing_parse_and_ok() {
        // Missing file → Missing (silent default), NOT a parse error.
        assert!(matches!(
            Policy::try_load_from(Path::new("/nonexistent/rules.toml")),
            Err(PolicyLoadError::Missing)
        ));

        // Present-but-broken file → Parse (so the loader can warn, honest about
        // ignoring the user's on-disk intent).
        let tmp = tempfile::TempDir::new().unwrap();
        let bad = tmp.path().join("bad.toml");
        std::fs::write(&bad, "this is not {{{ valid toml").unwrap();
        assert!(matches!(
            Policy::try_load_from(&bad),
            Err(PolicyLoadError::Parse(_))
        ));

        // Valid file → Ok with the parsed overrides intact.
        let good = tmp.path().join("good.toml");
        std::fs::write(&good, "[disabled]\nclauses=[\"UD-ARCH-002\"]\n").unwrap();
        let parsed = Policy::try_load_from(&good).expect("valid toml parses");
        assert!(parsed.is_disabled("UD-ARCH-002"));
    }

    #[test]
    fn broken_file_falls_back_to_strict_default_not_user_intent() {
        // A user who disabled a clause but then introduced a TOML typo must NOT
        // silently keep the disable (that would be fail-closed against the rest
        // of their intent in a confusing way): the broken file yields the strict
        // default (rule ON) — the accompanying stderr warning is what keeps this
        // honest. Here we assert the resolved policy is the strict default.
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("rules.toml");
        std::fs::write(
            &path,
            "[disabled]\nclauses = [\"UD-ARCH-002\"   # missing closing bracket\n",
        )
        .unwrap();
        let p = Policy::load_from(&path);
        assert!(!p.is_disabled("UD-ARCH-002"));
        assert!(!p.is_excluded("src/whatever.ts"));
    }

    #[test]
    fn disables_clause_case_insensitive() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("rules.toml");
        std::fs::write(&path, "[disabled]\nclauses = [\"ud-arch-002\"]\n").unwrap();
        let p = Policy::load_from(&path);
        assert!(p.is_disabled("UD-ARCH-002"));
        assert!(p.is_disabled("ud-arch-002"));
        assert!(!p.is_disabled("UD-SEC-001"));
    }

    #[test]
    fn legacy_and_current_lint_ids_disable_the_same_rule() {
        for (legacy, current) in crate::rules::LEGACY_LINT_ID_ALIASES {
            let legacy_policy = Policy {
                disabled: DisabledSection {
                    clauses: vec![legacy.to_ascii_lowercase()],
                },
                ..Default::default()
            };
            assert!(legacy_policy.is_disabled(current));

            let current_policy = Policy {
                disabled: DisabledSection {
                    clauses: vec![(*current).to_string()],
                },
                ..Default::default()
            };
            assert!(current_policy.is_disabled(legacy));
        }
    }

    #[test]
    fn excludes_glob_double_star() {
        let p = Policy {
            exclusions: ExclusionsSection {
                paths: vec!["src/legacy/**".into()],
            },
            ..Default::default()
        };
        assert!(p.is_excluded("src/legacy/old.ts"));
        assert!(p.is_excluded("src/legacy/sub/dir/x.ts"));
        assert!(!p.is_excluded("src/new.ts"));
    }

    #[test]
    fn excludes_glob_test_files() {
        let p = Policy {
            exclusions: ExclusionsSection {
                paths: vec!["**/*.test.ts".into()],
            },
            ..Default::default()
        };
        assert!(p.is_excluded("src/foo.test.ts"));
        assert!(p.is_excluded("foo.test.ts"));
        assert!(!p.is_excluded("src/foo.ts"));
    }

    #[test]
    fn excludes_suffix_match() {
        let p = Policy {
            exclusions: ExclusionsSection {
                paths: vec![".test.ts".into()],
            },
            ..Default::default()
        };
        assert!(p.is_excluded("src/foo.test.ts"));
    }

    #[test]
    fn glob_does_not_over_exclude_unrelated_paths() {
        // The old `contains()` glob excluded ANY path that merely contained the
        // fragment — turning governance silently OFF for unrelated files. The
        // anchored, segment-aware match must exclude ONLY what `src/legacy/**`
        // structurally targets.
        let p = Policy {
            exclusions: ExclusionsSection {
                paths: vec!["src/legacy/**".into()],
            },
            ..Default::default()
        };
        // Genuinely under src/legacy → excluded.
        assert!(p.is_excluded("src/legacy/old.ts"));
        assert!(p.is_excluded("src/legacy/a/b/c.ts"));
        // Merely CONTAIN the fragment but are NOT under src/legacy → NOT excluded.
        assert!(!p.is_excluded("other/src/legacy/x.ts"));
        assert!(!p.is_excluded("x/src/legacy"));
        assert!(!p.is_excluded("src/legacy-ish/x.ts"));
        assert!(!p.is_excluded("app/src/legacyfoo.ts"));
        assert!(!p.is_excluded("prefix-src/legacy/x.ts"));
    }

    #[test]
    fn glob_single_star_stays_within_one_segment() {
        // `*` matches within a single path segment and must NOT cross `/`.
        let p = Policy {
            exclusions: ExclusionsSection {
                paths: vec!["src/*.ts".into()],
            },
            ..Default::default()
        };
        assert!(p.is_excluded("src/app.ts"));
        assert!(p.is_excluded("src/deep.component.ts"));
        // A nested file is a different structure — a single `*` must not match.
        assert!(!p.is_excluded("src/sub/app.ts"));
        assert!(!p.is_excluded("src/app.tsx"));
    }

    #[test]
    fn glob_double_star_matches_zero_leading_segments() {
        // `**/…` must match with zero leading segments too (regression guard).
        let p = Policy {
            exclusions: ExclusionsSection {
                paths: vec!["**/node_modules/**".into()],
            },
            ..Default::default()
        };
        assert!(p.is_excluded("node_modules/x/index.js"));
        assert!(p.is_excluded("packages/a/node_modules/x/index.js"));
        assert!(!p.is_excluded("src/node_modules_helper.ts"));
    }

    #[test]
    fn write_template_is_idempotent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = Policy::write_default_template(tmp.path()).unwrap();
        assert!(path.exists());
        // Second call must NOT overwrite.
        let before = std::fs::read_to_string(&path).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let _ = Policy::write_default_template(tmp.path()).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn write_template_creates_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = Policy::write_default_template(tmp.path()).unwrap();
        // `ends_with` is component-based, so it matches regardless of the OS
        // path separator (backslash on Windows).
        assert!(path.ends_with(".umadev/rules.toml"));
    }

    #[test]
    fn extra_domains_merge_with_builtins() {
        let p = Policy {
            extra: ExtraSection {
                blocked_domains: vec!["internal-bad.corp".into()],
            },
            ..Default::default()
        };
        let blocked = concat!("media", "fire.com");
        let set = p.all_blocked_domains(&[blocked, concat!("game", "crack.net")]);
        assert!(set.contains(blocked));
        assert!(set.contains("internal-bad.corp"));
    }

    #[test]
    fn load_from_project_root() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join(".umadev");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("rules.toml"),
            "[disabled]\nclauses=[\"UD-CODE-001\"]\n",
        )
        .unwrap();
        let p = Policy::load(tmp.path());
        assert!(p.is_disabled("UD-CODE-001"));
    }
}
