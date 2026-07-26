//! Content-fingerprint workspace baselines for deterministic change attribution.
//!
//! Tool events are useful live signals, but they are not filesystem truth: a shell
//! can write without a typed write event, a file that was already dirty keeps the
//! same Git status after another edit, and a later verifier/QC pass can write after
//! the original base turn settles. This module snapshots user-owned workspace files
//! by path plus content fingerprint and derives the final changed-path set from a
//! second snapshot. The result is independent of which base or UmaDev-owned tool
//! performed the write.
//!
//! The walk never follows symlinks. Repository/runtime and dependency-cache trees
//! are excluded so Git metadata, UmaDev bookkeeping, and generated dependency
//! volumes cannot masquerade as product changes or make every resident turn hash
//! gigabytes of unrelated files. Each capture is deliberately O(included content
//! bytes): it re-hashes even already-dirty files so a second edit cannot hide behind
//! unchanged Git porcelain. A normal pre/post pair can therefore read at most 4 GiB
//! under the 2 GiB-per-capture ceiling. Crossing any file/byte/depth ceiling is an
//! `unverified` error, never an empty diff; an enforcing caller can therefore refuse
//! to publish success instead of hiding the missing coverage.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::io::Read;
use std::path::Path;

use sha2::{Digest, Sha256};
use thiserror::Error;

const DEFAULT_MAX_FILES: usize = 100_000;
const DEFAULT_MAX_ENTRIES: usize = 200_000;
const DEFAULT_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const DEFAULT_MAX_DEPTH: usize = 64;

/// A bounded, content-addressed snapshot of one workspace.
///
/// Use [`capture`](Self::capture) before a writer starts, then
/// [`changed_paths`](Self::changed_paths) after every base and UmaDev-owned
/// execution step has settled.
#[derive(Debug, Clone)]
pub struct WorkspaceBaseline {
    entries: BTreeMap<String, FileFingerprint>,
    limits: SnapshotLimits,
}

/// A failure to establish or compare a complete workspace baseline.
///
/// Callers that use the result as a hard execution post-condition should report
/// this as "unverified", not silently turn an unknown diff into success.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WorkspaceSnapshotError {
    /// The supplied root does not name a real directory.
    #[error("workspace snapshot root is not a directory: {0}")]
    InvalidRoot(String),
    /// A path could not be enumerated, inspected, or read completely.
    #[error("workspace snapshot could not read `{path}`: {reason}")]
    Io {
        /// Workspace-relative path, or `.` for the root.
        path: String,
        /// Underlying platform error.
        reason: String,
    },
    /// The bounded snapshot ceiling was exceeded.
    #[error("workspace snapshot limit exceeded: {0}")]
    Limit(String),
    /// A path cannot be represented by the UTF-8 contract surface.
    #[error("workspace snapshot found a non-UTF-8 path under `{0}`")]
    NonUtf8Path(String),
}

#[derive(Debug, Clone, Copy)]
struct SnapshotLimits {
    files: usize,
    entries: usize,
    bytes: u64,
    depth: usize,
}

