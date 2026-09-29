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
//! The walk never follows symlinks. Repository/runtime metadata, dependency
//! installs, build outputs, caches, and virtual environments are excluded by
//! name, and every directory that the project's own `.gitignore` files ignore is
//! pruned too (inside or outside a Git repository), except the CI and migration
//! directories the execution contract governs. Ignore rules only ever prune whole
//! directories: an individually ignored file such as `.env` stays covered. Git
//! metadata, UmaDev bookkeeping, and generated volumes therefore cannot masquerade
//! as product changes or make every resident turn read gigabytes of unrelated files.
//!
//! Files up to 1 MiB are fingerprinted by content, and each capture re-hashes even
//! already-dirty ones so a second edit cannot hide behind unchanged Git porcelain.
//! Larger files (media, archives, databases, model weights) are fingerprinted by
//! the metadata every write updates, so each costs one `stat` instead of a full
//! read. Hashed content is capped at 2 GiB per capture. Crossing any
//! file/byte/depth ceiling is an `unverified` error that names the heaviest
//! top-level entry, never an empty diff; an enforcing caller can therefore refuse
//! to publish success instead of hiding the missing coverage.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::io::Read;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use sha2::{Digest, Sha256};
use thiserror::Error;

const DEFAULT_MAX_FILES: usize = 100_000;
const DEFAULT_MAX_ENTRIES: usize = 200_000;
const DEFAULT_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const DEFAULT_MAX_DEPTH: usize = 64;
const DEFAULT_MAX_HASHED_FILE_BYTES: u64 = 1024 * 1024;

/// Largest `.gitignore` read for directory pruning. A larger (or unreadable)
/// rules file prunes nothing, which only widens coverage, so it is not an error.
const MAX_IGNORE_FILE_BYTES: u64 = 1024 * 1024;

/// Git matches ignore patterns case-insensitively where `core.ignorecase`
/// defaults to true, i.e. on the default Windows and macOS filesystems.
const IGNORE_RULES_CASE_INSENSITIVE: bool = cfg!(any(windows, target_os = "macos"));

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
    /// Ceiling on content read for hashing, across the whole capture.
    bytes: u64,
    depth: usize,
    /// Files longer than this are fingerprinted by metadata instead of content.
    hashed_file_bytes: u64,
}

impl Default for SnapshotLimits {
    fn default() -> Self {
        Self {
            files: DEFAULT_MAX_FILES,
            entries: DEFAULT_MAX_ENTRIES,
            bytes: DEFAULT_MAX_BYTES,
            depth: DEFAULT_MAX_DEPTH,
            hashed_file_bytes: DEFAULT_MAX_HASHED_FILE_BYTES,
        }
    }
}

/// SHA-256 over a domain tag and the entry's observed state: `f` for a
/// content-hashed file, `m` for a metadata-fingerprinted large file, and `l`
/// for a symlink's target text.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileFingerprint([u8; 32]);

/// Work attributed to one top-level workspace entry, so a crossed ceiling can
/// name the tree to exclude instead of leaving the user to guess.
#[derive(Debug, Default, Clone, Copy)]
struct Usage {
    files: u64,
    entries: u64,
    hashed_bytes: u64,
}

impl Usage {
    const fn is_empty(self) -> bool {
        self.files == 0 && self.entries == 0 && self.hashed_bytes == 0
    }
}

/// The snapshot ceiling a capture crossed.
#[derive(Debug, Clone, Copy)]
enum Ceiling {
    Files,
    Entries,
    Bytes,
}

impl Ceiling {
    const fn measure(self, usage: Usage) -> u64 {
        match self {
            Self::Files => usage.files,
            Self::Entries => usage.entries,
            Self::Bytes => usage.hashed_bytes,
        }
    }

    fn describe(self, amount: u64) -> String {
        match self {
            Self::Files => format!("{amount} files"),
            Self::Entries => format!("{amount} entries"),
            Self::Bytes => format!("{:.1} MiB hashed", amount as f64 / (1024.0 * 1024.0)),
        }
    }
}

struct SnapshotBuilder<'a> {
    root: &'a Path,
    limits: SnapshotLimits,
    files: usize,
    visited_entries: usize,
    bytes: u64,
    entries: BTreeMap<String, FileFingerprint>,
    /// `.gitignore` matchers of the directories on the current walk path,
    /// outermost first.
    ignore_rules: Vec<Gitignore>,
    /// Usage of the finished top-level entries that recorded any work.
    top_level: Vec<(String, Usage)>,
    /// The top-level entry being walked and its usage so far.
    current: Option<(String, Usage)>,
}

