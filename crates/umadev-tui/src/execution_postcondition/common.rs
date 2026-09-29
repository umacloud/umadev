use super::{Duration, Path, WorkspaceSnapshotError};

pub(crate) const MAX_FACT_PATHS: usize = 20;

/// A blocking inability to prove the resident turn satisfied its contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResidentExecutionBlocked {
    pub(crate) note: String,
}

impl ResidentExecutionBlocked {
    /// User-visible terminal failure note.
    pub(crate) fn into_note(self) -> String {
        self.note
    }
}

pub(crate) fn git_commit_blocked(code: &'static str, detail: &str) -> ResidentExecutionBlocked {
    ResidentExecutionBlocked {
        note: format!(
            "[blocked] Git 仅提交契约未通过 [{code}]: {detail}; this turn cannot be marked successful"
        ),
    }
}

pub(crate) fn combined_git_failure(
    code: &'static str,
    primary: &ResidentExecutionBlocked,
    recovery: &ResidentExecutionBlocked,
    detail: &str,
) -> ResidentExecutionBlocked {
    ResidentExecutionBlocked {
        note: format!(
            "[blocked] Git 仅提交契约未通过 [{code}]: {detail}\n原始失败: {}\n恢复失败: {}",
            primary.note, recovery.note
        ),
    }
}

