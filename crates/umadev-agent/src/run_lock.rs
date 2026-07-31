//! Advisory single-writer lock per workspace.
//!
//! Two concurrent `umadev` runs in the same workspace (e.g. the chat TUI plus a
//! scripted `umadev continue` in another terminal) share `workflow-state.json`,
//! `output/*`, and the provider config — running them at once silently corrupts
//! ordering and clobbers artifacts. This is the same hazard Terraform guards
//! with state locking and Git with `index.lock`.
//!
//! The ownership primitive is a kernel advisory lock held on the persistent
//! `.umadev/run.lock.guard` file. `.umadev/run.owner` is diagnostic owner
//! metadata; `.umadev/run.lock` is a permanent v2 compatibility fence that
//! makes older create-new clients fail closed. The kernel releases ownership on
//! drop or process death, so crash recovery never needs a racy
//! check→unlink→create takeover. Execution callers fail closed when ownership
//! cannot be proved.
//!
//! Cooperating processes must run as the same OS identity. Different OS
//! identities are unsupported and mutual exclusion is not guaranteed. The Unix
//! external namespace guard is host-local; Windows relies on one stable user
//! temp directory for that OS identity. The external guard protects new clients
//! from a replaced in-workspace namespace, but older clients know only the
//! permanent `.umadev/run.lock` fence. If that fence is deleted, `.umadev` is
//! replaced, or `git clean -fdx` removes it while an old client may run,
//! old/new mutual exclusion cannot be guaranteed.
//!
//! The workspace root path and every alias used to reach it must also remain
//! stable for the lifetime of a run: do not rename or replace the root, and do
//! not retarget a symlink/junction that was used to enter it. Callers continue
//! using their original project-root path after acquisition, so changing that
//! namespace can redirect later writes outside the identity that was locked.
//!
//! The filesystem must propagate `flock`/`LockFileEx`-equivalent advisory
//! locks coherently. Network mounts without a coherent lock manager are
//! unsupported. Even with coherent advisory locks, a shared workspace is
//! unsupported if its `.umadev` namespace can be replaced during an active
//! run. A single-machine process test cannot certify an NFS/SMB deployment.
//!
//! ## Owner liveness
//!
//! New releases recover automatically through the kernel guard. The owner
//! classifier remains for crash markers and migration diagnostics. An existing
//! legacy `run.lock` is never guessed safe or overwritten; migration is
//! explicit and requires all older UmaDev processes to remain stopped.
//!
//! The shared classifier distinguishes live, provably abandoned, and
//! unattributable owners using host, boot, PID and age. The v2 kernel lock does
//! not need it for takeover; temporary-rewind recovery still does. Legacy lock
//! files are diagnosed with it but remain fail-closed because deleting one
//! after a check would race an older writer's create-new operation.
//!
//! That verdict comes from the internal claim-owner classifier — the single
//! owner-liveness rule shared with the temporary-rewind crash marker in
//! [`crate::checkpoint`]. Two files answering the same question differently
//! is exactly how a live holder's claim gets reclaimed.
//!
//! Liveness probing is conservative: if we cannot determine whether the PID is
//! alive, we do not call it dead; the age fallback eventually frees a genuinely
//! abandoned unattributable claim.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use fs2::FileExt;
use umadev_state::fs::lock_error_is_contention;

mod fence;
mod namespace;
mod owner;

pub use fence::{inspect_fence, migrate_fence, RunLockFenceMigration, RunLockFenceStatus};
pub(crate) use owner::{
    boot_id, classify_claim_owner, hostname, pid_is_alive, ClaimOwner, OwnerLiveness,
};

use fence::ensure_v2_fence;
use namespace::{
    ensure_local_git_excludes, external_guard_path, open_guard_file, same_file_identity,
};
use owner::{holder_is_self, holder_nonce_matches, unique_nonce, write_owner};

#[cfg(test)]
use fence::V2_FENCE;
#[cfg(test)]
use owner::{is_stale, now_secs, Owner, STALE_SECS};

