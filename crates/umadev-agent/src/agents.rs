//! User-definable custom team roles — the cross-review roster as an OPEN set of
//! schedulable seats, not a closed list of eight hardcoded critics.
//!
//! The built-in team ([`crate::critics`]) seats eight fixed roles (PM / architect
//! / designer / frontend / backend / QA / security / DevOps). A real delivery team,
//! though, often needs a DOMAIN reviewer the product author cares about —
//! accessibility, i18n, data-privacy, a specific framework expert — that no fixed
//! roster can anticipate. This module lets the user DEFINE such a seat as a file in
//! their project: `<project_root>/.umadev/agents/<name>.md`. A loaded role JOINS
//! the cross-review team for the kinds it applies to, runs on its OWN read-only
//! fork exactly like a built-in critic, returns the SAME [`RoleVerdict`], and is
//! folded into the deterministic aggregation as PURELY ADVISORY input.
//!
//! File shape (markdown with a `---`-fenced frontmatter):
//!
//! ```text
//! ---
//! name: Accessibility Reviewer
//! applies_to: [preview, quality]
//! focus: Review the UI for WCAG 2.1 AA — semantic markup, keyboard nav, contrast.
//! ---
//! Your methodology, in prose. Checked on every applicable review node.
//! ```
//!
//! HARD INVARIANTS (identical to the built-in critic team — a custom seat is a
//! pure governance UPGRADE, never a new risk surface):
//!
//! 1. **Fail-soft discovery, fail-explicit execution.** A missing
//!    `.umadev/agents/` dir, an unreadable file, or a malformed file yields NO
//!    extra role (skipped) — the team works exactly as it does today. Once a
//!    valid custom seat is convened, a failed fork returns
//!    [`RoleVerdict::empty`], whose status is `Unavailable`: it never fabricates
//!    acceptance or a product blocker, and the coordinator may park a required
//!    review for an operational retry.
//! 2. **Advisory-only / deterministic floor governs.** A custom verdict is folded
//!    into the SAME advisory aggregation as a built-in critic. It can never drive
//!    loop termination and never bypasses a gate — the deterministic coverage /
//!    contract / verify floor still owns loop control.
//! 3. **Single-writer / read-only.** A custom seat reviews on an ISOLATED forked
//!    session and NEVER writes the workspace. Only the main session writes.
//! 4. **Scaled with the task.** A custom seat joins ONLY a review node that already
//!    convenes a built-in team (the caller gates on that), so a lean kind still
//!    convenes no team and a one-line tweak never pays for a review seat.

use std::collections::HashMap;
use std::path::Path;

use crate::continuous::ReviewKind;
use crate::critics::{CriticArtifacts, CriticConsult, RoleCritic, RoleVerdict};

/// The directory a project keeps its user-defined role files in, relative to the
/// project root. Each `*.md` file inside defines one custom review seat.
const AGENTS_DIR: &[&str] = &[".umadev", "agents"];

/// Discovery and content budgets for project-controlled custom reviewer roles.
/// A flooded directory or giant role file must not make every automatic review
/// allocate an attacker-chosen amount of memory.
const MAX_CUSTOM_ROLE_DIR_ENTRIES: usize = 256;
const MAX_CUSTOM_ROLE_FILES: usize = 64;
const MAX_CUSTOM_ROLE_FILE_BYTES: usize = 64 * 1024;
const MAX_CUSTOM_ROLE_TOTAL_BYTES: usize = 512 * 1024;

/// A user-defined cross-review seat, loaded from a `.umadev/agents/*.md` file.
///
/// Implements [`RoleCritic`] so it convenes on the SAME parallel read-only-fork
/// review path as the eight built-in critics and returns the SAME [`RoleVerdict`].
/// Holds its scope ([`applies`](Self::applies_to)) plus the specialist focus +
/// methodology that shape its judge prompt. Constructed only by [`load_custom_roles`]
/// (a malformed file never produces one — invariant 1).
pub struct CustomCritic {
    /// Stable role id (kebab-case slug of the name / filename) — used in the team
    /// ledger, the `CriticVerdict` event, and the seat-tagged blocking union, the
    /// SAME way a built-in seat's [`crate::critics::Seat::role_id`] is.
    role_id: String,
    /// Human title for the judge persona (e.g. `Accessibility Reviewer`).
    title: String,
    /// The short review focus that pins what this seat looks for.
    focus: String,
    /// The role's methodology (the file body) — appended to the judge system prompt.
    methodology: String,
    /// The review kinds this seat plugs into. EMPTY means "unscoped" — applies to
    /// every review-eligible node.
    applies: Vec<ReviewKind>,
}

