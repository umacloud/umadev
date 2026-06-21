//! Per-project governance policy — `.umadev/rules.toml`.
//!
//! Lets a project tune which clauses are enforced and add path exclusions,
//! without forking the rule engine. The hook and the `scan_content` entry
//! point consult this policy before applying each rule.
//!
//! ## File format
//! ```toml
//! # .umadev/rules.toml — project governance overrides
//! #
//! # Every clause defaults to enabled. List the ones you want OFF here:
//! [disabled]
//! clauses = ["UD-ARCH-002"]   # e.g. allow console.log in this project
//!
//! # Paths the host may write even though a rule would otherwise block them.
//! # Globs supported via simple suffix/substring match (see PathExclusion).
//! [exclusions]
//! paths = ["src/legacy/**", "**/*.test.ts"]
//!
//! # Custom extra domain blocklist entries (merged with the built-ins).
//! [extra]
//! blocked_domains = ["internal-bad-proxy.corp"]
//! ```
//!
//! ## Resolution
//! - Missing file → all defaults (everything on, no exclusions).
//! - Unparseable file → fail-open (all defaults) so a typo never blocks work.
//! - The policy is loaded once per hook invocation (it's a short-lived
//!   process); the agent runner caches it for the pipeline lifetime.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Per-project governance policy loaded from `.umadev/rules.toml`.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Policy {
    /// Clauses the project has chosen to disable (lowercased, e.g. "sd-arch-002").
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
    /// Clause ids to turn off, e.g. `["UD-ARCH-002"]`.
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

impl Policy {
    /// Load the policy from `<project_root>/.umadev/rules.toml`.
    /// Fail-open: a missing or unparseable file returns the default policy
    /// (everything enabled) so the host is never blocked by a config error.
    #[must_use]
    pub fn load(project_root: &Path) -> Self {
        let path = project_root.join(".umadev").join("rules.toml");
        Self::load_from(&path)
    }

    /// Load from an explicit path. Fail-open on any error.
    #[must_use]
    pub fn load_from(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).unwrap_or_default(),
            Err(_) => Self::default(),
        }
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
        if path.exists() {
            return Ok(path); // don't clobber user edits
        }
        std::fs::create_dir_all(&dir)?;
        std::fs::write(&path, DEFAULT_TEMPLATE)?;
        Ok(path)
    }

    /// `true` when `clause` is disabled by this policy. Clause ids are matched
    /// case-insensitively so `sd-arch-002` and `UD-ARCH-002` are equivalent.
    #[must_use]
    pub fn is_disabled(&self, clause: &str) -> bool {
        let lower = clause.to_ascii_lowercase();
        self.disabled
            .clauses
            .iter()
            .any(|c| c.to_ascii_lowercase() == lower)
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

/// Simple glob matcher supporting `**` (any segments) and `*` (any chars
/// within a path). Falls back to `ends_with` for bare suffix patterns.
/// Conservative: `*` here is treated like `**` (match any substring) — the
/// distinction between "one segment" and "any segments" rarely matters for
/// governance exclusions and the lenient match avoids false negatives that
/// would surprise users.
fn glob_match(pattern: &str, path: &str) -> bool {
    let mut pat = pattern.replace('\\', "/");
    // `**/` means "zero or more leading path segments" — so `**/*.test.ts`
    // matches both `src/foo.test.ts` AND `foo.test.ts`. Drop it entirely.
    while pat.contains("**/") {
        pat = pat.replace("**/", "");
    }
    // `/**` means "zero or more trailing segments" — drop the suffix match.
    while pat.contains("/**") {
        pat = pat.replace("/**", "");
    }
    // Remaining `**` → `*`.
    while pat.contains("**") {
        pat = pat.replace("**", "*");
    }
    if pat.contains('*') {
        let parts: Vec<&str> = pat.split('*').collect();
        let mut search_from = 0;
        for part in parts {
            if part.is_empty() {
                continue;
            }
            match path[search_from..].find(part) {
                Some(idx) => search_from += idx + part.len(),
                None => return false,
            }
        }
        return true;
    }
    path == pat || path.ends_with(&pat) || path.contains(&pat)
}

/// The default `.umadev/rules.toml` template written by `init`.
pub const DEFAULT_TEMPLATE: &str = "\
# UmaDev governance policy — .umadev/rules.toml
#
# Tune which rules are enforced in THIS project. Everything defaults to ON.
# Docs: each clause id maps to a rule in UMADEV_HOST_SPEC_V1.

# Clauses to turn OFF (case-insensitive). Example:
#   [disabled]
#   clauses = [\"UD-ARCH-002\"]   # allow console.log in this project
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
        assert!(path.to_string_lossy().contains(".umadev/rules.toml"));
    }

    #[test]
    fn extra_domains_merge_with_builtins() {
        let p = Policy {
            extra: ExtraSection {
                blocked_domains: vec!["internal-bad.corp".into()],
            },
            ..Default::default()
        };
        let set = p.all_blocked_domains(&["mediafire.com", "crack"]);
        assert!(set.contains("mediafire.com"));
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
