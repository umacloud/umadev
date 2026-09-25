use std::path::Path;
use std::process::Command;
use std::time::Duration;
use umadev_process::git::{hardened_git_command, GitAccess};

const STDERR_BYTES: usize = 256 * 1024;
const READER_GRACE: Duration = Duration::from_secs(1);

/// Run `git` in `root` through the shared hardened builder: the repository's
/// hooks, fsmonitor and filter drivers never run (see [`umadev_process::git`]).
/// `None` on any failure, including a repository config that cannot be read.
pub(crate) fn bounded_git_output(
    root: &Path,
    access: GitAccess,
    args: &[&str],
    timeout: Duration,
    stdout_bytes: usize,
) -> Option<std::process::Output> {
    let mut command = hardened_git_command(root, access).ok()?;
    command.args(args);
    bounded_output(command, timeout, stdout_bytes)
}

pub(crate) fn bounded_gh_output(
    root: &Path,
    args: &[&str],
    timeout: Duration,
    stdout_bytes: usize,
) -> Option<std::process::Output> {
    let mut command = Command::new("gh");
    command
        .args(args)
        .current_dir(root)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("GH_PAGER", "cat")
        .env("NO_COLOR", "1")
        .env("CLICOLOR", "0")
        .env("TERM", "dumb");
    bounded_output(command, timeout, stdout_bytes)
}

fn bounded_output(
    command: Command,
    timeout: Duration,
    stdout_bytes: usize,
) -> Option<std::process::Output> {
    let output = umadev_process::run_bounded_std_command(
        command,
        umadev_process::BoundedCommandOptions {
            timeout,
            stdout_bytes,
            stderr_bytes: STDERR_BYTES,
            reader_grace: READER_GRACE,
        },
    )
    .ok()?;
    if output.timed_out
        || output.stdout_truncated
        || output.stderr_truncated
        || output.status.is_none()
    {
        return None;
    }
    Some(std::process::Output {
        status: output.status?,
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::bounded_output;
    use std::process::Command;
    use std::time::Duration;

    #[cfg(unix)]
    #[test]
    fn wrapper_rejects_floods_and_hanging_descendants() {
        let mut flood = Command::new("sh");
        flood.args(["-c", "head -c 65536 /dev/zero"]);
        assert!(bounded_output(flood, Duration::from_secs(2), 1024).is_none());

        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("descendant-survived");
        let mut hang = Command::new("sh");
        hang.env("UMADEV_TEST_MARKER", &marker)
            .args(["-c", "(sleep 1; : > \"$UMADEV_TEST_MARKER\") & sleep 30"]);
        assert!(bounded_output(hang, Duration::from_millis(150), 1024).is_none());
        std::thread::sleep(Duration::from_millis(1_100));
        assert!(!marker.exists());
    }

    #[test]
    fn nonzero_exit_is_preserved_as_failure_status() {
        let mut command = Command::new("git");
        command.arg("--definitely-not-a-real-option");
        let output = bounded_output(command, Duration::from_secs(5), 8 * 1024).unwrap();
        assert!(!output.status.success());
    }
}
