//! Skill management — install/list/remove knowledge + rule + prompt packages.
//!
//! A Skill bundles domain expertise into a single installable unit:
//! - **Knowledge docs**: copied into the RAG index so the host's research
//!   and generation phases can cite them. The source subpath is PRESERVED so
//!   same-named files from different subdirs (`a/guide.md` vs `b/guide.md`)
//!   never clobber each other.
//! - **Governance rules**: the declared clause ids are merged into
//!   `.umadev/rules.toml` so the governance engine enforces them — concretely,
//!   each is removed from the `[disabled]` opt-out list (governance enforces
//!   every clause by default, so "enable" means "not disabled"). Idempotent +
//!   fail-open: a missing rules.toml means the clauses are already in force; an
//!   unparseable one is left untouched.
//! - **System prompt**: appended to `CLAUDE.md` / coach prompt so the host
//!   knows about the domain constraints.
//!
//! ## Usage
//! ```bash
//! umadev skill install ./my-skill/   # install from local dir
//! umadev skill list                  # list installed skills
//! umadev skill remove react-pro      # uninstall
//! ```

use std::path::{Path, PathBuf};

const MAX_SKILL_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_SKILL_DOCUMENT_BYTES: u64 = 8 * 1024 * 1024;
const MAX_SKILL_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
const MAX_PROJECT_TEXT_BYTES: u64 = 8 * 1024 * 1024;
const MAX_INSTALLED_SKILLS: usize = 1_024;

