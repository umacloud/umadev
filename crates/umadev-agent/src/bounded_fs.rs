//! Bounded, no-follow reads for workspace-controlled input.
//!
//! Agent hot paths must not call `read_to_string` on files a project can
//! replace. Besides allocating the whole file, a plain open follows links and
//! can block forever on a FIFO. This module is the single read boundary for
//! those paths: it opens the leaf without following it, refuses non-regular
//! files, and reads at most one byte past the caller's limit so concurrent file
//! growth cannot turn a bounded operation into an unbounded one.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{self, Read as _};
use std::path::{Component, Path, PathBuf};

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn open_regular_no_follow(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        // `O_NOFOLLOW` rejects a symlink leaf. `O_NONBLOCK` is inert for normal
        // files but prevents a hostile FIFO from hanging before we can inspect
        // and reject its file type.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }

    let file = options.open(path)?;
    if !file
        .metadata()
        .is_ok_and(|meta| umadev_state::fs::metadata_is_real_file(&meta))
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "workspace input is not a regular non-link file",
        ));
    }
    Ok(file)
}

fn is_link_like(metadata: &std::fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return true;
        }
    }
    false
}

fn normal_relative_components(path: &Path) -> io::Result<Vec<OsString>> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_os_string()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "workspace input contains an escaping path component",
                ));
            }
        }
    }
    Ok(parts)
}

fn relative_beneath(root: &Path, canonical_root: &Path, path: &Path) -> io::Result<PathBuf> {
    let parts = if path.is_absolute() {
        // Keep an absolute *lexical* form alongside the canonical root. On
        // Windows `canonicalize` uses a `\\?\` prefix while callers can supply
        // the same path as a normal `C:\...` absolute path. Comparing against
        // both forms accepts that representation difference without resolving
        // the input path and hiding a reparse-point component from the walk.
        let lexical_root = std::path::absolute(root).ok();
        let relative = path
            .strip_prefix(root)
            .ok()
            .or_else(|| {
                lexical_root
                    .as_deref()
                    .and_then(|root| path.strip_prefix(root).ok())
            })
            .or_else(|| path.strip_prefix(canonical_root).ok())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "workspace input is outside its trusted project root",
                )
            })?;
        normal_relative_components(relative)?
    } else {
        let mut path_parts = normal_relative_components(path)?;
        if root.is_relative() {
            let root_parts = normal_relative_components(root)?;
            if !root_parts.is_empty() && path_parts.starts_with(&root_parts) {
                path_parts.drain(..root_parts.len());
            }
        }
        path_parts
    };
    if parts.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "workspace input contains an escaping path component",
        ));
    }
    Ok(parts.into_iter().collect())
}

fn resolve_beneath(root: &Path, path: &Path, directory: bool) -> io::Result<(PathBuf, PathBuf)> {
    let canonical_root = std::fs::canonicalize(root)?;
    let relative = relative_beneath(root, &canonical_root, path)?;
    if !std::fs::symlink_metadata(&canonical_root)
        .is_ok_and(|meta| umadev_state::fs::metadata_is_real_dir(&meta))
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "project root is not a regular directory",
        ));
    }

    let mut cursor = canonical_root.clone();
    let mut components = relative.components().peekable();
    while let Some(Component::Normal(part)) = components.next() {
        cursor.push(part);
        let metadata = std::fs::symlink_metadata(&cursor)?;
        if is_link_like(&metadata) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "workspace input contains a symlink or reparse point",
            ));
        }
        let last = components.peek().is_none();
        let valid_kind = if last {
            if directory {
                umadev_state::fs::metadata_is_real_dir(&metadata)
            } else {
                umadev_state::fs::metadata_is_real_file(&metadata)
            }
        } else {
            umadev_state::fs::metadata_is_real_dir(&metadata)
        };
        if !valid_kind {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "workspace input has a non-directory ancestor or invalid leaf",
            ));
        }
    }

    let canonical_leaf = std::fs::canonicalize(&cursor)?;
    if !canonical_leaf.starts_with(&canonical_root) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "workspace input escapes its trusted project root",
        ));
    }
    Ok((canonical_root, cursor))
}