impl WorkspaceBaseline {
    /// Capture the current user-owned workspace tree.
    ///
    /// The walk is bounded to 100,000 files, 200,000 directory entries, 2 GiB
    /// of hashed content, and 64 directory levels. It never follows symlinks;
    /// the link target text itself is fingerprinted so replacing a link remains
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
            ignore_rules: Vec::new(),
            top_level: Vec::new(),
            current: None,
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
            self.record(|usage| usage.entries += 1);
            if self.visited_entries > self.limits.entries {
                return Err(self.limit_error(
                    Ceiling::Entries,
                    format!("directory entry count exceeded {}", self.limits.entries),
                ));
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
        let rules = load_ignore_rules(dir, &children);
        let scoped = rules.is_some();
        self.ignore_rules.extend(rules);
        let walked = self.walk_children(children, depth);
        if scoped {
            self.ignore_rules.pop();
        }
        walked
    }

    fn walk_children(
        &mut self,
        children: Vec<std::fs::DirEntry>,
        depth: usize,
    ) -> Result<(), WorkspaceSnapshotError> {
        for child in children {
            if depth > 0 {
                self.visit(&child, depth)?;
                continue;
            }
            let name = child.file_name().to_string_lossy().into_owned();
            self.current = Some((name, Usage::default()));
            self.visit(&child, depth)?;
            if let Some(finished) = self.current.take().filter(|(_, usage)| !usage.is_empty()) {
                self.top_level.push(finished);
            }
        }
        Ok(())
    }

    fn visit(
        &mut self,
        child: &std::fs::DirEntry,
        depth: usize,
    ) -> Result<(), WorkspaceSnapshotError> {
        let path = child.path();
        let metadata =
            std::fs::symlink_metadata(&path).map_err(|error| io_error(self.root, &path, &error))?;
        let file_type = metadata.file_type();
        if file_type.is_dir() {
            if !skip_directory(child.file_name().as_os_str()) && !self.ignored_directory(&path) {
                self.walk(&path, depth + 1)?;
            }
            return Ok(());
        }
        if !file_type.is_file() && !file_type.is_symlink() {
            return Ok(());
        }
        self.files = self.files.saturating_add(1);
        self.record(|usage| usage.files += 1);
        if self.files > self.limits.files {
            return Err(self.limit_error(
                Ceiling::Files,
                format!("file count exceeded {}", self.limits.files),
            ));
        }
        let relative = normalized_relative(self.root, &path)?;
        let fingerprint = if file_type.is_symlink() {
            self.fingerprint_symlink(&path, &metadata)?
        } else if metadata.len() > self.limits.hashed_file_bytes && metadata.modified().is_ok() {
            self.fingerprint_large_file(&path, &metadata)?
        } else {
            self.fingerprint_file(&path, &metadata)?
        };
        self.entries.insert(relative, fingerprint);
        Ok(())
    }

    /// Whether a project `.gitignore` on the current walk path ignores `dir`.
    /// The deepest rules file with a verdict wins, as in Git; directories the
    /// execution contract governs are never pruned.
    fn ignored_directory(&self, dir: &Path) -> bool {
        if self.ignore_rules.is_empty()
            || crate::execution_contract::is_sensitive_directory(&display_relative(self.root, dir))
        {
            return false;
        }
        for rules in self.ignore_rules.iter().rev() {
            let Ok(relative) = dir.strip_prefix(rules.path()) else {
                continue;
            };
            match rules.matched(relative, true) {
                ignore::Match::Ignore(_) => return true,
                ignore::Match::Whitelist(_) => return false,
                ignore::Match::None => {}
            }
        }
        false
    }

    /// Attribute work to the top-level entry being walked. The root's own
    /// listing belongs to no single entry and is not attributed.
    fn record(&mut self, update: impl FnOnce(&mut Usage)) {
        if let Some((_, usage)) = self.current.as_mut() {
            update(usage);
        }
    }