fn bounded_text(path: &Path, max_bytes: u64) -> std::io::Result<String> {
    let bytes = umadev_state::fs::read_bounded(path, max_bytes)?;
    String::from_utf8(bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

/// A Skill manifest — describes what the skill provides.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SkillManifest {
    /// Skill name (unique identifier).
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// Version string.
    #[serde(default)]
    pub version: String,
    /// Knowledge document paths (relative to the skill dir).
    #[serde(default)]
    pub knowledge: Vec<String>,
    /// Extra governance clause ids to enable.
    #[serde(default)]
    pub rules: Vec<String>,
    /// System prompt snippet to append to CLAUDE.md.
    #[serde(default)]
    pub system_prompt: String,
}

/// Result of installing a skill.
#[derive(Debug)]
pub struct SkillInstallResult {
    pub name: String,
    pub knowledge_copied: usize,
    pub rules_added: usize,
    pub prompt_updated: bool,
}

/// The skill registry — manages installed skills in `.umadev/skills/`.
pub struct SkillRegistry {
    /// Project root.
    project_root: PathBuf,
}

impl SkillRegistry {
    /// Create a registry for the given project root.
    pub fn new(project_root: &Path) -> Self {
        Self {
            project_root: project_root.to_path_buf(),
        }
    }

    /// The skills directory: `.umadev/skills/`.
    fn skills_dir(&self) -> PathBuf {
        self.project_root.join(".umadev").join("skills")
    }

    /// Install a skill from a source directory (must contain `manifest.json`).
    pub fn install(&self, source_dir: &Path) -> std::io::Result<SkillInstallResult> {
        if !umadev_state::fs::real_dir(source_dir) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "skill source must be a real directory",
            ));
        }
        let manifest_bytes = umadev_state::fs::read_bounded_beneath(
            source_dir,
            Path::new("manifest.json"),
            MAX_SKILL_MANIFEST_BYTES,
        )
        .map_err(|e| {
            std::io::Error::new(
                e.kind(),
                format!(
                    "failed to read manifest.json in {}: {e}",
                    source_dir.display()
                ),
            )
        })?;
        let manifest: SkillManifest = serde_json::from_slice(&manifest_bytes).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid manifest.json: {e}"),
            )
        })?;

        // A malicious manifest name (`../../..`) would escape skills_dir.
        safe_component(&manifest.name)?;
        reject_learned_skill_reserved_name(&manifest.name)?;
        let umadev_dir = umadev_state::fs::ensure_real_child_dir(&self.project_root, ".umadev")?;
        let skills_dir = umadev_state::fs::ensure_real_child_dir(&umadev_dir, "skills")?;
        let dest = umadev_state::fs::ensure_real_child_dir(&skills_dir, &manifest.name)?;

        // Write the manifest FIRST, atomically, so the install is recoverable
        // from the very first byte we commit: if any later step (knowledge
        // copy, CLAUDE.md append) fails, the skill is still discoverable by
        // `list()` and cleanable by `remove()` (which reads the manifest to
        // know what to undo). A bare half-install with no manifest used to be
        // invisible to both — impossible to recover except by hand.
        let manifest_out = dest.join("manifest.json");
        atomic_write(
            &manifest_out,
            &serde_json::to_string_pretty(&manifest).unwrap_or_default(),
        )?;

        // Copy knowledge docs.
        let mut knowledge_copied = 0;
        let knowledge_root = umadev_state::fs::ensure_real_child_dir(
            &umadev_state::fs::ensure_real_child_dir(&self.project_root, "knowledge")?,
            "skills",
        )?;
        let knowledge_dest =
            umadev_state::fs::ensure_real_child_dir(&knowledge_root, &manifest.name)?;
        let mut total_bytes = 0_u64;
        for rel_path in &manifest.knowledge {
            // A traversal path (`../../../etc/passwd`) would copy arbitrary host
            // files into the project — skip it rather than honour it.
            if !is_safe_relpath(rel_path) {
                continue;
            }
            let rel = Path::new(rel_path);
            let remaining = MAX_SKILL_TOTAL_BYTES.saturating_sub(total_bytes);
            if remaining == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("skill knowledge exceeds {MAX_SKILL_TOTAL_BYTES} bytes"),
                ));
            }
            let bytes = match umadev_state::fs::read_bounded_beneath(
                source_dir,
                rel,
                MAX_SKILL_DOCUMENT_BYTES.min(remaining),
            ) {
                Ok(bytes) => Some(bytes),
                Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                    return Err(error);
                }
                Err(_)
                    if std::fs::symlink_metadata(source_dir.join(rel)).is_ok_and(|metadata| {
                        umadev_state::fs::metadata_is_real_dir(&metadata)
                    }) =>
                {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!(
                            "knowledge entry `{rel_path}` is a directory; list individual files"
                        ),
                    ));
                }
                Err(_) => None,
            };
            let Some(bytes) = bytes else { continue };
            // Preserve the manifest-relative subpath while creating each
            // destination directory as a real child (never through a link).
            let parent = ensure_relative_parent(&knowledge_dest, rel.parent())?;
            let copy_dst = parent.join(rel.file_name().unwrap_or_default());
            umadev_state::fs::atomic_write(&copy_dst, &bytes)?;
            total_bytes =
                total_bytes.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
            knowledge_copied += 1;
        }

        // Append system prompt to CLAUDE.md.
        let prompt_updated = if manifest.system_prompt.is_empty() {
            false
        } else {
            let claude_md = self.project_root.join("CLAUDE.md");
            let existing = match bounded_text(&claude_md, MAX_PROJECT_TEXT_BYTES) {
                Ok(existing) => existing,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(error) => return Err(error),
            };
            let marker = format!("<!-- skill:{} -->", manifest.name);
            if existing.contains(&marker) {
                false
            } else {
                let block = format!(
                    "\n{marker}\n{}\n<!-- /skill:{} -->\n",
                    manifest.system_prompt, manifest.name
                );
                // Atomic: a crash mid-write must not truncate the user's
                // CLAUDE.md (the same atomic_write the manifest commit uses).
                atomic_write(&claude_md, &(existing + &block))?;
                true
            }
        };

        // Merge the skill's declared governance clauses into rules.toml so the
        // engine actually enforces them (honesty fix: the old impl only COUNTED
        // them). A present but unreadable/unparseable file is surfaced rather
        // than falsely reporting the clauses as enforced.
        let rules_added = self.merge_skill_rules(&manifest.rules)?;

        Ok(SkillInstallResult {
            name: manifest.name,
            knowledge_copied,
            rules_added,
            prompt_updated,
        })
    }

    /// Ensure every governance clause the skill DECLARES is actually enforced by
    /// the project's `.umadev/rules.toml`: remove each declared clause id from
    /// the `[disabled]` opt-out list. Governance enforces every clause by
    /// default, so "enable a clause" concretely means "make sure it isn't
    /// disabled". Idempotent: a missing rules.toml means the clauses are already
    /// in force (nothing to write); unreadable/unparseable files are errors so an
    /// install cannot claim enforcement it did not verify.
    /// Returns the number of declared clauses now guaranteed enforced.
    fn merge_skill_rules(&self, clauses: &[String]) -> std::io::Result<usize> {
        if clauses.is_empty() {
            return Ok(0);
        }
        let path = self.project_root.join(".umadev").join("rules.toml");
        // Absent file → every clause is enforced by default; the skill's
        // declared clauses are already in force.
        let text = match bounded_text(&path, MAX_PROJECT_TEXT_BYTES) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(clauses.len());
            }
            Err(error) => return Err(error),
        };
        // Do not clobber a file we cannot parse or claim that its clauses were
        // updated. Surface the error to the install caller.
        let mut doc = text.parse::<toml_edit::DocumentMut>().map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{} is not valid TOML: {error}", path.display()),
            )
        })?;
        let want: std::collections::HashSet<String> =
            clauses.iter().map(|c| c.to_ascii_lowercase()).collect();
        let mut changed = false;
        if let Some(disabled) = doc.get_mut("disabled") {
            let table = disabled.as_table_like_mut().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "rules.toml disabled section is not table-like",
                )
            })?;
            if let Some(clauses_item) = table.get_mut("clauses") {
                let clauses_array = clauses_item.as_array_mut().ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "rules.toml disabled.clauses is not an array",
                    )
                })?;
                let before = clauses_array.len();
                clauses_array.retain(|value| {
                    value
                        .as_str()
                        .is_none_or(|clause| !want.contains(&clause.to_ascii_lowercase()))
                });
                changed = clauses_array.len() != before;
            }
        }
        if changed {
            atomic_write(&path, &doc.to_string())?;
        }
        Ok(clauses.len())
    }

    /// List all installed skills.
    pub fn list(&self) -> Vec<SkillManifest> {
        let dir = self.skills_dir();
        let mut skills = Vec::new();
        if umadev_state::fs::real_dir(&dir) {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten().take(MAX_INSTALLED_SKILLS) {
                    if !std::fs::symlink_metadata(entry.path())
                        .is_ok_and(|metadata| umadev_state::fs::metadata_is_real_dir(&metadata))
                    {
                        continue;
                    }
                    let manifest_path = entry.path().join("manifest.json");
                    if let Ok(text) = bounded_text(&manifest_path, MAX_SKILL_MANIFEST_BYTES) {
                        if let Ok(manifest) = serde_json::from_str::<SkillManifest>(&text) {
                            skills.push(manifest);
                        }
                    }
                }
            }
        }
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        skills
    }

    /// Remove a skill by name.
    pub fn remove(&self, name: &str) -> std::io::Result<()> {
        safe_component(name)?;
        reject_learned_skill_reserved_name(name)?;
        let dir = self.skills_dir().join(name);
        if !dir.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("skill '{name}' is not installed"),
            ));
        }
        if !umadev_state::fs::real_dir(&dir) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "installed skill path is not a real directory",
            ));
        }

        // Load manifest to know what to clean up.
        let manifest_path = dir.join("manifest.json");
        let manifest_text = bounded_text(&manifest_path, MAX_SKILL_MANIFEST_BYTES)?;
        let manifest = serde_json::from_str::<SkillManifest>(&manifest_text).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "{} is not a valid skill manifest: {error}",
                    manifest_path.display()
                ),
            )
        })?;
        let knowledge_dir = self
            .project_root
            .join("knowledge")
            .join("skills")
            .join(name);
        if knowledge_dir.exists() && !umadev_state::fs::real_dir(&knowledge_dir) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "skill knowledge path is not a real directory",
            ));
        }
        if !manifest.system_prompt.is_empty() {
            remove_managed_prompt_block(&self.project_root, name)?;
        }
        if knowledge_dir.exists() {
            std::fs::remove_dir_all(&knowledge_dir)?;
        }

        // Remove the skill directory.
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }
}

