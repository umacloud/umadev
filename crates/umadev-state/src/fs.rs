use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
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
        options.custom_flags(libc::O_NOFOLLOW);
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
    // transient native errors (5/32/33); the helper has a hard 500 ms deadline.
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

pub fn read_bounded(path: &Path, max_bytes: u64) -> std::io::Result<Vec<u8>> {
    recover_pending(path)?;
    let mut file = open_read_no_follow(path)?;
    let length = file.metadata()?.len();
    if length > max_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("managed file exceeds {max_bytes} bytes"),
        ));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(length).unwrap_or(0));
    file.read_to_end(&mut bytes)?;
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
}
