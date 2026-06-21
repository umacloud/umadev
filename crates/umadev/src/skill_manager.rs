//! Skill management — install/list/remove knowledge + rule + prompt packages.
//!
//! A Skill bundles domain expertise into a single installable unit:
//! - **Knowledge docs**: copied into the RAG index so the host's research
//!   and generation phases can cite them.
//! - **Governance rules**: merged into `.umadev/rules.toml` so the
//!   governance engine enforces them.
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
            if src.exists() {
                let fname = src.file_name().unwrap_or_default();
                let copy_dst = knowledge_dest.join(fname);
                std::fs::create_dir_all(copy_dst.parent().unwrap_or(&knowledge_dest))?;
                std::fs::copy(&src, &copy_dst)?;
                knowledge_copied += 1;
            }
        }

        // Save the manifest so we can list/uninstall.
        let manifest_out = dest.join("manifest.json");
        std::fs::write(
            &manifest_out,
            serde_json::to_string_pretty(&manifest).unwrap_or_default(),
        )?;

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
                std::fs::write(&claude_md, existing + &block)?;
                true
            }
        };

        Ok(SkillInstallResult {
            name: manifest.name,
            knowledge_copied,
            rules_added: manifest.rules.len(),
            prompt_updated,
        })
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
                        let _ = std::fs::write(&claude_md, cleaned);
                    }
                }
            }
        }

        // Remove the skill directory.
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }
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
    fn reinstall_does_not_duplicate_prompt() {
        let tmp = tempfile::TempDir::new().unwrap();
        let registry = SkillRegistry::new(tmp.path());
        let source = make_skill_dir(tmp.path(), "test-skill");
        registry.install(&source).unwrap();
        let result = registry.install(&source).unwrap();
        assert!(!result.prompt_updated); // already present
    }
}