fn remove_managed_prompt_block(project_root: &Path, name: &str) -> std::io::Result<()> {
    let claude_md = project_root.join("CLAUDE.md");
    let content = match bounded_text(&claude_md, MAX_PROJECT_TEXT_BYTES) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let start_marker = format!("<!-- skill:{name} -->");
    if !content.contains(&start_marker) {
        return Ok(());
    }
    let end_marker = format!("<!-- /skill:{name} -->");
    if content.matches(&start_marker).count() != content.matches(&end_marker).count() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("CLAUDE.md has an incomplete managed block for skill `{name}`"),
        ));
    }
    let mut cleaned = String::new();
    let mut skip = false;
    for line in content.lines() {
        if line.contains(&start_marker) {
            skip = true;
            continue;
        }
        if line.contains(&end_marker) {
            skip = false;
            continue;
        }
        if !skip {
            cleaned.push_str(line);
            cleaned.push('\n');
        }
    }
    atomic_write(&claude_md, &cleaned)
}

fn atomic_write(path: &Path, content: &str) -> std::io::Result<()> {
    umadev_state::fs::atomic_write(path, content.as_bytes())
}

fn ensure_relative_parent(base: &Path, relative: Option<&Path>) -> std::io::Result<PathBuf> {
    let mut current = base.to_path_buf();
    let Some(relative) = relative else {
        return Ok(current);
    };
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "skill knowledge path escaped its destination",
            ));
        };
        let name = name.to_str().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "skill knowledge path is not valid UTF-8",
            )
        })?;
        current = umadev_state::fs::ensure_real_child_dir(&current, name)?;
    }
    Ok(current)
}

