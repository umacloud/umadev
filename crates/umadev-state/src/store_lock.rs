//! Bounded cross-process locks for one logical memory store.
//!
//! A directory is the atomic ownership primitive on every supported platform.
//! The owner record carries a nonce so an expired guard can never remove a
//! successor's lock after stale recovery. Acquisition has a hard deadline: a
//! memory write may fail open, but it may not wedge the host indefinitely.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fs2::FileExt as _;
use serde::{Deserialize, Serialize};

use crate::memory::MemoryStore;

const LOCKS_DIR: &str = "store-locks";
const OWNER_FILE: &str = "owner.json";
const LEASE_FILE: &str = "lease";
const OWNER_VERSION: u8 = 1;
const MAX_OWNER_BYTES: u64 = 4 * 1024;
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_millis(2);
const STALE_AFTER: Duration = Duration::from_secs(5 * 60);
const FUTURE_CLOCK_SKEW: Duration = Duration::from_secs(60);
static NONCE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LockOwner {
    version: u8,
    created_at_ms: u64,
    pid: u32,
    nonce: String,
}

/// RAII ownership of one logical store's cross-process lock.
///
/// Dropping the guard releases only the lock whose owner nonce still matches.
/// If stale recovery replaced it, the old guard becomes a harmless no-op.
#[derive(Debug)]
pub struct StoreLock {
    root: Arc<crate::fs::RootedDir>,
    lock: PathBuf,
    nonce: String,
    lease: Option<std::fs::File>,
    namespace_guard: std::fs::File,
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let owner_path = self.lock.join(OWNER_FILE);
        let Ok(owner) = read_owner(&self.root, &owner_path) else {
            return;
        };
        if owner.nonce != self.nonce {
            return;
        }
        let _ = self.root.remove_regular_file(&owner_path);
        // Keep the OS lease until the owner marker is gone. A contender then
        // cannot classify this directory as stale while release is in flight.
        drop(self.lease.take());
        let _ = self.root.remove_regular_file(&self.lock.join(LEASE_FILE));
        let _ = self.root.remove_empty_dir(&self.lock);
        let _ = fs2::FileExt::unlock(&self.namespace_guard);
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

fn unique_nonce(tag: &str) -> String {
    let sequence = NONCE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    format!("{tag}-{:x}-{nanos:x}-{sequence:x}", std::process::id())
}

fn ensure_lock_root(
    root: &crate::fs::RootedDir,
    state_directory: bool,
) -> std::io::Result<PathBuf> {
    let memory = if state_directory {
        PathBuf::from("memory")
    } else {
        root.ensure_dir(Path::new(".umadev"), false)?;
        PathBuf::from(".umadev/memory")
    };
    root.ensure_dir(&memory, true)?;
    let locks = memory.join(LOCKS_DIR);
    root.ensure_dir(&locks, false)?;
    Ok(locks)
}

fn read_owner(root: &crate::fs::RootedDir, path: &Path) -> std::io::Result<LockOwner> {
    let bytes = root.read_bounded(path, MAX_OWNER_BYTES)?;
    let owner: LockOwner = serde_json::from_slice(&bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    if owner.version != OWNER_VERSION || owner.nonce.is_empty() || owner.nonce.len() > 192 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid memory-store lock owner",
        ));
    }
    Ok(owner)
}

fn modified_at_ms(root: &crate::fs::RootedDir, path: &Path) -> Option<u64> {
    root.directory_modified(path)
        .ok()
        .flatten()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
}

fn lock_age_ms(root: &crate::fs::RootedDir, lock: &Path, now: u64) -> Option<u64> {
    let future_limit =
        now.saturating_add(u64::try_from(FUTURE_CLOCK_SKEW.as_millis()).unwrap_or(u64::MAX));
    let created_at = read_owner(root, &lock.join(OWNER_FILE))
        .ok()
        .map(|owner| owner.created_at_ms)
        .filter(|created_at| *created_at <= future_limit)
        .or_else(|| modified_at_ms(root, lock))?;
    Some(now.saturating_sub(created_at))
}

fn namespace_guard_path(lock: &Path) -> std::io::Result<PathBuf> {
    let name = lock.file_name().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "store lock has no name")
    })?;
    let mut guard = OsString::from(".");
    guard.push(name);
    guard.push(".guard");
    Ok(lock.with_file_name(guard))
}