/// Held for the duration of a pipeline block; releases the workspace lock on
/// drop. The route-compatible [`RunLock::acquire`] may return an unowned guard
/// for historical callers; [`RunLock::acquire_for_run`] always rejects that
/// state before execution.
#[derive(Debug)]
pub struct RunLock {
    path: PathBuf,
    namespace_guard: Option<File>,
    guard: Option<File>,
    nonce: String,
    owned: bool,
}
/// Why the lock is being taken — decides how a lock already held by **this same
/// process** is treated. The two intents are genuinely different:
///
/// - [`AcquireIntent::Route`] is the **input-routing / queue** layer (the chat
///   TUI deciding where a freshly-typed line goes). A same-PID lock means a run
///   this session already kicked off is still in flight, so the right answer is
///   *queue the input into it* — surfaced as a `WouldBlock` signal, never a
///   reclaim. Two run blocks could legitimately co-exist here (one running, the
///   user typing the next).
/// - [`AcquireIntent::Run`] is a real execution path. Unlocked same-PID metadata
///   is crash residue and is replaced safely after the kernel guard is acquired.
///   If the kernel guard is still held, it is an active writer and the second
///   execution is refused instead of manufacturing a second owner.
///
/// Both intents use the same kernel exclusion; the distinction changes only
/// the error surfaced for a same-process active owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AcquireIntent {
    /// Input-routing layer: same-PID lock → `WouldBlock` queue signal.
    Route,
    /// Real execution path: replace unlocked residue, but refuse an active writer.
    Run,
}
impl RunLock {
    /// Whether this guard proved exclusive ownership of the workspace lock.
    ///
    /// Most long-running workflows retain the historical fail-open behavior of
    /// [`acquire`](Self::acquire). Short host-owned mutations that must never run
    /// without a real single-writer guarantee can inspect this bit and fail closed.
    #[must_use]
    pub const fn is_owned(&self) -> bool {
        self.owned
    }

    /// Acquire the workspace run lock from the **input-routing / queue** layer.
    ///
    /// Use this where the caller is *deciding what to do with input*, not where
    /// it is about to drive the pipeline. A lock already held by **this** process
    /// means our session has a run in flight → the caller should queue the input
    /// into it; that case is signalled with [`io::ErrorKind::WouldBlock`].
    ///
    /// # Errors
    /// - `WouldBlock` when **this** process already holds the lock (queue signal).
    /// - `AlreadyExists` with an actionable message when another **live** run on
    ///   this host holds it.
    ///
    /// A crashed/killed holder's kernel lock is released automatically. Owner
    /// liveness is consulted only for compatibility metadata from older builds.
    /// Other IO failures return an un-owned guard so historical pipeline callers
    /// remain fail-open.
    pub fn acquire(project_root: &Path) -> io::Result<RunLock> {
        Self::acquire_with(project_root, AcquireIntent::Route)
    }

