//! Bounded, link-safe directory enumeration for explicit TUI commands.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// A command may inspect at most this many directory entries per invocation.
pub(super) const ENTRY_CAP: usize = 4_096;
pub(super) const RECURSIVE_DEPTH_CAP: usize = 6;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum EntryKind {
    File,
    Directory,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Entry {
    path: PathBuf,
    name: OsString,
    kind: EntryKind,
}

impl Entry {
    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn file_name(&self) -> &OsString {
        &self.name
    }

    pub(super) fn is_file(&self) -> bool {
        self.kind == EntryKind::File
    }

    pub(super) fn is_dir(&self) -> bool {
        self.kind == EntryKind::Directory
    }

    pub(super) fn file_len(&self) -> u64 {
        std::fs::symlink_metadata(&self.path).map_or(0, |metadata| {
            if metadata.file_type().is_symlink()
                || metadata_is_reparse(&metadata)
                || !metadata.is_file()
            {
                0
            } else {
                metadata.len()
            }
        })
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct Scan {
    pub(super) entries: Vec<Entry>,
    /// Every `ReadDir` item consumes budget, including an IO error.
    pub(super) inspected: usize,
    pub(super) errors: usize,
    /// Conservative: true when the budget was exhausted (the directory may have
    /// exactly `cap` entries, but we deliberately do not read entry `cap + 1`).
    pub(super) limit_reached: bool,
}

fn metadata_is_reparse(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        let _ = metadata;
        false
    }
}

fn real_kind(path: &Path) -> std::io::Result<Option<EntryKind>> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || metadata_is_reparse(&metadata) {
        return Ok(None);
    }
    if metadata.is_file() {
        Ok(Some(EntryKind::File))
    } else if metadata.is_dir() {
        Ok(Some(EntryKind::Directory))
    } else {
        Ok(None)
    }
}

/// Enumerate one real directory with a strict inspection budget. Results are
/// sorted after collection so every consumer gets stable presentation order.
pub(super) fn scan_dir_with_cap(dir: &Path, cap: usize) -> Scan {
    let mut scan = Scan::default();
    if cap == 0 {
        return scan;
    }
    if real_kind(dir).ok().flatten() != Some(EntryKind::Directory) {
        scan.errors = 1;
        return scan;
    }
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        scan.errors = 1;
        return scan;
    };
    for result in read_dir.take(cap) {
        scan.inspected += 1;
        let Ok(entry) = result else {
            scan.errors += 1;
            continue;
        };
        let path = entry.path();
        match real_kind(&path) {
            Ok(Some(kind)) => scan.entries.push(Entry {
                path,
                name: entry.file_name(),
                kind,
            }),
            Ok(None) => {}
            Err(_) => scan.errors += 1,
        }
    }
    scan.limit_reached = scan.inspected == cap;
    scan.entries.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.path.cmp(&right.path))
    });
    scan
}

pub(super) fn scan_dir(dir: &Path) -> Scan {
    scan_dir_with_cap(dir, ENTRY_CAP)
}

pub(super) fn directory_has_real_entry(dir: &Path) -> bool {
    !scan_dir_with_cap(dir, 1).entries.is_empty()
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct RecursiveCount {
    pub(super) matching_files: usize,
    pub(super) inspected: usize,
    pub(super) errors: usize,
    pub(super) limit_reached: bool,
}

/// Breadth-first bounded traversal. Directory links and Windows reparse points
/// are never queued, so the scan cannot escape the requested tree or loop.
pub(super) fn count_files_recursive(
    root: &Path,
    entry_cap: usize,
    depth_cap: usize,
    matches: impl Fn(&Path) -> bool,
) -> RecursiveCount {
    let mut result = RecursiveCount::default();
    if entry_cap == 0 || real_kind(root).ok().flatten() != Some(EntryKind::Directory) {
        return result;
    }
    let mut queue = VecDeque::from([(root.to_path_buf(), 0usize)]);
    while let Some((dir, depth)) = queue.pop_front() {
        if result.inspected >= entry_cap {
            result.limit_reached = true;
            break;
        }
        let remaining = entry_cap - result.inspected;
        let scan = scan_dir_with_cap(&dir, remaining);
        result.inspected += scan.inspected;
        result.errors += scan.errors;
        result.limit_reached |= scan.limit_reached;
        for entry in scan.entries {
            if entry.is_dir() {
                if depth < depth_cap {
                    queue.push_back((entry.path, depth + 1));
                }
            } else if matches(&entry.path) {
                result.matching_files += 1;
            }
        }
        if result.inspected >= entry_cap {
            result.limit_reached = true;
            break;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_scan_has_a_strict_global_budget() {
        let root = tempfile::tempdir().unwrap();
        for index in 0..12 {
            std::fs::write(root.path().join(format!("{index:02}.txt")), "x").unwrap();
        }
        let scan = scan_dir_with_cap(root.path(), 4);
        assert_eq!(scan.inspected, 4);
        assert_eq!(scan.entries.len(), 4);
        assert!(scan.limit_reached);
    }

    #[test]
    fn direct_scan_counts_an_unreadable_or_missing_directory_error() {
        let root = tempfile::tempdir().unwrap();
        let scan = scan_dir_with_cap(&root.path().join("missing"), 4);
        assert_eq!(scan.inspected, 0);
        assert_eq!(scan.errors, 1);
        assert!(scan.entries.is_empty());
    }

    #[test]
    fn direct_scan_sorts_the_bounded_result() {
        let root = tempfile::tempdir().unwrap();
        for name in ["z.txt", "a.txt", "m.txt"] {
            std::fs::write(root.path().join(name), "x").unwrap();
        }
        let names: Vec<_> = scan_dir_with_cap(root.path(), ENTRY_CAP)
            .entries
            .into_iter()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, ["a.txt", "m.txt", "z.txt"]);
    }

    #[test]
    fn recursive_scan_has_one_strict_budget_across_all_directories() {
        let root = tempfile::tempdir().unwrap();
        for directory in 0..5 {
            for file in 0..5 {
                let path = root.path().join(format!("d{directory}/{file}.md"));
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, "x").unwrap();
            }
        }
        let count = count_files_recursive(root.path(), 7, 6, |path| {
            path.extension().and_then(|value| value.to_str()) == Some("md")
        });
        assert_eq!(count.inspected, 7);
        assert!(count.limit_reached);
        assert!(count.matching_files <= 7);
    }

    #[cfg(unix)]
    #[test]
    fn recursive_scan_does_not_follow_symlinked_directories() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.md"), "secret").unwrap();
        symlink(outside.path(), root.path().join("linked")).unwrap();
        let count = count_files_recursive(root.path(), ENTRY_CAP, 6, |_| true);
        assert_eq!(count.matching_files, 0);
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_child_is_fail_open_and_does_not_escape_the_budget() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let locked = root.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::write(locked.join("hidden.md"), "x").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let count = count_files_recursive(root.path(), 8, 6, |_| true);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(count.inspected <= 8);
    }

    #[cfg(windows)]
    #[test]
    fn recursive_scan_does_not_follow_directory_junctions() {
        use std::process::{Command, Stdio};
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.md"), "secret").unwrap();
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
        let count = count_files_recursive(root.path(), ENTRY_CAP, 6, |_| true);
        assert_eq!(count.matching_files, 0);
    }
}