impl Default for SnapshotLimits {
    fn default() -> Self {
        Self {
            files: DEFAULT_MAX_FILES,
            entries: DEFAULT_MAX_ENTRIES,
            bytes: DEFAULT_MAX_BYTES,
            depth: DEFAULT_MAX_DEPTH,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileFingerprint([u8; 32]);

struct SnapshotBuilder<'a> {
    root: &'a Path,
    limits: SnapshotLimits,
    files: usize,
    visited_entries: usize,
    bytes: u64,
    entries: BTreeMap<String, FileFingerprint>,
}

impl WorkspaceBaseline {
    /// Capture the current user-owned workspace tree.
    ///
    /// The walk is bounded to 100,000 files, 200,000 directory entries, 2 GiB
    /// of file content, and 64 directory levels. It never follows symlinks; the
    /// link target text itself is fingerprinted so replacing a link remains
    /// attributable.
    pub fn capture(root: &Path) -> Result<Self, WorkspaceSnapshotError> {
        Self::capture_with_limits(root, SnapshotLimits::default())
    }

    fn capture_with_limits(
        root: &Path,
        limits: SnapshotLimits,
    ) -> Result<Self, WorkspaceSnapshotError> {
        let root_metadata = std::fs::symlink_metadata(root).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                WorkspaceSnapshotError::InvalidRoot(root.display().to_string())
            } else {
                WorkspaceSnapshotError::Io {
                    path: ".".to_string(),
                    reason: error.to_string(),
                }
            }
        })?;
        if !root_metadata.file_type().is_dir() || metadata_is_link_like(&root_metadata) {
            return Err(WorkspaceSnapshotError::InvalidRoot(
                root.display().to_string(),
            ));
        }
        let canonical_root =
            std::fs::canonicalize(root).map_err(|error| io_error(root, root, &error))?;
        let mut builder = SnapshotBuilder {
            root: &canonical_root,
            limits,
            files: 0,
            visited_entries: 0,
            bytes: 0,
            entries: BTreeMap::new(),
        };
        builder.walk(&canonical_root, 0)?;
        Ok(Self {
            entries: builder.entries,
            limits,
        })
    }

    /// Compare the current workspace with this baseline.
    ///
    /// The returned paths are normalized with `/`, sorted, and include creates,
    /// content/permission changes, symlink-target changes, and deletions. A file
    /// changed and then restored byte-for-byte is correctly absent.
    pub fn changed_paths(&self, root: &Path) -> Result<Vec<String>, WorkspaceSnapshotError> {
        let current = Self::capture_with_limits(root, self.limits)?;
        Ok(diff_entries(&self.entries, &current.entries))
    }
}