fn try_acquire_namespace_guard(
    root: &crate::fs::RootedDir,
    lock: &Path,
) -> std::io::Result<Option<std::fs::File>> {
    let file = root.open_private_lock(&namespace_guard_path(lock)?, false)?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(file)),
        Err(error) if crate::fs::lock_error_is_contention(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

fn reclaim_stale_lock(
    root: &crate::fs::RootedDir,
    lock: &Path,
    _namespace_guard: &std::fs::File,
    stale_after: Duration,
) -> bool {
    if !root.is_real_dir(lock).unwrap_or(false) {
        return false;
    }
    let stale_ms = u64::try_from(stale_after.as_millis()).unwrap_or(u64::MAX);
    if lock_age_ms(root, lock, now_ms()).is_none_or(|age| age <= stale_ms) {
        return false;
    }
    let lease_path = lock.join(LEASE_FILE);
    let lease = match root.open_private_existing_lock(&lease_path) {
        Ok(file) => match file.try_lock_exclusive() {
            Ok(()) => Some(file),
            Err(error) if crate::fs::lock_error_is_contention(&error) => return false,
            Err(_) => return false,
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return false,
    };
    let Some(parent) = lock.parent() else {
        return false;
    };
    let name = lock
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("store.lock");
    let isolated = parent.join(format!(".{name}.stale.{}", unique_nonce("reclaim")));
    // The sibling guard excludes creators and other reclaimers after the
    // internal liveness probe, so Windows can release the lease handle before
    // renaming its parent directory without opening an ownership race.
    drop(lease);
    if root.rename(lock, &isolated).is_err() {
        return false;
    }
    let _ = root.remove_regular_file(&isolated.join(OWNER_FILE));
    let _ = root.remove_regular_file(&isolated.join(LEASE_FILE));
    // Never recursively delete an unexpected entry. The stale lock is already
    // isolated under a unique name, so leaving it is safer than following it.
    let _ = root.remove_empty_dir(&isolated);
    true
}

fn acquire_with_timing(
    boundary: &Path,
    store: MemoryStore,
    timeout: Duration,
    poll: Duration,
    stale_after: Duration,
) -> std::io::Result<StoreLock> {
    let root = Arc::new(crate::fs::RootedDir::open(boundary)?);
    let lock_root = ensure_lock_root(&root, false)?;
    acquire_from_root_with_timing(root, &lock_root, store, timeout, poll, stale_after)
}

fn acquire_from_root_with_timing(
    root: Arc<crate::fs::RootedDir>,
    lock_root: &Path,
    store: MemoryStore,
    timeout: Duration,
    poll: Duration,
    stale_after: Duration,
) -> std::io::Result<StoreLock> {
    let lock = lock_root.join(format!("{}.lock", store.id()));
    let started = Instant::now();
    loop {
        let Some(namespace_guard) = try_acquire_namespace_guard(&root, &lock)? else {
            if started.elapsed() >= timeout {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    format!(
                        "memory store `{}` is busy in another UmaDev process",
                        store.id()
                    ),
                ));
            }
            std::thread::sleep(poll);
            continue;
        };
        match root.create_dir(&lock, false) {
            Ok(()) => {
                let owner = LockOwner {
                    version: OWNER_VERSION,
                    created_at_ms: now_ms(),
                    pid: std::process::id(),
                    nonce: unique_nonce(store.id()),
                };
                let bytes = serde_json::to_vec(&owner).map_err(std::io::Error::other)?;
                if let Err(error) = root.atomic_write(&lock.join(OWNER_FILE), &bytes, false) {
                    let _ = root.remove_regular_file(&lock.join(OWNER_FILE));
                    let _ = root.remove_empty_dir(&lock);
                    return Err(error);
                }
                let confirmed = read_owner(&root, &lock.join(OWNER_FILE));
                if confirmed.as_ref().is_ok_and(|seen| seen == &owner) {
                    let lease = match root
                        .open_private_lock(&lock.join(LEASE_FILE), false)
                        .and_then(|file| {
                            file.try_lock_exclusive()?;
                            Ok(file)
                        }) {
                        Ok(file) => file,
                        Err(error) => {
                            if read_owner(&root, &lock.join(OWNER_FILE))
                                .as_ref()
                                .is_ok_and(|seen| seen == &owner)
                            {
                                let _ = root.remove_regular_file(&lock.join(OWNER_FILE));
                                let _ = root.remove_regular_file(&lock.join(LEASE_FILE));
                                let _ = root.remove_empty_dir(&lock);
                            }
                            return Err(error);
                        }
                    };
                    if !read_owner(&root, &lock.join(OWNER_FILE))
                        .as_ref()
                        .is_ok_and(|seen| seen == &owner)
                    {
                        drop(lease);
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::WouldBlock,
                            "memory-store lock namespace changed during lease acquisition",
                        ));
                    }
                    return Ok(StoreLock {
                        root,
                        lock,
                        nonce: owner.nonce,
                        lease: Some(lease),
                        namespace_guard,
                    });
                }
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "memory-store lock ownership changed during acquisition",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let _ = reclaim_stale_lock(&root, &lock, &namespace_guard, stale_after);
                drop(namespace_guard);
                if started.elapsed() >= timeout {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        format!(
                            "memory store `{}` is busy in another UmaDev process",
                            store.id()
                        ),
                    ));
                }
                std::thread::sleep(poll);
            }
            Err(error) => return Err(error),
        }
    }
}