impl CustomCritic {
    /// Whether this seat reviews at the given node. An unscoped role (no
    /// `applies_to`) reviews every kind; a scoped one only its listed kinds.
    #[must_use]
    pub fn applies_to(&self, kind: ReviewKind) -> bool {
        self.applies.is_empty() || self.applies.contains(&kind)
    }

    /// The stable role id of this seat.
    #[must_use]
    pub fn role_id(&self) -> &str {
        &self.role_id
    }

    /// Build the strict-JSON judge system prompt for this seat: the specialist
    /// persona + its focus + methodology + the shared verdict shape every critic
    /// speaks. The seat is told to review ONLY through its specialist lens so it
    /// complements — never re-derives — the built-in general review.
    fn system_prompt(&self) -> String {
        let mut s = format!(
            "You are a STRICT senior {} on a COMMERCIAL product's delivery team, doing a \
             FOCUSED specialist cross-review from your OWN seat. Review the artifacts below \
             ONLY through your specialist lens described next — the other seats cover the \
             general product / architecture / code review, so do NOT re-derive that. Flag \
             only REAL issues a user or operator would actually feel from your discipline; \
             ignore style nits.",
            self.title
        );
        if !self.focus.is_empty() {
            s.push_str("\n\n## Your review focus\n");
            s.push_str(&self.focus);
        }
        if !self.methodology.is_empty() {
            s.push_str("\n\n## Your methodology\n");
            s.push_str(&crate::experts::excerpt(&self.methodology, 4000));
        }
        s.push_str(
            "\n\nReturn STRICT JSON only, exactly this shape: \
             {\"accepts\": <true|false>, \"blocking\": [\"<must-fix issue>\", ...], \
             \"advisory\": [\"<nice-to-have>\", ...], \"evidence\": [\"<file/where/why>\", ...]}",
        );
        s
    }
}

#[async_trait::async_trait]
impl RoleCritic for CustomCritic {
    fn role(&self) -> &str {
        &self.role_id
    }

    async fn review(
        &self,
        consult: &dyn CriticConsult,
        artifacts: CriticArtifacts<'_>,
    ) -> RoleVerdict {
        let system = self.system_prompt();
        let user = build_review_body(artifacts);
        // The SAME explicit-unavailability path the built-ins use: a fork that
        // cannot open, an offline brain, or an unparseable reply becomes
        // `RoleVerdict::empty` (`ReviewStatus::Unavailable`), never ACCEPT.
        consult.judge(self.role(), &system, user).await
    }
}

/// Assemble the user-facing review body from whatever artifacts the node filled.
/// A custom seat may run at the docs / preview / quality node, so it gets every
/// present surface (the [`crate::continuous::Blackboard`] leaves the unused ones
/// empty) — each excerpted within a budget so a big tree can't blow the prompt.
fn build_review_body(arts: CriticArtifacts<'_>) -> String {
    let mut u = format!(
        "## Requirement\n{}",
        crate::experts::excerpt(arts.requirement, 1200)
    );
    section(&mut u, "PRD", arts.prd, 3000, true);
    section(&mut u, "Architecture", arts.architecture, 3000, true);
    section(&mut u, "UI/UX spec", arts.uiux, 3000, true);
    section(
        &mut u,
        "Deterministic QA floor (already flagged — build on it)",
        arts.qa_floor,
        1500,
        false,
    );
    section(
        &mut u,
        "Deterministic security floor (already flagged — build on it)",
        arts.security_floor,
        1500,
        false,
    );
    section(&mut u, "Delivered code", arts.code, 14_000, false);
    u
}

/// Append one `## label` section to `out` when `body` is non-empty, excerpted to
/// `budget` chars (section-aware for the structured docs, plain for code/floors).
fn section(out: &mut String, label: &str, body: &str, budget: usize, sectioned: bool) {
    let b = body.trim();
    if b.is_empty() {
        return;
    }
    let excerpt = if sectioned {
        crate::experts::excerpt_sections(b, budget)
    } else {
        crate::experts::excerpt(b, budget)
    };
    out.push_str(&format!("\n\n## {label}\n{excerpt}"));
}