impl SnapshotBuilder<'_> {
    fn walk(&mut self, dir: &Path, depth: usize) -> Result<(), WorkspaceSnapshotError> {
        if depth > self.limits.depth {
            return Err(WorkspaceSnapshotError::Limit(format!(
                "directory depth exceeded {} at `{}`",
                self.limits.depth,
                display_relative(self.root, dir)
            )));
        }
        let before = safe_directory_metadata(self.root, dir)?;
        let before_identity =
            same_file::Handle::from_path(dir).map_err(|error| io_error(self.root, dir, &error))?;
        let mut children = Vec::new();
        for child in std::fs::read_dir(dir).map_err(|error| io_error(self.root, dir, &error))? {
            self.visited_entries = self.visited_entries.saturating_add(1);
            if self.visited_entries > self.limits.entries {
                return Err(WorkspaceSnapshotError::Limit(format!(
                    "directory entry count exceeded {}",
                    self.limits.entries
                )));
            }
            children.push(child.map_err(|error| io_error(self.root, dir, &error))?);
        }
        let after = safe_directory_metadata(self.root, dir)?;
        let after_identity =
            same_file::Handle::from_path(dir).map_err(|error| io_error(self.root, dir, &error))?;
        if before_identity != after_identity || !same_file_snapshot(&before, &after) {
            return Err(WorkspaceSnapshotError::Io {
                path: display_relative(self.root, dir),
                reason: "directory changed while it was enumerated".to_string(),
            });
        }
        children.sort_by_key(std::fs::DirEntry::file_name);
        for child in children {
            let path = child.path();
            let metadata = std::fs::symlink_metadata(&path)
                .map_err(|error| io_error(self.root, &path, &error))?;
            let file_type = metadata.file_type();
            if file_type.is_dir() {
                if !skip_directory(child.file_name().as_os_str()) {
                    self.walk(&path, depth + 1)?;
                }
                continue;
            }
            if !file_type.is_file() && !file_type.is_symlink() {
                continue;
            }
            self.files = self.files.saturating_add(1);
            if self.files > self.limits.files {
                return Err(WorkspaceSnapshotError::Limit(format!(
                    "file count exceeded {}",
                    self.limits.files
                )));
            }
            let relative = normalized_relative(self.root, &path)?;
            let fingerprint = if file_type.is_symlink() {
                self.fingerprint_symlink(&path, &metadata)?
            } else {
                self.fingerprint_file(&path, &metadata)?
            };
            self.entries.insert(relative, fingerprint);
        }
        Ok(())
    }

    fn fingerprint_file(
        &mut self,
        path: &Path,
        metadata: &std::fs::Metadata,
    ) -> Result<FileFingerprint, WorkspaceSnapshotError> {
        if metadata.len() > self.limits.bytes.saturating_sub(self.bytes) {
            return Err(WorkspaceSnapshotError::Limit(format!(
                "content exceeded {} bytes",
                self.limits.bytes
            )));
        }
        let preflight_identity = same_file::Handle::from_path(path)
            .map_err(|error| io_error(self.root, path, &error))?;
        let before_open = safe_regular_file_metadata(self.root, path)?;
        if !same_file_snapshot(metadata, &before_open) {
            return Err(WorkspaceSnapshotError::Io {
                path: display_relative(self.root, path),
                reason: "file changed before it could be opened".to_string(),
            });
        }
        let mut hasher = Sha256::new();
        hasher.update(b"f");
        hash_permissions(&mut hasher, &before_open);
        let mut file =
            open_file_no_follow(path).map_err(|error| io_error(self.root, path, &error))?;
        let opened = file
            .metadata()
            .map_err(|error| io_error(self.root, path, &error))?;
        let opened_identity = same_file::Handle::from_file(
            file.try_clone()
                .map_err(|error| io_error(self.root, path, &error))?,
        )
        .map_err(|error| io_error(self.root, path, &error))?;
        if !opened.file_type().is_file()
            || metadata_is_link_like(&opened)
            || opened_identity != preflight_identity
            || !same_file_snapshot(&before_open, &opened)
        {
            return Err(WorkspaceSnapshotError::Io {
                path: display_relative(self.root, path),
                reason: "opened file does not match the preflighted regular file".to_string(),
            });
        }
        let mut buffer = vec![0u8; 64 * 1024].into_boxed_slice();
        let mut observed = 0_u64;
        loop {
            let read = file
                .read(&mut buffer)
                .map_err(|error| io_error(self.root, path, &error))?;
            if read == 0 {
                break;
            }
            let read_bytes = u64::try_from(read).unwrap_or(u64::MAX);
            observed = observed.saturating_add(read_bytes);
            self.add_bytes(read_bytes)?;
            hasher.update(&buffer[..read]);
        }
        let after_read = file
            .metadata()
            .map_err(|error| io_error(self.root, path, &error))?;
        let current_path = safe_regular_file_metadata(self.root, path)?;
        let current_identity = same_file::Handle::from_path(path)
            .map_err(|error| io_error(self.root, path, &error))?;
        if opened_identity != current_identity
            || !same_file_snapshot(&opened, &after_read)
            || !same_file_snapshot(&opened, &current_path)
            || opened.len() != observed
        {
            return Err(WorkspaceSnapshotError::Io {
                path: display_relative(self.root, path),
                reason: "file changed while it was fingerprinted".to_string(),
            });
        }
        Ok(FileFingerprint(hasher.finalize().into()))
    }

    fn fingerprint_symlink(
        &mut self,
        path: &Path,
        metadata: &std::fs::Metadata,
    ) -> Result<FileFingerprint, WorkspaceSnapshotError> {
        let target = std::fs::read_link(path).map_err(|error| io_error(self.root, path, &error))?;
        let after =
            std::fs::symlink_metadata(path).map_err(|error| io_error(self.root, path, &error))?;
        let confirmed =
            std::fs::read_link(path).map_err(|error| io_error(self.root, path, &error))?;
        if !metadata_is_link_like(&after)
            || !same_file_snapshot(metadata, &after)
            || target != confirmed
        {
            return Err(WorkspaceSnapshotError::Io {
                path: display_relative(self.root, path),
                reason: "symlink changed while it was fingerprinted".to_string(),
            });
        }
        let target = target.to_str().ok_or_else(|| {
            WorkspaceSnapshotError::NonUtf8Path(display_relative(self.root, path))
        })?;
        self.add_bytes(u64::try_from(target.len()).unwrap_or(u64::MAX))?;
        let mut hasher = Sha256::new();
        hasher.update(b"l");
        hash_permissions(&mut hasher, metadata);
        hasher.update(target.as_bytes());
        Ok(FileFingerprint(hasher.finalize().into()))
    }

    fn add_bytes(&mut self, bytes: u64) -> Result<(), WorkspaceSnapshotError> {
        self.bytes = self.bytes.saturating_add(bytes);
        if self.bytes > self.limits.bytes {
            return Err(WorkspaceSnapshotError::Limit(format!(
                "content exceeded {} bytes",
                self.limits.bytes
            )));
        }
        Ok(())
    }
}