fn read_open_file(file: File, max_bytes: usize) -> io::Result<Vec<u8>> {
    let metadata = file.metadata()?;
    if metadata.len() > u64::try_from(max_bytes).unwrap_or(u64::MAX) {
        return Err(invalid_data(format!(
            "workspace input exceeds {max_bytes} bytes"
        )));
    }

    let read_limit = max_bytes
        .checked_add(1)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "byte limit is too large"))?;
    let mut bytes = Vec::with_capacity(
        usize::try_from(metadata.len())
            .unwrap_or(0)
            .min(max_bytes)
            .min(64 * 1024),
    );
    file.take(u64::try_from(read_limit).unwrap_or(u64::MAX))
        .read_to_end(&mut bytes)?;
    if bytes.len() > max_bytes {
        return Err(invalid_data(format!(
            "workspace input grew beyond {max_bytes} bytes while being read"
        )));
    }
    Ok(bytes)
}

/// Read a regular file without following its leaf and with a hard byte limit.
///
/// The metadata length is only an early rejection. The actual I/O uses
/// `take(max + 1)` and validates the resulting length, so a writer that grows
/// the already-open file cannot exceed the allocation/I/O contract.
pub(crate) fn read_bytes(path: &Path, max_bytes: usize) -> io::Result<Vec<u8>> {
    let file = open_regular_no_follow(path)?;
    read_open_file(file, max_bytes)
}

/// Read a bounded regular file as strict UTF-8.
pub(crate) fn read_utf8(path: &Path, max_bytes: usize) -> io::Result<String> {
    String::from_utf8(read_bytes(path, max_bytes)?).map_err(|error| {
        invalid_data(format!(
            "workspace input is not valid UTF-8: {}",
            error.utf8_error()
        ))
    })
}

/// Read a project-relative file while rejecting a symlink/reparse point in any
/// component below the trusted root and enforcing canonical containment.
pub(crate) fn read_utf8_beneath(root: &Path, path: &Path, max_bytes: usize) -> io::Result<String> {
    String::from_utf8(read_bytes_beneath(root, path, max_bytes)?).map_err(|error| {
        invalid_data(format!(
            "workspace input is not valid UTF-8: {}",
            error.utf8_error()
        ))
    })
}

/// Read bounded raw bytes beneath a trusted root, rejecting a symlink/reparse
/// point in the leaf or any parent component.
pub(crate) fn read_bytes_beneath(
    root: &Path,
    path: &Path,
    max_bytes: usize,
) -> io::Result<Vec<u8>> {
    let (canonical_root, resolved) = resolve_beneath(root, path, false)?;
    let file = open_regular_no_follow(&resolved)?;
    let opened = same_file::Handle::from_file(file.try_clone()?)?;
    let current = same_file::Handle::from_path(&resolved)?;
    let canonical_after = std::fs::canonicalize(&resolved)?;
    if opened != current || !canonical_after.starts_with(canonical_root) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "workspace input changed identity while opening",
        ));
    }
    read_open_file(file, max_bytes)
}

/// Read at most `max_bytes` from a regular file beneath `root`, returning the
/// opened file's observed length as scope metadata. Unlike [`read_bytes_beneath`]
/// this deliberately permits a larger file because callers need an explicitly
/// labelled prefix, while retaining the same containment and identity checks.
pub(crate) fn read_prefix_beneath(
    root: &Path,
    path: &Path,
    max_bytes: usize,
) -> io::Result<(Vec<u8>, u64)> {
    let (canonical_root, resolved) = resolve_beneath(root, path, false)?;
    let file = open_regular_no_follow(&resolved)?;
    let opened = same_file::Handle::from_file(file.try_clone()?)?;
    let current = same_file::Handle::from_path(&resolved)?;
    let canonical_after = std::fs::canonicalize(&resolved)?;
    if opened != current || !canonical_after.starts_with(canonical_root) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "workspace input changed identity while opening",
        ));
    }
    let before = file.metadata()?;
    let observed_len = before.len();
    let modified_before = before.modified().ok();
    let mut bytes = Vec::with_capacity(
        usize::try_from(observed_len)
            .unwrap_or(0)
            .min(max_bytes)
            .min(64 * 1024),
    );
    (&file)
        .take(u64::try_from(max_bytes).unwrap_or(u64::MAX))
        .read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    if after.len() != observed_len || after.modified().ok() != modified_before {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "workspace input changed while its bounded prefix was read",
        ));
    }
    Ok((bytes, observed_len))
}

