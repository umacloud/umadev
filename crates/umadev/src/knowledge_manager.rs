//! Knowledge management — add/list/search custom documents in the RAG index.
//!
//! Users can add their own domain documents to UmaDev's RAG knowledge base.
//! Documents are indexed with the existing BM25 + optional vector retrieval
//! layer, making them citable by the host during research/generation phases.
//!
//! **Markdown only.** The runtime RAG walker indexes `.md` exclusively, so
//! `add`/`search` accept only `.md`. A `.txt` would print `"[ok] Added"` yet the
//! base would never index it — so we reject non-markdown up front with a clear
//! message instead of staging a silent non-delivery.
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
use std::time::{Duration, Instant};

use fs2::FileExt as _;
use umadev_state::fs::RootedDir;

/// The custom knowledge directory: `knowledge/custom/`.
/// Files here are picked up by the existing RAG indexer automatically.
const CUSTOM_DIR: &str = "knowledge/custom";
const MAX_REGISTRY_BYTES: u64 = 4 * 1024 * 1024;
const MAX_DOCUMENT_BYTES: u64 = 8 * 1024 * 1024;
const MAX_TOTAL_DOCUMENT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_DOCUMENT_FILES: usize = 1_024;
const MAX_WALK_DEPTH: usize = 32;
const MAX_WALK_ENTRIES: usize = 4_096;
const KNOWLEDGE_MUTATION_LOCK: &str = ".umadev/knowledge-manager.lock";
const KNOWLEDGE_LOCK_TIMEOUT: Duration = Duration::from_secs(2);
const KNOWLEDGE_LOCK_POLL: Duration = Duration::from_millis(5);

fn with_mutation_lock<T>(
    project_root: &Path,
    operation: impl FnOnce() -> std::io::Result<T>,
) -> std::io::Result<T> {
    let root = RootedDir::open_no_follow(project_root)?;
    root.ensure_dir(Path::new(".umadev"), false)?;
    let lock = root.open_private_lock(Path::new(KNOWLEDGE_MUTATION_LOCK), false)?;
    let deadline = Instant::now() + KNOWLEDGE_LOCK_TIMEOUT;
    loop {
        match lock.try_lock_exclusive() {
            Ok(()) => break,
            Err(error)
                if umadev_state::fs::lock_error_is_contention(&error)
                    && Instant::now() < deadline =>
            {
                std::thread::sleep(KNOWLEDGE_LOCK_POLL);
            }
            Err(error) if umadev_state::fs::lock_error_is_contention(&error) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "another knowledge update is still running",
                ));
            }
            Err(error) => return Err(error),
        }
    }
    operation()
}

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
    /// Load from `.umadev/knowledge.json`. Read-only callers remain fail-open;
    /// add/remove use the strict loader below before changing any state.
    pub fn load(project_root: &Path) -> Self {
        Self::load_strict(project_root).unwrap_or_default()
    }

    /// Mutation paths must distinguish "missing" from "present but unreadable
    /// or corrupt". Treating both as an empty registry lets the next add replace
    /// every existing entry with a new one.
    fn load_strict(project_root: &Path) -> std::io::Result<Self> {
        let dir = project_root.join(".umadev");
        match std::fs::symlink_metadata(&dir) {
            Ok(metadata) if umadev_state::fs::metadata_is_real_dir(&metadata) => {}
            Ok(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "knowledge registry parent is not a real directory",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error),
        }
        let path = dir.join("knowledge.json");
        match umadev_state::fs::read_bounded(&path, MAX_REGISTRY_BYTES) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "{} is not a valid knowledge registry: {error}",
                        path.display()
                    ),
                )
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error),
        }
    }

    /// Save to `.umadev/knowledge.json` atomically (temp file + rename, like
    /// `mcp_manager`). A bare `fs::write` could be interrupted mid-write,
    /// leaving truncated JSON that a later mutation could otherwise silently
    /// replace — wiping the ENTIRE knowledge registry. A same-filesystem
    /// rename is atomic on POSIX, so a reader sees either the old file or the
    /// complete new one, never a half-written one.
    pub fn save(&self, project_root: &Path) -> std::io::Result<()> {
        let dir = project_root.join(".umadev");
        if !dir.exists() {
            std::fs::create_dir_all(&dir)?;
        }
        let path = dir.join("knowledge.json");
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        umadev_state::fs::atomic_write(&path, (json + "\n").as_bytes())
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
    with_mutation_lock(project_root, || {
        add_knowledge_unlocked(project_root, source, name)
    })
}

