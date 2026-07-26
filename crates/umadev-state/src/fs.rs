use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[must_use]
pub fn metadata_is_real_dir(meta: &fs::Metadata) -> bool {
    if !meta.file_type().is_dir() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return false;
        }
    }
    true
}

#[must_use]
pub fn metadata_is_real_file(meta: &fs::Metadata) -> bool {
    if !meta.file_type().is_file() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return false;
        }
    }
    true
}

#[must_use]
pub fn real_dir(path: &Path) -> bool {
    symlink_metadata_path(path).is_ok_and(|meta| metadata_is_real_dir(&meta))
}

#[must_use]
pub fn real_file(path: &Path) -> bool {
    symlink_metadata_path(path).is_ok_and(|meta| metadata_is_real_file(&meta))
}

pub fn ensure_real_child_dir(parent: &Path, name: &str) -> std::io::Result<PathBuf> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || !real_dir(parent)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "unsafe managed directory component",
        ));
    }
    let child = parent.join(name);
    match symlink_metadata_path(&child) {
        Ok(meta) if metadata_is_real_dir(&meta) => Ok(child),
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "managed path is not a real directory",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match create_dir_path(&child) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
            symlink_metadata_path(&child)
                .ok()
                .filter(metadata_is_real_dir)
                .map(|_| child)
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "managed directory changed during creation",
                    )
                })
        }
        Err(error) => Err(error),
    }
}

fn safe_file_or_absent(path: &Path) -> std::io::Result<bool> {
    match symlink_metadata_path(path) {
        Ok(meta) => Ok(metadata_is_real_file(&meta)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error),
    }
}

fn open_read_no_follow(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        // `O_NONBLOCK` is inert for regular files and prevents a hostile FIFO
        // from hanging this process before the post-open file-type check can
        // reject it.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    #[cfg(windows)]
    let file = retry_transient_windows_fs(|| options.open(path))?;
    #[cfg(not(windows))]
    let file = options.open(path)?;
    if !file
        .metadata()
        .is_ok_and(|meta| metadata_is_real_file(&meta))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "managed input is not a real file",
        ));
    }
    Ok(file)
}

fn open_temp_no_follow(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW).mode(0o600);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    #[cfg(windows)]
    {
        retry_transient_windows_fs(|| options.open(path))
    }
    #[cfg(not(windows))]
    {
        options.open(path)
    }
}

fn sibling(path: &Path, suffix: &str) -> std::io::Result<PathBuf> {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid filename"))?;
    Ok(path.with_file_name(format!(".{name}.{suffix}")))
}

fn pending_path(path: &Path) -> std::io::Result<PathBuf> {
    sibling(path, "umadev-replace-pending")
}