/// Acquire a bounded cross-process lock for one logical memory store.
///
/// The guard serializes the complete read-modify-write transaction. Callers
/// must return a clear no-write result on error; they must never continue from
/// an unlocked snapshot and replace the authoritative file.
pub fn acquire(boundary: &Path, store: MemoryStore) -> std::io::Result<StoreLock> {
    acquire_with_timing(boundary, store, ACQUIRE_TIMEOUT, POLL_INTERVAL, STALE_AFTER)
}

/// Acquire a project-scoped store lock through an already opened boundary.
/// The guard clones only the capability; it never resolves the ambient root
/// path again, so callers can use the same root for the protected read/write.
pub fn acquire_rooted(
    root: &crate::fs::RootedDir,
    store: MemoryStore,
) -> std::io::Result<StoreLock> {
    let root = Arc::new(root.try_clone()?);
    let lock_root = ensure_lock_root(&root, false)?;
    acquire_from_root_with_timing(
        root,
        &lock_root,
        store,
        ACQUIRE_TIMEOUT,
        POLL_INTERVAL,
        STALE_AFTER,
    )
}

/// Acquire a store lock when `state` is the `UmaDev` state directory itself.
/// This is the layout selected by the `UMADEV_HOME` environment override.
pub fn acquire_in_state(state: &Path, store: MemoryStore) -> std::io::Result<StoreLock> {
    let root = Arc::new(crate::fs::RootedDir::open(state)?);
    let lock_root = ensure_lock_root(&root, true)?;
    acquire_from_root_with_timing(
        root,
        &lock_root,
        store,
        ACQUIRE_TIMEOUT,
        POLL_INTERVAL,
        STALE_AFTER,
    )
}

/// State-directory counterpart of [`acquire_rooted`].
pub fn acquire_rooted_in_state(
    root: &crate::fs::RootedDir,
    store: MemoryStore,
) -> std::io::Result<StoreLock> {
    acquire_rooted_in_state_with_timeout(root, store, ACQUIRE_TIMEOUT)
}