fn add_knowledge_unlocked(
    project_root: &Path,
    source: &Path,
    name: Option<&str>,
) -> std::io::Result<AddResult> {
    // Validate the registry before copying anything. A corrupt registry must
    // never be silently replaced after files have already been staged.
    let mut registry = KnowledgeRegistry::load_strict(project_root)?;
    let source_metadata = std::fs::symlink_metadata(source).map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!("source unavailable at {}: {error}", source.display()),
        )
    })?;
    if !umadev_state::fs::metadata_is_real_file(&source_metadata)
        && !umadev_state::fs::metadata_is_real_dir(&source_metadata)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "knowledge source must be a regular file or real directory",
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
    let custom_dir = ensure_custom_root(project_root)?;
    let dest_dir = custom_dir.join(&entry_name);
    // Track whether THIS call created the dest dir: on a failure path we must only clean
    // up a dir we ourselves created, never remove_dir_all a PRE-EXISTING same-named
    // entry already-indexed files (which would delete the user prior add and leave a
    // phantom registry entry with no files on disk).
    let dest_pre_existed = match std::fs::symlink_metadata(&dest_dir) {
        Ok(metadata) if umadev_state::fs::metadata_is_real_dir(&metadata) => true,
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "knowledge destination is not a real directory",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error),
    };
    let dest_dir = umadev_state::fs::ensure_real_child_dir(&custom_dir, &entry_name)?;

    let mut files_copied = 0;
    let mut skipped_non_md = 0;
    let mut skipped_symlink = 0;
    let mut total_bytes = 0_u64;
    if umadev_state::fs::metadata_is_real_dir(&source_metadata) {
        for entry in walk_source(source) {
            // ONLY `.md` is indexed: the runtime RAG walker is markdown-only, so
            // copying a `.txt` would print "[ok] Added" and let our own search
            // find it while the BASE never sees it — a silent non-delivery.
            // Restrict here so what we accept is exactly what the base indexes.
            if is_markdown(&entry) {
                // A SYMLINK with an innocuous `.md` name can point at any host
                // file; SKIP it (continue) rather than abort the whole add via
                // `?`, so legit `.md` siblings still get indexed. `symlink_metadata`
                // does NOT follow the link. (Mirrors skill_manager's skip.)
                if std::fs::symlink_metadata(&entry).map_or(true, |m| m.file_type().is_symlink()) {
                    skipped_symlink += 1;
                    continue;
                }
                // Preserve the source's subdirectory structure — flattening to
                // the basename would silently overwrite same-named files from
                // different subdirs (a/x.md and b/x.md collide).
                let rel = entry.strip_prefix(source).unwrap_or(&entry);
                let parent = ensure_relative_parent(&dest_dir, rel.parent())?;
                let dest = parent.join(rel.file_name().unwrap_or_default());
                let copied = copy_no_follow_symlink(
                    &entry,
                    &dest,
                    MAX_TOTAL_DOCUMENT_BYTES.saturating_sub(total_bytes),
                )?;
                total_bytes = total_bytes.saturating_add(copied);
                files_copied += 1;
            } else if entry.extension().is_some() {
                skipped_non_md += 1;
            }
        }
        if files_copied == 0 {
            // Don't leave an empty registered entry (or a stray dest dir) that the
            // base will never index — clean up and tell the user plainly what was
            // skipped (non-markdown and/or symlinked files).
            if !dest_pre_existed {
                let _ = remove_custom_tree(project_root, &entry_name);
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "no .md files found under {} ({skipped_non_md} non-markdown, \
                     {skipped_symlink} symlinked file(s) skipped). UmaDev only indexes regular \
                     Markdown (.md); convert other docs to .md and avoid symlinks.",
                    source.display()
                ),
            ));
        }
    } else if umadev_state::fs::metadata_is_real_file(&source_metadata) {
        if !is_markdown(source) {
            if !dest_pre_existed {
                let _ = remove_custom_tree(project_root, &entry_name);
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "`{}` is not a Markdown file. UmaDev only indexes `.md` (the runtime RAG \
                     walker is markdown-only); convert it to .md and retry.",
                    source.display()
                ),
            ));
        }
        let dest = dest_dir.join(source.file_name().unwrap_or_default());
        let _ = copy_no_follow_symlink(source, &dest, MAX_TOTAL_DOCUMENT_BYTES)?;
        files_copied = 1;
    }

    // Update registry.
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
    with_mutation_lock(project_root, || {
        remove_knowledge_unlocked(project_root, name)
    })
}

