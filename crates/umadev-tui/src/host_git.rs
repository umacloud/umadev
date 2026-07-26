use std::path::{Path, PathBuf};

use crate::execution_postcondition::{ResidentExecutionBlocked, ResidentExecutionPostcondition};

/// Execute one ordinary Git commit as a host-owned, repository-validated
/// transaction without opening or consulting an AI base.
///
/// Compound commit+test/push/edit/amend requests are rejected. The caller owns
/// trust-mode approval; this boundary owns the cross-process writer lock,
/// frozen baseline, fixed Git invocation, rollback, and final tree validation.
pub async fn execute_host_git_commit(
    project_root: &Path,
    objective: &str,
) -> std::result::Result<String, String> {
    let Some(request) = umadev_agent::parse_host_git_commit_request(objective) else {
        return Err(umadev_i18n::tl("intent.git_commit_host_boundary").to_string());
    };
    if request.verifier.is_some() {
        return Err(umadev_i18n::tl("intent.git_commit_host_boundary").to_string());
    }
    let commit_text = request.commit_text;
    // Freeze one physical repository identity before acquiring the writer lock.
    // Every later read/write uses this owned canonical path, so retargeting a
    // symlink used to launch UmaDev cannot move the transaction to another repo.
    let project_root = freeze_project_root(project_root)?;
    if let Some(note) = umadev_agent::checkpoint::workspace_in_past_note(&project_root) {
        return Err(note);
    }
    let _run_lock = match umadev_agent::run_lock::RunLock::acquire_for_run(&project_root) {
        Ok(guard) if guard.is_owned() => guard,
        Ok(_) => {
            return Err(umadev_i18n::tlf(
                "intent.writer_lock_blocked",
                &["exclusive writer-lock ownership could not be proven"],
            ))
        }
        Err(error) => {
            return Err(umadev_i18n::tlf(
                "intent.writer_lock_blocked",
                &[&error.to_string()],
            ))
        }
    };
    let route = umadev_agent::deterministic_route(&commit_text);
    let guard = ResidentExecutionPostcondition::capture(&project_root, &route, &commit_text)
        .map_err(ResidentExecutionBlocked::into_note)?;
    guard
        .execute_git_commit(&project_root, &commit_text)
        .await
        .map(|receipt| receipt.reply())
        .map_err(ResidentExecutionBlocked::into_note)
}

fn freeze_project_root(project_root: &Path) -> std::result::Result<PathBuf, String> {
    std::fs::canonicalize(project_root).map_err(|error| {
        umadev_i18n::tlf(
            "intent.writer_lock_blocked",
            &[&format!(
                "workspace identity could not be frozen before Git commit: {error}"
            )],
        )
    })
}

#[cfg(unix)]
#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;
    use std::process::Command;

    use super::*;

    #[test]
    fn frozen_project_root_cannot_follow_a_later_symlink_retarget() {
        let temp = tempfile::tempdir().expect("tempdir");
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        let alias = temp.path().join("workspace");
        std::fs::create_dir_all(&first).expect("first repo");
        std::fs::create_dir_all(&second).expect("second repo");
        symlink(&first, &alias).expect("initial alias");

        let frozen = freeze_project_root(&alias).expect("freeze identity");
        std::fs::remove_file(&alias).expect("remove initial alias");
        symlink(&second, &alias).expect("retarget alias");

        assert_eq!(frozen, std::fs::canonicalize(&first).unwrap());
        assert_ne!(frozen, std::fs::canonicalize(&alias).unwrap());
    }

    #[tokio::test]
    async fn host_commit_refuses_a_workspace_parked_in_the_past() {
        let repo = tempfile::tempdir().expect("tempdir");
        git(repo.path(), &["init", "-q"]);
        git(
            repo.path(),
            &["config", "user.email", "umadev-test@example.invalid"],
        );
        git(repo.path(), &["config", "user.name", "UmaDev Test"]);
        std::fs::write(repo.path().join("tracked.txt"), "before\n").unwrap();
        git(repo.path(), &["add", "--", "tracked.txt"]);
        git(repo.path(), &["commit", "-q", "-m", "seed"]);
        std::fs::write(repo.path().join("tracked.txt"), "after\n").unwrap();
        let before = git_text(repo.path(), &["rev-parse", "HEAD"]);

        umadev_agent::checkpoint::mark_workspace_in_past(
            repo.path(),
            umadev_agent::checkpoint::InPastReason::Retryable,
        );
        let note = execute_host_git_commit(repo.path(), "提交git记录")
            .await
            .expect_err("a rewound workspace must not be committed");
        umadev_agent::checkpoint::clear_workspace_in_past(repo.path());

        assert_eq!(note, umadev_i18n::tl("checkpoint.workspace_in_past_halt"));
        assert_eq!(git_text(repo.path(), &["rev-parse", "HEAD"]), before);
    }

    fn git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_text(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }
}