/// A hard aggregate byte budget shared by a multi-file read operation.
///
/// The budget never returns a prefix of an oversized file. Once the remaining
/// allowance is smaller than the next complete file, that read fails with
/// [`io::ErrorKind::InvalidData`]; callers must surface the input as unavailable
/// or fail closed instead of treating a truncated prefix as complete evidence.
#[derive(Debug, Clone)]
pub(crate) struct Utf8ReadBudget {
    remaining_bytes: usize,
    per_file_bytes: usize,
}

impl Utf8ReadBudget {
    pub(crate) fn new(total_bytes: usize, per_file_bytes: usize) -> Self {
        Self {
            remaining_bytes: total_bytes,
            per_file_bytes,
        }
    }

    pub(crate) fn remaining_bytes(&self) -> usize {
        self.remaining_bytes
    }

    fn next_limit(&self) -> io::Result<usize> {
        let limit = self.remaining_bytes.min(self.per_file_bytes);
        if limit == 0 {
            return Err(invalid_data(
                "workspace input aggregate byte budget exhausted",
            ));
        }
        Ok(limit)
    }

    fn consume_bytes(&mut self, bytes: usize) {
        self.remaining_bytes = self.remaining_bytes.saturating_sub(bytes);
    }

    /// Read a trusted-path file without following its leaf while charging the
    /// complete UTF-8 byte length to this aggregate budget.
    pub(crate) fn read_utf8(&mut self, path: &Path) -> io::Result<String> {
        let content = read_utf8(path, self.next_limit()?)?;
        self.consume_bytes(content.len());
        Ok(content)
    }

    /// Read a workspace-controlled file beneath `root` while charging its
    /// complete UTF-8 byte length to this aggregate budget.
    pub(crate) fn read_utf8_beneath(&mut self, root: &Path, path: &Path) -> io::Result<String> {
        let content = read_utf8_beneath(root, path, self.next_limit()?)?;
        self.consume_bytes(content.len());
        Ok(content)
    }

    /// Read raw bytes beneath `root` while charging the complete file to the
    /// same aggregate budget. This is used by scanners whose equality/hash
    /// semantics must not perform a lossy UTF-8 conversion.
    pub(crate) fn read_bytes_beneath(&mut self, root: &Path, path: &Path) -> io::Result<Vec<u8>> {
        let bytes = read_bytes_beneath(root, path, self.next_limit()?)?;
        self.consume_bytes(bytes.len());
        Ok(bytes)
    }
}

/// Root-aware directory validation used before project-controlled walks.
pub(crate) fn is_real_directory_beneath(root: &Path, path: &Path) -> bool {
    // The trusted root itself is a valid directory boundary. `resolve_beneath`
    // intentionally requires at least one child component, so handle this one
    // exact lexical identity before descending. Do not canonicalize `path`
    // independently here: an in-tree symlink back to the root must still be
    // rejected rather than promoted to a trusted directory.
    if path == root {
        return std::fs::canonicalize(root)
            .ok()
            .and_then(|canonical| std::fs::symlink_metadata(canonical).ok())
            .is_some_and(|metadata| umadev_state::fs::metadata_is_real_dir(&metadata));
    }
    resolve_beneath(root, path, true).is_ok()
}