/// Reject a name that isn't a single safe path component (`..` / absolute /
/// separators would let `join` escape the skills dir — arbitrary delete/write).
fn safe_component(name: &str) -> std::io::Result<()> {
    use std::path::{Component, Path};
    let mut comps = Path::new(name).components();
    if matches!(comps.next(), Some(Component::Normal(_))) && comps.next().is_none() {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("unsafe name `{name}` — must be a single path component"),
        ))
    }
}

/// `.umadev/skills/` used to contain UmaDev's internal learned-skill ledger as
/// well as user-installed packages. New versions migrate that ledger away, but
/// old files may remain for downgrade/recovery. Never let a package install or
/// removal claim those names — notably `remove("receipts")` previously removed
/// the complete attribution history even though it had no package manifest.
fn reject_learned_skill_reserved_name(name: &str) -> std::io::Result<()> {
    let normalized = name.trim().to_ascii_lowercase();
    let reserved = matches!(
        normalized.as_str(),
        "receipts"
            | "skills.jsonl"
            | "migration-v1.json"
            | "learned-skills"
            | ".write.lock"
            | ".learned-skills-migration"
            | ".migration-v1.json.replace-pending"
            | ".skills.jsonl.replace-pending"
    ) || normalized.starts_with(".skills.jsonl.")
        || normalized.starts_with(".migration-v1.json.")
        || normalized.starts_with(".learned-skills-");
    if reserved {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("reserved UmaDev learned-skill name `{name}`"),
        ))
    } else {
        Ok(())
    }
}

