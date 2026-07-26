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
    ResidentExecutionBlocked {
        note: format!(
            "[blocked] 无法完整核对本轮工作区内容指纹,因此不能标记成功 / unable to \
             verify the complete workspace content fingerprint; this turn cannot be marked \
             successful: {error}"
        ),
    }
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

/// Async hot-path snapshot used before and after ordinary resident turns. Git
/// owns a dedicated process tree, has a hard deadline, and drains only bounded
/// output; an incomplete snapshot is discarded instead of being treated as a
/// truthful partial status.
pub(crate) async fn git_status_porcelain_bounded(root: &Path) -> Option<String> {
    let mut command = tokio::process::Command::new("git");
    command.arg("-C").arg(root).args(["status", "--porcelain"]);
    run_git_status_command(command, git_status_options()).await
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
    use super::{changed_files_after_git_status, run_git_status_command, GIT_STATUS_READER_GRACE};
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
}