/// Root-aware no-follow validation for a file that will be referenced rather
/// than read immediately (for example, a path handed to a borrowed base).
pub(crate) fn is_real_file_beneath(root: &Path, path: &Path) -> bool {
    resolve_beneath(root, path, false).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_limit_succeeds_and_oversize_is_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("input.txt");
        std::fs::write(&path, b"1234").unwrap();
        assert_eq!(read_bytes(&path, 4).unwrap(), b"1234");
        assert_eq!(
            read_bytes(&path, 3).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn directory_is_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(read_bytes(tmp.path(), 64).is_err());
        assert!(is_real_directory_beneath(tmp.path(), tmp.path()));
    }

    #[test]
    fn concurrent_growth_never_returns_more_than_the_limit() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("growing.txt");
        std::fs::write(&path, b"seed").unwrap();
        let writer_path = path.clone();
        let writer = std::thread::spawn(move || {
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(writer_path)
                .unwrap();
            for _ in 0..2_000 {
                let _ = file.write_all(b"0123456789abcdef");
            }
        });
        for _ in 0..100 {
            match read_bytes(&path, 128) {
                Ok(bytes) => assert!(bytes.len() <= 128),
                Err(error) => assert_eq!(error.kind(), io::ErrorKind::InvalidData),
            }
        }
        writer.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlink_and_special_file_are_rejected_without_blocking() {
        use std::os::unix::fs::symlink;
        use std::os::unix::net::UnixListener;

        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("target.txt");
        let link = tmp.path().join("link.txt");
        std::fs::write(&target, "outside").unwrap();
        symlink(&target, &link).unwrap();
        assert!(read_utf8(&link, 64).is_err());
        assert!(read_prefix_beneath(tmp.path(), &link, 64).is_err());

        let socket = tmp.path().join("socket");
        let _listener = UnixListener::bind(&socket).unwrap();
        assert!(read_bytes(&socket, 64).is_err());

        let fifo = tmp.path().join("fifo");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(status.success());
        assert!(read_bytes(&fifo, 64).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn parent_symlink_escape_is_rejected() {
        use std::os::unix::fs::symlink;

        let root = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        std::fs::write(outside.path().join("rules.md"), "escape").unwrap();
        symlink(outside.path(), root.path().join("linked")).unwrap();
        assert!(read_utf8_beneath(root.path(), &root.path().join("linked/rules.md"), 64,).is_err());
        assert!(
            read_prefix_beneath(root.path(), &root.path().join("linked/rules.md"), 64).is_err()
        );
        assert!(!is_real_directory_beneath(
            root.path(),
            &root.path().join("linked")
        ));
    }

    #[test]
    fn bounded_prefix_reports_observed_length_without_reading_past_the_limit() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("large.txt");
        std::fs::write(&path, b"0123456789").unwrap();
        let (prefix, observed_len) = read_prefix_beneath(tmp.path(), &path, 4).unwrap();
        assert_eq!(prefix, b"0123");
        assert_eq!(observed_len, 10);

        let exact = tmp.path().join("exact.txt");
        std::fs::write(&exact, b"1234").unwrap();
        let (whole, observed_len) = read_prefix_beneath(tmp.path(), &exact, 4).unwrap();
        assert_eq!(whole, b"1234");
        assert_eq!(observed_len, 4);
    }

    #[test]
    fn relative_project_root_and_curdir_are_supported_without_allowing_parent_dirs() {
        let tmp = tempfile::Builder::new()
            .prefix("umadev-bounded-relative-")
            .tempdir_in(".")
            .unwrap();
        let file = tmp.path().join("rules.md");
        std::fs::write(&file, "relative root works").unwrap();
        let cwd = std::env::current_dir().unwrap();
        let lexical_absolute_file = if file.is_absolute() {
            file.clone()
        } else {
            cwd.join(&file)
        };
        assert_eq!(
            read_utf8_beneath(Path::new("."), &lexical_absolute_file, 64).unwrap(),
            "relative root works"
        );

        let relative_root = tmp.path().strip_prefix(&cwd).unwrap_or(tmp.path());
        let relative_file = relative_root.join("rules.md");
        assert_eq!(
            read_utf8_beneath(relative_root, &relative_file, 64).unwrap(),
            "relative root works"
        );
        assert!(read_utf8_beneath(relative_root, Path::new("../escape"), 64).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn trusted_project_root_alias_remains_usable() {
        use std::os::unix::fs::symlink;

        let parent = tempfile::TempDir::new().unwrap();
        let real = parent.path().join("real-project");
        let alias = parent.path().join("project-alias");
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join("rules.md"), "alias works").unwrap();
        symlink(&real, &alias).unwrap();
        assert_eq!(
            read_utf8_beneath(&alias, &alias.join("rules.md"), 64).unwrap(),
            "alias works"
        );
    }

    #[test]
    fn aggregate_budget_never_returns_a_partial_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let first = tmp.path().join("first.txt");
        let second = tmp.path().join("second.txt");
        std::fs::write(&first, "1234").unwrap();
        std::fs::write(&second, "5678").unwrap();

        let mut budget = Utf8ReadBudget::new(6, 8);
        assert_eq!(
            budget.read_utf8_beneath(tmp.path(), &first).unwrap(),
            "1234"
        );
        let error = budget.read_utf8_beneath(tmp.path(), &second).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(budget.remaining_bytes(), 2);
    }
}
