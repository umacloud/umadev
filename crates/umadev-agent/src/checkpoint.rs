//! File-level checkpoint / rewind over the project workspace.
//!
//! Distinct from [`crate::state`] snapshots (which capture only the workflow
//! *phase*): a checkpoint is a full snapshot of the FILES the base wrote, so the
//! user can rewind a whole phase's worth of work — the feature every hot AI
//! coding tool ships (Cline shadow-git, Claude Code `/rewind`, Aider `/undo`).
//!
//! Implemented as a SHADOW git repo at `.umadev/checkpoints.git` (a separate
//! `--git-dir`, work-tree = project root) so it never touches the user's own
//! `.git` history. Every function is FAIL-OPEN: if `git` is missing or any step
//! fails it returns `None` / `Err` quietly and the pipeline is never blocked.
//!
//! This is the director's checkpoint — a phase-level safety net that composes
//! with (does not replace) a base CLI's own per-edit checkpointing.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Heavy paths excluded from checkpoints even without a project `.gitignore`, so
/// a checkpoint never tries to snapshot `node_modules`, build output, or the
/// user's real `.git`. Written to the shadow repo's `info/exclude` on init.
const SHADOW_EXCLUDES: &[&str] = &[
    "/.git/",
    "node_modules/",
    "target/",
    "dist/",
    "build/",
    ".next/",
    ".nuxt/",
    ".output/",
    ".venv/",
    "__pycache__/",
    ".umadev/",
    "*.log",
];

/// One checkpoint entry, newest-first in [`list_checkpoints`].
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Checkpoint {
    /// Short commit id — the handle passed to [`restore_checkpoint`].
    pub id: String,
    /// Human label (usually the phase or reason).
    pub label: String,
    /// ISO-8601 commit time.
    pub when: String,
}

/// Path to the shadow git directory.
fn git_dir(project_root: &Path) -> PathBuf {
    project_root.join(".umadev").join("checkpoints.git")
}

/// Run a shadow-git command (work-tree = project root). `None` if `git` can't be
/// spawned at all.
fn git(project_root: &Path, args: &[&str]) -> Option<std::process::Output> {
    Command::new("git")
        .arg("--git-dir")
        .arg(git_dir(project_root))
        .arg("--work-tree")
        .arg(project_root)
        .args(args)
        .output()
        .ok()
}

/// `true` when at least one checkpoint exists.
#[must_use]
pub fn has_checkpoints(project_root: &Path) -> bool {
    git_dir(project_root).join("HEAD").exists()
        && git(project_root, &["rev-parse", "--verify", "-q", "HEAD"])
            .is_some_and(|o| o.status.success())
}

/// Initialise the shadow repo on first use. Returns `false` (fail-open) when
/// `git` is missing or init fails.
fn ensure_init(project_root: &Path) -> bool {
    let gd = git_dir(project_root);
    if gd.join("HEAD").exists() {
        return true;
    }
    if std::fs::create_dir_all(&gd).is_err() {
        return false;
    }
    let ok = git(project_root, &["init", "-q"]).is_some_and(|o| o.status.success());
    if ok {
        // The shadow repo's own ignore file — keeps heavy dirs out of every
        // checkpoint regardless of whether the project ships a `.gitignore`.
        let info = gd.join("info");
        let _ = std::fs::create_dir_all(&info);
        let _ = std::fs::write(info.join("exclude"), SHADOW_EXCLUDES.join("\n"));
    }
    ok
}

