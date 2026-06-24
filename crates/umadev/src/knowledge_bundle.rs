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
//! The whole `knowledge/` tree (~5.5M of markdown, text-only) is embedded into
//! the binary at compile time via [`include_dir`]. On binary startup
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

use include_dir::{include_dir, Dir};
use std::path::{Path, PathBuf};

/// The embedded corpus. Compiled in from the repo's `knowledge/` tree.
/// Text-only (markdown / yaml), so this stays cheap to compile and ~5.5M on
/// disk; the `.DS_Store` noise is stripped from the tree before release.
static KNOWLEDGE: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/../../knowledge");

/// Marker filename written into the staged corpus recording which build's
/// corpus is currently on disk. When the running binary's marker differs we
/// re-stage; when it matches we skip the (cheap but unnecessary) re-extract.
const VERSION_MARKER: &str = ".umadev-knowledge-version";

/// Environment variable that [`umadev_agent::phases::knowledge_root`] reads to
/// find the bundled corpus. Set in-process by [`ensure_staged`].
const KNOWLEDGE_DIR_ENV: &str = "UMADEV_KNOWLEDGE_DIR";

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

    let want = corpus_version();
    if !is_current(&root, &want) {
        if let Err(e) = stage(&root, &want) {
            // Best-effort: a failed extract leaves recall empty but never
            // blocks the binary. Wipe a half-written tree so the env var below
            // doesn't point the index at a corrupt corpus.
            tracing::warn!("knowledge bundle: staging failed ({e}); knowledge recall disabled");
            let _ = std::fs::remove_dir_all(&root);
            return;
        }
        tracing::debug!("knowledge bundle: staged corpus to {}", root.display());
    }

    // Point every downstream `knowledge_root` at the staged corpus. Only set it
    // when we actually have a populated dir, and don't clobber an explicit
    // user override (a power user pointing at their own corpus wins).
    if root.is_dir() && std::env::var_os(KNOWLEDGE_DIR_ENV).is_none() {
        // Set once at startup before command dispatch, while no `knowledge_root`
        // consumer is running and the (idle) tokio workers touch no env — so this
        // races nothing. Every later `knowledge_root` then reads it via `var`.
        std::env::set_var(KNOWLEDGE_DIR_ENV, &root);
    }
}

/// Whether the staged corpus on disk already matches this build (marker hit).
fn is_current(root: &Path, want: &str) -> bool {
    std::fs::read_to_string(root.join(VERSION_MARKER)).is_ok_and(|got| got.trim() == want)
}

/// Extract the embedded corpus fresh into `root`, then drop the version marker.
/// A stale tree is removed first so a shrinking corpus doesn't leave orphans.
fn stage(root: &Path, version: &str) -> std::io::Result<()> {
    // Start clean: a previous (stale or partial) corpus is removed wholesale so
    // renamed/deleted files in a new build don't linger.
    if root.exists() {
        std::fs::remove_dir_all(root)?;
    }
    std::fs::create_dir_all(root)?;
    extract_dir(&KNOWLEDGE, root)?;
    // Marker written LAST: if anything above failed we never claim "current",
    // so the next launch re-stages.
    std::fs::write(root.join(VERSION_MARKER), version)?;
    Ok(())
}

/// Recursively write an embedded [`Dir`] under `dest`. Skips dotfiles (e.g. a
/// stray `.DS_Store`) so editor/OS noise never lands in the staged corpus.
fn extract_dir(dir: &Dir<'_>, dest: &Path) -> std::io::Result<()> {
    for file in dir.files() {
        let Some(name) = file.path().file_name() else {
            continue;
        };
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        std::fs::write(dest.join(name), file.contents())?;
    }
    for sub in dir.dirs() {
        let Some(name) = sub.path().file_name() else {
            continue;
        };
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let child = dest.join(name);
        std::fs::create_dir_all(&child)?;
        extract_dir(sub, &child)?;
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
        extract_dir(&KNOWLEDGE, &root).unwrap();

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
}