fn metadata_is_link_like(metadata: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        metadata.file_type().is_symlink()
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(any(unix, windows)))]
    {
        metadata.file_type().is_symlink()
    }
}

fn same_file_snapshot(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    left.len() == right.len()
        && left.modified().ok() == right.modified().ok()
        && same_permissions(left, right)
}

#[cfg(unix)]
fn same_permissions(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    left.permissions().mode() == right.permissions().mode()
}

#[cfg(not(unix))]
fn same_permissions(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    left.permissions().readonly() == right.permissions().readonly()
}

fn safe_path_metadata(
    root: &Path,
    path: &Path,
    want_directory: bool,
) -> Result<std::fs::Metadata, WorkspaceSnapshotError> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| WorkspaceSnapshotError::InvalidRoot(root.display().to_string()))?;
    let components: Vec<_> = relative.components().collect();
    if components
        .iter()
        .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(WorkspaceSnapshotError::Io {
            path: display_relative(root, path),
            reason: "path has a non-normal workspace-relative component".to_string(),
        });
    }

    let mut current = root.to_path_buf();
    let mut final_metadata = if components.is_empty() {
        Some(std::fs::symlink_metadata(root).map_err(|error| io_error(root, root, &error))?)
    } else {
        None
    };
    for (index, component) in components.iter().enumerate() {
        let std::path::Component::Normal(name) = component else {
            unreachable!("workspace-relative components were validated")
        };
        current.push(name);
        let metadata = std::fs::symlink_metadata(&current)
            .map_err(|error| io_error(root, &current, &error))?;
        if metadata_is_link_like(&metadata) {
            return Err(WorkspaceSnapshotError::Io {
                path: display_relative(root, &current),
                reason: "path contains a symlink or reparse point".to_string(),
            });
        }
        if index + 1 == components.len() {
            final_metadata = Some(metadata);
        } else if !metadata.file_type().is_dir() {
            return Err(WorkspaceSnapshotError::Io {
                path: display_relative(root, &current),
                reason: "path ancestor is not a directory".to_string(),
            });
        }
    }
    let metadata = final_metadata.ok_or_else(|| WorkspaceSnapshotError::Io {
        path: display_relative(root, path),
        reason: "path metadata is unavailable".to_string(),
    })?;
    let expected_type = if want_directory {
        metadata.file_type().is_dir()
    } else {
        metadata.file_type().is_file()
    };
    if !expected_type {
        return Err(WorkspaceSnapshotError::Io {
            path: display_relative(root, path),
            reason: if want_directory {
                "path is not a real directory".to_string()
            } else {
                "path is not a regular file".to_string()
            },
        });
    }
    let canonical = std::fs::canonicalize(path).map_err(|error| io_error(root, path, &error))?;
    if !canonical.starts_with(root) {
        return Err(WorkspaceSnapshotError::Io {
            path: display_relative(root, path),
            reason: "path resolves outside the workspace root".to_string(),
        });
    }
    Ok(metadata)
}

