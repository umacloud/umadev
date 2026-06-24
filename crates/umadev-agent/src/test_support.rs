//! Test-only helpers shared across the crate's unit tests.
//!
//! Gated on `#[cfg(test)]` — never compiled into the shipped library.

use std::ffi::OsString;
use std::sync::{Mutex, MutexGuard};
use tempfile::TempDir;

/// Serialises env-mutating tests. [`crate::phases::knowledge_root`] consults
/// `UMADEV_KNOWLEDGE_DIR` and `~/.umadev/knowledge`; tests that assert "no
/// corpus found" must neutralise both deterministically (regardless of the host
/// machine having run the `umadev` binary, which stages the embedded corpus to
/// `~/.umadev/knowledge`). Process env is global, so they take this lock.
static ENV_GUARD: Mutex<()> = Mutex::new(());

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
        let lock = ENV_GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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

fn restore(key: &str, val: Option<OsString>) {
    match val {
        Some(v) => std::env::set_var(key, v),
        None => std::env::remove_var(key),
    }
}