pub(crate) fn git_mutation_timeout() -> Duration {
    std::env::var("UMADEV_GIT_COMMIT_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(|seconds| seconds.clamp(1, 600))
        .map_or_else(|| Duration::from_secs(120), Duration::from_secs)
}

#[cfg(unix)]
pub(crate) fn same_permissions(left: &std::fs::Permissions, right: &std::fs::Permissions) -> bool {
    use std::os::unix::fs::PermissionsExt;
    left.mode() == right.mode()
}

#[cfg(not(unix))]
pub(crate) fn same_permissions(left: &std::fs::Permissions, right: &std::fs::Permissions) -> bool {
    left.readonly() == right.readonly()
}

pub(crate) fn safe_display_path(path: &str) -> String {
    const MAX_PATH_CHARS: usize = 320;
    let mut output = String::new();
    for (index, character) in umadev_agent::base_error::strip_ansi(path)
        .chars()
        .enumerate()
    {
        if index >= MAX_PATH_CHARS {
            output.push('…');
            break;
        }
        output.push(if character.is_control() {
            '�'
        } else {
            character
        });
    }
    output
}

pub(crate) fn display_paths(paths: &[String]) -> String {
    let mut shown = paths
        .iter()
        .take(MAX_FACT_PATHS)
        .map(|path| safe_display_path(path))
        .collect::<Vec<_>>()
        .join(", ");
    if paths.len() > MAX_FACT_PATHS {
        shown.push_str(&format!(" ... (+{})", paths.len() - MAX_FACT_PATHS));
    }
    shown
}

pub(crate) fn snapshot_blocked(error: WorkspaceSnapshotError) -> ResidentExecutionBlocked {
    // A size ceiling is the one snapshot failure the user can fix in place.
    let hint = if matches!(error, WorkspaceSnapshotError::Limit(_)) {
        "\n提示:把构建产物、依赖、数据等大目录写进项目的 .gitignore(不是 Git 仓库也生效)\
         后重试;只需分析时可先切换到 /mode plan / hint: add large build-output, dependency, \
         or data directories to the project's .gitignore (honored without a Git repository \
         too) and retry; for analysis only, switch to /mode plan"
    } else {
        ""
    };
    snapshot_blocked_with_hint(&error, hint)
}

/// [`snapshot_blocked`] for the capture at `root`. When `root` is the user's
/// home directory or a filesystem root, the launch directory itself is the
/// problem, so the note says to start from the project folder instead.
pub(crate) fn snapshot_blocked_at(
    root: &Path,
    error: WorkspaceSnapshotError,
) -> ResidentExecutionBlocked {
    snapshot_blocked_in(root, crate::config::home_dir().as_deref(), error)
}

pub(crate) fn snapshot_blocked_in(
    root: &Path,
    home: Option<&Path>,
    error: WorkspaceSnapshotError,
) -> ResidentExecutionBlocked {
    if !is_broad_launch_directory(root, home) {
        return snapshot_blocked(error);
    }
    let shown = safe_display_path(&root.display().to_string());
    let hint = format!(
        "\n提示:UmaDev 把启动目录当作项目,当前启动目录 `{shown}` 是用户主目录或磁盘根目录,\
         其下所有文件都会被计入。请在项目文件夹中启动 umadev(VS Code:文件 → 打开文件夹;\
         终端:先 cd 到项目目录) / hint: UmaDev treats its launch directory as the project, \
         and `{shown}` is your home directory or a drive root, so everything below it is in \
         scope. Start umadev from the project folder instead (VS Code: File > Open Folder; \
         a terminal: cd into the project first)"
    );
    snapshot_blocked_with_hint(&error, &hint)
}

fn snapshot_blocked_with_hint(
    error: &WorkspaceSnapshotError,
    hint: &str,
) -> ResidentExecutionBlocked {
    ResidentExecutionBlocked {
        note: format!(
            "[blocked] 无法完整核对本轮工作区内容指纹,因此不能标记成功 / unable to \
             verify the complete workspace content fingerprint; this turn cannot be marked \
             successful: {error}{hint}"
        ),
    }
}

/// Whether `root` is too broad to be one project: the user's home directory or
/// a filesystem root. VS Code's terminal opens in the home directory when no
/// folder is open, so starting UmaDev there is an easy mistake.
fn is_broad_launch_directory(root: &Path, home: Option<&Path>) -> bool {
    let Ok(root) = std::fs::canonicalize(root) else {
        return false;
    };
    root.parent().is_none()
        || home
            .and_then(|home| std::fs::canonicalize(home).ok())
            .is_some_and(|home| home == root)
}

/// Snapshot the working tree as `git status --porcelain` for legacy reality
/// prompt/fact rendering. Execution-contract enforcement uses the stronger
/// content-fingerprint baseline above.
const GIT_STATUS_TIMEOUT: Duration = Duration::from_secs(5);
const GIT_STATUS_STDOUT_BYTES: usize = 256 * 1024;
const GIT_STATUS_STDERR_BYTES: usize = 16 * 1024;
const GIT_STATUS_READER_GRACE: Duration = Duration::from_millis(500);

fn git_status_options() -> umadev_process::BoundedCommandOptions {
    umadev_process::BoundedCommandOptions {
        timeout: GIT_STATUS_TIMEOUT,
        stdout_bytes: GIT_STATUS_STDOUT_BYTES,
        stderr_bytes: GIT_STATUS_STDERR_BYTES,
        reader_grace: GIT_STATUS_READER_GRACE,
    }
}

async fn run_git_status_command(
    command: tokio::process::Command,
    options: umadev_process::BoundedCommandOptions,
) -> Option<String> {
    let output = umadev_process::run_bounded_command(command, options)
        .await
        .ok()?;
    if output.timed_out
        || output.stdout_truncated
        || !output.status.is_some_and(|status| status.success())
    {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// A read-only `git` invocation for the automatic working-tree snapshots taken
/// around every turn. These run without the user asking, so a repository that
/// arrived with its `.git/config` (an archive or shared folder rather than a
/// clone) must not be able to execute anything through it: fsmonitor, hooks and
/// filter drivers are all disabled (see [`umadev_process::git`]), and a
/// background snapshot never takes the index lock the base may need. `None`
/// when the repository configuration cannot be inspected; the snapshot is then
/// skipped like any other failed one.
async fn git_snapshot_command(root: &Path) -> Option<tokio::process::Command> {
    umadev_process::git::hardened_git_tokio_command(root, umadev_process::git::GitAccess::ReadOnly)
        .await
        .ok()
}

/// Async hot-path snapshot used before and after ordinary resident turns. Git
/// owns a dedicated process tree, has a hard deadline, and drains only bounded
/// output; an incomplete snapshot is discarded instead of being treated as a
/// truthful partial status.
pub(crate) async fn git_status_porcelain_bounded(root: &Path) -> Option<String> {
    let mut command = git_snapshot_command(root).await?;
    command.args([
        "status",
        "--porcelain",
        umadev_process::git::IGNORE_DIRTY_SUBMODULES,
    ]);
    run_git_status_command(command, git_status_options()).await
}

/// A compact `git diff --stat` of the working tree (unstaged changes), run in
/// `root`, used only to give the agentic system prompt a sense of what is
/// already modified. **Fail-open**: any failure returns `None` and the prompt
/// simply omits the diff-stat section.
pub(crate) async fn git_diff_stat(root: &Path) -> Option<String> {
    let mut command = git_snapshot_command(root).await?;
    command
        .args([
            "diff",
            "--stat",
            umadev_process::git::IGNORE_DIRTY_SUBMODULES,
        ])
        .args(umadev_process::git::NO_DIFF_PROGRAMS);
    let stat = run_git_status_command(command, git_status_options()).await?;
    let stat = stat.trim();
    (!stat.is_empty()).then(|| stat.to_string())
}

/// Compare a prior complete status with a fresh bounded snapshot. Any missing,
/// timed-out, or truncated side stays `None`, so callers never label a partial
/// repository view as the turn's real changed-file set.
pub(crate) async fn changed_files_after_git_status(
    before: Option<&str>,
    root: &Path,
) -> Option<Vec<String>> {
    let before = before?;
    let after = git_status_porcelain_bounded(root).await?;
    Some(changed_files_between(before, &after))
}

pub(crate) fn porcelain_path(line: &str) -> Option<String> {
    let trimmed = line.strip_prefix('\u{feff}').unwrap_or(line);
    if trimmed.trim().is_empty() {
        return None;
    }
    let rest = trimmed.get(3..).unwrap_or("").trim();
    if rest.is_empty() {
        return None;
    }
    let path = rest
        .rsplit(" -> ")
        .next()
        .unwrap_or(rest)
        .trim()
        .trim_matches('"');
    (!path.is_empty()).then(|| path.to_string())
}

/// Diff two legacy porcelain snapshots for transcript fact rendering.
pub(crate) fn changed_files_between(before: &str, after: &str) -> Vec<String> {
    use std::collections::{BTreeMap, BTreeSet};

    let parse = |snapshot: &str| -> BTreeMap<String, String> {
        snapshot
            .lines()
            .filter_map(|line| porcelain_path(line).map(|path| (path, line.trim_end().to_string())))
            .collect()
    };
    let before = parse(before);
    let after = parse(after);
    let mut changed = BTreeSet::new();
    for (path, line) in &after {
        if before.get(path).map(String::as_str) != Some(line.as_str()) {
            changed.insert(path.clone());
        }
    }
    for path in before.keys() {
        if !after.contains_key(path) {
            changed.insert(path.clone());
        }
    }
    changed.into_iter().collect()
}

/// Build the reality-anchored fact line shown after an agentic turn.
pub(crate) fn agentic_fact_line(changed: Option<&[String]>, claimed: bool) -> Option<String> {
    let changed = changed?;
    if changed.is_empty() {
        return Some(if claimed {
            "[note] 本轮无文件变更\n[warn] 底座报告了改动,但工作区没有实际文件变更 —— \
             可能未真正落盘或为复述,请核对 / base reported changes but the working \
             tree is unchanged — verify before trusting"
                .to_string()
        } else {
            "[note] 本轮无文件变更 / no file changes this turn".to_string()
        });
    }
    let mut list = changed
        .iter()
        .take(MAX_FACT_PATHS)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    if changed.len() > MAX_FACT_PATHS {
        list.push_str(&format!(" ... (+{})", changed.len() - MAX_FACT_PATHS));
    }
    Some(format!("[note] 本轮实际文件变更: {list}"))
}

#[cfg(all(test, unix))]
mod tests {
    use super::{
        changed_files_after_git_status, git_diff_stat, git_status_porcelain_bounded,
        run_git_status_command, GIT_STATUS_READER_GRACE,
    };
    use std::time::{Duration, Instant};

    fn options(timeout: Duration, stdout_bytes: usize) -> umadev_process::BoundedCommandOptions {
        umadev_process::BoundedCommandOptions {
            timeout,
            stdout_bytes,
            stderr_bytes: 1_024,
            reader_grace: GIT_STATUS_READER_GRACE,
        }
    }

    #[tokio::test]
    async fn bounded_git_status_accepts_complete_output_and_reaps_descendants() {
        let mut command = tokio::process::Command::new("sh");
        command.args(["-c", "sleep 30 & printf ' M file.txt\\n'; exit 0"]);
        let started = Instant::now();
        let status = run_git_status_command(command, options(Duration::from_secs(3), 1_024)).await;
        assert_eq!(status.as_deref(), Some(" M file.txt\n"));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "git-status helper waited for a descendant that inherited its pipes"
        );
    }

    #[tokio::test]
    async fn bounded_git_status_discards_timeout_and_truncated_output() {
        let mut slow = tokio::process::Command::new("sh");
        slow.args(["-c", "printf partial; sleep 30"]);
        assert!(
            run_git_status_command(slow, options(Duration::from_millis(50), 1_024))
                .await
                .is_none(),
            "a timed-out status must fail open, never expose a partial snapshot"
        );

        let mut flood = tokio::process::Command::new("sh");
        flood.args(["-c", "head -c 4096 /dev/zero | tr '\\0' x"]);
        assert!(
            run_git_status_command(flood, options(Duration::from_secs(3), 32))
                .await
                .is_none(),
            "a capped status must fail open, never expose a truncated snapshot"
        );
    }

    #[tokio::test]
    async fn status_diff_requires_a_complete_before_snapshot() {
        assert!(
            changed_files_after_git_status(None, std::path::Path::new("."))
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn automatic_status_snapshot_never_runs_repository_fsmonitor() {
        let repo = tempfile::TempDir::new().unwrap();
        let marker = repo.path().join("fsmonitor-ran");
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(repo.path())
                .args(args)
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "--quiet"]);
        let hook = format!("touch '{}'; exit 1", marker.display());
        git(&["config", "core.fsmonitor", &hook]);
        std::fs::write(repo.path().join("file.txt"), "x").unwrap();

        let status = git_status_porcelain_bounded(repo.path()).await;
        assert!(status.is_some_and(|out| out.contains("file.txt")));
        assert!(!marker.exists(), "repository core.fsmonitor was executed");
    }

    #[tokio::test]
    async fn automatic_snapshots_never_run_repository_filters_or_hooks() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let git = |args: &[&str]| {
            let mut command = std::process::Command::new("git");
            umadev_process::git::remove_git_environment(&mut command);
            let status = command
                .arg("-C")
                .arg(&repo)
                .args(["-c", "user.name=UmaDev", "-c", "user.email=umadev@local"])
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "--quiet"]);
        std::fs::write(repo.join(".gitattributes"), "* filter=x diff=x\n").unwrap();
        std::fs::write(repo.join("file.txt"), "one\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "--quiet", "--no-verify", "-m", "initial"]);
        for (key, marker) in [
            ("filter.x.clean", "clean-ran"),
            ("filter.x.process", "process-ran"),
            ("diff.x.textconv", "textconv-ran"),
        ] {
            let program = format!("touch '{}'; cat", temp.path().join(marker).display());
            git(&["config", key, &program]);
        }
        let hook = repo.join(".git/hooks/post-index-change");
        let script = format!(
            "#!/bin/sh\ntouch '{}'\n",
            temp.path().join("hook-ran").display()
        );
        std::fs::write(&hook, script).unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Stat-dirty and content-dirty: Git must re-read the file on every scan.
        std::fs::write(repo.join("file.txt"), "two\n").unwrap();

        let status = git_status_porcelain_bounded(&repo).await;
        assert!(status.is_some_and(|out| out.contains("file.txt")));
        let stat = git_diff_stat(&repo).await;
        assert!(stat.is_some_and(|out| out.contains("file.txt")));
        let ran = std::fs::read_dir(temp.path())
            .unwrap()
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .filter(|name| name.ends_with("-ran"))
            .collect::<Vec<_>>();
        assert!(ran.is_empty(), "repository programs executed: {ran:?}");
    }
}