/// Load every user-defined seat from `<project_root>/.umadev/agents/*.md`.
///
/// FAIL-OPEN by contract (invariant 1): a missing directory, an unreadable file,
/// or a malformed file simply yields no role for that file — the function NEVER
/// errors and NEVER breaks the team. Files are loaded in a deterministic (sorted)
/// order so the team order is stable across runs.
#[must_use]
pub fn load_custom_roles(project_root: &Path) -> Vec<CustomCritic> {
    let managed = project_root.join(AGENTS_DIR[0]);
    if !crate::bounded_fs::is_real_directory_beneath(project_root, &managed) {
        return Vec::new();
    }
    let dir = managed.join(AGENTS_DIR[1]);
    if !crate::bounded_fs::is_real_directory_beneath(project_root, &dir) {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(&dir) else {
        // No agents dir (the common case) -> no extra roles, team works as today.
        return Vec::new();
    };
    let mut paths = Vec::new();
    for (index, entry) in entries.enumerate() {
        if index >= MAX_CUSTOM_ROLE_DIR_ENTRIES {
            // Deterministic fail-open under directory flooding: do not let an
            // arbitrary filesystem iteration prefix decide which seats join.
            return Vec::new();
        }
        let Ok(entry) = entry else {
            return Vec::new();
        };
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("md") {
            paths.push(path);
        }
    }
    paths.sort();
    let mut roles = Vec::new();
    let mut total_bytes = 0usize;
    for path in paths.into_iter().take(MAX_CUSTOM_ROLE_FILES) {
        let Ok(text) =
            crate::bounded_fs::read_utf8_beneath(project_root, &path, MAX_CUSTOM_ROLE_FILE_BYTES)
        else {
            continue;
        };
        let Some(next_total) = total_bytes.checked_add(text.len()) else {
            break;
        };
        if next_total > MAX_CUSTOM_ROLE_TOTAL_BYTES {
            break;
        }
        total_bytes = next_total;
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        if let Some(role) = parse_role(stem, &text) {
            roles.push(role);
        }
    }
    roles
}

/// Build the custom-seat slice of a review team for one node — the loaded roles
/// that [`apply`](CustomCritic::applies_to) to `kind`, boxed as [`RoleCritic`]s
/// ready to convene alongside the built-ins. The CALLER decides whether to convene
/// them at all (it gates on the built-in team being non-empty so a lean kind still
/// convenes none — invariant 4); this just produces the applicable seats.
#[must_use]
pub fn custom_team_for(project_root: &Path, kind: ReviewKind) -> Vec<Box<dyn RoleCritic>> {
    load_custom_roles(project_root)
        .into_iter()
        .filter(|r| r.applies_to(kind))
        .map(|r| Box::new(r) as Box<dyn RoleCritic>)
        .collect()
}

/// Parse one file into a [`CustomCritic`], or `None` when it's too malformed to be
/// a usable seat (no resolvable id, or no focus AND no methodology — nothing to
/// review with). Lenient by design: a file with no frontmatter is treated as a
/// methodology body named after its filename, so a plain note still works.
fn parse_role(filename_stem: &str, text: &str) -> Option<CustomCritic> {
    let (front, body) = split_frontmatter(text);
    let fm = parse_frontmatter(&front);

    let name = first_present(&fm, &["name", "title", "role"])
        .map(String::as_str)
        .unwrap_or(filename_stem)
        .trim()
        .to_string();
    let mut role_id = slugify(&name);
    if role_id.is_empty() {
        role_id = slugify(filename_stem);
    }
    if role_id.is_empty() {
        return None;
    }
    let title = if name.is_empty() {
        role_id.clone()
    } else {
        name
    };

    let focus = first_present(&fm, &["focus", "prompt", "review", "remit"])
        .cloned()
        .unwrap_or_default()
        .trim()
        .to_string();
    let methodology = body.trim().to_string();
    // A seat with neither a focus nor a body has nothing to review with -> skip it
    // (a blank / garbage file is fail-open dropped, never a half-formed seat).
    if focus.is_empty() && methodology.is_empty() {
        return None;
    }

    let applies = parse_applies(first_present(
        &fm,
        &[
            "applies_to",
            "applies",
            "stage",
            "stages",
            "reviews",
            "scope",
        ],
    ));

    Some(CustomCritic {
        role_id,
        title,
        focus,
        methodology,
        applies,
    })
}

/// Look up the first present, non-empty value among a set of accepted keys.
fn first_present<'a>(fm: &'a HashMap<String, String>, keys: &[&str]) -> Option<&'a String> {
    keys.iter()
        .filter_map(|k| fm.get(*k))
        .find(|v| !v.trim().is_empty())
}

