//! Ships the curated `knowledge/` corpus *inside* the binary and stages it on
//! the user's machine so knowledge recall works in any project with zero setup.
//!
//! ## Why this exists
//!
//! The agent's commercial-engineering standards (layering, security baseline,
//! design-system rules, …) live in the repo's `knowledge/` tree and are
//! retrieved at runtime by [`umadev_agent::phases::knowledge_root`]. Before
//! this module, `knowledge_root` could only find a `knowledge/` dir *inside the
//! user's own project* (which they never have) or one pointed to by
//! `UMADEV_KNOWLEDGE_DIR` (which nobody sets). So for end users the corpus
//! resolved to an empty path → recall returned nothing → the commercial
//! standards never reached the borrowed brain.
//!
//! ## What this does
//!
//! A build-time allowlisted snapshot of the `knowledge/` tree is embedded into
//! the binary via [`include_dir::include_dir!`]. Hidden directories, symlinks, generated
//! indexes, and binary caches are excluded before macro expansion. On startup
//! [`ensure_staged`] extracts it once to `~/.umadev/knowledge` (guarded by a
//! `.version` marker so an already-current corpus is skipped) and points
//! `UMADEV_KNOWLEDGE_DIR` at it. Every later `knowledge_root` call — TUI, CLI,
//! director loop alike — then discovers the full corpus through the env var (or
//! the new `~/.umadev/knowledge` fallback branch in `knowledge_root` itself).
//!
//! ## Fail-open contract
//!
//! Mirrors the rest of UmaDev: this can never block or crash startup. Every
//! step (resolve home, write files, set env var) is best-effort; on *any*
//! failure the function returns and knowledge recall simply degrades to the
//! prior empty behaviour. The binary keeps running regardless.

use fs2::FileExt as _;
use include_dir::{include_dir, Dir};
use std::path::{Path, PathBuf};
use umadev_state::fs::RootedDir;

/// The embedded, build-time-sanitized text corpus.
static KNOWLEDGE: Dir<'static> = include_dir!("$OUT_DIR/embedded-knowledge");

/// Marker filename written into the staged corpus recording which build's
/// corpus is currently on disk. When the running binary's marker differs we
/// re-stage; when it matches we skip the (cheap but unnecessary) re-extract.
const VERSION_MARKER: &str = ".umadev-knowledge-version";

/// Environment variable that [`umadev_agent::phases::knowledge_root`] reads to
/// find the bundled corpus. Set in-process by [`ensure_staged`].
const KNOWLEDGE_DIR_ENV: &str = "UMADEV_KNOWLEDGE_DIR";
const STAGE_LOCK: &str = ".knowledge-stage.lock";
const MAX_STAGED_DEPTH: usize = 64;
const MAX_STAGED_NODES: usize = 100_000;
const MAX_STAGED_FILES: usize = 50_000;
const MAX_STAGED_BYTES: u64 = 512 * 1024 * 1024;

/// Build-time identity of the embedded corpus: the crate version plus the
/// embedded file count. Cheap to compute and changes whenever the shipped
/// corpus changes across a release, so a user upgrading UmaDev re-stages the
/// new corpus without an expensive content hash on every launch.
fn corpus_version() -> String {
    format!(
        "umadev={} files={}",
        env!("CARGO_PKG_VERSION"),
        count_files(&KNOWLEDGE)
    )
}

/// Recursively count embedded files (the cheap fingerprint half of the marker).
fn count_files(dir: &Dir<'_>) -> usize {
    let mut n = dir.files().count();
    for sub in dir.dirs() {
        n += count_files(sub);
    }
    n
}

/// Resolve `~/.umadev/knowledge` cross-platform, mirroring the binary's other
/// global-state lookups (`HOME` on unix, `USERPROFILE` on Windows). Returns
/// `None` when no home directory can be resolved — the fail-open caller then
/// stages nothing.
fn staged_root() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|h| !h.is_empty()))?;
    Some(PathBuf::from(home).join(".umadev").join("knowledge"))
}