/// A manifest knowledge path may have subdirectories but must stay UNDER the
/// skill source dir — every component must be `Normal` (no `..`, root, or prefix).
fn is_safe_relpath(rel: &str) -> bool {
    use std::path::{Component, Path};
    let p = Path::new(rel);
    !rel.is_empty() && p.components().all(|c| matches!(c, Component::Normal(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_skill_dir(tmp: &Path, name: &str) -> PathBuf {
        let dir = tmp.join("source-skill");
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = SkillManifest {
            name: name.into(),
            description: "Test skill".into(),
            version: "1.0".into(),
            knowledge: vec!["guide.md".into()],
            rules: vec!["UD-ARCH-001".into()],
            system_prompt: "Always use TypeScript strict mode.".into(),
        };
        std::fs::write(
            dir.join("manifest.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();
        std::fs::write(dir.join("guide.md"), "# Guide\nUse strict mode.").unwrap();
        dir
    }

    #[test]
    fn install_creates_skill_and_copies_knowledge() {
        let tmp = tempfile::TempDir::new().unwrap();
        let registry = SkillRegistry::new(tmp.path());
        let source = make_skill_dir(tmp.path(), "react-pro");
        let result = registry.install(&source).unwrap();
        assert_eq!(result.name, "react-pro");
        assert_eq!(result.knowledge_copied, 1);
        assert!(result.prompt_updated);
    }

    #[test]
    fn list_shows_installed_skills() {
        let tmp = tempfile::TempDir::new().unwrap();
        let registry = SkillRegistry::new(tmp.path());
        let source = make_skill_dir(tmp.path(), "react-pro");
        registry.install(&source).unwrap();
        let skills = registry.list();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "react-pro");
    }

    #[test]
    fn remove_cleans_up() {
        let tmp = tempfile::TempDir::new().unwrap();
        let registry = SkillRegistry::new(tmp.path());
        let source = make_skill_dir(tmp.path(), "react-pro");
        registry.install(&source).unwrap();
        assert_eq!(registry.list().len(), 1);
        registry.remove("react-pro").unwrap();
        assert!(registry.list().is_empty());
    }

    #[test]
    fn install_missing_manifest_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        let registry = SkillRegistry::new(tmp.path());
        let empty = tmp.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(registry.install(&empty).is_err());
    }

    #[test]
    fn remove_nonexistent_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        let registry = SkillRegistry::new(tmp.path());
        assert!(registry.remove("nope").is_err());
    }

    #[test]
    fn learned_skill_internal_names_are_reserved_case_insensitively() {
        for name in [
            "receipts",
            "RECEIPTS",
            "skills.jsonl",
            "migration-v1.json",
            "learned-skills",
            ".write.lock",
            ".migration-v1.json.replace-pending",
            ".migration-v1.json.123.tmp",
            ".skills.jsonl.replace-pending",
            ".skills.jsonl.123.tmp",
            ".learned-skills-migration-old",
        ] {
            assert!(
                reject_learned_skill_reserved_name(name).is_err(),
                "{name} must remain owned by the learned-skill migration"
            );
        }
        assert!(reject_learned_skill_reserved_name("ordinary-package").is_ok());
    }

    #[test]
    fn package_install_and_remove_cannot_claim_legacy_receipts() {
        let tmp = tempfile::TempDir::new().unwrap();
        let registry = SkillRegistry::new(tmp.path());
        let receipts = tmp.path().join(".umadev/skills/receipts");
        std::fs::create_dir_all(&receipts).unwrap();
        let sentinel = receipts.join("sr1-sentinel.receipt.json");
        std::fs::write(&sentinel, "keep").unwrap();

        assert_eq!(
            registry.remove("receipts").unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "keep");

        let source = make_skill_dir(tmp.path(), "receipts");
        assert_eq!(
            registry.install(&source).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "keep");
        assert!(registry.list().is_empty());
    }

    #[test]
    fn prompt_block_has_markers() {
        let tmp = tempfile::TempDir::new().unwrap();
        let registry = SkillRegistry::new(tmp.path());
        let source = make_skill_dir(tmp.path(), "test-skill");
        registry.install(&source).unwrap();
        let claude_md = std::fs::read_to_string(tmp.path().join("CLAUDE.md")).unwrap();
        assert!(claude_md.contains("<!-- skill:test-skill -->"));
        assert!(claude_md.contains("<!-- /skill:test-skill -->"));
    }

    #[test]
    fn partial_install_is_recoverable_manifest_written_first() {
        // If a mid-install copy fails, the manifest must already be on disk so
        // the skill is visible to list() and cleanable by remove(). We force a
        // copy failure by pointing a knowledge entry at a DIRECTORY (fs::copy of
        // a dir errors on every platform).
        let tmp = tempfile::TempDir::new().unwrap();
        let registry = SkillRegistry::new(tmp.path());
        let source = tmp.path().join("bad-skill");
        std::fs::create_dir_all(&source).unwrap();
        // A knowledge entry that is itself a directory → copy fails.
        std::fs::create_dir_all(source.join("a-dir")).unwrap();
        let manifest = SkillManifest {
            name: "half".into(),
            description: "broken".into(),
            version: "1.0".into(),
            knowledge: vec!["a-dir".into()],
            rules: vec![],
            system_prompt: String::new(),
        };
        std::fs::write(
            source.join("manifest.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();

        // Install errors (the copy fails) ...
        let res = registry.install(&source);
        assert!(res.is_err(), "expected the dir-copy to fail the install");
        // ... but the manifest was committed first, so the skill is now
        // listable AND removable — no orphaned, unrecoverable half-install.
        assert_eq!(registry.list().len(), 1);
        assert_eq!(registry.list()[0].name, "half");
        registry.remove("half").unwrap();
        assert!(registry.list().is_empty());
    }

    #[test]
    fn reinstall_does_not_duplicate_prompt() {
        let tmp = tempfile::TempDir::new().unwrap();
        let registry = SkillRegistry::new(tmp.path());
        let source = make_skill_dir(tmp.path(), "test-skill");
        registry.install(&source).unwrap();
        let result = registry.install(&source).unwrap();
        assert!(!result.prompt_updated); // already present
    }

    #[test]
    fn remove_preserves_unrelated_claude_md_content() {
        // The atomic CLAUDE.md rewrite on remove must keep the user's own text
        // intact — only the skill's marked block is stripped.
        let tmp = tempfile::TempDir::new().unwrap();
        let registry = SkillRegistry::new(tmp.path());
        std::fs::write(
            tmp.path().join("CLAUDE.md"),
            "# My project rules\nKeep this line.\n",
        )
        .unwrap();
        let source = make_skill_dir(tmp.path(), "test-skill");
        registry.install(&source).unwrap();
        registry.remove("test-skill").unwrap();
        let after = std::fs::read_to_string(tmp.path().join("CLAUDE.md")).unwrap();
        assert!(after.contains("Keep this line."), "user content preserved");
        assert!(
            !after.contains("<!-- skill:test-skill -->"),
            "block removed"
        );
    }

    #[test]
    fn install_merges_declared_rules_into_rules_toml() {
        // The declared clauses must actually be ENFORCED (removed from the
        // `[disabled]` opt-out list) — not merely counted.
        let tmp = tempfile::TempDir::new().unwrap();
        let registry = SkillRegistry::new(tmp.path());
        std::fs::create_dir_all(tmp.path().join(".umadev")).unwrap();
        std::fs::write(
            tmp.path().join(".umadev/rules.toml"),
            "[disabled]\nclauses = [\"UD-ARCH-001\", \"UD-ARCH-999\"]\n",
        )
        .unwrap();
        // make_skill_dir declares rules = ["UD-ARCH-001"].
        let source = make_skill_dir(tmp.path(), "react-pro");
        let result = registry.install(&source).unwrap();
        assert_eq!(result.rules_added, 1, "one declared clause merged");
        let after = std::fs::read_to_string(tmp.path().join(".umadev/rules.toml")).unwrap();
        assert!(
            !after.contains("UD-ARCH-001"),
            "the skill's clause is un-disabled (now enforced): {after}"
        );
        assert!(
            after.contains("UD-ARCH-999"),
            "an unrelated disabled clause is preserved: {after}"
        );
    }

    #[test]
    fn install_preserves_knowledge_subpaths_no_clobber() {
        // Two same-named docs in different subdirs must NOT overwrite each other.
        let tmp = tempfile::TempDir::new().unwrap();
        let registry = SkillRegistry::new(tmp.path());
        let source = tmp.path().join("multi-skill");
        std::fs::create_dir_all(source.join("a")).unwrap();
        std::fs::create_dir_all(source.join("b")).unwrap();
        std::fs::write(source.join("a/guide.md"), "# A guide").unwrap();
        std::fs::write(source.join("b/guide.md"), "# B guide").unwrap();
        let manifest = SkillManifest {
            name: "multi".into(),
            description: "two same-named docs".into(),
            version: "1.0".into(),
            knowledge: vec!["a/guide.md".into(), "b/guide.md".into()],
            rules: vec![],
            system_prompt: String::new(),
        };
        std::fs::write(
            source.join("manifest.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();
        let result = registry.install(&source).unwrap();
        assert_eq!(result.knowledge_copied, 2);
        let base = tmp.path().join("knowledge/skills/multi");
        assert_eq!(
            std::fs::read_to_string(base.join("a/guide.md")).unwrap(),
            "# A guide"
        );
        assert_eq!(
            std::fs::read_to_string(base.join("b/guide.md")).unwrap(),
            "# B guide",
            "b/guide.md must not be clobbered by a/guide.md"
        );
    }

    #[cfg(unix)]
    #[test]
    fn install_skips_symlinked_knowledge_pointing_outside_skill_dir() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::TempDir::new().unwrap();
        let registry = SkillRegistry::new(tmp.path());
        // A secret outside the skill source dir.
        let secret = tmp.path().join("secret.md");
        std::fs::write(&secret, "# host secret").unwrap();

        let source = tmp.path().join("evil-skill");
        std::fs::create_dir_all(&source).unwrap();
        // The declared knowledge file is a symlink to the secret (lexically a
        // single Normal component — clears is_safe_relpath).
        symlink(&secret, source.join("guide.md")).unwrap();
        let manifest = SkillManifest {
            name: "evil".into(),
            description: "exfil attempt".into(),
            version: "1.0".into(),
            knowledge: vec!["guide.md".into()],
            rules: vec![],
            system_prompt: String::new(),
        };
        std::fs::write(
            source.join("manifest.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let result = registry.install(&source).unwrap();
        // The symlink was skipped, so nothing was copied ...
        assert_eq!(result.knowledge_copied, 0);
        // ... and the secret did NOT land in the RAG knowledge dir.
        let landed = tmp
            .path()
            .join("knowledge")
            .join("skills")
            .join("evil")
            .join("guide.md");
        assert!(!landed.exists(), "symlinked secret must not be copied in");
    }

    #[test]
    fn remove_refuses_a_corrupt_manifest_without_partial_cleanup() {
        let tmp = tempfile::TempDir::new().unwrap();
        let registry = SkillRegistry::new(tmp.path());
        let source = make_skill_dir(tmp.path(), "kept-skill");
        registry.install(&source).unwrap();
        let skill_dir = tmp.path().join(".umadev/skills/kept-skill");
        let knowledge = tmp.path().join("knowledge/skills/kept-skill/guide.md");
        std::fs::write(skill_dir.join("manifest.json"), "{ damaged").unwrap();

        let error = registry.remove("kept-skill").unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(skill_dir.exists());
        assert!(knowledge.exists());
        assert!(std::fs::read_to_string(tmp.path().join("CLAUDE.md"))
            .unwrap()
            .contains("<!-- skill:kept-skill -->"));
    }

    #[test]
    fn install_refuses_to_claim_rules_from_invalid_toml() {
        let tmp = tempfile::TempDir::new().unwrap();
        let registry = SkillRegistry::new(tmp.path());
        std::fs::create_dir_all(tmp.path().join(".umadev")).unwrap();
        let rules = tmp.path().join(".umadev/rules.toml");
        std::fs::write(&rules, "[disabled\nclauses = [").unwrap();
        let source = make_skill_dir(tmp.path(), "strict-rules");

        let error = registry.install(&source).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(
            std::fs::read_to_string(&rules).unwrap(),
            "[disabled\nclauses = ["
        );
    }

    #[test]
    fn install_refuses_to_claim_rules_from_wrong_field_shape() {
        let tmp = tempfile::TempDir::new().unwrap();
        let registry = SkillRegistry::new(tmp.path());
        std::fs::create_dir_all(tmp.path().join(".umadev")).unwrap();
        let rules = tmp.path().join(".umadev/rules.toml");
        let original = "[disabled]\nclauses = \"UD-ARCH-001\"\n";
        std::fs::write(&rules, original).unwrap();
        let source = make_skill_dir(tmp.path(), "wrong-rules-shape");

        let error = registry.install(&source).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read_to_string(&rules).unwrap(), original);
    }

    #[test]
    fn remove_refuses_an_incomplete_prompt_block_without_truncating_claude_md() {
        let tmp = tempfile::TempDir::new().unwrap();
        let registry = SkillRegistry::new(tmp.path());
        let source = make_skill_dir(tmp.path(), "broken-prompt");
        registry.install(&source).unwrap();
        let claude_md = tmp.path().join("CLAUDE.md");
        let damaged = std::fs::read_to_string(&claude_md)
            .unwrap()
            .replace("<!-- /skill:broken-prompt -->", "user content after block");
        std::fs::write(&claude_md, &damaged).unwrap();

        let error = registry.remove("broken-prompt").unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read_to_string(&claude_md).unwrap(), damaged);
        assert!(tmp.path().join(".umadev/skills/broken-prompt").exists());
        assert!(tmp
            .path()
            .join("knowledge/skills/broken-prompt/guide.md")
            .exists());
    }

    #[cfg(unix)]
    #[test]
    fn remove_surfaces_prompt_read_errors_before_deleting_skill_data() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::TempDir::new().unwrap();
        let registry = SkillRegistry::new(tmp.path());
        let source = make_skill_dir(tmp.path(), "linked-prompt");
        registry.install(&source).unwrap();
        let claude_md = tmp.path().join("CLAUDE.md");
        std::fs::remove_file(&claude_md).unwrap();
        let outside = tmp.path().join("outside.md");
        std::fs::write(&outside, "# Outside").unwrap();
        symlink(&outside, &claude_md).unwrap();

        assert!(registry.remove("linked-prompt").is_err());
        assert!(tmp.path().join(".umadev/skills/linked-prompt").exists());
        assert!(tmp
            .path()
            .join("knowledge/skills/linked-prompt/guide.md")
            .exists());
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "# Outside");
    }
}
