//! The team **constitution** — a visible, user-editable charter of the team's
//! non-negotiable operating principles (Wave C of the development-team
//! repositioning).
//!
//! ## Why this exists
//!
//! The L0 firmware ([`crate::context::compose_firmware`]) already injects these
//! non-negotiables into every work turn (the craft law + anti-AI-slop design law
//! in [`crate::experts`]), and the governance kernel
//! ([`umadev_governance::scan_content`]) actually *enforces* a subset of them on
//! every file write. But until now they were invisible: the user could not READ
//! the bar the team builds to, let alone EDIT it. This module RENDERS those
//! non-negotiables as a plain-language charter the user opens with
//! `/constitution` and edits at [`constitution_rel_path`].
//!
//! ## Accuracy contract (derived, never invented)
//!
//! Every article maps to a REAL enforced rule or a REAL injected craft
//! principle — not an aspiration. The governance-derived articles (icons /
//! colors / secrets / sensitive paths) cite the exact clause id the kernel
//! blocks on; the craft articles mirror the firmware's own wording
//! ([`crate::experts::anti_slop_law`] / [`crate::experts::agentic_engineering_rules`]).
//! A unit test in this module asserts the cited governance rules really fire on a
//! violating sample, so the charter can never promise a rule that isn't enforced.
//!
//! ## Fail-open by contract (mirrors the governance kernel)
//!
//! A missing `.umadev/` dir, an unreadable or unwritable file → the built-in
//! default charter in memory. This module NEVER panics and NEVER returns an error
//! that could block the caller — a charter the user can't write is still a
//! charter they can read.

use std::path::{Path, PathBuf};

use umadev_i18n::{t, Lang};

const MAX_CONSTITUTION_BYTES: usize = 1024 * 1024;

/// Repo-relative path of the charter file the user reads + edits.
///
/// Lives under `.umadev/` next to the other per-project artifacts
/// (`plan.json`, `rules.toml`, `learned/`). Kept as one constant so the TUI
/// command, the generator, and the tests all name the same file.
pub const fn constitution_rel_path() -> &'static str {
    ".umadev/constitution.md"
}

/// Absolute path of the charter for a given project root.
fn constitution_path(root: &Path) -> PathBuf {
    root.join(constitution_rel_path())
}

/// One non-negotiable article of the charter.
struct Article {
    /// The governance clause the kernel blocks on, when this article maps to an
    /// ENFORCED rule (cited in the rendered charter as `(UD-…)` so the user can
    /// trace it to the real enforcement). `None` for a craft / governance
    /// principle that is firmware-injected but not a single pre-write rule.
    clause: Option<&'static str>,
    /// i18n key of the plain-language principle text.
    text_key: &'static str,
}

/// A titled group of articles (rendered as a `##` section).
struct Section {
    /// i18n key of the section heading.
    heading_key: &'static str,
    /// The articles under this heading, in charter order.
    articles: &'static [Article],
}

/// The charter's structure — the single source of truth for which
/// non-negotiables the team operates by, in rendered order. Each entry is
/// derived from a real enforced rule or a real injected craft principle (see the
/// module-level accuracy contract).
const SECTIONS: &[Section] = &[
    Section {
        heading_key: "constitution.section.craft",
        articles: &[
            Article {
                clause: Some("UD-CODE-001"),
                text_key: "constitution.article.icons",
            },
            Article {
                clause: Some("UD-CODE-002"),
                text_key: "constitution.article.tokens",
            },
            Article {
                clause: Some("UD-CODE-002"),
                text_key: "constitution.article.antislop",
            },
            Article {
                clause: Some("UD-CODE-003"),
                text_key: "constitution.article.contract",
            },
            Article {
                clause: None,
                text_key: "constitution.article.craft",
            },
            Article {
                clause: None,
                text_key: "constitution.article.evidence",
            },
        ],
    },
    Section {
        heading_key: "constitution.section.security",
        articles: &[
            Article {
                clause: Some("UD-SEC-003"),
                text_key: "constitution.article.secrets",
            },
            Article {
                clause: Some("UD-SEC-001"),
                text_key: "constitution.article.sensitive_paths",
            },
        ],
    },
    Section {
        heading_key: "constitution.section.governance",
        articles: &[
            Article {
                clause: None,
                text_key: "constitution.article.floor",
            },
            Article {
                clause: None,
                text_key: "constitution.article.irreversible",
            },
        ],
    },
];