fn safe_directory_metadata(
    root: &Path,
    path: &Path,
) -> Result<std::fs::Metadata, WorkspaceSnapshotError> {
    safe_path_metadata(root, path, true)
}

fn safe_regular_file_metadata(
    root: &Path,
    path: &Path,
) -> Result<std::fs::Metadata, WorkspaceSnapshotError> {
    safe_path_metadata(root, path, false)
}

fn open_file_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options.open(path)
}

fn diff_entries(
    before: &BTreeMap<String, FileFingerprint>,
    after: &BTreeMap<String, FileFingerprint>,
) -> Vec<String> {
    let mut changed = BTreeSet::new();
    for (path, fingerprint) in after {
        if before.get(path) != Some(fingerprint) {
            changed.insert(path.clone());
        }
    }
    for path in before.keys() {
        if !after.contains_key(path) {
            changed.insert(path.clone());
        }
    }
    changed.into_iter().collect()
}

fn normalized_relative(root: &Path, path: &Path) -> Result<String, WorkspaceSnapshotError> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| WorkspaceSnapshotError::InvalidRoot(root.display().to_string()))?;
    let value = relative
        .to_str()
        .ok_or_else(|| WorkspaceSnapshotError::NonUtf8Path(display_relative(root, relative)))?;
    Ok(value.replace('\\', "/"))
}

fn display_relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn io_error(root: &Path, path: &Path, error: &std::io::Error) -> WorkspaceSnapshotError {
    WorkspaceSnapshotError::Io {
        path: {
            let relative = display_relative(root, path);
            if relative.is_empty() {
                ".".to_string()
            } else {
                relative
            }
        },
        reason: error.to_string(),
    }
}

fn skip_directory(name: &OsStr) -> bool {
    matches!(
        name.to_str(),
        Some(
            ".git"
                | ".umadev"
                | "node_modules"
                | "target"
                | "dist"
                | "build"
                | ".output"
                | ".turbo"
                | ".next"
                | ".nuxt"
                | ".svelte-kit"
                | ".cache"
                | "coverage"
                | "vendor"
                | "__pycache__"
                | ".venv"
                | "venv"
                | ".gradle"
        )
    )
}

#[cfg(unix)]
fn hash_permissions(hasher: &mut Sha256, metadata: &std::fs::Metadata) {
    use std::os::unix::fs::PermissionsExt;
    hasher.update(metadata.permissions().mode().to_le_bytes());
}

