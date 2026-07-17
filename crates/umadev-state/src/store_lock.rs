//! Bounded cross-process locks for one logical memory store.
//!
//! A directory is the atomic ownership primitive on every supported platform.
//! The owner record carries a nonce so an expired guard can never remove a
//! successor's lock after stale recovery. Acquisition has a hard deadline: a
//! memory write may fail open, but it may not wedge the host indefinitely.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::memory::MemoryStore;

const LOCKS_DIR: &str = "store-locks";
const OWNER_FILE: &str = "owner.json";
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
    path: PathBuf,
    nonce: String,
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let owner_path = self.path.join(OWNER_FILE);
        let Ok(owner) = read_owner(&owner_path) else {
            return;
        };
        if owner.nonce != self.nonce {
            return;
        }
        let _ = crate::fs::remove_regular_file(&owner_path);
        let _ = crate::fs::remove_empty_dir(&self.path);
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

fn ensure_lock_root(boundary: &Path) -> std::io::Result<PathBuf> {
    let root = std::fs::canonicalize(boundary)?;
    if !crate::fs::real_dir(&root) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "memory-store lock boundary is not a real directory",
        ));
    }
    let umadev = crate::fs::ensure_real_child_dir(&root, ".umadev")?;
    let memory = crate::fs::ensure_real_child_dir(&umadev, "memory")?;
    crate::fs::ensure_real_child_dir(&memory, LOCKS_DIR)
}

fn read_owner(path: &Path) -> std::io::Result<LockOwner> {
    let bytes = crate::fs::read_bounded(path, MAX_OWNER_BYTES)?;
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

fn modified_at_ms(path: &Path) -> Option<u64> {
    std::fs::symlink_metadata(path)
        .ok()
        .filter(crate::fs::metadata_is_real_dir)
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
}

fn lock_age_ms(lock: &Path, now: u64) -> Option<u64> {
    let future_limit =
        now.saturating_add(u64::try_from(FUTURE_CLOCK_SKEW.as_millis()).unwrap_or(u64::MAX));
    let created_at = read_owner(&lock.join(OWNER_FILE))
        .ok()
        .map(|owner| owner.created_at_ms)
        .filter(|created_at| *created_at <= future_limit)
        .or_else(|| modified_at_ms(lock))?;
    Some(now.saturating_sub(created_at))
}

fn reclaim_stale_lock(lock: &Path, stale_after: Duration) -> bool {
    if !crate::fs::real_dir(lock) {
        return false;
    }
    let stale_ms = u64::try_from(stale_after.as_millis()).unwrap_or(u64::MAX);
    if lock_age_ms(lock, now_ms()).is_none_or(|age| age <= stale_ms) {
        return false;
    }
    let Some(parent) = lock.parent() else {
        return false;
    };
    let name = lock
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("store.lock");
    let isolated = parent.join(format!(".{name}.stale.{}", unique_nonce("reclaim")));
    if std::fs::rename(lock, &isolated).is_err() {
        return false;
    }
    let _ = crate::fs::remove_regular_file(&isolated.join(OWNER_FILE));
    // Never recursively delete an unexpected entry. The stale lock is already
    // isolated under a unique name, so leaving it is safer than following it.
    let _ = crate::fs::remove_empty_dir(&isolated);
    true
}

fn acquire_with_timing(
    boundary: &Path,
    store: MemoryStore,
    timeout: Duration,
    poll: Duration,
    stale_after: Duration,
) -> std::io::Result<StoreLock> {
    let lock_root = ensure_lock_root(boundary)?;
    let lock = lock_root.join(format!("{}.lock", store.id()));
    let started = Instant::now();
    loop {
        match crate::fs::create_dir(&lock) {
            Ok(()) => {
                let owner = LockOwner {
                    version: OWNER_VERSION,
                    created_at_ms: now_ms(),
                    pid: std::process::id(),
                    nonce: unique_nonce(store.id()),
                };
                let bytes = serde_json::to_vec(&owner).map_err(std::io::Error::other)?;
                if let Err(error) = crate::fs::atomic_write(&lock.join(OWNER_FILE), &bytes) {
                    let _ = crate::fs::remove_regular_file(&lock.join(OWNER_FILE));
                    let _ = crate::fs::remove_empty_dir(&lock);
                    return Err(error);
                }
                let confirmed = read_owner(&lock.join(OWNER_FILE));
                if confirmed.as_ref().is_ok_and(|seen| seen == &owner) {
                    return Ok(StoreLock {
                        path: lock,
                        nonce: owner.nonce,
                    });
                }
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "memory-store lock ownership changed during acquisition",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let _ = reclaim_stale_lock(&lock, stale_after);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_owner_is_reclaimed_and_old_guard_cannot_remove_successor() {
        let temp = tempfile::tempdir().unwrap();
        let first = acquire(temp.path(), MemoryStore::Pitfalls).unwrap();
        let stale_owner = LockOwner {
            version: OWNER_VERSION,
            created_at_ms: 0,
            pid: 1,
            nonce: first.nonce.clone(),
        };
        crate::fs::atomic_write(
            &first.path.join(OWNER_FILE),
            &serde_json::to_vec(&stale_owner).unwrap(),
        )
        .unwrap();
        assert!(reclaim_stale_lock(&first.path, Duration::ZERO));

        let second = acquire(temp.path(), MemoryStore::Pitfalls).unwrap();
        let successor_path = second.path.clone();
        drop(first);
        assert!(crate::fs::real_dir(&successor_path));
        drop(second);
        assert!(!successor_path.exists());
    }

    #[test]
    fn ownerless_crash_lock_is_recovered_after_its_deadline() {
        let temp = tempfile::tempdir().unwrap();
        let root = ensure_lock_root(temp.path()).unwrap();
        let lock = root.join(format!("{}.lock", MemoryStore::Beliefs.id()));
        std::fs::create_dir(&lock).unwrap();
        std::thread::sleep(Duration::from_millis(2));
        assert!(reclaim_stale_lock(&lock, Duration::ZERO));
        let guard = acquire(temp.path(), MemoryStore::Beliefs).unwrap();
        drop(guard);
        assert!(!lock.exists());
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

        let lock = ensure_lock_root(temp.path())
            .unwrap()
            .join(format!("{}.lock", MemoryStore::Pitfalls.id()));
        let mut owner = read_owner(&lock.join(OWNER_FILE)).unwrap();
        owner.created_at_ms = 0;
        crate::fs::atomic_write(&lock.join(OWNER_FILE), &serde_json::to_vec(&owner).unwrap())
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
        assert!(!lock.exists());
    }
}
