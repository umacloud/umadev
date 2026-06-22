//! Knowledge management — add/list/search custom documents in the RAG index.
//!
//! Users can add their own domain documents to UmaDev's RAG knowledge base.
//! Documents are indexed with the existing BM25 + optional vector retrieval
//! layer, making them citable by the host during research/generation phases.
//!
//! ## Usage
//! ```bash
//! umadev knowledge-manage add ./my-docs/        # add a directory of .md files
//! umadev knowledge-manage add ./api-spec.md     # add a single file
//! umadev knowledge-manage list                  # list all custom knowledge
//! umadev knowledge-manage search "React Hooks"  # BM25 search across all knowledge
//! umadev knowledge-manage remove my-api-spec    # remove by registered name
//! ```

use std::path::{Path, PathBuf};

/// The custom knowledge directory: `knowledge/custom/`.
/// Files here are picked up by the existing RAG indexer automatically.
const CUSTOM_DIR: &str = "knowledge/custom";

/// Registry of custom-added documents (stored in `.umadev/knowledge.json`).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct KnowledgeRegistry {
    /// Map of registered name → source path (for display/removal).
    #[serde(default)]
    pub entries: std::collections::BTreeMap<String, KnowledgeEntry>,
}

/// One knowledge entry.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct KnowledgeEntry {
    /// Display name.
    pub name: String,
    /// Original source path.
    pub source: String,
    /// Number of files copied.
    pub file_count: usize,
}

impl KnowledgeRegistry {
    /// Load from `.umadev/knowledge.json`. Fail-open: empty if missing.
    pub fn load(project_root: &Path) -> Self {
        let path = project_root.join(".umadev").join("knowledge.json");
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Save to `.umadev/knowledge.json` atomically (temp file + rename, like
    /// `mcp_manager`). A bare `fs::write` could be interrupted mid-write,
    /// leaving truncated JSON that `load`'s `unwrap_or_default()` then silently
    /// discards — wiping the ENTIRE knowledge registry. A same-filesystem
    /// rename is atomic on POSIX, so a reader sees either the old file or the
    /// complete new one, never a half-written one.
    pub fn save(&self, project_root: &Path) -> std::io::Result<()> {
        let dir = project_root.join(".umadev");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("knowledge.json");
        let json = serde_json::to_string_pretty(self).unwrap_or_default();
        // Per-process temp name so concurrent writers can't share + clobber the
        // same scratch file before the rename.
        let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
        std::fs::write(&tmp, json + "\n")?;
        if let Err(e) = std::fs::rename(&tmp, &path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        Ok(())
    }
}

/// Result of adding knowledge.
#[derive(Debug)]
pub struct AddResult {
    pub name: String,
    pub files_copied: usize,
    pub dest_dir: PathBuf,
}

/// Reject a name that isn't a single safe path component. `..`, an absolute
/// path, or anything with a separator would let `join` escape the custom-knowledge
/// dir — enabling arbitrary-directory deletion (`remove_dir_all`) or writes
/// outside the project.
fn safe_component(name: &str) -> std::io::Result<()> {
    use std::path::{Component, Path};
    let mut comps = Path::new(name).components();
    if matches!(comps.next(), Some(Component::Normal(_))) && comps.next().is_none() {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "unsafe name `{name}` — must be a single path component (no '/', '..', or absolute path)"
            ),
        ))
    }
}

/// Add a file or directory to the custom knowledge base.
pub fn add_knowledge(
    project_root: &Path,
    source: &Path,
    name: Option<&str>,
) -> std::io::Result<AddResult> {
    if !source.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("source not found: {}", source.display()),
        ));
    }

    let entry_name = name.map_or_else(
        || {
            source
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("custom")
                .to_string()
        },
        String::from,
    );

    safe_component(&entry_name)?;
    let dest_dir = project_root.join(CUSTOM_DIR).join(&entry_name);
    std::fs::create_dir_all(&dest_dir)?;

    let mut files_copied = 0;
    if source.is_dir() {
        for entry in walk_source(source) {
            if entry.extension().is_some_and(|e| e == "md" || e == "txt") {
                // Preserve the source's subdirectory structure — flattening to
                // the basename would silently overwrite same-named files from
                // different subdirs (a/x.md and b/x.md collide).
                let rel = entry.strip_prefix(source).unwrap_or(&entry);
                let dest = dest_dir.join(rel);
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::copy(&entry, &dest)?;
                files_copied += 1;
            }
        }
    } else if source.is_file() {
        let dest = dest_dir.join(source.file_name().unwrap_or_default());
        std::fs::copy(source, &dest)?;
        files_copied = 1;
    }

    // Update registry.
    let mut registry = KnowledgeRegistry::load(project_root);
    registry.entries.insert(
        entry_name.clone(),
        KnowledgeEntry {
            name: entry_name.clone(),
            source: source.to_string_lossy().to_string(),
            file_count: files_copied,
        },
    );
    registry.save(project_root)?;

    Ok(AddResult {
        name: entry_name,
        files_copied,
        dest_dir,
    })
}

/// Remove custom knowledge by name.
pub fn remove_knowledge(project_root: &Path, name: &str) -> std::io::Result<()> {
    safe_component(name)?;
    let mut registry = KnowledgeRegistry::load(project_root);
    if !registry.entries.contains_key(name) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("knowledge '{name}' not found"),
        ));
    }
    let dir = project_root.join(CUSTOM_DIR).join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    registry.entries.remove(name);
    registry.save(project_root)?;
    Ok(())
}