#[cfg(windows)]
fn retry_transient_windows_fs<T>(
    mut operation: impl FnMut() -> std::io::Result<T>,
) -> std::io::Result<T> {
    let started = std::time::Instant::now();
    let retry_for = std::time::Duration::from_secs(2);
    loop {
        match operation() {
            Ok(value) => return Ok(value),
            Err(error)
                if matches!(error.raw_os_error(), Some(5 | 32 | 33))
                    && started.elapsed() < retry_for =>
            {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(error) => return Err(error),
        }
    }
}

/// Retry a transient Windows filesystem denial for a bounded interval so the
/// whole durable-store family shares one robustness level.
///
/// An antivirus scanner, search indexer, or a just-closing handle can briefly
/// deny a managed-path operation with a sharing/lock/access-denied native
/// error. This wrapper re-attempts only those transient denials and, off
/// Windows, is a single pass-through call. Fail-open: it never changes the
/// operation's own success or its terminal error, it only re-runs a transient
/// failure. Sibling durable-store paths (lock directories, ledger tail reads)
/// route their raw `std::fs` calls through here or the retrying helpers below.
pub fn retry_transient<T>(operation: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    #[cfg(windows)]
    {
        retry_transient_windows_fs(operation)
    }
    #[cfg(not(windows))]
    {
        let mut operation = operation;
        operation()
    }
}

#[cfg(windows)]
fn symlink_metadata_path(path: &Path) -> std::io::Result<fs::Metadata> {
    retry_transient_windows_fs(|| fs::symlink_metadata(path))
}

#[cfg(not(windows))]
fn symlink_metadata_path(path: &Path) -> std::io::Result<fs::Metadata> {
    fs::symlink_metadata(path)
}

#[cfg(windows)]
fn create_dir_path(path: &Path) -> std::io::Result<()> {
    retry_transient_windows_fs(|| fs::create_dir(path))
}

#[cfg(not(windows))]
fn create_dir_path(path: &Path) -> std::io::Result<()> {
    fs::create_dir(path)
}

#[cfg(windows)]
fn rename_path(from: &Path, to: &Path) -> std::io::Result<()> {
    retry_transient_windows_fs(|| fs::rename(from, to))
}

#[cfg(not(windows))]
fn rename_path(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::rename(from, to)
}

#[cfg(windows)]
fn remove_file_path(path: &Path) -> std::io::Result<()> {
    retry_transient_windows_fs(|| fs::remove_file(path))
}

#[cfg(not(windows))]
fn remove_file_path(path: &Path) -> std::io::Result<()> {
    fs::remove_file(path)
}

#[cfg(windows)]
fn remove_dir_path(path: &Path) -> std::io::Result<()> {
    retry_transient_windows_fs(|| fs::remove_dir(path))
}

#[cfg(not(windows))]
fn remove_dir_path(path: &Path) -> std::io::Result<()> {
    fs::remove_dir(path)
}

/// Mutating crash-recovery for an interrupted [`atomic_write`]. It renames a
/// leftover `pending` sibling back onto an absent `target`, restoring the last
/// committed bytes after a writer died mid-replace.
///
/// This MUST run only on a write path holding the store lock ([`atomic_write`]
/// and [`remove_regular_file`]). Its `rename(pending -> target)` replaces on
/// Windows, so an unlocked reader that ran it could clobber a value a
/// concurrent writer just committed to `target`, silently reverting the write.
/// The read path therefore never calls this; it uses [`open_read_source`],
/// which resolves the current value without mutating the namespace.
fn recover_pending(path: &Path) -> std::io::Result<()> {
    let pending = pending_path(path)?;
    match symlink_metadata_path(&pending) {
        Ok(meta) if !metadata_is_real_file(&meta) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "unsafe replacement recovery file",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    }
    match symlink_metadata_path(path) {
        Ok(meta) if metadata_is_real_file(&meta) => remove_file_path(&pending),
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "unsafe replacement target",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => rename_path(&pending, path),
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
fn rename_replacing(temp: &Path, target: &Path) -> std::io::Result<()> {
    // Windows can briefly deny a rename while an antivirus scanner, indexer,
    // or another just-closing handle still owns the file. Retry only those
    // transient native errors (5/32/33); the helper has a bounded 2-second
    // deadline.
    match symlink_metadata_path(target) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return rename_path(temp, target);
        }
        Ok(metadata) if metadata_is_real_file(&metadata) => {}
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "unsafe replacement target",
            ));
        }
        Err(error) => return Err(error),
    }
    let pending = pending_path(target)?;
    if !safe_file_or_absent(target)? || !safe_file_or_absent(&pending)? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "unsafe replacement path",
        ));
    }
    rename_path(target, &pending)?;
    match rename_path(temp, target) {
        Ok(()) => {
            let _ = remove_file_path(&pending);
            Ok(())
        }
        Err(error) => match rename_path(&pending, target) {
            Ok(()) => Err(error),
            Err(restore) => Err(std::io::Error::new(
                error.kind(),
                format!(
                    "{error}; previous data remains recoverable at {} ({restore})",
                    pending.display()
                ),
            )),
        },
    }
}

#[cfg(not(windows))]
fn rename_replacing(temp: &Path, target: &Path) -> std::io::Result<()> {
    fs::rename(temp, target)
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "managed file has no parent",
        )
    })?;
    if !real_dir(parent) || !safe_file_or_absent(path)? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "unsafe managed output path",
        ));
    }
    recover_pending(path)?;
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let temp = sibling(
        path,
        &format!("{}.{}.{}.tmp", std::process::id(), stamp, sequence),
    )?;
    let mut file = open_temp_no_follow(&temp)?;
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    drop(file);
    if !real_dir(parent) || !safe_file_or_absent(path)? {
        let _ = fs::remove_file(&temp);
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "managed output path changed during write",
        ));
    }
    let result = rename_replacing(&temp, path);
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    #[cfg(unix)]
    if result.is_ok() {
        if let Ok(directory) = File::open(parent) {
            let _ = directory.sync_all();
        }
    }
    result
}

/// Creates a new private regular file without following links or replacing an
/// existing path. Callers should assemble non-streaming output before calling
/// this function so a failure cannot expose a logically partial artifact.
pub fn write_new_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "managed file has no parent",
        )
    })?;
    if !real_dir(parent) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "unsafe managed output directory",
        ));
    }
    let mut file = open_temp_no_follow(path)?;
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(error);
    }
    drop(file);
    #[cfg(unix)]
    if let Ok(directory) = File::open(parent) {
        let _ = directory.sync_all();
    }
    Ok(())
}