/// Stage the embedded corpus to `~/.umadev/knowledge` (once per build) and point
/// `UMADEV_KNOWLEDGE_DIR` at it, so every `knowledge_root` consumer across the
/// TUI / CLI / director discovers the full curated KB with zero user setup.
///
/// Call this **early in `main`**, before any command dispatch. It is cheap on
/// the common path (an up-to-date marker → just sets the env var and returns).
///
/// **Fail-open**: any failure (no home dir, unwritable disk, partial extract)
/// is swallowed — knowledge recall then degrades to its prior empty behaviour
/// and startup proceeds. Never panics, never blocks.
pub fn ensure_staged() {
    let Some(root) = staged_root() else {
        // No home dir → cannot stage. Recall degrades to empty; do not crash.
        tracing::debug!("knowledge bundle: no home dir; skipping corpus staging");
        return;
    };
    let Some(home) = root.parent().and_then(Path::parent) else {
        return;
    };
    let Ok(home_root) = RootedDir::open(home) else {
        return;
    };
    if home_root.ensure_dir(Path::new(".umadev"), false).is_err() {
        return;
    }
    let Ok(umadev_root) = home_root.open_dir(Path::new(".umadev")) else {
        return;
    };

    let want = corpus_version();
    if !is_current_in(&umadev_root, &want) {
        let Ok(lock) = umadev_root.open_private_lock(Path::new(STAGE_LOCK), false) else {
            return;
        };
        if lock.try_lock_exclusive().is_err() {
            return;
        }
        // Another process may have completed staging between the first marker
        // read and lock acquisition.
        if !is_current_in(&umadev_root, &want) {
            if let Err(e) = stage_in(&umadev_root, Path::new("knowledge"), &want) {
                tracing::warn!("knowledge bundle: staging failed ({e}); knowledge recall disabled");
                return;
            }
            tracing::debug!("knowledge bundle: staged corpus to {}", root.display());
        }
    }

    // Point every downstream `knowledge_root` at the staged corpus. Only set it
    // when we actually have a populated dir, and don't clobber an explicit
    // user override (a power user pointing at their own corpus wins).
    let published = umadev_root
        .open_dir(Path::new("knowledge"))
        .and_then(|knowledge| knowledge.matches_path(&root))
        .unwrap_or(false);
    if published && std::env::var_os(KNOWLEDGE_DIR_ENV).is_none() {
        // Set once at startup before command dispatch, while no `knowledge_root`
        // consumer is running and the (idle) tokio workers touch no env — so this
        // races nothing. Every later `knowledge_root` then reads it via `var`.
        std::env::set_var(KNOWLEDGE_DIR_ENV, &root);
    }
}

/// Whether the staged corpus on disk already matches this build (marker hit).
fn is_current(root: &Path, want: &str) -> bool {
    RootedDir::open_no_follow(root)
        .ok()
        .and_then(|root| root.read_bounded(Path::new(VERSION_MARKER), 4 * 1024).ok())
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .is_some_and(|got| got.trim() == want)
}

fn is_current_in(root: &RootedDir, want: &str) -> bool {
    root.read_bounded(&Path::new("knowledge").join(VERSION_MARKER), 4 * 1024)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .is_some_and(|got| got.trim() == want)
}

/// Extract the embedded corpus fresh into `root`, then drop the version marker.
/// A stale tree is removed first so a shrinking corpus doesn't leave orphans.
fn stage(root: &Path, version: &str) -> std::io::Result<()> {
    let parent = root.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "knowledge staging root has no parent",
        )
    })?;
    let name = root.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "knowledge staging root has no name",
        )
    })?;
    let parent = RootedDir::open_no_follow(parent)?;
    stage_in(&parent, Path::new(name), version)
}

fn stage_in(root: &RootedDir, dest: &Path, version: &str) -> std::io::Result<()> {
    match root.is_real_dir(dest) {
        Ok(true) => remove_staged_tree(root, dest)?,
        Ok(false) => match root.regular_file_exists(dest) {
            Ok(false) => {}
            Ok(true) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "knowledge staging target is a regular file",
                ));
            }
            Err(error) => return Err(error),
        },
        Err(error) => return Err(error),
    }
    root.ensure_dir(dest, false)?;
    if let Err(error) = extract_dir(&KNOWLEDGE, root, dest)
        .and_then(|()| root.atomic_write(&dest.join(VERSION_MARKER), version.as_bytes(), false))
    {
        let _ = remove_staged_tree(root, dest);
        return Err(error);
    }
    Ok(())
}

