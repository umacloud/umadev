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
        let manifest_path = source_dir.join("manifest.json");
        let manifest_text = std::fs::read_to_string(&manifest_path).map_err(|e| {
            std::io::Error::new(
                e.kind(),
                format!(
                    "failed to read manifest.json in {}: {e}",
                    source_dir.display()
                ),
            )
        })?;
        let manifest: SkillManifest = serde_json::from_str(&manifest_text).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid manifest.json: {e}"),
            )
        })?;

        // A malicious manifest name (`../../..`) would escape skills_dir.
        safe_component(&manifest.name)?;
        let dest = self.skills_dir().join(&manifest.name);
        std::fs::create_dir_all(&dest)?;

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
        let knowledge_dest = self
            .project_root
            .join("knowledge")
            .join("skills")
            .join(&manifest.name);
        for rel_path in &manifest.knowledge {
            // A traversal path (`../../../etc/passwd`) would copy arbitrary host
            // files into the project — skip it rather than honour it.
            if !is_safe_relpath(rel_path) {
                continue;
            }
            let src = source_dir.join(rel_path);
            // A SYMLINK clears the lexical `is_safe_relpath` check (its own path
            // components are all `Normal`) yet can point at any host file —
            // `fs::copy` would follow it and pull, say, ~/.ssh/id_rsa into the
            // RAG index. `symlink_metadata` doesn't follow the link, so we can
            // skip a symlinked source rather than honour it.
            let Ok(meta) = std::fs::symlink_metadata(&src) else {
                continue; // missing / unreadable
            };
            if meta.file_type().is_symlink() {
                continue; // refuse to follow a symlink out of the skill dir
            }
            if meta.file_type().is_file() {
                // Preserve the manifest-relative subpath — flattening to the
                // basename let `a/guide.md` and `b/guide.md` silently overwrite
                // each other. `is_safe_relpath` (above) guarantees `rel_path` is
                // all-`Normal`, so the join stays under `knowledge_dest`. Mirrors
                // knowledge_manager's subpath-preserving copy.
                let copy_dst = knowledge_dest.join(rel_path);
                std::fs::create_dir_all(copy_dst.parent().unwrap_or(&knowledge_dest))?;
                std::fs::copy(&src, &copy_dst)?;
                knowledge_copied += 1;
            } else {
                // A non-file, non-symlink entry (e.g. a directory) is an invalid
                // knowledge declaration — surface it as an install error rather
                // than silently dropping it. The manifest is already committed
                // (above), so the failed install stays recoverable.
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "knowledge entry `{rel_path}` is not a regular file (a directory?); \
                         list individual `.md` files in the manifest"
                    ),
                ));
            }
        }

        // Append system prompt to CLAUDE.md.
        let prompt_updated = if manifest.system_prompt.is_empty() {
            false
        } else {
            let claude_md = self.project_root.join("CLAUDE.md");
            let existing = std::fs::read_to_string(&claude_md).unwrap_or_default();
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
        // them). Fail-open — never fails the install.
        let rules_added = self.merge_skill_rules(&manifest.rules);

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
    /// disabled". Idempotent + fail-open — a missing rules.toml means the clauses
    /// are already in force (nothing to write); an unparseable file is left
    /// untouched (governance's own loader already warns and enforces all rules).
    /// Returns the number of declared clauses now guaranteed enforced.
    fn merge_skill_rules(&self, clauses: &[String]) -> usize {
        if clauses.is_empty() {
            return 0;
        }
        let path = self.project_root.join(".umadev").join("rules.toml");
        // Absent file → every clause is enforced by default; the skill's
        // declared clauses are already in force.
        let Ok(text) = std::fs::read_to_string(&path) else {
            return clauses.len();
        };
        // Do NOT clobber a file we can't parse — governance's loader falls back
        // to all-rules-on, which enforces the skill's clauses anyway.
        let Ok(mut doc) = text.parse::<toml_edit::DocumentMut>() else {
            return clauses.len();
        };
        let want: std::collections::HashSet<String> =
            clauses.iter().map(|c| c.to_ascii_lowercase()).collect();
        let mut changed = false;
        if let Some(arr) = doc
            .get_mut("disabled")
            .and_then(toml_edit::Item::as_table_like_mut)
            .and_then(|t| t.get_mut("clauses"))
            .and_then(toml_edit::Item::as_array_mut)
        {
            let before = arr.len();
            arr.retain(|v| {
                v.as_str()
                    .is_none_or(|s| !want.contains(&s.to_ascii_lowercase()))
            });
            changed = arr.len() != before;
        }
        if changed {
            let _ = atomic_write(&path, &doc.to_string());
        }
        clauses.len()
    }

    /// List all installed skills.
    pub fn list(&self) -> Vec<SkillManifest> {
        let dir = self.skills_dir();
        let mut skills = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let manifest_path = entry.path().join("manifest.json");
                if let Ok(text) = std::fs::read_to_string(&manifest_path) {
                    if let Ok(manifest) = serde_json::from_str::<SkillManifest>(&text) {
                        skills.push(manifest);
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
        let dir = self.skills_dir().join(name);
        if !dir.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("skill '{name}' is not installed"),
            ));
        }

        // Load manifest to know what to clean up.
        let manifest_path = dir.join("manifest.json");
        if let Ok(text) = std::fs::read_to_string(&manifest_path) {
            if let Ok(manifest) = serde_json::from_str::<SkillManifest>(&text) {
                // Remove knowledge docs.
                let knowledge_dir = self
                    .project_root
                    .join("knowledge")
                    .join("skills")
                    .join(name);
                let _ = std::fs::remove_dir_all(&knowledge_dir);

                // Remove system prompt block from CLAUDE.md.
                if !manifest.system_prompt.is_empty() {
                    let claude_md = self.project_root.join("CLAUDE.md");
                    if let Ok(content) = std::fs::read_to_string(&claude_md) {
                        let start_marker = format!("<!-- skill:{} -->", name);
                        let end_marker = format!("<!-- /skill:{} -->", name);
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
                        // Atomic rewrite — never leave a half-written CLAUDE.md.
                        let _ = atomic_write(&claude_md, &cleaned);
                    }
                }
            }
        }

        // Remove the skill directory.
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }
}

/// Atomically write `content` to `path` (write a per-process temp file in the
/// same dir, then rename). A same-filesystem rename is atomic on POSIX, so a
/// crash mid-write never leaves a truncated manifest a later `list()` would
/// choke on — readers see either the old file or the complete new one. Falls
/// back to a direct write only if the rename itself fails (cross-device).
fn atomic_write(path: &Path, content: &str) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, content)?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        // Last resort: a direct write. Less crash-safe, but better than failing
        // the install outright on a filesystem that rejects the rename.
        std::fs::write(path, content).map_err(|_| e)?;
    }
    Ok(())
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
}