    fn limit_error(&self, ceiling: Ceiling, message: String) -> WorkspaceSnapshotError {
        let heaviest = self
            .top_level
            .iter()
            .chain(self.current.as_ref())
            .map(|(name, usage)| (name, ceiling.measure(*usage)))
            .filter(|(_, amount)| *amount > 0)
            .max_by_key(|(_, amount)| *amount);
        WorkspaceSnapshotError::Limit(match heaviest {
            Some((name, amount)) => format!(
                "{message}; largest top-level entry: `{name}` ({})",
                ceiling.describe(amount)
            ),
            None => message,
        })
    }

    fn fingerprint_file(
        &mut self,
        path: &Path,
        metadata: &std::fs::Metadata,
    ) -> Result<FileFingerprint, WorkspaceSnapshotError> {
        if metadata.len() > self.limits.bytes.saturating_sub(self.bytes) {
            return Err(self.limit_error(Ceiling::Bytes, self.byte_limit_message()));
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

    /// Fingerprint a file above the content-hash threshold from the metadata a
    /// write updates (length, modification time, permissions, and on Unix the
    /// inode) without reading it, so large binaries cost one `stat` per capture.
    fn fingerprint_large_file(
        &self,
        path: &Path,
        metadata: &std::fs::Metadata,
    ) -> Result<FileFingerprint, WorkspaceSnapshotError> {
        let current = safe_regular_file_metadata(self.root, path)?;
        if !same_file_snapshot(metadata, &current) {
            return Err(WorkspaceSnapshotError::Io {
                path: display_relative(self.root, path),
                reason: "file changed while it was fingerprinted".to_string(),
            });
        }
        let modified = current
            .modified()
            .map_err(|error| io_error(self.root, path, &error))?;
        let mut hasher = Sha256::new();
        hasher.update(b"m");
        hash_permissions(&mut hasher, &current);
        hasher.update(current.len().to_le_bytes());
        hash_modified(&mut hasher, modified);
        hash_file_identity(&mut hasher, &current);
        Ok(FileFingerprint(hasher.finalize().into()))
    }

    fn add_bytes(&mut self, bytes: u64) -> Result<(), WorkspaceSnapshotError> {
        self.bytes = self.bytes.saturating_add(bytes);
        self.record(|usage| usage.hashed_bytes = usage.hashed_bytes.saturating_add(bytes));
        if self.bytes > self.limits.bytes {
            return Err(self.limit_error(Ceiling::Bytes, self.byte_limit_message()));
        }
        Ok(())
    }

    fn byte_limit_message(&self) -> String {
        format!("hashed content exceeded {} bytes", self.limits.bytes)
    }
}

/// Load the matcher for `dir`'s own `.gitignore`, if it has a readable one.
/// The file is read like every other workspace file: never through a link or
/// reparse point, and never past a fixed size, so a hostile rules file cannot
/// redirect or stall the capture.
fn load_ignore_rules(dir: &Path, children: &[std::fs::DirEntry]) -> Option<Gitignore> {
    let path = children
        .iter()
        .find(|child| child.file_name() == ".gitignore")?
        .path();
    let metadata = std::fs::symlink_metadata(&path).ok()?;
    if !metadata.file_type().is_file()
        || metadata_is_link_like(&metadata)
        || metadata.len() > MAX_IGNORE_FILE_BYTES
    {
        return None;
    }
    let file = open_file_no_follow(&path).ok()?;
    let opened = file.metadata().ok()?;
    if !opened.file_type().is_file() || metadata_is_link_like(&opened) {
        return None;
    }
    let mut text = Vec::new();
    file.take(MAX_IGNORE_FILE_BYTES + 1)
        .read_to_end(&mut text)
        .ok()?;
    if u64::try_from(text.len()).unwrap_or(u64::MAX) > MAX_IGNORE_FILE_BYTES {
        return None;
    }
    let mut builder = GitignoreBuilder::new(dir);
    builder
        .case_insensitive(IGNORE_RULES_CASE_INSENSITIVE)
        .ok()?;
    let text = String::from_utf8_lossy(&text);
    for line in text.trim_start_matches('\u{feff}').lines() {
        // Git skips a pattern it cannot parse; so does the snapshot.
        let _ = builder.add_line(None, line);
    }
    builder.build().ok().filter(|rules| !rules.is_empty())
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

/// Directory names that are tool-owned wherever they appear: repository and
/// runtime metadata, dependency installs, build outputs, caches, and virtual
/// environments. Names that are source in some ecosystems (`bin`, `obj`, `out`,
/// `packages`, `lib`) are deliberately absent; a project that generates into
/// them excludes them through its own `.gitignore`.
fn skip_directory(name: &OsStr) -> bool {
    matches!(
        name.to_str(),
        Some(
            ".git"
                | ".umadev"
                | "node_modules"
                | "bower_components"
                | ".pnpm-store"
                | "target"
                | "dist"
                | "build"
                | ".output"
                | "DerivedData"
                | ".turbo"
                | ".next"
                | ".nuxt"
                | ".svelte-kit"
                | ".angular"
                | ".parcel-cache"
                | ".cache"
                | "coverage"
                | "vendor"
                | "__pycache__"
                | ".mypy_cache"
                | ".pytest_cache"
                | ".ruff_cache"
                | ".venv"
                | "venv"
                | ".tox"
                | ".nox"
                | ".gradle"
                | ".dart_tool"
                | ".terraform"
                | ".vs"
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

fn hash_modified(hasher: &mut Sha256, modified: SystemTime) {
    let (after_epoch, offset) = match modified.duration_since(UNIX_EPOCH) {
        Ok(offset) => (true, offset),
        Err(before) => (false, before.duration()),
    };
    hasher.update([u8::from(after_epoch)]);
    hasher.update(offset.as_secs().to_le_bytes());
    hasher.update(offset.subsec_nanos().to_le_bytes());
}

/// A same-length, same-mtime replacement still changes the inode on Unix.
#[cfg(unix)]
fn hash_file_identity(hasher: &mut Sha256, metadata: &std::fs::Metadata) {
    use std::os::unix::fs::MetadataExt as _;
    hasher.update(metadata.dev().to_le_bytes());
    hasher.update(metadata.ino().to_le_bytes());
}

/// Windows exposes no stable file index without opening the file, and file
/// tunneling reuses creation times across replacements, so identity there rests
/// on length and modification time alone.
#[cfg(not(unix))]
fn hash_file_identity(_hasher: &mut Sha256, _metadata: &std::fs::Metadata) {}

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
        const TOOL_OWNED: &[&str] = &[
            ".git",
            ".umadev",
            "node_modules",
            "bower_components",
            ".pnpm-store",
            "target",
            "dist",
            "build",
            ".output",
            "DerivedData",
            ".turbo",
            ".next",
            ".svelte-kit",
            ".angular",
            ".parcel-cache",
            "coverage",
            "vendor",
            ".mypy_cache",
            ".pytest_cache",
            ".ruff_cache",
            ".tox",
            ".nox",
            ".dart_tool",
            ".terraform",
            ".vs",
        ];
        let root = tempfile::tempdir().unwrap();
        for dir in TOOL_OWNED {
            std::fs::create_dir_all(root.path().join(dir)).unwrap();
        }
        let before = WorkspaceBaseline::capture(root.path()).unwrap();
        for dir in TOOL_OWNED {
            std::fs::write(root.path().join(dir).join("noise"), "changed").unwrap();
        }
        assert!(before.changed_paths(root.path()).unwrap().is_empty());
    }

    #[test]
    fn project_gitignore_prunes_directories_but_never_individual_files() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join(".gitignore"),
            "# build output\nout/\n/release\n*.log\n.env\n",
        )
        .unwrap();
        for dir in ["out", "release", "src", "src/out"] {
            std::fs::create_dir_all(root.path().join(dir)).unwrap();
        }
        let before = WorkspaceBaseline::capture(root.path()).unwrap();

        // Ignored directories are generated volumes: nothing inside them counts.
        std::fs::write(root.path().join("out/app.exe"), "binary").unwrap();
        std::fs::write(root.path().join("src/out/cache.bin"), "binary").unwrap();
        std::fs::write(root.path().join("release/setup.msi"), "binary").unwrap();
        // Ignored files are still workspace content; `.env` is a credential
        // surface the execution contract must keep seeing.
        std::fs::write(root.path().join("debug.log"), "log").unwrap();
        std::fs::write(root.path().join(".env"), "TOKEN=secret").unwrap();
        std::fs::write(root.path().join("src/main.rs"), "fn main() {}").unwrap();
        assert_eq!(
            before.changed_paths(root.path()).unwrap(),
            [".env", "debug.log", "src/main.rs"]
        );
    }

    #[test]
    fn nested_gitignore_rules_take_precedence_like_git() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(".gitignore"), "generated/\n").unwrap();
        std::fs::create_dir_all(root.path().join("pkg")).unwrap();
        // A deeper rules file re-includes what its parent ignored, and its own
        // patterns apply only beneath it.
        std::fs::write(root.path().join("pkg/.gitignore"), "!generated/\ncache/\n").unwrap();
        for dir in ["generated", "pkg/generated", "pkg/cache", "cache"] {
            std::fs::create_dir_all(root.path().join(dir)).unwrap();
        }
        let before = WorkspaceBaseline::capture(root.path()).unwrap();

        for dir in ["generated", "pkg/generated", "pkg/cache", "cache"] {
            std::fs::write(root.path().join(dir).join("file"), "changed").unwrap();
        }
        assert_eq!(
            before.changed_paths(root.path()).unwrap(),
            ["cache/file", "pkg/generated/file"]
        );
    }