/// Open the current readable contents of a managed file WITHOUT mutating the
/// namespace, tolerating an in-progress [`atomic_write`] replacement window.
///
/// On Windows `atomic_write` replaces a value in two steps: it first moves the
/// previous bytes to a `pending` sibling, then moves the replacement onto
/// `target`. During that gap `target` is briefly absent while `pending` still
/// holds the previous, still-committed value. A reader must fall back to
/// `pending` when `target` is absent, but it must NEVER rename `pending` into
/// place: readers do not hold the store lock, so such a rename could replace a
/// value a concurrent writer already committed to `target`, silently reverting
/// it (a data-integrity and privacy defect). The authoritative rename-recovery
/// is left to the next writer under the lock ([`recover_pending`]).
///
/// At every instant at least one of `target`/`pending` exists while the logical
/// file exists, so a single re-check of `target` resolves the narrow case where
/// a writer finished (removing `pending`) between our two probes. Off Windows
/// `pending` is never produced, so the common path opens `target` directly and
/// the observable result is unchanged.
fn open_read_source(path: &Path) -> std::io::Result<File> {
    match open_read_no_follow(path) {
        Ok(file) => Ok(file),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let pending = pending_path(path)?;
            match open_read_no_follow(&pending) {
                Ok(file) => Ok(file),
                Err(pending_error) if pending_error.kind() == std::io::ErrorKind::NotFound => {
                    // Either the file genuinely does not exist, or a writer
                    // published `target` and removed `pending` between the two
                    // probes above. Re-check `target` before reporting absence.
                    open_read_no_follow(path)
                }
                Err(pending_error) => Err(pending_error),
            }
        }
        Err(error) => Err(error),
    }
}

pub fn read_bounded(path: &Path, max_bytes: u64) -> std::io::Result<Vec<u8>> {
    let file = open_read_source(path)?;
    let length = file.metadata()?.len();
    if length > max_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("managed file exceeds {max_bytes} bytes"),
        ));
    }
    let mut bytes = Vec::with_capacity(
        usize::try_from(length.min(max_bytes))
            .unwrap_or(0)
            .min(64 * 1024),
    );
    // Metadata is only a preflight: another writer can grow the already-open
    // file between `metadata()` and EOF. Read at most one byte beyond the
    // contract so the allocation and I/O remain bounded even in that race.
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > max_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("managed file exceeds {max_bytes} bytes while being read"),
        ));
    }
    Ok(bytes)
}

/// Read a workspace-controlled regular file beneath `root` with a hard byte
/// ceiling and without following symlink/reparse components.
///
/// The relative path must contain only normal components. Every ancestor is
/// inspected before opening, the leaf is opened with the platform's no-follow
/// flag (`O_NONBLOCK` is also used on Unix), and the opened handle's identity is
/// checked against the path before any bytes are consumed. The final canonical
/// path must remain beneath the canonical root. A concurrent grow is bounded by
/// reading at most one byte beyond `max_bytes` and then rejecting the input.
pub fn read_bounded_beneath(
    root: &Path,
    relative: &Path,
    max_bytes: u64,
) -> std::io::Result<Vec<u8>> {
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_os_string()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "managed input contains an escaping path component",
                ));
            }
        }
    }
    if parts.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "managed input path is empty",
        ));
    }

    let canonical_root = fs::canonicalize(root)?;
    if !symlink_metadata_path(&canonical_root).is_ok_and(|meta| metadata_is_real_dir(&meta)) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "managed input root is not a real directory",
        ));
    }

    let mut path = canonical_root.clone();
    for (index, part) in parts.iter().enumerate() {
        path.push(part);
        let metadata = symlink_metadata_path(&path)?;
        let is_leaf = index + 1 == parts.len();
        let accepted = if is_leaf {
            metadata_is_real_file(&metadata)
        } else {
            metadata_is_real_dir(&metadata)
        };
        if !accepted {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                if is_leaf {
                    "managed input is not a regular non-link file"
                } else {
                    "managed input ancestor is not a real directory"
                },
            ));
        }
    }

    let before = same_file::Handle::from_path(&path)?;
    let file = open_read_no_follow(&path)?;
    let opened = same_file::Handle::from_file(file.try_clone()?)?;
    if opened != before {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "managed input changed identity while opening",
        ));
    }
    let canonical_path = fs::canonicalize(&path)?;
    if !canonical_path.starts_with(&canonical_root) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "managed input resolves outside its trusted root",
        ));
    }

    let opened_metadata = file.metadata()?;
    let length = opened_metadata.len();
    if length > max_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("managed file exceeds {max_bytes} bytes"),
        ));
    }
    let mut bytes = Vec::with_capacity(
        usize::try_from(length.min(max_bytes))
            .unwrap_or(0)
            .min(64 * 1024),
    );
    (&file)
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > max_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("managed file exceeds {max_bytes} bytes while being read"),
        ));
    }
    let after_metadata = file.metadata()?;
    let current = same_file::Handle::from_path(&path)?;
    let stable_metadata = opened_metadata.len() == after_metadata.len()
        && opened_metadata.modified().ok() == after_metadata.modified().ok()
        && after_metadata.len() == u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if opened != current || !stable_metadata {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "managed input changed while it was being read",
        ));
    }
    Ok(bytes)
}