fn remove_knowledge_unlocked(project_root: &Path, name: &str) -> std::io::Result<()> {
    safe_component(name)?;
    let mut registry = KnowledgeRegistry::load_strict(project_root)?;
    if !registry.entries.contains_key(name) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("knowledge '{name}' not found"),
        ));
    }
    remove_custom_tree(project_root, name)?;
    registry.entries.remove(name);
    registry.save(project_root)?;
    Ok(())
}

fn remove_custom_tree(project_root: &Path, name: &str) -> std::io::Result<()> {
    let root = RootedDir::open_no_follow(project_root)?;
    let relative = Path::new(CUSTOM_DIR).join(name);
    let files = root.list_regular_tree(
        &relative,
        MAX_WALK_DEPTH,
        MAX_WALK_ENTRIES,
        MAX_DOCUMENT_FILES,
        MAX_TOTAL_DOCUMENT_BYTES,
    )?;
    for file in files {
        root.remove_regular_file(&file.relative)?;
    }
    root.remove_empty_directory_tree(&relative, MAX_WALK_DEPTH, MAX_WALK_ENTRIES)?;
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
    if !umadev_state::fs::real_dir(&custom_dir) {
        return vec![];
    }
    let query_lower = query.to_ascii_lowercase();
    let query_terms: Vec<&str> = query_lower.split_whitespace().collect();
    let mut results: Vec<SearchResult> = Vec::new();

    let mut remaining_bytes = MAX_TOTAL_DOCUMENT_BYTES;
    for entry in walk_source(&custom_dir) {
        if remaining_bytes == 0 {
            break;
        }
        // Mirror `add`: only `.md` is indexed/searched, since only `.md` ever
        // reaches the base's RAG walker.
        if !is_markdown(&entry) {
            continue;
        }
        let Ok(bytes) =
            umadev_state::fs::read_bounded(&entry, MAX_DOCUMENT_BYTES.min(remaining_bytes))
        else {
            continue;
        };
        remaining_bytes =
            remaining_bytes.saturating_sub(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        let Ok(content) = String::from_utf8(bytes) else {
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

/// Is this a Markdown file? `.md` (case-insensitive) is the ONLY extension the
/// runtime RAG walker indexes, so it's the only thing we accept.
fn is_markdown(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("md"))
}

/// Copy `src` → `dst`, but REFUSE a symlinked source. A symlink with an
/// innocuous `.md` name (and a `Normal` lexical path that clears the `..`
/// component check) could otherwise point at any host file — `/etc/passwd`,
/// `~/.ssh/id_rsa` — and `fs::copy` follows it, pulling that file into the RAG
/// index. `symlink_metadata` does NOT follow the link, so we can detect and
/// reject it before copying.
fn copy_no_follow_symlink(src: &Path, dst: &Path, remaining: u64) -> std::io::Result<u64> {
    if remaining == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "knowledge import exceeds the aggregate byte limit",
        ));
    }
    let bytes = umadev_state::fs::read_bounded(src, MAX_DOCUMENT_BYTES.min(remaining)).map_err(
        |error| {
            std::io::Error::new(
                error.kind(),
                format!("refusing to index `{}`: {error}", src.display()),
            )
        },
    )?;
    umadev_state::fs::atomic_write(dst, &bytes)?;
    Ok(u64::try_from(bytes.len()).unwrap_or(u64::MAX))
}

fn ensure_custom_root(project_root: &Path) -> std::io::Result<PathBuf> {
    let knowledge = umadev_state::fs::ensure_real_child_dir(project_root, "knowledge")?;
    umadev_state::fs::ensure_real_child_dir(&knowledge, "custom")
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
                "knowledge path escaped its destination",
            ));
        };
        let name = name.to_str().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "knowledge path is not valid UTF-8",
            )
        })?;
        current = umadev_state::fs::ensure_real_child_dir(&current, name)?;
    }
    Ok(current)
}

/// Recursively walk a directory, yielding file paths. Symlinked directories are
/// NOT descended into and symlinked files are still YIELDED (so the caller's
/// `copy_no_follow_symlink` can reject them with a clear message) — but the walk
/// itself never follows a directory symlink out of the tree.
fn walk_source(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut entries_seen = 0;
    walk_source_bounded(dir, &mut files, 0, &mut entries_seen);
    files.sort();
    files
}