/// Render the default charter as Markdown in `lang`. Pure + deterministic (no
/// I/O), so the TUI, the firmware link, and the tests all share one renderer.
/// Each enforced article is prefixed with its `(UD-…)` clause id so the user can
/// trace a principle to the rule the kernel blocks on.
#[must_use]
pub fn render_constitution(lang: Lang) -> String {
    let mut md = String::new();
    md.push_str("# ");
    md.push_str(t(lang, "constitution.title"));
    md.push_str("\n\n");
    md.push_str(t(lang, "constitution.intro"));
    md.push('\n');
    for section in SECTIONS {
        md.push_str("\n## ");
        md.push_str(t(lang, section.heading_key));
        md.push('\n');
        for article in section.articles {
            md.push_str("- ");
            if let Some(clause) = article.clause {
                md.push('(');
                md.push_str(clause);
                md.push_str(") ");
            }
            md.push_str(t(lang, article.text_key));
            md.push('\n');
        }
    }
    md.push_str("\n---\n\n");
    md.push_str(t(lang, "constitution.footer"));
    md.push('\n');
    md
}

/// The result of resolving the charter for a project.
pub struct ConstitutionDoc {
    /// The charter Markdown to show the user (the existing file when present,
    /// else the freshly-generated default).
    pub markdown: String,
    /// Where the charter lives (or would live) — surfaced so the UI can tell the
    /// user which file to edit.
    pub path: PathBuf,
    /// `true` when this call just generated + wrote a fresh default; `false` when
    /// an existing (possibly user-edited) file was read and left untouched.
    pub generated: bool,
}

/// Persist `markdown` to `path`, creating the parent dir. Best-effort; the
/// caller treats any error as "couldn't write" and falls back to the in-memory
/// charter (fail-open).
fn persist(path: &Path, markdown: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, markdown)
}

/// Resolve the charter for `root`: show the user's existing file if present
/// (NEVER clobbered — an edited charter is the user's own), else generate the
/// default for the current UI language, best-effort persist it, and return it.
///
/// **Fail-open:** an unreadable existing file or a failed write degrades to the
/// in-memory default with `generated = true`; this never panics or errors.
#[must_use]
pub fn ensure_constitution(root: &Path) -> ConstitutionDoc {
    let path = constitution_path(root);
    // An existing, non-empty file is the user's charter — show it verbatim.
    if let Ok(text) = crate::bounded_fs::read_utf8_beneath(root, &path, MAX_CONSTITUTION_BYTES) {
        if !text.trim().is_empty() {
            return ConstitutionDoc {
                markdown: text,
                path,
                generated: false,
            };
        }
    }
    // Otherwise generate the default and try to write it (fail-open).
    let markdown = render_constitution(umadev_i18n::current());
    let _ = persist(&path, &markdown);
    ConstitutionDoc {
        markdown,
        path,
        generated: true,
    }
}

/// Force-rewrite the charter at `root` with the freshly-generated default in the
/// current UI language, discarding any prior (possibly edited) file — the
/// explicit "regenerate" path (e.g. a `--regenerate` flag). Fail-open: a failed
/// write still returns the in-memory default.
#[must_use]
pub fn regenerate_constitution(root: &Path) -> ConstitutionDoc {
    let path = constitution_path(root);
    let markdown = render_constitution(umadev_i18n::current());
    let _ = persist(&path, &markdown);
    ConstitutionDoc {
        markdown,
        path,
        generated: true,
    }
}

/// Read the existing charter file, if present and non-empty. Read-only +
/// fail-open: a missing dir / unreadable file → `None`.
#[must_use]
pub fn read_constitution(root: &Path) -> Option<String> {
    crate::bounded_fs::read_utf8_beneath(root, &constitution_path(root), MAX_CONSTITUTION_BYTES)
        .ok()
        .filter(|text| !text.trim().is_empty())
}