/// Snapshot the whole workspace as a checkpoint labelled `label`. Returns the
/// short commit id, or `None` (fail-open) if `git` is unavailable. An empty
/// snapshot (nothing changed) still produces a checkpoint via `--allow-empty`.
#[must_use]
pub fn create_checkpoint(project_root: &Path, label: &str) -> Option<String> {
    if !ensure_init(project_root) {
        return None;
    }
    git(project_root, &["add", "-A"])?;
    git(
        project_root,
        &[
            "-c",
            "user.email=umadev@local",
            "-c",
            "user.name=UmaDev",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            label,
        ],
    )?;
    let head = git(project_root, &["rev-parse", "--short", "HEAD"])?;
    head.status
        .success()
        .then(|| String::from_utf8_lossy(&head.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// List checkpoints, newest first (capped at 50). Empty when none / `git`
/// missing.
#[must_use]
pub fn list_checkpoints(project_root: &Path) -> Vec<Checkpoint> {
    // `--all` so checkpoints that a rewind moved off the linear HEAD history
    // (the pre-rewind snapshot + any forward checkpoints, preserved under a
    // `umadev-saved-*` ref by restore_checkpoint) are still listed.
    let Some(out) = git(
        project_root,
        &["log", "--all", "--pretty=%h%x1f%cI%x1f%s", "-n", "50"],
    ) else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '\u{1f}');
            Some(Checkpoint {
                id: parts.next()?.to_string(),
                when: parts.next()?.to_string(),
                label: parts.next().unwrap_or("").to_string(),
            })
        })
        .collect()
}

/// Rewind the workspace files to checkpoint `id` (`git reset --hard`). The
/// CURRENT state is auto-checkpointed first, so a rewind is itself undoable.
/// Untracked / excluded paths (`node_modules`, …) are left untouched.
///
/// # Errors
/// Returns `Err` with a human message when no checkpoints exist, `git` is
/// unavailable, or the id is unknown.
pub fn restore_checkpoint(project_root: &Path, id: &str) -> Result<(), String> {
    if !has_checkpoints(project_root) {
        return Err("还没有任何检查点可回滚".to_string());
    }
    // Make the rewind reversible AND keep forward checkpoints reachable: snapshot
    // the present, then anchor that snapshot (whose history includes every
    // checkpoint newer than `id`) under a `umadev-saved-<sha>` ref BEFORE the
    // reset. Otherwise `reset --hard` would orphan them and `list_checkpoints`
    // (which uses `git log --all`) couldn't show them.
    let _ = create_checkpoint(project_root, &format!("auto: 回滚到 {id} 前的现场"));
    if let Some(sha) = git(project_root, &["rev-parse", "--short", "HEAD"])
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
    {
        let branch = format!("umadev-saved-{sha}");
        let _ = git(project_root, &["branch", "-f", &branch, "HEAD"]);
    }
    let out = git(project_root, &["reset", "--hard", id]).ok_or("git 不可用")?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    #[test]
    fn create_list_restore_roundtrip() {
        if !git_available() {
            return; // fail-open environment without git — nothing to assert
        }
        let tmp = tempfile::TempDir::new().expect("tmp");
        let root = tmp.path();
        std::fs::write(root.join("a.txt"), "v1").unwrap();
        let c1 = create_checkpoint(root, "phase: frontend").expect("checkpoint 1");

        // Mutate + add a file, then checkpoint again.
        std::fs::write(root.join("a.txt"), "v2").unwrap();
        std::fs::write(root.join("b.txt"), "new").unwrap();
        let _c2 = create_checkpoint(root, "phase: backend").expect("checkpoint 2");

        let list = list_checkpoints(root);
        assert_eq!(list.len(), 2, "two checkpoints, newest first");
        assert_eq!(list[0].label, "phase: backend");

        // Rewind to c1: a.txt back to v1, b.txt removed.
        restore_checkpoint(root, &c1).expect("restore");
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "v1");
        assert!(!root.join("b.txt").exists(), "file added after c1 is gone");

        // The rewind must NOT lose history: the forward checkpoint ("phase:
        // backend") and the auto pre-rewind snapshot are still listed (preserved
        // under a umadev-saved-* ref), so the rewind itself stays undoable.
        let after = list_checkpoints(root);
        assert!(
            after.iter().any(|c| c.label == "phase: backend"),
            "forward checkpoint survives the rewind"
        );
        assert!(
            after.iter().any(|c| c.label.contains("回滚到")),
            "pre-rewind snapshot is reachable"
        );
    }

    #[test]
    fn restore_without_checkpoints_errors() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        assert!(restore_checkpoint(tmp.path(), "deadbeef").is_err());
    }

    #[test]
    fn has_checkpoints_false_on_empty() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        assert!(!has_checkpoints(tmp.path()));
    }
}