#[cfg(not(unix))]
fn hash_permissions(hasher: &mut Sha256, metadata: &std::fs::Metadata) {
    hasher.update([u8::from(metadata.permissions().readonly())]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn detects_same_length_rewrite_of_an_already_existing_file() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("src.rs");
        std::fs::write(&path, "aaaa").unwrap();
        let before = WorkspaceBaseline::capture(root.path()).unwrap();

        // Same path and byte length: a status/metadata-only comparison can miss
        // this on coarse-timestamp filesystems; the content fingerprint cannot.
        std::fs::write(&path, "bbbb").unwrap();
        assert_eq!(before.changed_paths(root.path()).unwrap(), ["src.rs"]);
    }

    #[test]
    fn reports_creates_deletes_and_restores_deterministically() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("delete.rs"), "old").unwrap();
        std::fs::write(root.path().join("restore.rs"), "same").unwrap();
        let before = WorkspaceBaseline::capture(root.path()).unwrap();

        std::fs::remove_file(root.path().join("delete.rs")).unwrap();
        std::fs::write(root.path().join("new.rs"), "new").unwrap();
        std::fs::write(root.path().join("restore.rs"), "changed").unwrap();
        std::fs::write(root.path().join("restore.rs"), "same").unwrap();
        assert_eq!(
            before.changed_paths(root.path()).unwrap(),
            ["delete.rs", "new.rs"]
        );
    }

    #[test]
    fn excludes_repository_runtime_and_dependency_cache_trees() {
        let root = tempfile::tempdir().unwrap();
        for dir in [
            ".git",
            ".umadev",
            "node_modules",
            "target",
            "dist",
            "build",
            ".output",
            ".turbo",
            ".next",
            ".svelte-kit",
            "coverage",
            "vendor",
        ] {
            std::fs::create_dir_all(root.path().join(dir)).unwrap();
        }
        let before = WorkspaceBaseline::capture(root.path()).unwrap();
        for dir in [
            ".git",
            ".umadev",
            "node_modules",
            "target",
            "dist",
            "build",
            ".output",
            ".turbo",
            ".next",
            ".svelte-kit",
            "coverage",
            "vendor",
        ] {
            std::fs::write(root.path().join(dir).join("noise"), "changed").unwrap();
        }
        assert!(before.changed_paths(root.path()).unwrap().is_empty());
    }

    #[test]
    fn bounded_capture_returns_explicit_file_and_byte_limit_errors() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a"), "a").unwrap();
        std::fs::write(root.path().join("b"), "b").unwrap();
        let file_error = WorkspaceBaseline::capture_with_limits(
            root.path(),
            SnapshotLimits {
                files: 1,
                entries: 10,
                bytes: 10,
                depth: 10,
            },
        )
        .unwrap_err();
        assert!(matches!(file_error, WorkspaceSnapshotError::Limit(_)));
        let byte_error = WorkspaceBaseline::capture_with_limits(
            root.path(),
            SnapshotLimits {
                files: 10,
                entries: 10,
                bytes: 1,
                depth: 10,
            },
        )
        .unwrap_err();
        assert!(matches!(byte_error, WorkspaceSnapshotError::Limit(_)));
    }

    #[test]
    fn directory_entry_budget_counts_empty_directories_before_collecting_them() {
        let root = tempfile::tempdir().unwrap();
        for name in ["one", "two", "three"] {
            std::fs::create_dir(root.path().join(name)).unwrap();
        }
        let error = WorkspaceBaseline::capture_with_limits(
            root.path(),
            SnapshotLimits {
                files: 10,
                entries: 2,
                bytes: 10,
                depth: 10,
            },
        )
        .unwrap_err();
        assert!(matches!(error, WorkspaceSnapshotError::Limit(_)));
    }

    #[cfg(unix)]
    #[test]
    fn fingerprints_symlink_target_without_following_it() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("a"), "outside-a").unwrap();
        std::fs::write(outside.path().join("b"), "outside-b").unwrap();
        let link = root.path().join("link");
        symlink(outside.path().join("a"), &link).unwrap();
        let before = WorkspaceBaseline::capture(root.path()).unwrap();

        std::fs::remove_file(&link).unwrap();
        symlink(outside.path().join("b"), &link).unwrap();
        assert_eq!(before.changed_paths(root.path()).unwrap(), ["link"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_cannot_be_used_as_the_snapshot_root() {
        use std::os::unix::fs::symlink;

        let parent = tempfile::tempdir().unwrap();
        let real = parent.path().join("real");
        let alias = parent.path().join("alias");
        std::fs::create_dir(&real).unwrap();
        symlink(&real, &alias).unwrap();
        assert!(matches!(
            WorkspaceBaseline::capture(&alias),
            Err(WorkspaceSnapshotError::InvalidRoot(_))
        ));
    }

    #[test]
    fn missing_root_is_not_silently_an_empty_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let missing: PathBuf = root.path().join("missing");
        assert!(matches!(
            WorkspaceBaseline::capture(&missing),
            Err(WorkspaceSnapshotError::InvalidRoot(_))
        ));
    }
}