fn walk_source_bounded(
    dir: &Path,
    files: &mut Vec<PathBuf>,
    depth: usize,
    entries_seen: &mut usize,
) {
    if depth > MAX_WALK_DEPTH
        || files.len() >= MAX_DOCUMENT_FILES
        || *entries_seen >= MAX_WALK_ENTRIES
    {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let remaining = MAX_WALK_ENTRIES.saturating_sub(*entries_seen);
    let mut entries = entries.flatten().take(remaining).collect::<Vec<_>>();
    *entries_seen = (*entries_seen).saturating_add(entries.len());
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        if files.len() >= MAX_DOCUMENT_FILES {
            return;
        }
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_symlink() {
            // A symlink (file or dir): yield it as a candidate file so the copy
            // step rejects it; never descend a symlinked directory.
            files.push(path);
        } else if ft.is_dir() {
            walk_source_bounded(&path, files, depth + 1, entries_seen);
        } else if ft.is_file() {
            files.push(path);
        }
    }
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
    fn add_directory_indexes_only_markdown() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src_dir = tmp.path().join("my-docs");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(src_dir.join("a.md"), "# A").unwrap();
        std::fs::write(src_dir.join("b.md"), "# B").unwrap();
        std::fs::write(src_dir.join("c.txt"), "text").unwrap(); // skipped: not .md
        let result = add_knowledge(tmp.path(), &src_dir, Some("my-docs")).unwrap();
        assert_eq!(result.files_copied, 2, "only the two .md files are indexed");
    }

    #[test]
    fn add_rejects_oversized_document() {
        let tmp = tempfile::TempDir::new().unwrap();
        let source = tmp.path().join("huge.md");
        let file = std::fs::File::create(&source).unwrap();
        file.set_len(MAX_DOCUMENT_BYTES + 1).unwrap();
        let error = add_knowledge(tmp.path(), &source, Some("huge")).unwrap_err();
        assert!(matches!(
            error.kind(),
            std::io::ErrorKind::InvalidData | std::io::ErrorKind::PermissionDenied
        ));
        assert!(!tmp.path().join(CUSTOM_DIR).join("huge/huge.md").exists());
    }

    #[test]
    fn add_single_txt_file_is_rejected_with_clear_message() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("notes.txt");
        std::fs::write(&src, "plain text").unwrap();
        let err = add_knowledge(tmp.path(), &src, Some("notes")).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains(".md"));
        // No half-staged entry left behind.
        assert!(list_knowledge(tmp.path()).is_empty());
        assert!(!tmp.path().join(CUSTOM_DIR).join("notes").exists());
    }

    #[test]
    fn add_dir_with_no_markdown_is_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src_dir = tmp.path().join("txt-only");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(src_dir.join("a.txt"), "x").unwrap();
        std::fs::write(src_dir.join("b.rst"), "y").unwrap();
        let err = add_knowledge(tmp.path(), &src_dir, Some("txt-only")).unwrap_err();
        assert!(err.to_string().contains(".md"));
        assert!(list_knowledge(tmp.path()).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn add_rejects_symlinked_markdown_pointing_outside_tree() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::TempDir::new().unwrap();
        // A secret file OUTSIDE the source tree.
        let secret = tmp.path().join("secret.md");
        std::fs::write(&secret, "# host secret").unwrap();
        // Source dir whose only "doc" is a symlink to the secret.
        let src_dir = tmp.path().join("docs");
        std::fs::create_dir_all(&src_dir).unwrap();
        symlink(&secret, src_dir.join("link.md")).unwrap();

        let err = add_knowledge(tmp.path(), &src_dir, Some("docs")).unwrap_err();
        // The symlink is rejected, so no .md is copied → "no .md files" error.
        let msg = err.to_string();
        assert!(
            msg.contains("symbolic link") || msg.contains("no .md files"),
            "symlink must be refused, got: {msg}"
        );
        // The secret content must NOT have landed in the index.
        let copied = src_dir.join("link.md");
        let _ = copied; // (sanity) the destination under CUSTOM_DIR holds nothing.
        let dest = tmp.path().join(CUSTOM_DIR).join("docs").join("link.md");
        assert!(!dest.exists(), "symlinked secret must not be copied in");
        // And the dest dir must NOT be left behind on the rejected add.
        assert!(
            !tmp.path().join(CUSTOM_DIR).join("docs").exists(),
            "stray dest dir must be cleaned up"
        );
    }

    #[cfg(unix)]
    #[test]
    fn add_rejects_symlinked_destination_tree() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::TempDir::new().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let custom = tmp.path().join("knowledge/custom");
        std::fs::create_dir_all(custom.parent().unwrap()).unwrap();
        symlink(outside.path(), &custom).unwrap();
        let source = tmp.path().join("guide.md");
        std::fs::write(&source, "# private").unwrap();

        assert!(add_knowledge(tmp.path(), &source, Some("guide")).is_err());
        assert!(std::fs::read_dir(outside.path()).unwrap().next().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn add_skips_symlinked_markdown_but_still_indexes_legit_siblings() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::TempDir::new().unwrap();
        // A secret OUTSIDE the source tree (the symlink target).
        let secret = tmp.path().join("secret.md");
        std::fs::write(&secret, "# host secret").unwrap();
        // Source dir with TWO legit `.md` files AND one symlink to the secret.
        let src_dir = tmp.path().join("docs");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(src_dir.join("real-a.md"), "# A").unwrap();
        std::fs::write(src_dir.join("real-b.md"), "# B").unwrap();
        symlink(&secret, src_dir.join("link.md")).unwrap();

        // The whole add must NOT abort on the symlink — the two real .md siblings
        // are still indexed, the symlink is skipped (not copied).
        let result = add_knowledge(tmp.path(), &src_dir, Some("docs")).unwrap();
        assert_eq!(
            result.files_copied, 2,
            "legit siblings indexed, symlink skipped"
        );
        let base = tmp.path().join(CUSTOM_DIR).join("docs");
        assert!(base.join("real-a.md").exists());
        assert!(base.join("real-b.md").exists());
        assert!(
            !base.join("link.md").exists(),
            "the symlinked secret must never be copied in"
        );
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

    #[test]
    fn add_refuses_to_replace_a_corrupt_registry() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join(".umadev");
        std::fs::create_dir_all(&state).unwrap();
        let registry_path = state.join("knowledge.json");
        std::fs::write(&registry_path, b"{ damaged registry").unwrap();
        let source = tmp.path().join("guide.md");
        std::fs::write(&source, "# Guide").unwrap();

        let error = add_knowledge(tmp.path(), &source, Some("guide")).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(
            std::fs::read(&registry_path).unwrap(),
            b"{ damaged registry"
        );
        assert!(!tmp.path().join(CUSTOM_DIR).join("guide").exists());
    }

    #[test]
    fn remove_refuses_to_mutate_when_registry_is_corrupt() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join(".umadev");
        std::fs::create_dir_all(&state).unwrap();
        let registry_path = state.join("knowledge.json");
        std::fs::write(&registry_path, b"not json").unwrap();
        let existing = tmp.path().join(CUSTOM_DIR).join("kept");
        std::fs::create_dir_all(&existing).unwrap();
        std::fs::write(existing.join("guide.md"), "# Keep").unwrap();

        let error = remove_knowledge(tmp.path(), "kept").unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(existing.join("guide.md").exists());
        assert_eq!(std::fs::read(&registry_path).unwrap(), b"not json");
    }

    #[test]
    fn knowledge_walk_has_a_global_directory_entry_budget() {
        let tmp = tempfile::TempDir::new().unwrap();
        for index in 0..8 {
            std::fs::write(tmp.path().join(format!("doc-{index}.md")), "# Doc").unwrap();
        }
        let mut files = Vec::new();
        let mut entries_seen = MAX_WALK_ENTRIES - 2;
        walk_source_bounded(tmp.path(), &mut files, 0, &mut entries_seen);
        assert_eq!(entries_seen, MAX_WALK_ENTRIES);
        assert!(files.len() <= 2);
    }

    #[test]
    fn concurrent_adds_preserve_every_registry_entry() {
        use std::sync::{Arc, Barrier};

        let tmp = tempfile::TempDir::new().unwrap();
        let root = Arc::new(tmp.path().to_path_buf());
        let barrier = Arc::new(Barrier::new(9));
        let mut workers = Vec::new();
        for index in 0..8 {
            let source = root.join(format!("source-{index}.md"));
            std::fs::write(&source, format!("# Source {index}")).unwrap();
            let root = Arc::clone(&root);
            let barrier = Arc::clone(&barrier);
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                add_knowledge(&root, &source, Some(&format!("entry-{index}")))
            }));
        }
        barrier.wait();
        for worker in workers {
            worker.join().unwrap().unwrap();
        }

        assert_eq!(list_knowledge(&root).len(), 8);
    }
}