/// Split a `---`-fenced markdown frontmatter from the body. Returns
/// `(frontmatter, body)`. When there is no well-formed opening+closing fence the
/// frontmatter is empty and the whole text is the body (lenient fail-open).
fn split_frontmatter(text: &str) -> (String, String) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let lines: Vec<&str> = text.lines().collect();
    // Skip leading blank lines to find the opening fence.
    let mut open = 0;
    while open < lines.len() && lines[open].trim().is_empty() {
        open += 1;
    }
    if open >= lines.len() || lines[open].trim() != "---" {
        return (String::new(), text.to_string());
    }
    // Find the closing fence.
    let mut close = None;
    for (j, line) in lines.iter().enumerate().skip(open + 1) {
        if line.trim() == "---" {
            close = Some(j);
            break;
        }
    }
    let Some(close) = close else {
        // Unterminated fence -> treat the whole thing as a body (fail-open).
        return (String::new(), text.to_string());
    };
    let front = lines[(open + 1)..close].join("\n");
    let body = lines[(close + 1)..].join("\n");
    (front, body)
}

/// Parse a simple `key: value` frontmatter block into a lower-cased key map.
/// Deliberately a tolerant YAML-subset (no nested structures, no dep) — quotes
/// and comments are stripped; an unparseable line is skipped, never fatal.
fn parse_frontmatter(front: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in front.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let val = v
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .trim()
                .to_string();
            if !key.is_empty() {
                map.insert(key, val);
            }
        }
    }
    map
}

