//! Permanent legacy-client fence inspection, creation, and doctor migration.
//!
//! This pathname is the only exclusion understood by older create-new clients.
//! Deleting it or replacing `.umadev` while an old release may run makes old/new
//! mutual exclusion impossible; doctor migration therefore keeps it present
//! in-place and requires older releases to remain stopped.

use std::io;
use std::path::Path;

use fs2::FileExt;

#[cfg(unix)]
use std::fs::File;

use super::namespace::{
    external_guard_path, lock_error_is_contention, open_guard_file, same_file_identity,
};
use super::owner::{
    boot_id, classify_claim_owner, hostname, older_than_stale, pid_is_alive, ClaimOwner, Owner,
    OwnerLiveness,
};

pub(super) const V2_FENCE: &[u8] =
    b"pid=0 host=__umadev_v2__ ts=18446744073709551615 boot=__v2__ protocol=2\n";
/// On-disk compatibility-fence state reported to `umadev doctor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunLockFenceStatus {
    /// The workspace has never needed a v2 run lock.
    Absent,
    /// The permanent v2 compatibility fence is complete.
    Current,
    /// A legacy owner row or an interrupted v2 fence needs explicit migration.
    LegacyOrIncomplete,
}

/// Result of an explicit `umadev doctor --fix` fence migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunLockFenceMigration {
    /// No fence existed, so there was nothing to migrate.
    Absent,
    /// The fence was already the exact v2 value.
    AlreadyCurrent,
    /// A partial v2 fence was completed in place.
    RepairedPartial,
    /// A legacy fence accepted by the explicit offline migration was replaced.
    MigratedLegacy,
}
/// Inspect the compatibility fence without creating or changing workspace state.
///
/// # Errors
///
/// Refuses linked/reparse/non-regular managed paths and unreadable state instead
/// of reporting a false clean bill of health.
pub fn inspect_fence(project_root: &Path) -> io::Result<RunLockFenceStatus> {
    let root = std::fs::canonicalize(project_root)?;
    if !umadev_state::fs::real_dir(&root) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "workspace root is not a real directory",
        ));
    }
    let dir = root.join(".umadev");
    match std::fs::symlink_metadata(&dir) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(RunLockFenceStatus::Absent);
        }
        Ok(metadata) if umadev_state::fs::metadata_is_real_dir(&metadata) => {}
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "managed .umadev path is not a real directory",
            ));
        }
        Err(error) => return Err(error),
    }
    let path = dir.join("run.lock");
    match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(RunLockFenceStatus::Absent),
        Ok(metadata) if umadev_state::fs::metadata_is_real_file(&metadata) => {
            let bytes = umadev_state::fs::read_bounded(&path, 4 * 1024)?;
            if bytes == V2_FENCE {
                Ok(RunLockFenceStatus::Current)
            } else {
                Ok(RunLockFenceStatus::LegacyOrIncomplete)
            }
        }
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "workspace run-lock fence is not a regular file",
        )),
        Err(error) => Err(error),
    }
}