/// A small firmware block carrying the user's **edited** charter, or `""` when
/// there is nothing worth injecting.
///
/// The firmware already injects the built-in craft + anti-slop law on every work
/// turn, so a charter that is still the pristine generated default (in ANY
/// language — the file may predate a `/lang` switch) adds no signal and is
/// skipped to avoid spending tokens twice. Only a charter the user has actually
/// CUSTOMIZED is surfaced, head-truncated to `budget_chars`, so user edits to the
/// team's operating principles actually reach the base. Fail-open: no file →
/// `""`.
#[must_use]
pub fn user_charter_firmware_block(root: &Path, budget_chars: usize) -> String {
    let Some(text) = read_constitution(root) else {
        return String::new();
    };
    let trimmed = text.trim();
    // Pristine default (any language) → already covered by the built-in law.
    if Lang::ALL
        .iter()
        .any(|lang| render_constitution(*lang).trim() == trimmed)
    {
        return String::new();
    }
    let body = crate::experts::excerpt(trimmed, budget_chars);
    format!(
        "# TEAM CHARTER (user-maintained — {})\n\nThe user has customized the \
         team's operating charter below. Treat these as the team's non-negotiable \
         principles for this project, alongside your built-in craft law:\n\n{body}",
        constitution_rel_path()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_charter_carries_the_real_non_negotiables() {
        // The English render must name the enforced non-negotiables in plain
        // language: emoji (icons), color tokens, the API contract, secrets — plus
        // the deterministic-floor + irreversible-action governance principles.
        let md = render_constitution(Lang::En);
        let lower = md.to_lowercase();
        assert!(lower.contains("emoji"), "icons/emoji article: {md}");
        assert!(
            lower.contains("token") && lower.contains("color"),
            "color-token article: {md}"
        );
        assert!(lower.contains("contract"), "API-contract article: {md}");
        assert!(lower.contains("secret"), "hardcoded-secret article: {md}");
        assert!(
            lower.contains("irreversible"),
            "irreversible-action article: {md}"
        );
        // The cited governance clause ids are present so a principle is traceable
        // to the rule the kernel blocks on.
        for clause in ["UD-CODE-001", "UD-CODE-002", "UD-CODE-003", "UD-SEC-003"] {
            assert!(md.contains(clause), "cites {clause}: {md}");
        }
    }

    #[test]
    fn cited_governance_clauses_actually_fire() {
        // ACCURACY: every governance clause the charter cites must be a rule the
        // kernel REALLY blocks on — the charter can't promise unenforced rules.
        use umadev_governance::{check_color_tokens, check_emoji, check_hardcoded_secret};

        // UD-CODE-001 — emoji as a functional icon in a UI file.
        let d = check_emoji("src/Btn.tsx", "<button>\u{1f680} Go</button>");
        assert!(d.block && d.clause == "UD-CODE-001", "emoji: {d:?}");

        // UD-CODE-002 — a hardcoded hex color literal in a component.
        let d = check_color_tokens("src/Card.tsx", "const c = '#9333ea';");
        assert!(d.block && d.clause == "UD-CODE-002", "color: {d:?}");

        // UD-SEC-003 — a real-looking hardcoded secret in source.
        let d = check_hardcoded_secret(
            "src/api.ts",
            concat!("const key = \"AKIA7K3M", "9P2QX4RT6V8W0Z1A2B3C4D5E6F7\";"),
        );
        assert!(d.block && d.clause == "UD-SEC-003", "secret: {d:?}");

        // Every CITED clause id in the charter is one the kernel can emit
        // (UD-CODE-* are also registered spec clauses; UD-SEC-* are kernel-only).
        for clause in ["UD-CODE-001", "UD-CODE-002", "UD-CODE-003"] {
            assert!(
                umadev_spec::get_clause(clause).is_some(),
                "{clause} is a real registered spec clause"
            );
        }
    }

    #[test]
    fn ensure_generates_then_reads_back_without_clobbering() {
        let tmp = tempfile::TempDir::new().unwrap();
        // First call: no file → generate + write + flag generated.
        let first = ensure_constitution(tmp.path());
        assert!(first.generated, "first call generates");
        assert!(first.path.exists(), "the charter file was written");
        assert!(first.markdown.contains("UD-CODE-001"));

        // Simulate the user editing the file.
        let edited = format!("{}\n\n- (custom) No Comic Sans, ever.\n", first.markdown);
        std::fs::write(&first.path, &edited).unwrap();

        // Second call: existing file is shown verbatim, NOT regenerated/clobbered.
        let second = ensure_constitution(tmp.path());
        assert!(!second.generated, "existing file is not regenerated");
        assert_eq!(second.markdown, edited, "user edits survive");
        assert!(second.markdown.contains("Comic Sans"), "edit preserved");
    }

    #[test]
    fn regenerate_overwrites_an_edited_charter() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = constitution_path(tmp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "stale user content").unwrap();
        let doc = regenerate_constitution(tmp.path());
        assert!(doc.generated);
        assert!(!doc.markdown.contains("stale user content"));
        assert!(
            doc.markdown.contains("UD-CODE-001"),
            "fresh default written"
        );
        // And the on-disk file now holds the fresh default.
        assert!(read_constitution(tmp.path())
            .unwrap()
            .contains("UD-CODE-001"));
    }

    #[test]
    fn fail_open_on_an_unwritable_root() {
        // A root whose PARENT is a regular file can never be persisted (creating a
        // directory under a file fails on every OS), yet the call must still return
        // the in-memory default — never panic/error. (A bare `/nonexistent/...`
        // path is not cross-platform: on windows a leading `/` is drive-relative
        // and `C:\nonexistent\...` is usually creatable, so the write would
        // unexpectedly succeed and the read-back would not be `None`.)
        let tmp = tempfile::TempDir::new().unwrap();
        let blocker = tmp.path().join("not-a-dir");
        std::fs::write(&blocker, b"x").unwrap();
        let unwritable = blocker.join("umadev/constitution/root/xyz");
        let doc = ensure_constitution(&unwritable);
        assert!(doc.generated);
        assert!(!doc.markdown.is_empty(), "in-memory default still produced");
        assert!(doc.markdown.contains("UD-CODE-001"));
        // Read-only helper under the unwritable root is None (fail-open).
        assert!(read_constitution(&unwritable).is_none());
    }

    #[test]
    fn firmware_block_only_surfaces_a_user_edited_charter() {
        let tmp = tempfile::TempDir::new().unwrap();
        // No file → nothing injected.
        assert!(user_charter_firmware_block(tmp.path(), 1_400).is_empty());

        // Pristine generated default → still nothing (the built-in law covers it).
        let _ = ensure_constitution(tmp.path());
        assert!(
            user_charter_firmware_block(tmp.path(), 1_400).is_empty(),
            "the pristine default must NOT be re-injected"
        );

        // A genuinely edited charter → injected as a labelled, bounded block.
        let path = constitution_path(tmp.path());
        std::fs::write(&path, "# My team rules\n\n- We pair on every PR.\n").unwrap();
        let block = user_charter_firmware_block(tmp.path(), 1_400);
        assert!(block.contains("TEAM CHARTER"), "labelled: {block}");
        assert!(block.contains("pair on every PR"), "carries edits: {block}");
        assert!(block.contains(constitution_rel_path()), "names the file");
    }

    #[test]
    fn firmware_block_respects_the_budget() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = constitution_path(tmp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("# Big charter\n\n{}", "x".repeat(10_000))).unwrap();
        let block = user_charter_firmware_block(tmp.path(), 1_400);
        // Header + the truncated body, bounded near the budget (small overhead for
        // the label is fine).
        assert!(
            block.chars().count() <= 1_400 + 400,
            "block stays near budget: {} chars",
            block.chars().count()
        );
    }

    #[test]
    fn all_three_languages_render_a_non_empty_charter() {
        // Trilingual: every language renders a full charter, and the localized
        // renders differ (no silent English fallback for zh users).
        let en = render_constitution(Lang::En);
        let zh = render_constitution(Lang::ZhCn);
        let tw = render_constitution(Lang::ZhTw);
        for md in [&en, &zh, &tw] {
            assert!(
                md.contains("UD-CODE-001"),
                "clause ids are language-neutral"
            );
            assert!(md.len() > 200, "non-trivial charter");
        }
        assert_ne!(en, zh, "zh-CN render differs from English");
        assert_ne!(zh, tw, "zh-TW render differs from zh-CN");
    }
}