pub fn remove_regular_file(path: &Path) -> std::io::Result<bool> {
    recover_pending(path)?;
    match symlink_metadata_path(path) {
        Ok(meta) if metadata_is_real_file(&meta) => {
            remove_file_path(path)?;
            Ok(true)
        }
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refusing to remove a non-regular managed path",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Remove an empty managed directory without following a link or mount-like
/// reparse point. Windows transient sharing conflicts are retried briefly.
pub fn remove_empty_dir(path: &Path) -> std::io::Result<bool> {
    match symlink_metadata_path(path) {
        Ok(meta) if metadata_is_real_dir(&meta) => {
            remove_dir_path(path)?;
            Ok(true)
        }
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refusing to remove a non-directory managed path",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Create one directory without accepting an existing entry. Windows transient
/// sharing conflicts are retried for a bounded interval.
pub fn create_dir(path: &Path) -> std::io::Result<()> {
    create_dir_path(path)
}

/// Rename a managed path, retrying transient Windows sharing violations so the
/// durable-store family (lock directories, ledger rotation, tombstone
/// publication) shares one Windows robustness level.
///
/// Operands are not validated for links or reparse points; callers that need
/// that must check beforehand, exactly as the internal atomic-write path does.
/// Fail-open: it never widens the outcome of a genuine, non-transient failure.
pub fn rename(from: &Path, to: &Path) -> std::io::Result<()> {
    rename_path(from, to)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_atomic_writes_replace_the_same_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.json");
        atomic_write(&path, b"one").unwrap();
        atomic_write(&path, b"two").unwrap();
        assert_eq!(read_bounded(&path, 16).unwrap(), b"two");
    }

    #[test]
    fn bounded_read_rejects_oversized_input() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.json");
        atomic_write(&path, b"oversized").unwrap();
        assert_eq!(
            read_bounded(&path, 3).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn bounded_beneath_read_rejects_oversized_input() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("state")).unwrap();
        fs::write(temp.path().join("state/value.json"), b"oversized").unwrap();
        assert_eq!(
            read_bounded_beneath(temp.path(), Path::new("state/value.json"), 3)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[cfg(unix)]
    #[test]
    fn bounded_beneath_read_rejects_linked_ancestors_and_fifo_without_blocking() {
        use std::os::unix::fs::symlink;
        use std::time::{Duration, Instant};

        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("value.json"), b"outside").unwrap();
        symlink(outside.path(), temp.path().join("linked")).unwrap();
        assert!(read_bounded_beneath(temp.path(), Path::new("linked/value.json"), 64).is_err());

        let fifo = temp.path().join("input.fifo");
        assert!(std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success());
        let started = Instant::now();
        assert!(read_bounded_beneath(temp.path(), Path::new("input.fifo"), 64).is_err());
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a FIFO must be rejected before a blocking read"
        );
    }

    #[test]
    fn private_create_never_replaces_an_existing_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("export.zip");
        write_new_private(&path, b"one").unwrap();
        assert_eq!(
            write_new_private(&path, b"two").unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert_eq!(read_bounded(&path, 16).unwrap(), b"one");
    }

    #[cfg(windows)]
    #[test]
    fn transient_windows_file_conflicts_are_retried_but_other_errors_are_not() {
        let mut transient_attempts = 0;
        retry_transient_windows_fs(|| {
            transient_attempts += 1;
            if transient_attempts < 3 {
                Err(std::io::Error::from_raw_os_error(32))
            } else {
                Ok(())
            }
        })
        .unwrap();
        assert_eq!(transient_attempts, 3);

        let mut permanent_attempts = 0;
        let error = retry_transient_windows_fs(|| {
            permanent_attempts += 1;
            Err::<(), _>(std::io::Error::from_raw_os_error(87))
        })
        .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(87));
        assert_eq!(permanent_attempts, 1);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_read_replaced_or_removed() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().join("outside");
        fs::write(&outside, "keep").unwrap();
        let link = temp.path().join("state.json");
        symlink(&outside, &link).unwrap();
        assert!(atomic_write(&link, b"replace").is_err());
        assert!(read_bounded(&link, 64).is_err());
        assert!(remove_regular_file(&link).is_err());
        assert_eq!(fs::read_to_string(outside).unwrap(), "keep");
    }

    #[test]
    fn read_in_replace_window_reads_pending_without_mutating_the_namespace() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.json");
        atomic_write(&path, b"old").unwrap();

        // Reproduce the Windows two-phase replace window by hand: the previous
        // bytes are parked in `pending` while `target` is momentarily absent.
        let pending = pending_path(&path).unwrap();
        fs::rename(&path, &pending).unwrap();
        assert!(!path.exists());
        assert!(real_file(&pending));

        // A reader must return the still-committed previous value from
        // `pending` WITHOUT renaming it into place. The buggy read path used to
        // run the mutating recovery here and recreate `target`, which is what
        // let an unlocked reader clobber a concurrent writer's committed bytes.
        assert_eq!(read_bounded(&path, 64).unwrap(), b"old");
        assert!(!path.exists(), "a read must not recreate target");
        assert!(real_file(&pending), "a read must leave pending untouched");

        // The next writer performs the real recovery under its lock; its new
        // value wins and the earlier read never reverted anything.
        atomic_write(&path, b"new").unwrap();
        assert_eq!(read_bounded(&path, 64).unwrap(), b"new");
        assert!(!pending.exists());
    }

    #[test]
    fn read_prefers_target_over_a_leftover_pending_sibling() {
        // Both `target` (NEW) and a stale `pending` (OLD) present: a reader must
        // return the authoritative `target`, never the parked previous value,
        // and must not disturb either file.
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.json");
        atomic_write(&path, b"new").unwrap();
        let pending = pending_path(&path).unwrap();
        fs::write(&pending, b"old").unwrap();
        assert_eq!(read_bounded(&path, 64).unwrap(), b"new");
        assert!(real_file(&path));
        assert!(real_file(&pending));
    }

    #[test]
    fn a_read_in_the_replace_window_cannot_revert_a_committed_write() {
        use std::sync::{Arc, Barrier};

        // The reported race, exercised on every platform: a reader arrives while
        // (target absent, pending=OLD), a writer commits NEW to target, and the
        // reader must never restore OLD over the committed NEW. Deterministically
        // correct for the fixed read path; a reintroduced clobber would surface
        // here as an intermittent reverted value.
        for _ in 0..64 {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("state.json");
            atomic_write(&path, b"old").unwrap();
            let pending = pending_path(&path).unwrap();

            // Enter the mid-replace window by hand (Unix never produces it).
            fs::rename(&path, &pending).unwrap();

            let gate = Arc::new(Barrier::new(2));
            let reader_gate = Arc::clone(&gate);
            let reader_path = path.clone();
            let reader = std::thread::spawn(move || {
                reader_gate.wait();
                if let Ok(bytes) = read_bounded(&reader_path, 64) {
                    assert!(bytes == b"old" || bytes == b"new", "reverted/torn read");
                }
            });

            gate.wait();
            // Commit NEW: publish it at target, then drop the previous sibling.
            let staged = path.with_file_name(".state.json.staged");
            fs::write(&staged, b"new").unwrap();
            fs::rename(&staged, &path).unwrap();
            let _ = fs::remove_file(&pending);
            reader.join().unwrap();

            // The committed write survives; the read never reverted it.
            assert_eq!(read_bounded(&path, 64).unwrap(), b"new");
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_two_phase_replace_survives_concurrent_reads() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        // Drive the real Windows `atomic_write` two-phase replace against a
        // hammering reader: every observation is a complete OLD/NEW value and
        // the final committed write is never reverted.
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.json");
        atomic_write(&path, b"old").unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let reader_path = path.clone();
        let reader_stop = Arc::clone(&stop);
        let reader = std::thread::spawn(move || {
            while !reader_stop.load(Ordering::Relaxed) {
                if let Ok(bytes) = read_bounded(&reader_path, 64) {
                    assert!(bytes == b"old" || bytes == b"new", "reverted/torn read");
                }
            }
        });
        for _ in 0..500 {
            atomic_write(&path, b"new").unwrap();
            atomic_write(&path, b"old").unwrap();
        }
        atomic_write(&path, b"new").unwrap();
        stop.store(true, Ordering::Relaxed);
        reader.join().unwrap();
        assert_eq!(read_bounded(&path, 64).unwrap(), b"new");
    }
}
