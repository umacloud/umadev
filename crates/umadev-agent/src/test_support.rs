//! Test-only helpers shared across the crate's unit tests.
//!
//! Gated on `#[cfg(test)]` — never compiled into the shipped library.

use std::ffi::OsString;
use std::sync::{Mutex, MutexGuard};
use tempfile::TempDir;

/// The crate's **one** home/knowledge env lock.
///
/// `HOME` / `USERPROFILE` / `UMADEV_KNOWLEDGE_DIR` are process-global, and the
/// libtest harness runs this crate's tests on parallel threads of a *single*
/// process. Any test that mutates one of those three vars must serialise against
/// **every other** test that mutates or reads them — and it must do so on this
/// exact mutex. Two separate mutexes guarding the same global is the same as no
/// mutex at all: guard A can restore the real `HOME` out from under a test that
/// only holds guard B, at which point the developer's real
/// `~/.umadev/knowledge` (staged there by the `umadev` binary) leaks into a test
/// that asserts "no corpus reachable". So: **do not add a second HOME lock.**
/// Route new home-mutating tests through [`NoBundledCorpus`] or [`TempHome`].
static ENV_GUARD: Mutex<()> = Mutex::new(());

/// Take the shared home/knowledge env lock. Poison is deliberately ignored: a
/// panicking test elsewhere must not cascade into unrelated failures here.
fn env_guard() -> MutexGuard<'static, ()> {
    ENV_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// RAII guard that points `HOME`/`USERPROFILE` at a corpus-free temp dir and
/// clears `UMADEV_KNOWLEDGE_DIR`, so `knowledge_root`'s bundled-corpus fallbacks
/// resolve to nothing. Restores the prior env on drop. Hold it for the lifetime
/// of any test that depends on "no bundled corpus reachable".
pub(crate) struct NoBundledCorpus {
    _lock: MutexGuard<'static, ()>,
    scratch: TempDir,
    prev_home: Option<OsString>,
    prev_userprofile: Option<OsString>,
    prev_kdir: Option<OsString>,
}

impl NoBundledCorpus {
    /// Take the env lock and isolate `HOME`/`USERPROFILE`/`UMADEV_KNOWLEDGE_DIR`.
    pub(crate) fn new() -> Self {
        let lock = env_guard();
        let prev_home = std::env::var_os("HOME");
        let prev_userprofile = std::env::var_os("USERPROFILE");
        let prev_kdir = std::env::var_os("UMADEV_KNOWLEDGE_DIR");
        let scratch = TempDir::new().unwrap();
        // A fresh temp home has no ~/.umadev/knowledge.
        std::env::set_var("HOME", scratch.path());
        std::env::set_var("USERPROFILE", scratch.path());
        std::env::remove_var("UMADEV_KNOWLEDGE_DIR");
        Self {
            _lock: lock,
            scratch,
            prev_home,
            prev_userprofile,
            prev_kdir,
        }
    }

    /// The temp home dir the guard installed (so a test can stage a corpus under
    /// `<home>/.umadev/knowledge` to exercise the home-dir fallback branch).
    pub(crate) fn home(&self) -> &std::path::Path {
        self.scratch.path()
    }
}

impl Drop for NoBundledCorpus {
    fn drop(&mut self) {
        restore("HOME", self.prev_home.take());
        restore("USERPROFILE", self.prev_userprofile.take());
        restore("UMADEV_KNOWLEDGE_DIR", self.prev_kdir.take());
    }
}

/// RAII guard that isolates the lessons subsystem's home resolution to a
/// throwaway temp dir, so a real sediment/promotion can neither READ nor POLLUTE
/// the developer's actual `~/.umadev/learned`.
///
/// It retains [`ENV_GUARD`] compatibility with [`NoBundledCorpus`], while using
/// a thread-local override so parallel unrelated tests cannot create files in
/// its scratch directory.
pub(crate) struct TempHome {
    _lock: MutexGuard<'static, ()>,
    _tmp: TempDir,
}

impl TempHome {
    /// Take the shared env lock and install a lessons-only thread-local home.
    /// Avoiding process-global HOME mutation makes parallel tests deterministic.
    pub(crate) fn new() -> Self {
        let lock = env_guard();
        let tmp = TempDir::new().unwrap();
        crate::lessons::set_test_home_override(Some(tmp.path().to_path_buf()));
        Self {
            _lock: lock,
            _tmp: tmp,
        }
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        crate::lessons::set_test_home_override(None);
    }
}

/// Pin this test process's installation state directory (`~/.umadev`) to a
/// scratch directory, so approval memory, saved-run stamps and the provenance
/// key never touch the developer's real home. Idempotent; the directory lives
/// for the whole process and is shared by every test in it.
pub(crate) fn isolate_state_directory() {
    umadev_state::privacy::pin_state_directory(|| {
        TempDir::with_prefix("umadev-test-state-")
            .expect("scratch state directory")
            .keep()
    });
}

fn restore(key: &str, val: Option<OsString>) {
    match val {
        Some(v) => std::env::set_var(key, v),
        None => std::env::remove_var(key),
    }
}