/// Explicitly migrate a legacy or interrupted compatibility fence.
///
/// The migration first owns the same kernel guard as a normal run, then updates
/// the existing regular file *in place*. Keeping the pathname present throughout
/// is essential: older create-new clients continue to fail closed before, during,
/// and after migration. A legacy row is changed when its local PID is provably
/// gone, or when a syntactically-valid remote/unattributable owner is older than
/// the conservative stale window and the user explicitly requested this offline
/// repair. A non-empty prefix of the exact v2 fence is safe to complete because
/// a v2 writer holds this same kernel guard while creating it. During migration,
/// every older UmaDev process must remain stopped and the fence must not be deleted.
///
/// # Errors
///
/// Refuses a live/unattributable legacy owner, arbitrary corrupt or empty bytes,
/// linked/reparse paths, guard contention, and every ownership ambiguity.
pub fn migrate_fence(project_root: &Path) -> io::Result<RunLockFenceMigration> {
    let root = std::fs::canonicalize(project_root)?;
    if !umadev_state::fs::real_dir(&root) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "workspace root is not a real directory",
        ));
    }
    let root_identity = std::fs::symlink_metadata(&root)?;
    let dir = root.join(".umadev");
    match std::fs::symlink_metadata(&dir) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(RunLockFenceMigration::Absent);
        }
        Ok(metadata) if umadev_state::fs::metadata_is_real_dir(&metadata) => {}
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "managed .umadev path is not a real directory",
            ));
        }
        Err(error) => return Err(error),
    }

    let path = dir.join("run.lock");
    match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(RunLockFenceMigration::Absent);
        }
        Ok(metadata) if umadev_state::fs::metadata_is_real_file(&metadata) => {}
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "workspace run-lock fence is not a regular file",
            ));
        }
        Err(error) => return Err(error),
    }

    let namespace_guard_path = external_guard_path(&root, &root_identity)?;
    let namespace_guard = open_guard_file(&namespace_guard_path)?;
    FileExt::try_lock_exclusive(&namespace_guard).map_err(|error| {
        if lock_error_is_contention(&error) {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                "a run or another doctor currently owns the workspace namespace lock",
            )
        } else {
            error
        }
    })?;
    if !std::fs::symlink_metadata(&root)
        .is_ok_and(|current| same_file_identity(&root_identity, &current))
        || !std::fs::symlink_metadata(&dir)
            .is_ok_and(|metadata| umadev_state::fs::metadata_is_real_dir(&metadata))
        || !std::fs::symlink_metadata(&path)
            .is_ok_and(|metadata| umadev_state::fs::metadata_is_real_file(&metadata))
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "workspace lock namespace changed during migration",
        ));
    }

    let guard_path = dir.join("run.lock.guard");
    let guard = open_guard_file(&guard_path)?;
    FileExt::try_lock_exclusive(&guard).map_err(|error| {
        if lock_error_is_contention(&error) {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                "a run or another doctor currently owns the workspace lock",
            )
        } else {
            error
        }
    })?;

    let result = (|| -> io::Result<RunLockFenceMigration> {
        let before = umadev_state::fs::read_bounded(&path, 4 * 1024)?;
        let outcome = if before == V2_FENCE {
            RunLockFenceMigration::AlreadyCurrent
        } else if !before.is_empty() && V2_FENCE.starts_with(&before) {
            rewrite_fence_in_place(&path, &before)?;
            RunLockFenceMigration::RepairedPartial
        } else {
            let text =
                std::str::from_utf8(&before).map_err(|_| unsafe_legacy_migration_error(&path))?;
            let owner = Owner::parse(text).ok_or_else(|| unsafe_legacy_migration_error(&path))?;
            let liveness = classify_claim_owner(
                ClaimOwner {
                    pid: owner.pid,
                    host: &owner.host,
                    boot: &owner.boot,
                },
                &hostname(),
                &boot_id(),
                std::process::id(),
                pid_is_alive(owner.pid),
            );
            let explicitly_migratable = liveness == OwnerLiveness::Abandoned
                || (liveness == OwnerLiveness::AgeOnly && older_than_stale(&owner, &path))
                || (owner.host.is_empty() && older_than_stale(&owner, &path));
            if owner.protocol != 0 || owner.pid == 0 || !explicitly_migratable {
                return Err(unsafe_legacy_migration_error(&path));
            }
            rewrite_fence_in_place(&path, &before)?;
            RunLockFenceMigration::MigratedLegacy
        };
        Ok(outcome)
    })();

    // Explicitly release both kernel locks on every semantic rejection. Relying
    // only on descriptor drop left a short self-contention window on Linux when
    // doctor immediately retried another malformed legacy row in the same process.
    let guard_unlock = FileExt::unlock(&guard);
    let namespace_unlock = FileExt::unlock(&namespace_guard);
    match result {
        Err(error) => Err(error),
        Ok(outcome) => {
            guard_unlock?;
            namespace_unlock?;
            Ok(outcome)
        }
    }
}

fn rewrite_fence_in_place(path: &Path, expected: &[u8]) -> io::Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};

    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let mut file = umadev_state::fs::retry_transient(|| options.open(path))?;
    if !file
        .metadata()
        .is_ok_and(|metadata| umadev_state::fs::metadata_is_real_file(&metadata))
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "workspace run-lock fence changed type during migration",
        ));
    }
    let mut observed = Vec::new();
    file.read_to_end(&mut observed)?;
    if observed != expected {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "workspace run-lock fence changed during migration",
        ));
    }
    file.seek(SeekFrom::Start(0))?;
    file.write_all(V2_FENCE)?;
    file.set_len(V2_FENCE.len() as u64)?;
    file.sync_all()?;

    let published = umadev_state::fs::read_bounded(path, 4 * 1024)?;
    if published != V2_FENCE {
        return Err(io::Error::other(
            "workspace run-lock fence publication could not be verified",
        ));
    }
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn unsafe_legacy_migration_error(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "cannot prove that the legacy lock owner is gone at {}; stop old UmaDev processes \
             and retry after the six-hour stale window. Keep every older release stopped and \
             do not delete the compatibility fence during migration. Empty or arbitrarily \
             corrupt bytes are refused because UmaDev never guesses them safe",
            path.display()
        ),
    )
}

pub(super) fn ensure_v2_fence(path: &Path) -> io::Result<()> {
    match umadev_state::fs::read_bounded(path, 4 * 1024) {
        Ok(bytes) if bytes == V2_FENCE => return Ok(()),
        Ok(_) => return Err(legacy_fence_error(path)),
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            return Err(legacy_fence_error(path));
        }
        Err(_) => {}
    }

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
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
    match umadev_state::fs::retry_transient(|| options.open(path)) {
        Ok(mut file) => {
            use std::io::Write;
            // A partial fence is intentionally left fail-closed after a crash;
            // doctor/manual migration may repair it, but no later run guesses.
            file.write_all(V2_FENCE)?;
            file.flush()?;
            file.sync_all()?;
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let bytes = umadev_state::fs::read_bounded(path, 4 * 1024)
                .map_err(|_| legacy_fence_error(path))?;
            if bytes != V2_FENCE {
                return Err(legacy_fence_error(path));
            }
        }
        Err(error) => return Err(error),
    }
    Ok(())
}

fn legacy_fence_error(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "检测到旧版或不完整的工作区锁 {}，为避免新旧版本并发写入，本次执行已拒绝。\
             请确认所有旧版 UmaDev 进程已结束，再用新版 doctor 清理迁移 / \
             legacy or incomplete workspace lock detected; keep old UmaDev releases stopped, do not delete the fence, and migrate it with the new doctor",
            path.display()
        ),
    )
}