/// Normalise a display name to a stable kebab-case id. ASCII alphanumerics
/// lower-case; runs of other ASCII collapse to a single `-`; non-ASCII letters
/// (e.g. CJK) are kept verbatim so a Chinese role name still yields a stable id.
fn slugify(s: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in s.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !ch.is_ascii() && ch.is_alphanumeric() {
            out.push(ch);
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

/// Parse an `applies_to` value into the review kinds it scopes to. An absent /
/// empty value, or an explicit `all` / `*`, yields an EMPTY vec — meaning
/// "unscoped, applies to every review-eligible node". Unknown tokens are ignored
/// (fail-open: a typo'd scope widens, never narrows to nothing by accident).
fn parse_applies(raw: Option<&String>) -> Vec<ReviewKind> {
    let Some(raw) = raw else {
        return Vec::new();
    };
    let mut out: Vec<ReviewKind> = Vec::new();
    for tok in raw.split([',', ' ', ';', '[', ']']) {
        let t = tok.trim().trim_matches(['"', '\'']).to_ascii_lowercase();
        if t.is_empty() {
            continue;
        }
        match t.as_str() {
            "all" | "any" | "*" => return Vec::new(), // explicit "all" -> unscoped
            "docs" | "doc" | "documents" | "document" | "planning" | "design-docs" => {
                push_unique(&mut out, ReviewKind::Docs);
            }
            "preview" | "frontend" | "ui" | "ux" | "design" => {
                push_unique(&mut out, ReviewKind::Preview);
            }
            "quality" | "qa" | "code" | "delivery" | "security" | "test" => {
                push_unique(&mut out, ReviewKind::Quality);
            }
            _ => {} // unknown scope token -> ignored
        }
    }
    out
}

/// Push `kind` if not already present (keeps the scope list deduped + ordered).
fn push_unique(out: &mut Vec<ReviewKind>, kind: ReviewKind) {
    if !out.contains(&kind) {
        out.push(kind);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a custom-role file under `<root>/.umadev/agents/<name>.md`.
    fn write_role(root: &Path, name: &str, content: &str) {
        let dir = root.join(".umadev").join("agents");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(name), content).unwrap();
    }

    #[test]
    fn loads_accessibility_role_from_frontmatter() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_role(
            tmp.path(),
            "accessibility.md",
            "---\nname: Accessibility Reviewer\napplies_to: [preview, quality]\n\
             focus: Review the UI for WCAG 2.1 AA — semantic markup, keyboard nav, contrast.\n---\n\
             Walk every interactive element; verify focus order and visible focus ring.\n",
        );
        let roles = load_custom_roles(tmp.path());
        assert_eq!(roles.len(), 1, "the one well-formed role loads");
        let r = &roles[0];
        assert_eq!(r.role_id(), "accessibility-reviewer");
        assert_eq!(r.title, "Accessibility Reviewer");
        assert!(r.focus.contains("WCAG"));
        assert!(r.methodology.contains("focus order"));
        // Scoped to preview + quality only.
        assert!(r.applies_to(ReviewKind::Preview));
        assert!(r.applies_to(ReviewKind::Quality));
        assert!(!r.applies_to(ReviewKind::Docs), "not scoped to docs");
    }

    #[test]
    fn missing_dir_is_fail_open_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        // No .umadev/agents dir at all -> no roles, no error (built-in team intact).
        assert!(load_custom_roles(tmp.path()).is_empty());
        // The custom slice for any node is likewise empty.
        assert!(custom_team_for(tmp.path(), ReviewKind::Quality).is_empty());
        assert!(custom_team_for(tmp.path(), ReviewKind::Docs).is_empty());
    }

    #[test]
    fn malformed_and_blank_files_are_skipped() {
        let tmp = tempfile::TempDir::new().unwrap();
        // A blank file -> no focus, no body -> skipped.
        write_role(tmp.path(), "blank.md", "   \n\n");
        // A frontmatter-only file with no focus and no body -> skipped.
        write_role(tmp.path(), "empty-meta.md", "---\nname: Ghost\n---\n");
        // A non-md file -> ignored entirely.
        write_role(tmp.path(), "notes.txt", "ignored");
        // One genuinely usable role mixed in -> it (and only it) loads.
        write_role(
            tmp.path(),
            "i18n.md",
            "---\nname: i18n Reviewer\n---\nCheck every user-facing string is externalised.\n",
        );
        let roles = load_custom_roles(tmp.path());
        assert_eq!(roles.len(), 1, "only the substantive role survives");
        assert_eq!(roles[0].role_id(), "i18n-reviewer");
    }

    #[test]
    fn oversized_role_and_directory_flood_fail_open() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_role(
            tmp.path(),
            "oversized.md",
            &"x".repeat(MAX_CUSTOM_ROLE_FILE_BYTES + 1),
        );
        assert!(load_custom_roles(tmp.path()).is_empty());

        let agents = tmp.path().join(".umadev/agents");
        std::fs::remove_file(agents.join("oversized.md")).unwrap();
        for index in 0..=MAX_CUSTOM_ROLE_DIR_ENTRIES {
            std::fs::write(agents.join(format!("noise-{index:04}.txt")), "x").unwrap();
        }
        assert!(
            load_custom_roles(tmp.path()).is_empty(),
            "a flooded directory must be bounded and must not select an arbitrary prefix"
        );
    }

    #[cfg(unix)]
    #[test]
    fn custom_roles_reject_symlinked_file_and_directory() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        std::fs::write(outside.path().join("escape.md"), "Review everything.").unwrap();

        std::fs::create_dir_all(tmp.path().join(".umadev/agents")).unwrap();
        symlink(
            outside.path().join("escape.md"),
            tmp.path().join(".umadev/agents/escape.md"),
        )
        .unwrap();
        assert!(load_custom_roles(tmp.path()).is_empty());

        std::fs::remove_dir_all(tmp.path().join(".umadev")).unwrap();
        symlink(outside.path(), tmp.path().join(".umadev")).unwrap();
        assert!(load_custom_roles(tmp.path()).is_empty());
    }

    #[test]
    fn no_frontmatter_falls_back_to_filename_and_body() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_role(
            tmp.path(),
            "data-privacy.md",
            "Audit PII handling: no secrets in source, data minimisation, deletion paths.\n",
        );
        let roles = load_custom_roles(tmp.path());
        assert_eq!(roles.len(), 1);
        assert_eq!(roles[0].role_id(), "data-privacy");
        // No applies_to -> unscoped (applies to every review kind).
        for k in [ReviewKind::Docs, ReviewKind::Preview, ReviewKind::Quality] {
            assert!(roles[0].applies_to(k));
        }
    }

    #[test]
    fn custom_team_for_filters_by_kind() {
        let tmp = tempfile::TempDir::new().unwrap();
        // A quality-only seat.
        write_role(
            tmp.path(),
            "sec.md",
            "---\nname: Threat Modeler\napplies_to: quality\n---\nThreat-model every mutating route.\n",
        );
        // An unscoped seat.
        write_role(
            tmp.path(),
            "any.md",
            "---\nname: Brand Guardian\n---\nKeep the product on-brand and on-voice.\n",
        );
        // Quality node: both seats apply.
        assert_eq!(custom_team_for(tmp.path(), ReviewKind::Quality).len(), 2);
        // Docs node: only the unscoped seat applies (the quality-only one is out).
        let docs = custom_team_for(tmp.path(), ReviewKind::Docs);
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].role(), "brand-guardian");
    }

    /// A stub consult that echoes a fixed verdict (tagged with the seat's role on
    /// normalize) — proves the custom critic builds a prompt + threads the verdict
    /// without a real runtime, exactly like the built-in critic tests.
    struct StubConsult(RoleVerdict);

    #[async_trait::async_trait]
    impl CriticConsult for StubConsult {
        async fn judge(&self, role: &str, system: &str, user: String) -> RoleVerdict {
            // The custom seat's persona + focus + methodology reach the judge.
            assert!(system.contains("specialist"));
            assert!(user.contains("## Requirement"));
            self.0.clone().normalized(role)
        }
    }

    #[tokio::test]
    async fn custom_critic_review_threads_blocking_verdict_advisory() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_role(
            tmp.path(),
            "accessibility.md",
            "---\nname: Accessibility Reviewer\napplies_to: quality\n\
             focus: WCAG 2.1 AA — contrast, focus, labels.\n---\nAudit every control.\n",
        );
        let mut roles = load_custom_roles(tmp.path());
        let role = roles.pop().unwrap();
        // A BLOCKING custom verdict is a plain RoleVerdict — folded advisory-only,
        // it carries NO mechanism to terminate a loop or bypass a gate.
        let stub = StubConsult(RoleVerdict {
            accepts: false,
            blocking: vec!["按钮无可见焦点环,键盘用户无法定位".into()],
            evidence: vec!["LoginForm.tsx".into()],
            ..Default::default()
        });
        let arts = CriticArtifacts {
            requirement: "做一个登录页",
            code: "// LoginForm.tsx\n<button>Login</button>",
            ..Default::default()
        };
        let v = role.review(&stub, arts).await;
        assert_eq!(v.role, "accessibility-reviewer");
        assert!(!v.accepts);
        assert_eq!(
            v.blocking,
            vec!["按钮无可见焦点环,键盘用户无法定位".to_string()]
        );
        assert_eq!(v.evidence, vec!["LoginForm.tsx".to_string()]);
    }

    #[tokio::test]
    async fn custom_critic_missing_verdict_is_unavailable() {
        // A custom seat handed an empty consult has no trustworthy judgement. It
        // carries no semantic blocker, but it must not be collapsed into a pass.
        let role = parse_role(
            "perf",
            "---\nname: Performance Reviewer\n---\nCheck for N+1 and unbounded queries.\n",
        )
        .unwrap();
        // empty("") -> unavailable with an unset role; normalize tags it with the seat.
        let stub = StubConsult(RoleVerdict::empty(""));
        let arts = CriticArtifacts {
            requirement: "an app",
            ..Default::default()
        };
        let v = role.review(&stub, arts).await;
        assert_eq!(v.status(), crate::critics::ReviewStatus::Unavailable);
        assert!(!v.accepts, "absence is not a trustworthy acceptance");
        assert!(v.blocking.is_empty());
        assert_eq!(v.role, "performance-reviewer");
    }

    #[test]
    fn applies_to_parsing_handles_aliases_and_all() {
        // Explicit "all" -> unscoped.
        let r = parse_role("x", "---\napplies_to: all\n---\nbody").unwrap();
        assert!(r.applies.is_empty());
        // Aliases map onto kinds; unknown tokens are ignored.
        let r = parse_role("y", "---\napplies_to: frontend, code, bogus\n---\nbody").unwrap();
        assert!(r.applies_to(ReviewKind::Preview)); // frontend -> preview
        assert!(r.applies_to(ReviewKind::Quality)); // code -> quality
        assert!(!r.applies_to(ReviewKind::Docs));
    }
}
