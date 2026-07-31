//! Kernel guard files, the external workspace namespace, and local Git excludes.
//!
//! The external guard excludes cooperating v2 clients even if an in-workspace
//! guard pathname is replaced. On Unix it is host-local. On Windows it assumes
//! one stable user temp directory for the same OS identity. It does not protect
//! an older client: old releases know only the permanent in-workspace fence.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

pub(super) fn open_guard_file(path: &Path) -> io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW).mode(0o600);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = umadev_state::fs::retry_transient(|| options.open(path))?;
    if !file
        .metadata()
        .is_ok_and(|metadata| umadev_state::fs::metadata_is_real_file(&metadata))
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "workspace run-lock guard is not a regular file",
        ));
    }
    Ok(file)
}

pub(super) fn external_guard_path(
    root: &Path,
    metadata: &std::fs::Metadata,
) -> io::Result<PathBuf> {
    use sha2::{Digest, Sha256};

    let base = external_guard_dir()?;
    let mut hasher = Sha256::new();
    hasher.update(b"umadev-run-lock-v2\0");
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::MetadataExt;
        hasher.update(root.as_os_str().as_bytes());
        hasher.update(metadata.dev().to_le_bytes());
        hasher.update(metadata.ino().to_le_bytes());
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use std::os::windows::fs::MetadataExt;
        for unit in root.as_os_str().encode_wide() {
            hasher.update(unit.to_le_bytes());
        }
        // Stable std exposes creation_time but not the by-handle file index.
        // Combined with the canonical path, this distinguishes an ordinary
        // replacement root while the outer namespace guard protects the actual
        // `.umadev` replacement/git-clean cases.
        hasher.update(metadata.creation_time().to_le_bytes());
    }
    #[cfg(not(any(unix, windows)))]
    {
        hasher.update(root.to_string_lossy().as_bytes());
        hasher.update(
            metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |duration| duration.as_nanos())
                .to_le_bytes(),
        );
    }
    Ok(base.join(format!("{:x}.guard", hasher.finalize())))
}

pub(super) fn external_guard_dir() -> io::Result<PathBuf> {
    #[cfg(unix)]
    let uid = current_unix_uid();
    #[cfg(unix)]
    let base = PathBuf::from(format!("/tmp/.umadev-run-locks-{uid}"));
    #[cfg(windows)]
    // Correct exclusion assumes this OS identity receives one stable user temp directory.
    let base = std::env::temp_dir().join("umadev-run-locks");
    #[cfg(not(any(unix, windows)))]
    let base = std::env::temp_dir().join("umadev-run-locks");

    match std::fs::symlink_metadata(&base) {
        Ok(metadata) if umadev_state::fs::metadata_is_real_dir(&metadata) => {}
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "external run-lock namespace is not a real directory",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            #[cfg(unix)]
            let builder = {
                let mut builder = std::fs::DirBuilder::new();
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
                builder
            };
            #[cfg(not(unix))]
            let builder = std::fs::DirBuilder::new();
            match builder.create(&base) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(error) => return Err(error),
    }
    let metadata = std::fs::symlink_metadata(&base)?;
    if !umadev_state::fs::metadata_is_real_dir(&metadata) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "external run-lock namespace changed during creation",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // A private, same-UID directory keeps other local users from replacing
        // guard entries. We intentionally refuse instead of chmod'ing a path we
        // did not create in this invocation.
        if metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "external run-lock namespace is not private to this OS identity",
            ));
        }
    }
    Ok(base)
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn current_unix_uid() -> u32 {
    // SAFETY: `geteuid` has no preconditions and only returns process identity metadata.
    unsafe { libc::geteuid() }
}
#[cfg(unix)]
pub(super) fn same_file_identity(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(windows)]
pub(super) fn same_file_identity(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    left.creation_time() == right.creation_time()
        && left.file_attributes() == right.file_attributes()
}

#[cfg(not(any(unix, windows)))]
pub(super) fn same_file_identity(_left: &std::fs::Metadata, _right: &std::fs::Metadata) -> bool {
    false
}

pub(super) fn ensure_local_git_excludes(root: &Path) {
    use std::io::Write;

    const ROWS: [&str; 3] = [
        "/.umadev/run.lock",
        "/.umadev/run.lock.guard",
        "/.umadev/run.owner",
    ];
    let git_dir = root.join(".git");
    if !umadev_state::fs::real_dir(&git_dir) {
        return;
    }
    let Ok(info_dir) = umadev_state::fs::ensure_real_child_dir(&git_dir, "info") else {
        return;
    };
    let path = info_dir.join("exclude");
    let contents = match umadev_state::fs::read_bounded(&path, 1024 * 1024) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(contents) => contents,
            Err(_) => return,
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(_) => return,
    };
    let existing = contents.lines().collect::<std::collections::HashSet<_>>();
    let missing = ROWS
        .into_iter()
        .filter(|row| !existing.contains(row))
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return;
    }
    let mut addition = String::new();
    if !contents.is_empty() && !contents.ends_with('\n') {
        addition.push('\n');
    }
    for row in missing {
        addition.push_str(row);
        addition.push('\n');
    }

    // Append rather than replacing the whole user/IDE-owned file: a concurrent
    // writer may at worst produce duplicate rows, but UmaDev never loses a rule
    // that was added after our read.
    let mut options = std::fs::OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW).mode(0o600);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let Ok(mut file) = umadev_state::fs::retry_transient(|| options.open(&path)) else {
        return;
    };
    if !file
        .metadata()
        .is_ok_and(|metadata| umadev_state::fs::metadata_is_real_file(&metadata))
    {
        return;
    }
    let _ = file
        .write_all(addition.as_bytes())
        .and_then(|()| file.flush());
}