/// State-directory counterpart of [`acquire_rooted`] with an explicit bounded
/// acquisition budget.
///
/// Durable-store contract tests use this seam to separate lock/transaction
/// correctness from the product's deliberately short fail-open deadline. Normal
/// callers should use [`acquire_rooted_in_state`], which retains the shared
/// two-second product budget.
#[doc(hidden)]
pub fn acquire_rooted_in_state_with_timeout(
    root: &crate::fs::RootedDir,
    store: MemoryStore,
    timeout: Duration,
) -> std::io::Result<StoreLock> {
    let root = Arc::new(root.try_clone()?);
    let lock_root = ensure_lock_root(&root, true)?;
    acquire_from_root_with_timing(root, &lock_root, store, timeout, POLL_INTERVAL, STALE_AFTER)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_lease_prevents_timestamp_based_stale_reclamation() {
        let temp = tempfile::tempdir().unwrap();
        let first = acquire(temp.path(), MemoryStore::Pitfalls).unwrap();
        let stale_owner = LockOwner {
            version: OWNER_VERSION,
            created_at_ms: 0,
            pid: 1,
            nonce: first.nonce.clone(),
        };
        first
            .root
            .atomic_write(
                &first.lock.join(OWNER_FILE),
                &serde_json::to_vec(&stale_owner).unwrap(),
                false,
            )
            .unwrap();
        assert!(!reclaim_stale_lock(
            &first.root,
            &first.lock,
            &first.namespace_guard,
            Duration::ZERO
        ));
        let lock_path = temp.path().join(&first.lock);
        drop(first);
        assert!(!lock_path.exists());

        let second = acquire(temp.path(), MemoryStore::Pitfalls).unwrap();
        let successor_path = temp.path().join(&second.lock);
        drop(second);
        assert!(!successor_path.exists());
    }

    #[test]
    fn ownerless_crash_lock_is_recovered_after_its_deadline() {
        let temp = tempfile::tempdir().unwrap();
        let rooted = crate::fs::RootedDir::open(temp.path()).unwrap();
        let lock_root = ensure_lock_root(&rooted, false).unwrap();
        let lock = lock_root.join(format!("{}.lock", MemoryStore::Beliefs.id()));
        rooted.create_dir(&lock, false).unwrap();
        std::thread::sleep(Duration::from_millis(2));
        let namespace_guard = try_acquire_namespace_guard(&rooted, &lock)
            .unwrap()
            .unwrap();
        assert!(reclaim_stale_lock(
            &rooted,
            &lock,
            &namespace_guard,
            Duration::ZERO
        ));
        drop(namespace_guard);
        let guard = acquire(temp.path(), MemoryStore::Beliefs).unwrap();
        drop(guard);
        assert!(!temp.path().join(lock).exists());
    }

    #[test]
    fn different_store_locks_do_not_block_each_other() {
        let temp = tempfile::tempdir().unwrap();
        let pitfalls = acquire(temp.path(), MemoryStore::Pitfalls).unwrap();
        let beliefs = acquire(temp.path(), MemoryStore::Beliefs).unwrap();
        drop(beliefs);
        drop(pitfalls);
    }

    #[test]
    fn explicit_state_lock_does_not_append_a_second_umadev_directory() {
        let state = tempfile::tempdir().unwrap();
        let guard = acquire_in_state(state.path(), MemoryStore::Pitfalls).unwrap();
        assert!(guard.lock.starts_with("memory/store-locks"));
        assert!(!state.path().join(".umadev").exists());
        let lock_path = state.path().join(&guard.lock);
        drop(guard);
        assert!(!lock_path.exists());
    }

    #[test]
    fn crashed_process_lock_child() {
        let Some(root) = std::env::var_os("UMADEV_STATE_CRASH_LOCK_ROOT") else {
            return;
        };
        let _guard = acquire(Path::new(&root), MemoryStore::Pitfalls).unwrap();
        // Deliberately skip Rust destructors to model a killed base/host process.
        std::process::exit(0);
    }

    #[test]
    fn crashed_process_stale_lock_is_recovered() {
        let temp = tempfile::tempdir().unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "store_lock::tests::crashed_process_lock_child",
                "--nocapture",
            ])
            .env("UMADEV_STATE_CRASH_LOCK_ROOT", temp.path())
            .status()
            .unwrap();
        assert!(status.success());

        let rooted = crate::fs::RootedDir::open(temp.path()).unwrap();
        let lock = ensure_lock_root(&rooted, false)
            .unwrap()
            .join(format!("{}.lock", MemoryStore::Pitfalls.id()));
        let mut owner = read_owner(&rooted, &lock.join(OWNER_FILE)).unwrap();
        owner.created_at_ms = 0;
        rooted
            .atomic_write(
                &lock.join(OWNER_FILE),
                &serde_json::to_vec(&owner).unwrap(),
                false,
            )
            .unwrap();
        let recovered = acquire_with_timing(
            temp.path(),
            MemoryStore::Pitfalls,
            Duration::from_millis(250),
            Duration::from_millis(1),
            Duration::ZERO,
        )
        .unwrap();
        drop(recovered);
        assert!(!temp.path().join(lock).exists());
    }

    #[cfg(unix)]
    #[test]
    fn rooted_lock_lifecycle_never_touches_a_replacement_boundary() {
        use std::os::unix::fs::symlink;

        let container = tempfile::tempdir().unwrap();
        let boundary = container.path().join("boundary");
        let moved = container.path().join("moved");
        let outside = container.path().join("outside");
        std::fs::create_dir(&boundary).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let rooted = crate::fs::RootedDir::open(&boundary).unwrap();
        let lock_root = ensure_lock_root(&rooted, false).unwrap();

        std::fs::rename(&boundary, &moved).unwrap();
        symlink(&outside, &boundary).unwrap();
        let guard = acquire_from_root_with_timing(
            Arc::new(rooted),
            &lock_root,
            MemoryStore::Pitfalls,
            Duration::from_millis(50),
            Duration::from_millis(1),
            Duration::ZERO,
        )
        .unwrap();
        assert!(outside.read_dir().unwrap().next().is_none());
        drop(guard);
        assert!(outside.read_dir().unwrap().next().is_none());
        assert!(!moved
            .join(".umadev/memory/store-locks/pitfalls.lock")
            .exists());
    }
}