    #[test]
    fn ignore_rules_never_prune_contract_governed_directories() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(".gitignore"), ".github/\nmigrations/\n").unwrap();
        for dir in [".github/workflows", "db/migrations"] {
            std::fs::create_dir_all(root.path().join(dir)).unwrap();
        }
        let before = WorkspaceBaseline::capture(root.path()).unwrap();

        std::fs::write(root.path().join(".github/workflows/ci.yml"), "on: push").unwrap();
        std::fs::write(root.path().join("db/migrations/001.sql"), "drop table").unwrap();
        assert_eq!(
            before.changed_paths(root.path()).unwrap(),
            [".github/workflows/ci.yml", "db/migrations/001.sql"]
        );
    }

    #[test]
    fn large_files_are_fingerprinted_by_metadata_outside_the_byte_budget() {
        let limits = SnapshotLimits {
            bytes: 8,
            hashed_file_bytes: 16,
            ..SnapshotLimits::default()
        };
        let root = tempfile::tempdir().unwrap();
        let large = root.path().join("video.mp4");
        std::fs::write(&large, [7u8; 4096]).unwrap();
        std::fs::write(root.path().join("notes.txt"), "small").unwrap();
        // 4 KiB of media under an 8-byte hashing budget: only `notes.txt` is read.
        let before = WorkspaceBaseline::capture_with_limits(root.path(), limits).unwrap();
        assert!(before.changed_paths(root.path()).unwrap().is_empty());

        // A same-length rewrite is visible through the modification time.
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&large)
            .unwrap();
        std::io::Write::write_all(&mut &file, &[9u8; 4096]).unwrap();
        file.set_modified(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(86_400))
            .unwrap();
        drop(file);
        assert_eq!(before.changed_paths(root.path()).unwrap(), ["video.mp4"]);

        // Growing the file is visible through its length.
        let before = WorkspaceBaseline::capture_with_limits(root.path(), limits).unwrap();
        std::fs::write(&large, [7u8; 8192]).unwrap();
        assert_eq!(before.changed_paths(root.path()).unwrap(), ["video.mp4"]);
    }

    #[test]
    fn limit_errors_name_the_heaviest_top_level_entry() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("src")).unwrap();
        std::fs::create_dir_all(root.path().join("assets/raw")).unwrap();
        std::fs::write(root.path().join("src/main.rs"), "fn main() {}").unwrap();
        for index in 0..4 {
            std::fs::write(root.path().join(format!("assets/raw/{index}.png")), "png").unwrap();
        }
        let error = WorkspaceBaseline::capture_with_limits(
            root.path(),
            SnapshotLimits {
                files: 4,
                ..SnapshotLimits::default()
            },
        )
        .unwrap_err();
        let WorkspaceSnapshotError::Limit(message) = error else {
            panic!("expected a limit error, got {error:?}");
        };
        assert!(
            message.contains("file count exceeded 4")
                && message.contains("largest top-level entry: `assets` (4 files)"),
            "{message}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_gitignore_is_never_read_as_rules() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("rules"), "src/\n").unwrap();
        symlink(outside.path().join("rules"), root.path().join(".gitignore")).unwrap();
        std::fs::create_dir_all(root.path().join("src")).unwrap();
        let before = WorkspaceBaseline::capture(root.path()).unwrap();

        std::fs::write(root.path().join("src/lib.rs"), "pub fn f() {}").unwrap();
        assert_eq!(before.changed_paths(root.path()).unwrap(), ["src/lib.rs"]);
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
                ..SnapshotLimits::default()
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
                ..SnapshotLimits::default()
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
                ..SnapshotLimits::default()
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
