//! Shared no-follow, bounded workspace-walk primitive.
//!
//! Every recursive directory walker in this crate that feeds a scan — the
//! acceptance source collector (and its LLM-judge code digest), the design-token
//! locator, the owned SAST / config-secret / secret-leak scans, the
//! test-integrity snapshot, and the proof-pack packager — classifies each
//! directory entry through [`classify_no_follow`] instead of the
//! symlink-FOLLOWING [`std::path::Path::is_dir`]. This closes a whole class of
//! defect: a symlink placed INSIDE the workspace could otherwise make a walker
//! descend (`is_dir()` follows the link) and pull files from OUTSIDE the
//! workspace into scanned / judged / packaged context, and a symlink CYCLE could
//! recurse unbounded. It mirrors the no-follow contract of the
//! `umadev_contract::backend` source collector.
//!
//! Boundedness (a depth and/or file-count cap) stays the caller's
//! responsibility — each site keeps its own existing caps — because refusing to
//! follow a directory symlink already makes an in-tree symlink cycle
//! unreachable (a cycle requires following a link), and the caps remain the
//! guard against a pathological but real (unlinked) monorepo.

use std::path::Path;

/// No-follow classification of a directory entry.
///
/// Produced by [`classify_no_follow`] via `symlink_metadata` (lstat), so a
/// symlink is classified AS a symlink ([`EntryKind::Skip`]) — never as its
/// target. A directory walker descends only into [`EntryKind::Dir`] and collects
/// only [`EntryKind::File`], so it can never follow a link OUT of the workspace
/// nor loop through a symlink cycle.
pub(crate) enum EntryKind {
    /// A real directory (not a symlink) — safe to descend into.
    Dir,
    /// A real regular file (not a symlink) — a candidate to collect.
    File,
    /// A symlink, a non-regular entry (socket / fifo / device), or an entry
    /// whose metadata could not be read. The caller skips it. Skipping a symlink
    /// is exactly what stops a walk from escaping the workspace or cycling.
    Skip,
}

/// Classify a filesystem entry WITHOUT following symlinks.
///
/// Uses [`std::fs::symlink_metadata`] (lstat): a symlink resolves to
/// [`EntryKind::Skip`], a real directory to [`EntryKind::Dir`], a real file to
/// [`EntryKind::File`]. Fail-open: any metadata error (unreadable / vanished
/// entry) is [`EntryKind::Skip`], never a panic and never an abort of the
/// surrounding walk.
pub(crate) fn classify_no_follow(path: &Path) -> EntryKind {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return EntryKind::Skip;
    };
    let ft = meta.file_type();
    if ft.is_symlink() {
        EntryKind::Skip
    } else if ft.is_dir() {
        EntryKind::Dir
    } else if ft.is_file() {
        EntryKind::File
    } else {
        EntryKind::Skip
    }
}

#[cfg(test)]
mod tests {
    use super::{classify_no_follow, EntryKind};

    #[test]
    fn classifies_real_dir_and_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("d");
        std::fs::create_dir(&dir).unwrap();
        let file = tmp.path().join("f.txt");
        std::fs::write(&file, "x").unwrap();
        assert!(matches!(classify_no_follow(&dir), EntryKind::Dir));
        assert!(matches!(classify_no_follow(&file), EntryKind::File));
    }

    #[test]
    fn missing_entry_is_skip_fail_open() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("nope");
        assert!(matches!(classify_no_follow(&missing), EntryKind::Skip));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_skip_never_followed() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        // A symlink pointing at a real directory must still classify as Skip —
        // NOT Dir — so a walker never descends through it.
        let real_dir = tmp.path().join("real_dir");
        std::fs::create_dir(&real_dir).unwrap();
        let dir_link = tmp.path().join("dir_link");
        symlink(&real_dir, &dir_link).unwrap();
        assert!(matches!(classify_no_follow(&dir_link), EntryKind::Skip));

        // A symlink pointing at a real file must classify as Skip, not File.
        let real_file = tmp.path().join("real_file");
        std::fs::write(&real_file, "x").unwrap();
        let file_link = tmp.path().join("file_link");
        symlink(&real_file, &file_link).unwrap();
        assert!(matches!(classify_no_follow(&file_link), EntryKind::Skip));

        // A dangling symlink (target does not exist) is also Skip, fail-open.
        let dangling = tmp.path().join("dangling");
        symlink(tmp.path().join("does_not_exist"), &dangling).unwrap();
        assert!(matches!(classify_no_follow(&dangling), EntryKind::Skip));
    }
}
