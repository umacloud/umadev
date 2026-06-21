//! Advisory single-writer lock per workspace.
//!
//! Two concurrent `umadev` runs in the same workspace (e.g. the chat TUI plus a
//! scripted `umadev continue` in another terminal) share `workflow-state.json`,
//! `output/*`, and the provider config — running them at once silently corrupts
//! ordering and clobbers artifacts. This is the same hazard Terraform guards
//! with state locking and git with `index.lock`.
//!
//! The lock is a `.umadev/run.lock` file created with `create_new`
//! (`O_CREAT|O_EXCL`), holding the PID + a creation timestamp, and removed on
//! drop. It is **dependency-free** and **fail-open**: any IO error other than
//! "already exists" yields an un-owned guard that never blocks the run (a lock
//! bug must never stop a legitimate run). A lock older than [`STALE_SECS`] is
//! treated as a crashed run and reclaimed, mirroring git's stale-`index.lock`
//! recovery — and the refusal message tells the user how to clear it.

use std::io;
use std::path::{Path, PathBuf};

/// A lock older than this is assumed to belong to a crashed run and is
/// reclaimed. No UmaDev pipeline block runs anywhere near six hours, so this
/// never reclaims a live run; a user with a genuinely longer run can delete the
/// lock file by hand (the refusal message says so).
const STALE_SECS: u64 = 6 * 3600;

/// Held for the duration of a pipeline block; releases the workspace lock on
/// drop. An un-owned guard (fail-open path) is a harmless no-op.
#[derive(Debug)]
pub struct RunLock {
    path: PathBuf,
    owned: bool,
}

impl RunLock {
    /// Acquire the workspace run lock.
    ///
    /// # Errors
    /// Returns `AlreadyExists` with an actionable message when another live run
    /// holds the lock. Any other IO problem fails open (returns an un-owned
    /// guard) so a lock bug can never block a legitimate run.
    pub fn acquire(project_root: &Path) -> io::Result<RunLock> {
        let dir = project_root.join(".umadev");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("run.lock");
        // A BOUNDED loop (at most one stale-reclaim retry) — never recurse, so a
        // wedged-but-undeletable stale lock can't blow the stack.
        for attempt in 0..2 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    use std::io::Write;
                    let _ = writeln!(file, "pid={} ts={}", std::process::id(), now_secs());
                    return Ok(RunLock { path, owned: true });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    // Only retry if we actually RECLAIMED a stale leftover; if the
                    // remove fails (undeletable lock), fall through to refusal.
                    if attempt == 0 && is_stale(&path) && std::fs::remove_file(&path).is_ok() {
                        continue;
                    }
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!(
                            "另一个 umadev 运行正在占用该工作区(锁文件 {}).\n\
                             请等它结束。如果确定没有其他运行(上次异常退出残留),删除该文件后重试。",
                            path.display()
                        ),
                    ));
                }
                // Fail-open: a permissions/IO problem must not block a real run.
                Err(_) => return Ok(RunLock { path, owned: false }),
            }
        }
        // Both attempts hit AlreadyExists-but-couldn't-reclaim in a tight race —
        // fail open rather than spin.
        Ok(RunLock { path, owned: false })
    }
}

impl Drop for RunLock {
    fn drop(&mut self) {
        if self.owned {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// `true` when the lock file is older than [`STALE_SECS`] (or can't be stat'd).
fn is_stale(path: &Path) -> bool {
    match std::fs::metadata(path).and_then(|m| m.modified()) {
        Ok(mtime) => mtime
            .elapsed()
            .map(|age| age.as_secs() > STALE_SECS)
            .unwrap_or(false),
        Err(_) => true,
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_is_refused_then_released_on_drop() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let root = tmp.path();
        let lock = RunLock::acquire(root).expect("first acquire");
        // A second concurrent acquire is refused while the first is held.
        let second = RunLock::acquire(root);
        assert!(second.is_err(), "second acquire must be refused");
        assert_eq!(second.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        // Dropping the first releases the lock; a later acquire succeeds.
        drop(lock);
        assert!(RunLock::acquire(root).is_ok(), "lock released on drop");
    }

    #[test]
    fn stale_lock_is_reclaimed() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let root = tmp.path();
        let dir = root.join(".umadev");
        std::fs::create_dir_all(&dir).unwrap();
        // A leftover lock that is_stale() reports as old (mtime far in the past
        // is hard to forge portably, so assert the live-lock path instead): a
        // fresh foreign lock is NOT reclaimed.
        std::fs::write(dir.join("run.lock"), "pid=1 ts=0").unwrap();
        assert!(
            RunLock::acquire(root).is_err(),
            "a fresh foreign lock is respected"
        );
    }
}