    /// Acquire the workspace run lock for a real **execution** block
    /// (`run_initial_block`, the `continue_after_*` blocks, `run_light`,
    /// `redo_phase`).
    ///
    /// Unlocked same-PID metadata is treated as crash residue after obtaining
    /// the kernel guard. A still-locked same-PID owner is active and returns
    /// `WouldBlock`; it is never deleted or converted into a second writer.
    ///
    /// # Errors
    /// Returns `WouldBlock` for an active same-process writer,
    /// `AlreadyExists` for another/legacy writer, and `PermissionDenied` when
    /// exclusive ownership cannot be proved. Execution never continues unowned.
    pub fn acquire_for_run(project_root: &Path) -> io::Result<RunLock> {
        let lock = Self::acquire_with(project_root, AcquireIntent::Run)?;
        if !lock.is_owned() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "无法建立工作区单写者锁,已拒绝无锁执行 / unable to establish the workspace single-writer lock; unlocked execution was refused",
            ));
        }
        // WORKSPACE-INTEGRITY BACKSTOP, while holding the writer guard. A previous run that was
        // SIGKILLed / OOM-killed / whose terminal was closed inside a temporary
        // evidence rewind left the user's tracked source files reverted to an earlier
        // step's state — no destructor ran, so nothing put them back. Restore the
        // PRESENT before this run starts writing on top of a tree that is silently in
        // the past. Fail-open and strictly conservative: it no-ops unless the marker's
        // owner is provably gone, and it can only reset to a checkpoint we ourselves
        // wrote (see `checkpoint::recover_abandoned_temp_rewind`).
        if let Some(note) = crate::checkpoint::recover_abandoned_temp_rewind(project_root) {
            // A `tracing::warn!` alone is invisible to the person this is FOR: under the TUI
            // the log goes to a file. Hand the note to the surface that can actually speak
            // (the transcript drains it), and keep the log line for post-mortems.
            tracing::warn!("{note}");
            crate::checkpoint::record_workspace_notice(note);
        }
        Ok(lock)
    }

    /// Shared acquisition core. `intent` only changes how a lock held by **this**
    /// process is handled (see [`AcquireIntent`]); every external-holder path is
    /// identical for both intents.
    fn acquire_with(project_root: &Path, intent: AcquireIntent) -> io::Result<RunLock> {
        let fallback_path = project_root.join(".umadev").join("run.lock");
        let root = match std::fs::canonicalize(project_root) {
            Ok(root) if umadev_state::fs::real_dir(&root) => root,
            _ => return Ok(Self::unowned(fallback_path)),
        };
        let Ok(root_identity) = std::fs::symlink_metadata(&root) else {
            return Ok(Self::unowned(fallback_path));
        };
        // This guard is deliberately OUTSIDE the workspace: replacing
        // `.umadev`, deleting ignored files with `git clean -fdx`, or swapping
        // either in-workspace guard pathname must not manufacture a second guard
        // inode and a second writer.
        let Ok(namespace_guard_path) = external_guard_path(&root, &root_identity) else {
            return Ok(Self::unowned(fallback_path));
        };
        let Ok(namespace_guard) = open_guard_file(&namespace_guard_path) else {
            return Ok(Self::unowned(fallback_path));
        };
        if let Err(error) = FileExt::try_lock_exclusive(&namespace_guard) {
            if !lock_error_is_contention(&error) {
                return Ok(Self::unowned(fallback_path));
            }
            return Err(lock_busy_error(
                &root.join(".umadev/run.owner"),
                &root.join(".umadev/run.lock"),
                intent,
            ));
        }
        if !std::fs::symlink_metadata(&root)
            .is_ok_and(|current| same_file_identity(&root_identity, &current))
        {
            let _ = FileExt::unlock(&namespace_guard);
            return Ok(Self::unowned(fallback_path));
        }
        let Ok(dir) = umadev_state::fs::ensure_real_child_dir(&root, ".umadev") else {
            let _ = FileExt::unlock(&namespace_guard);
            return Ok(Self::unowned(fallback_path));
        };
        let fence_path = dir.join("run.lock");
        let path = dir.join("run.owner");
        let guard_path = dir.join("run.lock.guard");
        let Ok(guard) = open_guard_file(&guard_path) else {
            let _ = FileExt::unlock(&namespace_guard);
            return Ok(Self::unowned(path));
        };

        if let Err(error) = FileExt::try_lock_exclusive(&guard) {
            let _ = FileExt::unlock(&namespace_guard);
            if !lock_error_is_contention(&error) {
                return Ok(Self::unowned(path));
            }
            return Err(lock_busy_error(&path, &fence_path, intent));
        }

        // Lock metadata is local runtime state, not a source change. UmaDev's
        // generated root .gitignore already ignores `.umadev/`; repositories
        // adopted without /init may not have that rule, so add exact lock-only
        // rows to Git's local (untracked) exclude file when `.git` is a real
        // directory. Best-effort and deliberately narrow: no user-owned source
        // or tracked ignore file is changed.
        ensure_local_git_excludes(&root);

        // Atomically fence older releases before publishing the v2 owner. The
        // permanent regular file is deliberately parseable by the v1 protocol
        // but can never become age-stale, so an old create_new/remove loop always
        // refuses instead of becoming a second writer.
        if let Err(error) = ensure_v2_fence(&fence_path) {
            let _ = FileExt::unlock(&guard);
            let _ = FileExt::unlock(&namespace_guard);
            return Err(error);
        }

        let nonce = unique_nonce();
        if write_owner(&path, &nonce).is_err() || !holder_nonce_matches(&path, &nonce) {
            let _ = FileExt::unlock(&guard);
            let _ = FileExt::unlock(&namespace_guard);
            return Ok(Self::unowned(path));
        }

        Ok(RunLock {
            path,
            namespace_guard: Some(namespace_guard),
            guard: Some(guard),
            nonce,
            owned: true,
        })
    }

    fn unowned(path: PathBuf) -> RunLock {
        RunLock {
            path,
            namespace_guard: None,
            guard: None,
            nonce: String::new(),
            owned: false,
        }
    }
}

impl Drop for RunLock {
    fn drop(&mut self) {
        if self.owned && holder_nonce_matches(&self.path, &self.nonce) {
            let _ = umadev_state::fs::remove_regular_file(&self.path);
        }
        if let Some(guard) = self.guard.take() {
            let _ = FileExt::unlock(&guard);
        }
        if let Some(namespace_guard) = self.namespace_guard.take() {
            let _ = FileExt::unlock(&namespace_guard);
        }
        self.owned = false;
    }
}
fn lock_busy_error(owner_path: &Path, fence_path: &Path, intent: AcquireIntent) -> io::Error {
    if holder_is_self(owner_path) {
        let message = match intent {
            AcquireIntent::Route => {
                "本会话已有一个 umadev run 正在进行中 —— \
                 你的输入会排队发给这个 run,而不是另起新 run。"
            }
            AcquireIntent::Run => {
                "本会话已有一个 umadev 执行块正在持有工作区写锁；\
                 为避免并发写入，本次执行不会另起 writer。"
            }
        };
        return io::Error::new(io::ErrorKind::WouldBlock, message);
    }
    foreign_holder_error(fence_path)
}
fn foreign_holder_error(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "另一个 umadev 运行正在占用该工作区(锁文件 {}).\n\
             请等它结束；崩溃遗留锁会由操作系统自动释放，无需手动删除。",
            path.display()
        ),
    )
}

#[cfg(test)]
#[path = "run_lock_tests.rs"]
mod tests;
