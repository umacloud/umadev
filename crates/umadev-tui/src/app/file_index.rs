//! Non-blocking workspace file discovery used by input affordances.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use super::App;

const FILE_CAP: usize = 2_000;
const ENTRY_CAP: usize = 8_192;
const SCAN_DEPTH: usize = 12;

#[derive(Clone, Debug, Default)]
pub(super) struct WorkspaceFileIndex {
    paths: Vec<String>,
    recent_source: Option<String>,
}

fn is_link_like(_entry: &std::fs::DirEntry, kind: std::fs::FileType) -> bool {
    if kind.is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        return std::fs::symlink_metadata(_entry.path()).map_or(true, |metadata| {
            metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        });
    }
    #[cfg(not(windows))]
    false
}

fn visit_repo_files(
    root: &Path,
    entry_cap: usize,
    mut visit: impl FnMut(&std::fs::DirEntry) -> bool,
) -> usize {
    let mut queue = VecDeque::from([(root.to_path_buf(), 0usize)]);
    let mut scanned = 0usize;
    while let Some((dir, depth)) = queue.pop_front() {
        if scanned >= entry_cap {
            break;
        }
        let Ok(read_dir) = std::fs::read_dir(dir) else {
            continue;
        };
        let remaining = entry_cap - scanned;
        let mut entries = Vec::new();
        for result in read_dir.take(remaining) {
            scanned += 1;
            if let Ok(entry) = result {
                entries.push(entry);
            }
        }
        entries.sort_by(|left, right| {
            let left = left.file_name().to_string_lossy().to_lowercase();
            let right = right.file_name().to_string_lossy().to_lowercase();
            left.cmp(&right)
        });
        for entry in entries {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || name == "target" || name == "node_modules" {
                continue;
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if is_link_like(&entry, kind) {
                continue;
            }
            if kind.is_dir() {
                if depth < SCAN_DEPTH {
                    queue.push_back((entry.path(), depth + 1));
                }
            } else if kind.is_file() && !visit(&entry) {
                return scanned;
            }
        }
    }
    scanned
}

#[cfg(test)]
pub(super) fn collect_repo_files(root: &Path) -> Vec<String> {
    build_workspace_file_index(root).paths
}

fn build_workspace_file_index(root: &Path) -> WorkspaceFileIndex {
    const SOURCE_EXTENSIONS: &[&str] = &[
        "rs", "ts", "tsx", "js", "jsx", "mjs", "cjs", "py", "go", "java", "rb", "php", "c", "cc",
        "cpp", "h", "hpp", "cs", "swift", "kt", "vue", "svelte", "css", "scss", "less", "html",
    ];
    let mut paths = Vec::new();
    let mut best: Option<(std::time::SystemTime, String)> = None;
    visit_repo_files(root, ENTRY_CAP, |entry| {
        let path = entry.path();
        let Some(relative) = path
            .strip_prefix(root)
            .ok()
            .map(|path| path.to_string_lossy().replace('\\', "/"))
        else {
            return true;
        };
        if paths.len() < FILE_CAP {
            paths.push(relative.clone());
        }
        let is_source = path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                SOURCE_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str())
            });
        if !is_source {
            return true;
        }
        let Ok(modified) = entry.metadata().and_then(|metadata| metadata.modified()) else {
            return true;
        };
        if best.as_ref().is_none_or(|(current, current_path)| {
            modified > *current || (modified == *current && relative < *current_path)
        }) {
            best = Some((modified, relative));
        }
        true
    });
    paths.sort();
    paths.dedup();
    WorkspaceFileIndex {
        paths,
        recent_source: best.map(|(_, path)| path),
    }
}

fn spawn_scan<T: Default + Send + Sync + 'static>(
    name: &str,
    scan: impl FnOnce() -> T + Send + std::panic::UnwindSafe + 'static,
) -> Option<Arc<OnceLock<T>>> {
    let result = Arc::new(OnceLock::new());
    let thread_result = Arc::clone(&result);
    std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            let value = std::panic::catch_unwind(scan).unwrap_or_default();
            let _ = thread_result.set(value);
        })
        .ok()
        .map(|_| result)
}

impl App {
    pub(super) fn start_workspace_file_scan(&self) {
        if self.workspace_scan_handle.borrow().is_some()
            || (self.example_file.borrow().is_some() && self.mention_files.borrow().is_some())
        {
            return;
        }
        let root = self.project_root.clone();
        let handle = spawn_scan("umadev-file-index", move || {
            build_workspace_file_index(&root)
        });
        if let Some(handle) = handle {
            *self.workspace_scan_handle.borrow_mut() = Some(handle);
        } else {
            self.finish_workspace_file_scan(WorkspaceFileIndex {
                paths: Vec::new(),
                recent_source: None,
            });
        }
    }

    fn finish_workspace_file_scan(&self, index: WorkspaceFileIndex) {
        if self.mention_files.borrow().is_none() {
            *self.mention_files.borrow_mut() = Some(index.paths);
        }
        if self.example_file.borrow().is_none() {
            *self.example_file.borrow_mut() = Some(index.recent_source);
        }
        self.workspace_scan_handle.borrow_mut().take();
    }

    fn poll_workspace_file_scan(&self) {
        let result = self
            .workspace_scan_handle
            .borrow()
            .as_ref()
            .and_then(|handle| handle.get().cloned());
        if let Some(index) = result {
            self.finish_workspace_file_scan(index);
        }
    }

    pub(super) fn resolve_example_file(&self) -> Option<String> {
        if let Some(cached) = self.example_file.borrow().as_ref() {
            return cached.clone();
        }
        self.start_workspace_file_scan();
        self.poll_workspace_file_scan();
        if let Some(cached) = self.example_file.borrow().as_ref() {
            return cached.clone();
        }
        // Freeze the first visible tip to the generic example if a slow/network
        // workspace is still indexing. The eventual index still feeds mentions,
        // but never changes already-rendered first-run copy between frames.
        *self.example_file.borrow_mut() = Some(None);
        None
    }

    pub(super) fn ensure_mention_files(&self) {
        if self.mention_files.borrow().is_some() {
            return;
        }
        self.start_workspace_file_scan();
        self.poll_workspace_file_scan();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walker_has_a_strict_global_entry_budget() {
        let root = tempfile::tempdir().unwrap();
        for index in 0..12 {
            std::fs::create_dir(root.path().join(format!("dir-{index:02}"))).unwrap();
        }
        let visited = visit_repo_files(root.path(), 4, |_| true);
        assert_eq!(visited, 4);
    }

    #[test]
    fn file_index_is_sorted_and_ignores_hidden_and_build_directories() {
        let root = tempfile::tempdir().unwrap();
        for relative in ["src/z.rs", "src/a.rs", ".git/leak", "target/leak.rs"] {
            let path = root.path().join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "x").unwrap();
        }
        assert_eq!(collect_repo_files(root.path()), ["src/a.rs", "src/z.rs"]);
    }

    #[cfg(unix)]
    #[test]
    fn file_index_does_not_follow_directory_symlinks() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.rs"), "secret").unwrap();
        symlink(outside.path(), root.path().join("linked")).unwrap();
        assert!(collect_repo_files(root.path()).is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn file_index_does_not_follow_directory_junctions() {
        use std::process::{Command, Stdio};
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.rs"), "secret").unwrap();
        let linked = root.path().join("linked");
        let status = Command::new("cmd.exe")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&linked)
            .arg(outside.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "test junction creation failed");
        assert!(collect_repo_files(root.path()).is_empty());
    }
}