/// List all custom knowledge entries.
pub fn list_knowledge(project_root: &Path) -> Vec<KnowledgeEntry> {
    let registry = KnowledgeRegistry::load(project_root);
    registry.entries.values().cloned().collect()
}

/// Simple BM25-style search across all custom knowledge files.
/// Returns matching file paths and a snippet preview.
pub fn search_knowledge(project_root: &Path, query: &str, max_results: usize) -> Vec<SearchResult> {
    let custom_dir = project_root.join(CUSTOM_DIR);
    if !custom_dir.exists() {
        return vec![];
    }
    let query_lower = query.to_ascii_lowercase();
    let query_terms: Vec<&str> = query_lower.split_whitespace().collect();
    let mut results: Vec<SearchResult> = Vec::new();

    for entry in walk_source(&custom_dir) {
        if !entry.extension().is_some_and(|e| e == "md" || e == "txt") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&entry) else {
            continue;
        };
        let content_lower = content.to_ascii_lowercase();
        let score: usize = query_terms
            .iter()
            .map(|term| content_lower.matches(term).count())
            .sum();
        if score > 0 {
            let preview = content
                .lines()
                .find(|line| {
                    let ll = line.to_ascii_lowercase();
                    query_terms.iter().any(|term| ll.contains(term))
                })
                .unwrap_or("")
                .chars()
                .take(80)
                .collect();
            results.push(SearchResult {
                path: entry.to_string_lossy().to_string(),
                score,
                preview,
            });
        }
    }

    results.sort_by_key(|r| std::cmp::Reverse(r.score));
    results.truncate(max_results);
    results
}

/// One search result.
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub path: String,
    pub score: usize,
    pub preview: String,
}

/// Recursively walk a directory, yielding file paths.
fn walk_source(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            files.extend(walk_source(&path));
        } else if ft.is_file() {
            files.push(path);
        }
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_single_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("guide.md");
        std::fs::write(&src, "# Guide\nBest practices for React.").unwrap();
        let result = add_knowledge(tmp.path(), &src, Some("react-guide")).unwrap();
        assert_eq!(result.name, "react-guide");
        assert_eq!(result.files_copied, 1);
        assert!(result.dest_dir.exists());
    }

    #[test]
    fn add_directory() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src_dir = tmp.path().join("my-docs");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(src_dir.join("a.md"), "# A").unwrap();
        std::fs::write(src_dir.join("b.md"), "# B").unwrap();
        std::fs::write(src_dir.join("c.txt"), "text").unwrap();
        let result = add_knowledge(tmp.path(), &src_dir, Some("my-docs")).unwrap();
        assert_eq!(result.files_copied, 3);
    }

    #[test]
    fn list_shows_entries() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("guide.md");
        std::fs::write(&src, "# Guide").unwrap();
        add_knowledge(tmp.path(), &src, Some("test")).unwrap();
        let list = list_knowledge(tmp.path());
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "test");
    }

    #[test]
    fn remove_cleans_up() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("guide.md");
        std::fs::write(&src, "# Guide").unwrap();
        add_knowledge(tmp.path(), &src, Some("test")).unwrap();
        assert_eq!(list_knowledge(tmp.path()).len(), 1);
        remove_knowledge(tmp.path(), "test").unwrap();
        assert!(list_knowledge(tmp.path()).is_empty());
    }

    #[test]
    fn search_finds_matches() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("guide.md");
        std::fs::write(&src, "# React Hooks\nuseState is the most common hook.").unwrap();
        add_knowledge(tmp.path(), &src, Some("hooks")).unwrap();
        let results = search_knowledge(tmp.path(), "useState hook", 5);
        assert!(!results.is_empty());
        assert!(results[0].score > 0);
        let preview_lower = results[0].preview.to_ascii_lowercase();
        assert!(preview_lower.contains("usestate") || preview_lower.contains("hook"));
    }

    #[test]
    fn search_no_match_returns_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("guide.md");
        std::fs::write(&src, "# React").unwrap();
        add_knowledge(tmp.path(), &src, Some("r")).unwrap();
        let results = search_knowledge(tmp.path(), "Kotlin coroutines", 5);
        assert!(results.is_empty());
    }

    #[test]
    fn add_missing_source_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(add_knowledge(tmp.path(), Path::new("/nonexistent"), None).is_err());
    }

    #[test]
    fn remove_nonexistent_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(remove_knowledge(tmp.path(), "nope").is_err());
    }

    #[test]
    fn save_is_atomic_and_leaves_no_temp_file() {
        // After an atomic save: the registry round-trips intact AND no `.tmp-*`
        // scratch file is left behind in `.umadev/`.
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("guide.md");
        std::fs::write(&src, "# Guide").unwrap();
        add_knowledge(tmp.path(), &src, Some("kept")).unwrap();

        let reloaded = KnowledgeRegistry::load(tmp.path());
        assert!(reloaded.entries.contains_key("kept"));

        let udir = tmp.path().join(".umadev");
        let leftover: Vec<_> = std::fs::read_dir(&udir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .contains("knowledge.json.tmp")
            })
            .collect();
        assert!(leftover.is_empty(), "atomic save left a temp file behind");
    }
}