fn remove_staged_tree(root: &RootedDir, relative: &Path) -> std::io::Result<()> {
    let files = root.list_regular_tree(
        relative,
        MAX_STAGED_DEPTH,
        MAX_STAGED_NODES,
        MAX_STAGED_FILES,
        MAX_STAGED_BYTES,
    )?;
    for file in files {
        root.remove_regular_file(&file.relative)?;
    }
    root.remove_empty_directory_tree(relative, MAX_STAGED_DEPTH, MAX_STAGED_NODES)?;
    Ok(())
}

/// Recursively write an embedded [`Dir`] beneath a pinned staging root.
fn extract_dir(dir: &Dir<'_>, root: &RootedDir, dest: &Path) -> std::io::Result<()> {
    for file in dir.files() {
        let Some(name) = file.path().file_name() else {
            continue;
        };
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        root.atomic_write(&dest.join(name), file.contents(), false)?;
    }
    for sub in dir.dirs() {
        let Some(name) = sub.path().file_name() else {
            continue;
        };
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let child = dest.join(name);
        root.ensure_dir(&child, true)?;
        extract_dir(sub, root, &child)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The corpus actually compiled in (catches a broken `include_dir!` path or
    /// an emptied tree before it ships as a silent no-op).
    #[test]
    fn embedded_corpus_is_non_empty() {
        assert!(
            count_files(&KNOWLEDGE) > 100,
            "embedded knowledge corpus looks empty/truncated: {} files",
            count_files(&KNOWLEDGE)
        );
        // A couple of load-bearing standards must be present so recall has real
        // commercial-engineering content, not just stray files.
        assert!(
            KNOWLEDGE
                .get_file("backend/01-standards/application-layering-and-packaging.md")
                .is_some(),
            "backend layering standard missing from embedded corpus"
        );
        assert!(
            KNOWLEDGE.get_dir(".umadev").is_none(),
            "generated knowledge indexes must never be embedded"
        );
    }

    /// Recursively assert no dotfile leaked into the staged tree.
    fn assert_no_dotfiles(d: &Path) {
        for e in std::fs::read_dir(d).unwrap().flatten() {
            let name = e.file_name();
            assert!(
                !name.to_string_lossy().starts_with('.'),
                "dotfile leaked into staged corpus: {:?}",
                e.path()
            );
            if e.path().is_dir() {
                assert_no_dotfiles(&e.path());
            }
        }
    }

    /// Extraction lands real files under a fresh root and skips dotfiles.
    #[test]
    fn extract_writes_corpus_and_skips_dotfiles() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("knowledge");
        std::fs::create_dir_all(&root).unwrap();
        let rooted = RootedDir::open_no_follow(&root).unwrap();
        extract_dir(&KNOWLEDGE, &rooted, Path::new("")).unwrap();

        // Files exist on disk.
        let staged = root.join("backend/01-standards/application-layering-and-packaging.md");
        assert!(staged.is_file(), "expected staged standard at {staged:?}");

        // No dotfiles leaked into the staged tree.
        assert_no_dotfiles(&root);
    }

    /// Staging writes the version marker and `is_current` then short-circuits.
    #[test]
    fn stage_then_is_current_skips_restage() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("knowledge");
        let want = corpus_version();

        assert!(!is_current(&root, &want), "empty root must not be current");
        stage(&root, &want).unwrap();
        assert!(
            is_current(&root, &want),
            "after staging, the marker must report current"
        );
        // A different build's marker forces a re-stage.
        assert!(!is_current(&root, "umadev=0.0.0 files=1"));
    }

    /// Re-staging over an existing tree removes orphaned files (shrinking corpus).
    #[test]
    fn restage_removes_orphans() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("knowledge");
        let want = corpus_version();
        stage(&root, &want).unwrap();

        let orphan = root.join("orphan-should-be-removed.md");
        std::fs::write(&orphan, "stale").unwrap();
        assert!(orphan.exists());

        stage(&root, &want).unwrap();
        assert!(!orphan.exists(), "re-stage must wipe orphaned files");
    }

    #[cfg(unix)]
    #[test]
    fn stage_refuses_a_symlink_staging_target() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = tmp.path().join("knowledge");
        let sentinel = outside.path().join("sentinel.txt");
        std::fs::write(&sentinel, "keep").unwrap();
        symlink(outside.path(), &target).unwrap();

        assert!(stage(&target, &corpus_version()).is_err());
        assert_eq!(std::fs::read_to_string(sentinel).unwrap(), "keep");
        assert!(
            !outside.path().join(VERSION_MARKER).exists(),
            "staging must not write through a linked target"
        );
    }
}
